//! Docker and Kubernetes through the user's own `docker` and `kubectl` CLIs.
//!
//! No SDK or API client: every call is one CLI invocation with JSON output
//! (`docker ps --format '{{json .}}'`, `kubectl get … -o json`), so the user's contexts,
//! credential helpers, kubeconfig plugins (`aws eks get-token`, `gke-gcloud-auth-plugin`)
//! and proxies work as they do in their shell. This crate builds the argument lists
//! ([`Invocation`]), parses the output into typed rows ([`docker`], [`kube`]) and runs the
//! CLIs on this computer ([`local`]). Running them on an SSH Host is done by core, over
//! the Host's shared session, with [`Invocation::shell_line`].

pub mod clusters;
pub mod docker;
pub mod kube;
mod lines;
pub mod local;
mod time;

pub use lines::LineSplitter;
pub use time::{format_age, parse_rfc3339};

/// Which CLI a call runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tool {
    /// `docker` (also Podman's `docker` shim, Colima, OrbStack, Rancher Desktop).
    Docker,
    /// `kubectl`.
    Kubectl,
    /// The Azure CLI (`az`), for AKS.
    Az,
    /// The Google Cloud CLI (`gcloud`), for GKE.
    Gcloud,
    /// The AWS CLI (`aws`), for EKS.
    Aws,
    /// `kubelogin`, Azure's kubectl sign-in plugin.
    Kubelogin,
    /// Rancher's API (no CLI; for messages).
    Rancher,
}

impl Tool {
    /// The executable's name on `PATH`.
    pub fn program(self) -> &'static str {
        match self {
            Tool::Docker => "docker",
            Tool::Kubectl => "kubectl",
            Tool::Az => "az",
            Tool::Gcloud => "gcloud",
            Tool::Aws => "aws",
            Tool::Kubelogin => "kubelogin",
            Tool::Rancher => "rancher",
        }
    }

    /// What to tell the user when the CLI is missing.
    pub fn install_hint(self) -> &'static str {
        match self {
            Tool::Docker => {
                "Install Docker Desktop, OrbStack, Colima or the docker CLI, or set the \
                 executable's path in the connection."
            }
            Tool::Kubectl => {
                "Install kubectl (https://kubernetes.io/docs/tasks/tools/), or set the \
                 executable's path in the connection."
            }
            Tool::Az => {
                "Install the Azure CLI (https://learn.microsoft.com/cli/azure/install-azure-cli) \
                 and run `az login`."
            }
            Tool::Gcloud => {
                "Install the Google Cloud CLI (https://cloud.google.com/sdk/docs/install), run \
                 `gcloud auth login`, and add `gcloud components install gke-gcloud-auth-plugin`."
            }
            Tool::Aws => {
                "Install the AWS CLI v2 (https://aws.amazon.com/cli/) and sign in \
                 (`aws configure sso` or `aws configure`)."
            }
            Tool::Kubelogin => {
                "Install kubelogin (`az aks install-cli`, or `brew install \
                 Azure/kubelogin/kubelogin`)."
            }
            Tool::Rancher => "Rancher is reached over its API; no program is needed.",
        }
    }
}

/// Why a call failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ContainersError {
    /// The CLI is not installed (or not on `PATH`): show a hint, not an error.
    #[error("{} was not found. {}", .0.program(), .0.install_hint())]
    Missing(Tool),
    /// The CLI ran and failed; `message` is what it printed on standard error.
    #[error("{message}")]
    Failed {
        /// The CLI.
        tool: Tool,
        /// Exit code, when there was one.
        code: Option<i32>,
        /// Its error output (trimmed).
        message: String,
    },
    /// The output was not what this version expects.
    #[error("could not read {} output: {message}", .tool.program())]
    Parse {
        /// The CLI.
        tool: Tool,
        /// What was wrong.
        message: String,
    },
    /// Starting or talking to the process failed.
    #[error("{0}")]
    Io(String),
    /// No answer in time; the process was stopped.
    #[error("{} did not answer within {secs} s", .tool.program())]
    TimedOut {
        /// The CLI.
        tool: Tool,
        /// The limit.
        secs: u64,
    },
    /// kubectl could not sign in (expired login, missing exec plugin); `hint` says what
    /// to run.
    #[error("{message}")]
    SignIn {
        /// kubectl's message.
        message: String,
        /// What to do (`Run az login`, `Install kubelogin …`).
        hint: String,
    },
    /// A Production connection needs the user's typed confirmation first.
    #[error("Production connection: confirm this action first")]
    NeedsConfirmation,
    /// The connection is read-only.
    #[error("This connection is read-only")]
    ReadOnly,
    /// Not available here (e.g. a port forward on a Host).
    #[error("{0}")]
    Unsupported(String),
    /// Stopped by the user.
    #[error("Cancelled")]
    Cancelled,
}

impl ContainersError {
    /// Whether this is the missing-CLI case (shown as a hint).
    pub fn is_missing(&self) -> bool {
        matches!(self, ContainersError::Missing(_))
    }
}

/// Result type of this crate.
pub type Result<T, E = ContainersError> = std::result::Result<T, E>;

/// Where a connection's CLIs point: global options added to every call.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Target {
    /// `docker --context`; `None` is the CLI's current context.
    pub docker_context: Option<String>,
    /// `kubectl --kubeconfig` (`~/` is the home folder); `None` is `KUBECONFIG` or
    /// `~/.kube/config`.
    pub kubeconfig: Option<String>,
    /// `kubectl --context`; `None` is the kubeconfig's current context.
    pub kube_context: Option<String>,
}

impl Target {
    fn global_args(&self, tool: Tool) -> Vec<String> {
        let mut v = Vec::new();
        match tool {
            Tool::Docker => {
                if let Some(c) = non_empty(&self.docker_context) {
                    v.extend(["--context".to_owned(), c.to_owned()]);
                }
            }
            Tool::Kubectl => {
                if let Some(k) = non_empty(&self.kubeconfig) {
                    v.extend(["--kubeconfig".to_owned(), k.to_owned()]);
                }
                if let Some(c) = non_empty(&self.kube_context) {
                    v.extend(["--context".to_owned(), c.to_owned()]);
                }
            }
            Tool::Az | Tool::Gcloud | Tool::Aws | Tool::Kubelogin | Tool::Rancher => {}
        }
        v
    }

    /// An invocation of `tool` with this target's global options, then `args`.
    pub fn invocation<I, S>(&self, tool: Tool, args: I) -> Invocation
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut all = self.global_args(tool);
        all.extend(args.into_iter().map(Into::into));
        Invocation { tool, args: all }
    }
}

fn non_empty(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// One CLI call: the tool and its arguments (no shell involved locally).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    /// The CLI.
    pub tool: Tool,
    /// Arguments after the program. An argument starting with `~/` is a path under the
    /// home folder (expanded locally, left to the remote shell on a Host).
    pub args: Vec<String>,
}

impl Invocation {
    /// The call as one POSIX shell line for an SSH Host, starting with `program`
    /// (`docker`, or the connection's own path).
    pub fn shell_line(&self, program: &str) -> String {
        let mut out = shell_word(program);
        for a in &self.args {
            out.push(' ');
            out.push_str(&shell_word(a));
        }
        out
    }
}

/// `s` as one POSIX shell word: single-quoted unless it is plainly safe, with a leading
/// `~/` left outside the quotes so the remote shell expands it.
pub fn shell_word(s: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "-_./:=@,+%".contains(c);
    if !s.is_empty() && s.chars().all(safe) {
        return s.to_owned();
    }
    if let Some(rest) = s.strip_prefix("~/") {
        return format!("~/{}", shell_word(rest));
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// A listing to fetch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Query {
    /// `docker ps` (`all`: stopped ones too).
    Containers {
        /// Include stopped containers (`-a`).
        all: bool,
    },
    /// `docker images`.
    Images,
    /// `docker volume ls`.
    Volumes,
    /// `docker network ls`.
    Networks,
    /// The kubeconfig's contexts (`kubectl config view`).
    Contexts,
    /// `kubectl get namespaces`.
    Namespaces,
    /// `kubectl get pods` in a namespace (`None`: all namespaces).
    Pods {
        /// Namespace, or all.
        namespace: Option<String>,
    },
    /// `kubectl get deployments`.
    Deployments {
        /// Namespace, or all.
        namespace: Option<String>,
    },
    /// `kubectl get services`.
    Services {
        /// Namespace, or all.
        namespace: Option<String>,
    },
}

/// The rows of a [`Query`].
#[derive(Clone, Debug, PartialEq)]
pub enum Listing {
    /// Containers.
    Containers(Vec<docker::Container>),
    /// Images.
    Images(Vec<docker::Image>),
    /// Volumes.
    Volumes(Vec<docker::Volume>),
    /// Networks.
    Networks(Vec<docker::Network>),
    /// Kubeconfig contexts.
    Contexts(kube::Contexts),
    /// Namespaces.
    Namespaces(Vec<kube::Namespace>),
    /// Pods.
    Pods(Vec<kube::Pod>),
    /// Deployments.
    Deployments(Vec<kube::Deployment>),
    /// Services.
    Services(Vec<kube::Service>),
}

impl Query {
    /// The CLI it runs.
    pub fn tool(&self) -> Tool {
        match self {
            Query::Containers { .. } | Query::Images | Query::Volumes | Query::Networks => {
                Tool::Docker
            }
            _ => Tool::Kubectl,
        }
    }

    /// The call that lists it.
    pub fn invocation(&self, target: &Target) -> Invocation {
        let tool = self.tool();
        let args: Vec<String> = match self {
            Query::Containers { all } => docker::ps_args(*all),
            Query::Images => docker::images_args(),
            Query::Volumes => docker::volumes_args(),
            Query::Networks => docker::networks_args(),
            Query::Contexts => {
                // The context list must not depend on the chosen context.
                let t = Target {
                    kube_context: None,
                    ..target.clone()
                };
                return t.invocation(tool, kube::contexts_args());
            }
            Query::Namespaces => kube::get_args("namespaces", None, true),
            Query::Pods { namespace } => kube::get_args("pods", namespace.as_deref(), false),
            Query::Deployments { namespace } => {
                kube::get_args("deployments", namespace.as_deref(), false)
            }
            Query::Services { namespace } => {
                kube::get_args("services", namespace.as_deref(), false)
            }
        };
        target.invocation(tool, args)
    }

    /// Parse the CLI's standard output.
    pub fn parse(&self, stdout: &str) -> Result<Listing> {
        Ok(match self {
            Query::Containers { .. } => Listing::Containers(docker::parse_containers(stdout)?),
            Query::Images => Listing::Images(docker::parse_images(stdout)?),
            Query::Volumes => Listing::Volumes(docker::parse_volumes(stdout)?),
            Query::Networks => Listing::Networks(docker::parse_networks(stdout)?),
            Query::Contexts => Listing::Contexts(kube::parse_contexts(stdout)?),
            Query::Namespaces => Listing::Namespaces(kube::parse_namespaces(stdout)?),
            Query::Pods { .. } => Listing::Pods(kube::parse_pods(stdout)?),
            Query::Deployments { .. } => Listing::Deployments(kube::parse_deployments(stdout)?),
            Query::Services { .. } => Listing::Services(kube::parse_services(stdout)?),
        })
    }
}

/// Something that changes state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// `docker start`.
    Start {
        /// Container id.
        id: String,
        /// Its name, for messages and the typed confirmation.
        name: String,
    },
    /// `docker stop`.
    Stop {
        /// Container id.
        id: String,
        /// Its name.
        name: String,
    },
    /// `docker restart`.
    Restart {
        /// Container id.
        id: String,
        /// Its name.
        name: String,
    },
    /// `docker rm -f` (always confirmed).
    Remove {
        /// Container id.
        id: String,
        /// Its name.
        name: String,
    },
    /// `kubectl scale deployment` (always confirmed).
    Scale {
        /// Namespace.
        namespace: String,
        /// Deployment.
        name: String,
        /// New replica count.
        replicas: u32,
    },
    /// `kubectl rollout restart deployment` (always confirmed).
    RolloutRestart {
        /// Namespace.
        namespace: String,
        /// Deployment.
        name: String,
    },
    /// `kubectl delete pod` (always confirmed).
    DeletePod {
        /// Namespace.
        namespace: String,
        /// Pod.
        name: String,
    },
}

impl Action {
    /// The CLI it runs.
    pub fn tool(&self) -> Tool {
        match self {
            Action::Start { .. }
            | Action::Stop { .. }
            | Action::Restart { .. }
            | Action::Remove { .. } => Tool::Docker,
            _ => Tool::Kubectl,
        }
    }

    /// The call.
    pub fn invocation(&self, target: &Target) -> Invocation {
        let args: Vec<String> = match self {
            Action::Start { id, .. } => vec!["start".into(), id.clone()],
            Action::Stop { id, .. } => vec!["stop".into(), id.clone()],
            Action::Restart { id, .. } => vec!["restart".into(), id.clone()],
            Action::Remove { id, .. } => vec!["rm".into(), "-f".into(), id.clone()],
            Action::Scale {
                namespace,
                name,
                replicas,
            } => vec![
                "scale".into(),
                "-n".into(),
                namespace.clone(),
                format!("deployment/{name}"),
                format!("--replicas={replicas}"),
            ],
            Action::RolloutRestart { namespace, name } => vec![
                "rollout".into(),
                "restart".into(),
                "-n".into(),
                namespace.clone(),
                format!("deployment/{name}"),
            ],
            Action::DeletePod { namespace, name } => vec![
                "delete".into(),
                "pod".into(),
                "-n".into(),
                namespace.clone(),
                name.clone(),
                "--wait=false".into(),
            ],
        };
        target.invocation(self.tool(), args)
    }

    /// The name of what it acts on (the word typed to confirm on Production).
    pub fn subject(&self) -> &str {
        match self {
            Action::Start { name, .. }
            | Action::Stop { name, .. }
            | Action::Restart { name, .. }
            | Action::Remove { name, .. }
            | Action::Scale { name, .. }
            | Action::RolloutRestart { name, .. }
            | Action::DeletePod { name, .. } => name,
        }
    }

    /// Whether it stops, removes or changes something running: asks for the typed name on
    /// Production connections. Only starting a container is not.
    pub fn is_destructive(&self) -> bool {
        !matches!(self, Action::Start { .. })
    }

    /// Whether it always asks first, whatever the environment.
    pub fn always_confirm(&self) -> bool {
        matches!(
            self,
            Action::Remove { .. }
                | Action::Scale { .. }
                | Action::RolloutRestart { .. }
                | Action::DeletePod { .. }
        )
    }

    /// Button label.
    pub fn label(&self) -> &'static str {
        match self {
            Action::Start { .. } => "Start",
            Action::Stop { .. } => "Stop",
            Action::Restart { .. } => "Restart",
            Action::Remove { .. } => "Remove",
            Action::Scale { .. } => "Scale",
            Action::RolloutRestart { .. } => "Restart rollout",
            Action::DeletePod { .. } => "Delete pod",
        }
    }

    /// The question asked before it runs.
    pub fn question(&self) -> String {
        match self {
            Action::Start { name, .. } => format!("Start container {name}?"),
            Action::Stop { name, .. } => format!("Stop container {name}?"),
            Action::Restart { name, .. } => format!("Restart container {name}?"),
            Action::Remove { name, .. } => {
                format!(
                    "Remove container {name}? It is stopped first; its data outside volumes is lost."
                )
            }
            Action::Scale {
                namespace,
                name,
                replicas,
            } => format!("Scale deployment {namespace}/{name} to {replicas} replicas?"),
            Action::RolloutRestart { namespace, name } => {
                format!("Restart deployment {namespace}/{name}? Its pods are replaced one by one.")
            }
            Action::DeletePod { namespace, name } => {
                format!("Delete pod {namespace}/{name}? Its controller, if any, starts a new one.")
            }
        }
    }

    /// What to say when it worked.
    pub fn done_message(&self) -> String {
        match self {
            Action::Start { name, .. } => format!("Started {name}"),
            Action::Stop { name, .. } => format!("Stopped {name}"),
            Action::Restart { name, .. } => format!("Restarted {name}"),
            Action::Remove { name, .. } => format!("Removed {name}"),
            Action::Scale { name, replicas, .. } => format!("Scaled {name} to {replicas}"),
            Action::RolloutRestart { name, .. } => format!("Restarting {name}"),
            Action::DeletePod { name, .. } => format!("Deleting {name}"),
        }
    }
}

/// The kind of a Kubernetes object, for describe / YAML.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KubeKind {
    /// Pod.
    Pod,
    /// Deployment.
    Deployment,
    /// Service.
    Service,
    /// Namespace (cluster-wide).
    Namespace,
}

impl KubeKind {
    /// The resource name kubectl takes.
    pub fn resource(self) -> &'static str {
        match self {
            KubeKind::Pod => "pod",
            KubeKind::Deployment => "deployment",
            KubeKind::Service => "service",
            KubeKind::Namespace => "namespace",
        }
    }
}

/// The kind of a Docker object, for inspect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DockerKind {
    /// Container.
    Container,
    /// Image.
    Image,
    /// Volume.
    Volume,
    /// Network.
    Network,
}

/// A read-only detail view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inspect {
    /// `docker <kind> inspect` (JSON).
    Docker {
        /// Object kind.
        kind: DockerKind,
        /// Id or name.
        id: String,
    },
    /// `kubectl describe`.
    Describe {
        /// Object kind.
        kind: KubeKind,
        /// Namespace (ignored for namespaces).
        namespace: String,
        /// Name.
        name: String,
    },
    /// `kubectl get -o yaml`.
    Yaml {
        /// Object kind.
        kind: KubeKind,
        /// Namespace (ignored for namespaces).
        namespace: String,
        /// Name.
        name: String,
    },
}

impl Inspect {
    /// The CLI it runs.
    pub fn tool(&self) -> Tool {
        match self {
            Inspect::Docker { .. } => Tool::Docker,
            _ => Tool::Kubectl,
        }
    }

    /// The editor language of its output.
    pub fn language(&self) -> &'static str {
        match self {
            Inspect::Docker { .. } => "json",
            Inspect::Yaml { .. } => "yaml",
            Inspect::Describe { .. } => "text",
        }
    }

    /// The call.
    pub fn invocation(&self, target: &Target) -> Invocation {
        let ns_args = |kind: KubeKind, namespace: &str| -> Vec<String> {
            if kind == KubeKind::Namespace {
                Vec::new()
            } else {
                vec!["-n".into(), namespace.to_owned()]
            }
        };
        let args: Vec<String> = match self {
            Inspect::Docker { kind, id } => {
                let sub = match kind {
                    DockerKind::Container => "container",
                    DockerKind::Image => "image",
                    DockerKind::Volume => "volume",
                    DockerKind::Network => "network",
                };
                vec![sub.into(), "inspect".into(), id.clone()]
            }
            Inspect::Describe {
                kind,
                namespace,
                name,
            } => {
                let mut v = vec!["describe".into(), kind.resource().into()];
                v.extend(ns_args(*kind, namespace));
                v.push(name.clone());
                v
            }
            Inspect::Yaml {
                kind,
                namespace,
                name,
            } => {
                let mut v = vec!["get".into(), kind.resource().into()];
                v.extend(ns_args(*kind, namespace));
                v.extend([name.clone(), "-o".into(), "yaml".into()]);
                v
            }
        };
        target.invocation(self.tool(), args)
    }
}

/// Whose logs to read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogSource {
    /// A Docker container.
    Docker {
        /// Container id or name.
        id: String,
    },
    /// A pod's container.
    Pod {
        /// Namespace.
        namespace: String,
        /// Pod.
        pod: String,
        /// Container (`None`: the pod's default one).
        container: Option<String>,
    },
}

/// How much log to read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogOptions {
    /// Lines from the end to start with (`None`: everything).
    pub tail: Option<u32>,
    /// Keep reading new lines (`-f`).
    pub follow: bool,
    /// Prefix each line with its time (`--timestamps`).
    pub timestamps: bool,
}

impl Default for LogOptions {
    fn default() -> Self {
        Self {
            tail: Some(500),
            follow: true,
            timestamps: false,
        }
    }
}

impl LogSource {
    /// The CLI it runs.
    pub fn tool(&self) -> Tool {
        match self {
            LogSource::Docker { .. } => Tool::Docker,
            LogSource::Pod { .. } => Tool::Kubectl,
        }
    }

    /// The `logs` call.
    pub fn invocation(&self, options: LogOptions, target: &Target) -> Invocation {
        let mut args: Vec<String> = vec!["logs".into()];
        match self {
            LogSource::Docker { .. } => {
                args.push(format!(
                    "--tail={}",
                    options.tail.map_or("all".to_owned(), |n| n.to_string())
                ));
            }
            LogSource::Pod { .. } => {
                // kubectl's default for no tail is everything (-1).
                args.push(format!("--tail={}", options.tail.map_or(-1, i64::from)));
            }
        }
        if options.follow {
            args.push("-f".into());
        }
        if options.timestamps {
            args.push("--timestamps".into());
        }
        match self {
            LogSource::Docker { id } => args.push(id.clone()),
            LogSource::Pod {
                namespace,
                pod,
                container,
            } => {
                args.extend(["-n".into(), namespace.clone(), pod.clone()]);
                if let Some(c) = container.as_deref().filter(|c| !c.is_empty()) {
                    args.extend(["-c".into(), c.to_owned()]);
                }
            }
        }
        target.invocation(self.tool(), args)
    }
}

/// Where an interactive shell runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellTarget {
    /// `docker exec -it`.
    Docker {
        /// Container id or name.
        id: String,
        /// Its name (tab title).
        name: String,
    },
    /// `kubectl exec -it`.
    Pod {
        /// Namespace.
        namespace: String,
        /// Pod.
        pod: String,
        /// Container (`None`: the pod's default one).
        container: Option<String>,
    },
}

/// The command run inside: bash when the image has it, else sh.
pub const SHELL_COMMAND: [&str; 3] = ["sh", "-c", "bash || sh"];

impl ShellTarget {
    /// The CLI it runs.
    pub fn tool(&self) -> Tool {
        match self {
            ShellTarget::Docker { .. } => Tool::Docker,
            ShellTarget::Pod { .. } => Tool::Kubectl,
        }
    }

    /// A short title for the terminal tab.
    pub fn title(&self) -> String {
        match self {
            ShellTarget::Docker { name, .. } => format!("{name} · shell"),
            ShellTarget::Pod { pod, container, .. } => match container {
                Some(c) => format!("{pod}/{c} · shell"),
                None => format!("{pod} · shell"),
            },
        }
    }

    /// The `exec -it` call.
    pub fn invocation(&self, target: &Target) -> Invocation {
        let mut args: Vec<String> = vec!["exec".into(), "-it".into()];
        match self {
            ShellTarget::Docker { id, .. } => args.push(id.clone()),
            ShellTarget::Pod {
                namespace,
                pod,
                container,
            } => {
                args.extend(["-n".into(), namespace.clone(), pod.clone()]);
                if let Some(c) = container.as_deref().filter(|c| !c.is_empty()) {
                    args.extend(["-c".into(), c.to_owned()]);
                }
                args.push("--".into());
            }
        }
        args.extend(SHELL_COMMAND.iter().map(|s| (*s).to_owned()));
        target.invocation(self.tool(), args)
    }
}

/// What a port forward connects to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ForwardKind {
    /// A pod.
    Pod,
    /// A service (kubectl picks one of its pods).
    Service,
}

/// `kubectl port-forward` to a pod or service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortForward {
    /// Namespace.
    pub namespace: String,
    /// Pod or service.
    pub kind: ForwardKind,
    /// Its name.
    pub name: String,
    /// Port on this computer (`0`: any free port).
    pub local_port: u16,
    /// Port on the pod or service.
    pub remote_port: u16,
}

impl PortForward {
    /// The call. Binds to 127.0.0.1 only.
    pub fn invocation(&self, target: &Target) -> Invocation {
        let what = match self.kind {
            ForwardKind::Pod => format!("pod/{}", self.name),
            ForwardKind::Service => format!("service/{}", self.name),
        };
        let local = if self.local_port == 0 {
            String::new()
        } else {
            self.local_port.to_string()
        };
        target.invocation(
            Tool::Kubectl,
            [
                "port-forward".to_owned(),
                "-n".to_owned(),
                self.namespace.clone(),
                what,
                format!("{local}:{}", self.remote_port),
                "--address".to_owned(),
                "127.0.0.1".to_owned(),
            ],
        )
    }

    /// `namespace/pod/name :remote`.
    pub fn label(&self) -> String {
        let kind = match self.kind {
            ForwardKind::Pod => "pod",
            ForwardKind::Service => "svc",
        };
        format!(
            "{kind}/{}.{} :{}",
            self.name, self.namespace, self.remote_port
        )
    }
}

/// The local port in kubectl's `Forwarding from 127.0.0.1:8080 -> 80` line.
pub fn forwarding_port(line: &str) -> Option<u16> {
    let rest = line.trim().strip_prefix("Forwarding from ")?;
    let addr = rest.split(" -> ").next()?;
    addr.rsplit(':').next()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kube_target() -> Target {
        Target {
            docker_context: None,
            kubeconfig: Some("~/.kube/prod".into()),
            kube_context: Some("prod-eu".into()),
        }
    }

    #[test]
    fn shell_words_quote_what_needs_it() {
        assert_eq!(shell_word("docker"), "docker");
        assert_eq!(shell_word("deployment/web"), "deployment/web");
        assert_eq!(shell_word("bash || sh"), "'bash || sh'");
        assert_eq!(shell_word("it's"), "'it'\\''s'");
        assert_eq!(shell_word("{{json .}}"), "'{{json .}}'");
        assert_eq!(shell_word("~/.kube/my config"), "~/'.kube/my config'");
        assert_eq!(shell_word(""), "''");
    }

    #[test]
    fn queries_carry_the_target() {
        let t = kube_target();
        let inv = Query::Pods {
            namespace: Some("shop".into()),
        }
        .invocation(&t);
        assert_eq!(
            inv.args,
            [
                "--kubeconfig",
                "~/.kube/prod",
                "--context",
                "prod-eu",
                "get",
                "pods",
                "-o",
                "json",
                "-n",
                "shop"
            ]
        );
        let all = Query::Deployments { namespace: None }.invocation(&t);
        assert!(all.args.ends_with(&["-A".to_owned()]));
        // Listing contexts ignores the chosen one.
        let ctx = Query::Contexts.invocation(&t);
        assert_eq!(
            ctx.args,
            [
                "--kubeconfig",
                "~/.kube/prod",
                "config",
                "view",
                "-o",
                "json"
            ]
        );
        let ps = Query::Containers { all: true }.invocation(&Target {
            docker_context: Some("colima".into()),
            ..Target::default()
        });
        assert_eq!(
            ps.shell_line("docker"),
            "docker --context colima ps -a --format '{{json .}}'"
        );
    }

    #[test]
    fn actions_build_their_calls() {
        let t = Target::default();
        let scale = Action::Scale {
            namespace: "shop".into(),
            name: "web".into(),
            replicas: 5,
        };
        assert_eq!(
            scale.invocation(&t).shell_line("kubectl"),
            "kubectl scale -n shop deployment/web --replicas=5"
        );
        assert!(scale.is_destructive() && scale.always_confirm());
        let start = Action::Start {
            id: "abc".into(),
            name: "web".into(),
        };
        assert!(!start.is_destructive() && !start.always_confirm());
        let rm = Action::Remove {
            id: "abc".into(),
            name: "web".into(),
        };
        assert_eq!(rm.invocation(&t).args, ["rm", "-f", "abc"]);
        assert_eq!(rm.subject(), "web");
        let stop = Action::Stop {
            id: "abc".into(),
            name: "web".into(),
        };
        assert!(stop.is_destructive() && !stop.always_confirm());
        assert_eq!(
            Action::RolloutRestart {
                namespace: "shop".into(),
                name: "web".into()
            }
            .invocation(&t)
            .args,
            ["rollout", "restart", "-n", "shop", "deployment/web"]
        );
    }

    #[test]
    fn logs_shell_and_inspect_calls() {
        let t = Target::default();
        let opts = LogOptions {
            tail: Some(200),
            follow: true,
            timestamps: false,
        };
        assert_eq!(
            LogSource::Docker { id: "web".into() }
                .invocation(opts, &t)
                .args,
            ["logs", "--tail=200", "-f", "web"]
        );
        assert_eq!(
            LogSource::Pod {
                namespace: "shop".into(),
                pod: "web-1".into(),
                container: Some("nginx".into()),
            }
            .invocation(
                LogOptions {
                    tail: None,
                    follow: false,
                    timestamps: true
                },
                &t
            )
            .args,
            [
                "logs",
                "--tail=-1",
                "--timestamps",
                "-n",
                "shop",
                "web-1",
                "-c",
                "nginx"
            ]
        );
        let shell = ShellTarget::Docker {
            id: "57f1".into(),
            name: "web".into(),
        };
        assert_eq!(
            shell.invocation(&t).shell_line("docker"),
            "docker exec -it 57f1 sh -c 'bash || sh'"
        );
        let pod = ShellTarget::Pod {
            namespace: "shop".into(),
            pod: "web-1".into(),
            container: None,
        };
        assert_eq!(
            pod.invocation(&kube_target()).shell_line("kubectl"),
            "kubectl --kubeconfig ~/.kube/prod --context prod-eu exec -it -n shop web-1 -- sh -c 'bash || sh'"
        );
        assert_eq!(
            Inspect::Yaml {
                kind: KubeKind::Deployment,
                namespace: "shop".into(),
                name: "web".into()
            }
            .invocation(&t)
            .args,
            ["get", "deployment", "-n", "shop", "web", "-o", "yaml"]
        );
        assert_eq!(
            Inspect::Describe {
                kind: KubeKind::Namespace,
                namespace: String::new(),
                name: "shop".into()
            }
            .invocation(&t)
            .args,
            ["describe", "namespace", "shop"]
        );
        assert_eq!(
            Inspect::Docker {
                kind: DockerKind::Volume,
                id: "data".into()
            }
            .invocation(&t)
            .args,
            ["volume", "inspect", "data"]
        );
    }

    #[test]
    fn port_forwards() {
        let f = PortForward {
            namespace: "shop".into(),
            kind: ForwardKind::Service,
            name: "web".into(),
            local_port: 0,
            remote_port: 80,
        };
        assert_eq!(
            f.invocation(&Target::default()).args,
            [
                "port-forward",
                "-n",
                "shop",
                "service/web",
                ":80",
                "--address",
                "127.0.0.1"
            ]
        );
        assert_eq!(f.label(), "svc/web.shop :80");
        assert_eq!(
            forwarding_port("Forwarding from 127.0.0.1:53124 -> 80"),
            Some(53124)
        );
        assert_eq!(
            forwarding_port("Forwarding from [::1]:8080 -> 80"),
            Some(8080)
        );
        assert_eq!(forwarding_port("Handling connection for 8080"), None);
    }

    #[test]
    fn missing_cli_reads_as_a_hint() {
        let e = ContainersError::Missing(Tool::Kubectl);
        assert!(e.is_missing());
        assert!(e.to_string().starts_with("kubectl was not found."));
    }
}
