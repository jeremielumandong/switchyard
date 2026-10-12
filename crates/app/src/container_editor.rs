//! "New Docker connection" and "Add Kubernetes cluster" dialog, also used to edit a saved
//! container connection.
//!
//! A cluster comes from one of six sources. A context already in the kubeconfig (or a
//! local cluster: kind, minikube, k3d, k3s, Docker Desktop, Rancher Desktop) is picked
//! from `kubectl config view`. AKS, GKE and EKS clusters are listed with the provider's
//! own CLI (`az`, `gcloud`, `aws`) and added to the kubeconfig with it, so cloud
//! credentials stay with those CLIs. A Rancher cluster takes the server URL and an API
//! token: the token goes to the keychain (or vault) with the saved connection, and the
//! core writes the cluster's kubeconfig to a Switchyard-managed file. Every call runs on
//! the core runtime; this view only sends commands and draws their answers.

use gpui_kit::component::Sizable as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, FontWeight, Hsla,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, px,
};
use secrecy::SecretString;
use switchyard_core::containers::ContainersError;
use switchyard_core::containers::clusters::{
    AWS_REGIONS, CloudCluster, ClusterProvider, DiscoverOptions, RancherCluster,
    plugin_install_hint,
};
use switchyard_core::containers::kube::{Context as KubeContext, Contexts};
use switchyard_core::store::{
    ContainerConnection, ContainerPlatform, EnvironmentLabel, Profile, ProfileId, RancherSource,
};
use switchyard_core::{Command, RequestId, RuntimeHandle};

use crate::app_state::{Profiles, next_id};
use crate::appearance::{rpx, ts};
use crate::theme::{MONO, Palette, palette};
use crate::ui::{self, Kind};
use crate::workspace::Workspace;

/// Where a new cluster comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// A context already in the kubeconfig.
    Existing,
    /// A local cluster's context.
    Local,
    /// Azure Kubernetes Service (`az`).
    Aks,
    /// Google Kubernetes Engine (`gcloud`).
    Gke,
    /// Amazon EKS (`aws`).
    Eks,
    /// A Rancher-managed cluster (server URL + API token).
    Rancher,
}

impl Source {
    const ALL: [Source; 6] = [
        Source::Local,
        Source::Aks,
        Source::Gke,
        Source::Eks,
        Source::Rancher,
        Source::Existing,
    ];

    fn title(self) -> &'static str {
        match self {
            Source::Existing => "Kubeconfig context",
            Source::Local => "Local cluster",
            Source::Aks => "Azure AKS",
            Source::Gke => "Google GKE",
            Source::Eks => "Amazon EKS",
            Source::Rancher => "Rancher",
        }
    }

    fn sub(self) -> &'static str {
        match self {
            Source::Existing => "Any context in ~/.kube/config",
            Source::Local => "kind, minikube, k3d, k3s, Docker / Rancher Desktop",
            Source::Aks => "Listed with az; az aks get-credentials",
            Source::Gke => "Listed with gcloud; get-credentials",
            Source::Eks => "Listed with aws; update-kubeconfig",
            Source::Rancher => "Server URL and API token",
        }
    }

    fn badge(self) -> &'static str {
        match self {
            Source::Existing => "CFG",
            Source::Local => "LOC",
            Source::Aks => "AKS",
            Source::Gke => "GKE",
            Source::Eks => "EKS",
            Source::Rancher => "RAN",
        }
    }

    fn provider(self) -> Option<ClusterProvider> {
        match self {
            Source::Aks => Some(ClusterProvider::Aks),
            Source::Gke => Some(ClusterProvider::Gke),
            Source::Eks => Some(ClusterProvider::Eks),
            Source::Rancher => Some(ClusterProvider::Rancher),
            Source::Local => Some(ClusterProvider::Local),
            Source::Existing => None,
        }
    }
}

/// Which page shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// Kubernetes: pick a source.
    Source,
    /// Find the cluster (contexts, cloud list, Rancher list).
    Find,
    /// Name, environment, context, …; Test and Save.
    Details,
}

#[derive(Clone, Debug, PartialEq)]
enum TestState {
    Idle,
    Testing(RequestId),
    Passed(String),
    Failed(String),
}

/// Events for the workspace.
pub enum ContainerEditorEvent {
    /// Close the dialog.
    Close,
}

/// The dialog.
pub struct ContainerEditor {
    core: RuntimeHandle,
    profiles: Profiles,
    platform: ContainerPlatform,
    existing: Option<ContainerConnection>,
    source: Source,
    step: Step,
    /// `kubectl config view` request and answer.
    ctx_request: Option<RequestId>,
    contexts: Option<Result<Contexts, ContainersError>>,
    /// Cloud listing.
    discover_request: Option<RequestId>,
    clusters: Option<Result<Vec<CloudCluster>, ContainersError>>,
    chosen_cluster: Option<usize>,
    /// AKS `--admin`.
    admin: bool,
    cred_request: Option<RequestId>,
    cred_error: Option<ContainersError>,
    aws_request: Option<RequestId>,
    aws_profiles: Vec<String>,
    aws_profile: Option<String>,
    aws_region: String,
    /// Rancher listing.
    rancher_request: Option<RequestId>,
    rancher_clusters: Option<Result<Vec<RancherCluster>, ContainersError>>,
    rancher_cluster: Option<RancherCluster>,
    // Fields.
    name: Entity<InputState>,
    context: Entity<InputState>,
    kubeconfig: Entity<InputState>,
    namespace: Entity<InputState>,
    executable: Entity<InputState>,
    gcp_project: Entity<InputState>,
    rancher_server: Entity<InputState>,
    rancher_token: Entity<InputState>,
    rancher_pem: Entity<InputState>,
    host: Option<ProfileId>,
    env: EnvironmentLabel,
    read_only: bool,
    provider: Option<ClusterProvider>,
    test: TestState,
    save_request: Option<RequestId>,
    error: Option<String>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<ContainerEditorEvent> for ContainerEditor {}

fn input(
    window: &mut Window,
    cx: &mut Context<ContainerEditor>,
    value: Option<&str>,
    placeholder: &str,
    masked: bool,
) -> Entity<InputState> {
    let v = value.unwrap_or_default().to_owned();
    let ph = placeholder.to_owned();
    cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(ph)
            .masked(masked)
            .default_value(v)
    })
}

fn blank(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_owned())
}

/// The contexts shown for `source`: local ones only for Local.
fn contexts_for(contexts: &Contexts, source: Source) -> Vec<&KubeContext> {
    contexts
        .contexts
        .iter()
        .filter(|c| source != Source::Local || c.provider == ClusterProvider::Local)
        .collect()
}

impl ContainerEditor {
    /// A new dialog for `platform`; `existing` edits a saved connection.
    pub fn new(
        core: RuntimeHandle,
        profiles: Profiles,
        platform: ContainerPlatform,
        existing: Option<ContainerConnection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let e = existing.as_ref();
        let platform = e.map_or(platform, |c| c.platform);
        let name = input(
            window,
            cx,
            e.map(|c| c.name.as_str()).or(Some(platform.display_name())),
            "Name",
            false,
        );
        let context = input(
            window,
            cx,
            e.and_then(|c| c.context.as_deref()),
            "Current context",
            false,
        );
        let kubeconfig = input(
            window,
            cx,
            e.and_then(|c| c.kubeconfig.as_deref()),
            "KUBECONFIG or ~/.kube/config",
            false,
        );
        let namespace = input(
            window,
            cx,
            e.and_then(|c| c.namespace.as_deref()),
            "The context's (else default)",
            false,
        );
        let executable = input(
            window,
            cx,
            e.and_then(|c| c.executable.as_deref()),
            platform.cli(),
            false,
        );
        let gcp_project = input(window, cx, None, "gcloud's configured project", false);
        let rancher = e.and_then(|c| c.rancher.as_ref());
        let rancher_server = input(
            window,
            cx,
            rancher.map(|r| r.server.as_str()),
            "https://rancher.example.com",
            false,
        );
        let rancher_token = input(
            window,
            cx,
            None,
            if e.is_some_and(|c| c.secret.is_some()) {
                "Stored · type to replace"
            } else {
                "token-xxxxx:…"
            },
            true,
        );
        let rancher_pem = input(
            window,
            cx,
            rancher.and_then(|r| r.trusted_ca_pem.as_deref()),
            "Self-signed: path to the server's PEM (optional)",
            false,
        );
        let subs = vec![
            cx.subscribe_in(&name, window, |this, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    this.save(cx);
                }
            }),
        ];
        let step = match (&existing, platform) {
            (Some(_), _) | (None, ContainerPlatform::Docker) => Step::Details,
            (None, ContainerPlatform::Kubernetes) => Step::Source,
        };
        let source = if rancher.is_some() {
            Source::Rancher
        } else {
            Source::Existing
        };
        Self {
            core,
            profiles,
            platform,
            source,
            step,
            ctx_request: None,
            contexts: None,
            discover_request: None,
            clusters: None,
            chosen_cluster: None,
            admin: false,
            cred_request: None,
            cred_error: None,
            aws_request: None,
            aws_profiles: Vec::new(),
            aws_profile: None,
            aws_region: AWS_REGIONS[0].to_owned(),
            rancher_request: None,
            rancher_clusters: None,
            rancher_cluster: rancher.map(|r| RancherCluster {
                id: r.cluster_id.clone(),
                name: r.cluster_name.clone(),
                state: String::new(),
                driver: String::new(),
                version: String::new(),
                can_kubeconfig: true,
            }),
            name,
            context,
            kubeconfig,
            namespace,
            executable,
            gcp_project,
            rancher_server,
            rancher_token,
            rancher_pem,
            host: e.and_then(|c| c.host_id.clone()),
            env: e.map_or(EnvironmentLabel::Development, |c| c.environment),
            read_only: e.is_some_and(|c| c.read_only),
            provider: e
                .and_then(|c| c.provider.as_deref())
                .and_then(ClusterProvider::from_key),
            test: TestState::Idle,
            save_request: None,
            error: None,
            existing,
            _subs: subs,
        }
    }

    /// The pending save.
    pub fn request(&self) -> Option<RequestId> {
        self.save_request
    }

    /// Whether `request` is one of this dialog's lookups.
    fn owns(&self, request: RequestId) -> bool {
        [
            self.ctx_request,
            self.discover_request,
            self.cred_request,
            self.aws_request,
            self.rancher_request,
        ]
        .contains(&Some(request))
    }

    fn value(&self, i: &Entity<InputState>, cx: &Context<Self>) -> String {
        i.read(cx).value().to_string()
    }

    fn set(&self, i: &Entity<InputState>, v: &str, window: &mut Window, cx: &mut Context<Self>) {
        let v = v.to_owned();
        i.update(cx, |i, cx| i.set_value(v, window, cx));
    }

    // ------------------------------------------------------------ steps

    fn pick_source(&mut self, source: Source, cx: &mut Context<Self>) {
        self.source = source;
        self.step = Step::Find;
        self.error = None;
        self.provider = source.provider();
        match source {
            Source::Existing | Source::Local => {
                if self.contexts.is_none() {
                    self.load_contexts();
                }
            }
            Source::Eks => {
                if self.aws_profiles.is_empty() {
                    let request = next_id();
                    self.aws_request = Some(request);
                    self.core.send(Command::AwsProfiles { request });
                }
            }
            Source::Aks | Source::Gke | Source::Rancher => {}
        }
        // AKS lists right away (no options needed).
        if source == Source::Aks && self.clusters.is_none() {
            self.discover(cx);
        }
        cx.notify();
    }

    fn back(&mut self, cx: &mut Context<Self>) {
        self.step = match self.step {
            Step::Details
                if self.existing.is_none() && self.platform == ContainerPlatform::Kubernetes =>
            {
                Step::Find
            }
            Step::Find => {
                self.clusters = None;
                self.chosen_cluster = None;
                self.rancher_clusters = None;
                self.cred_error = None;
                Step::Source
            }
            s => s,
        };
        self.error = None;
        cx.notify();
    }

    fn load_contexts(&mut self) {
        let request = next_id();
        self.ctx_request = Some(request);
        self.contexts = None;
        self.core.send(Command::KubeContexts {
            request,
            kubeconfig: None,
        });
    }

    fn discover(&mut self, cx: &mut Context<Self>) {
        let Some(provider) = self.source.provider() else {
            return;
        };
        let request = next_id();
        self.discover_request = Some(request);
        self.clusters = None;
        self.chosen_cluster = None;
        self.cred_error = None;
        let options = DiscoverOptions {
            aws_region: (provider == ClusterProvider::Eks).then(|| self.aws_region.clone()),
            aws_profile: self.aws_profile.clone(),
            gcp_project: blank(&self.value(&self.gcp_project, cx)),
        };
        self.core.send(Command::DiscoverClusters {
            request,
            provider,
            options,
        });
        cx.notify();
    }

    /// Run the provider's get-credentials for the chosen cluster.
    fn add_credentials(&mut self, cx: &mut Context<Self>) {
        let Some(Ok(list)) = &self.clusters else {
            return;
        };
        let Some(cluster) = self.chosen_cluster.and_then(|i| list.get(i)).cloned() else {
            return;
        };
        let request = next_id();
        self.cred_request = Some(request);
        self.cred_error = None;
        self.core.send(Command::AddClusterCredentials {
            request,
            cluster,
            admin: self.admin,
        });
        cx.notify();
    }

    fn rancher_token(&self, cx: &Context<Self>) -> Option<SecretString> {
        let t = self.value(&self.rancher_token, cx);
        (!t.trim().is_empty()).then(|| SecretString::from(t.trim().to_owned()))
    }

    fn list_rancher(&mut self, cx: &mut Context<Self>) {
        let server = self.value(&self.rancher_server, cx);
        if server.trim().is_empty() {
            self.error = Some("Enter the Rancher server URL".into());
            cx.notify();
            return;
        }
        let token = self.rancher_token(cx);
        let saved = self
            .existing
            .as_ref()
            .filter(|c| c.secret.is_some())
            .map(|c| c.id.clone());
        if token.is_none() && saved.is_none() {
            self.error = Some("Enter an API token (Rancher: Account & API Keys)".into());
            cx.notify();
            return;
        }
        self.error = None;
        let request = next_id();
        self.rancher_request = Some(request);
        self.rancher_clusters = None;
        self.core.send(Command::RancherClusters {
            request,
            server: server.trim().to_owned(),
            token,
            connection: saved,
            trusted_ca_pem: blank(&self.value(&self.rancher_pem, cx)),
        });
        cx.notify();
    }

    /// A context was picked (or written by a provider CLI): fill the details.
    fn use_context(
        &mut self,
        name: String,
        provider: Option<ClusterProvider>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set(&self.context, &name, window, cx);
        self.set(&self.name, &name, window, cx);
        if let Some(p) = provider {
            self.provider = Some(p);
        }
        self.step = Step::Details;
        self.test = TestState::Idle;
        cx.notify();
    }

    fn use_rancher(
        &mut self,
        cluster: RancherCluster,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set(&self.name, &cluster.name, window, cx);
        self.set(&self.context, "", window, cx);
        self.rancher_cluster = Some(cluster);
        self.provider = Some(ClusterProvider::Rancher);
        self.step = Step::Details;
        self.test = TestState::Idle;
        cx.notify();
    }

    // ------------------------------------------------------------ answers

    /// Route an answer of the core to the dialog.
    pub fn on_event(
        &mut self,
        ev: switchyard_core::Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use switchyard_core::Event;
        match ev {
            Event::KubeContexts { request, result } if self.ctx_request == Some(request) => {
                self.ctx_request = None;
                self.contexts = Some(result);
            }
            Event::ClustersDiscovered { request, result }
                if self.discover_request == Some(request) =>
            {
                self.discover_request = None;
                if let Ok(list) = &result
                    && list.len() == 1
                {
                    self.chosen_cluster = Some(0);
                }
                self.clusters = Some(result);
            }
            Event::ClusterCredentials { request, result } if self.cred_request == Some(request) => {
                self.cred_request = None;
                match result {
                    Ok(context) => {
                        let provider = self.source.provider();
                        let name = self
                            .chosen_cluster
                            .and_then(|i| match &self.clusters {
                                Some(Ok(l)) => l.get(i).map(|c| c.name.clone()),
                                _ => None,
                            })
                            .unwrap_or_else(|| context.clone());
                        self.use_context(context, provider, window, cx);
                        self.set(&self.name, &name, window, cx);
                    }
                    Err(e) => self.cred_error = Some(e),
                }
            }
            Event::AwsProfiles { request, profiles } if self.aws_request == Some(request) => {
                self.aws_request = None;
                if self.aws_profile.is_none() {
                    self.aws_profile = profiles.first().cloned();
                }
                self.aws_profiles = profiles;
            }
            Event::RancherClusters { request, result } if self.rancher_request == Some(request) => {
                self.rancher_request = None;
                self.rancher_clusters = Some(result);
            }
            _ => return,
        }
        cx.notify();
    }

    /// A test finished.
    pub fn on_test_result(
        &mut self,
        request: RequestId,
        result: Result<String, String>,
        cx: &mut Context<Self>,
    ) {
        if self.test == TestState::Testing(request) {
            self.test = match result {
                Ok(s) => TestState::Passed(s),
                Err(e) => TestState::Failed(e),
            };
            cx.notify();
        }
    }

    /// A save failed.
    pub fn set_error(&mut self, message: String, cx: &mut Context<Self>) {
        self.save_request = None;
        self.error = Some(message);
        cx.notify();
    }

    // ------------------------------------------------------------ save

    fn build(&self, cx: &Context<Self>) -> Result<ContainerConnection, String> {
        let name = self.value(&self.name, cx).trim().to_owned();
        if name.is_empty() {
            return Err("Name is required".into());
        }
        let mut c = match &self.existing {
            Some(c) => c.clone(),
            None => ContainerConnection::new(name.clone(), self.platform),
        };
        c.name = name;
        c.environment = self.env;
        c.read_only = self.read_only;
        c.host_id = self.host.clone();
        c.context = blank(&self.value(&self.context, cx));
        c.executable = blank(&self.value(&self.executable, cx));
        if self.platform == ContainerPlatform::Kubernetes {
            c.kubeconfig = blank(&self.value(&self.kubeconfig, cx));
            c.namespace = blank(&self.value(&self.namespace, cx));
            c.provider = self.provider.map(|p| p.key().to_owned());
        }
        c.rancher = match (&self.rancher_cluster, self.source) {
            (Some(rc), Source::Rancher) => Some(RancherSource {
                server: self.value(&self.rancher_server, cx).trim().to_owned(),
                cluster_id: rc.id.clone(),
                cluster_name: rc.name.clone(),
                trusted_ca_pem: blank(&self.value(&self.rancher_pem, cx)),
            }),
            _ => None,
        };
        if c.rancher.is_some() {
            c.host_id = None;
            c.kubeconfig = None;
        }
        Profile::Containers(c.clone())
            .validate()
            .map_err(|v| v.message)?;
        Ok(c)
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        if self.step != Step::Details || self.save_request.is_some() {
            return;
        }
        match self.build(cx) {
            Ok(c) => {
                if c.rancher.is_some() && c.secret.is_none() && self.rancher_token(cx).is_none() {
                    self.error = Some("Enter the Rancher API token".into());
                    cx.notify();
                    return;
                }
                let request = next_id();
                self.save_request = Some(request);
                self.error = None;
                let secret = if c.rancher.is_some() {
                    self.rancher_token(cx)
                } else {
                    None
                };
                self.core.send(Command::SaveProfile {
                    request,
                    profile: Profile::Containers(c),
                    secret,
                });
            }
            Err(e) => self.error = Some(e),
        }
        cx.notify();
    }

    fn run_test(&mut self, cx: &mut Context<Self>) {
        match self.build(cx) {
            Ok(connection) => {
                let request = next_id();
                self.test = TestState::Testing(request);
                self.error = None;
                let secret = if connection.rancher.is_some() {
                    self.rancher_token(cx)
                } else {
                    None
                };
                self.core.send(Command::TestContainers {
                    request,
                    connection,
                    secret,
                });
            }
            Err(e) => self.error = Some(e),
        }
        cx.notify();
    }

    // ------------------------------------------------------------ drawing

    fn label(&self, text: &'static str, p: &Palette) -> gpui_kit::Div {
        div()
            .text_size(ts::LABEL)
            .text_color(p.fg2)
            .font_weight(FontWeight::MEDIUM)
            .child(text)
    }

    fn field(&self, text: &'static str, i: &Entity<InputState>, p: &Palette) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap(rpx(5.))
            .child(self.label(text, p))
            .child(Input::new(i).text_size(ts::BODY))
            .into_any_element()
    }

    fn hint(&self, text: impl Into<SharedString>, color: Hsla, p: &Palette) -> AnyElement {
        div()
            .px(rpx(10.))
            .py(rpx(6.))
            .rounded(px(6.))
            .bg(color.opacity(0.08))
            .border_1()
            .border_color(color.opacity(0.4))
            .text_size(ts::BODY)
            .text_color(p.fg)
            .child(text.into())
            .into_any_element()
    }

    fn error_card(&self, e: &ContainersError, p: &Palette) -> AnyElement {
        match e {
            ContainersError::Missing(tool) => self.hint(
                format!("{} was not found. {}", tool.program(), tool.install_hint()),
                p.stg,
                p,
            ),
            ContainersError::SignIn { message, hint } => {
                self.hint(format!("{hint}\n{message}"), p.stg, p)
            }
            e => self.hint(e.to_string(), p.prod, p),
        }
    }

    fn render_sources(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap(rpx(10.))
            .child(
                div()
                    .text_size(ts::BODY)
                    .text_color(p.fg2)
                    .child("Where does the cluster run? Switchyard uses your kubectl and each provider's own CLI; it never stores cloud credentials."),
            )
            .child(div().flex().flex_wrap().gap(rpx(10.)).children(
                Source::ALL.iter().map(|s| {
                    let s = *s;
                    div()
                        .id(SharedString::from(format!("src-{}", s.badge())))
                        .w(rpx(240.))
                        .flex()
                        .items_center()
                        .gap(rpx(10.))
                        .p(rpx(10.))
                        .rounded(px(8.))
                        .border_1()
                        .border_color(p.bd2)
                        .bg(p.bg)
                        .cursor_pointer()
                        .hover(|st| st.border_color(p.acc))
                        .on_click(cx.listener(move |t, _, _, cx| t.pick_source(s, cx)))
                        .child(ui::monogram(s.badge(), 34., p))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .min_w_0()
                                .child(
                                    div()
                                        .text_size(ts::UI)
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(s.title()),
                                )
                                .child(
                                    div()
                                        .text_size(ts::SMALL)
                                        .text_color(p.fg3)
                                        .child(s.sub()),
                                ),
                        )
                }),
            ))
            .into_any_element()
    }

    fn render_contexts(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let body = match &self.contexts {
            None => ui::shimmer(240., p),
            Some(Err(e)) => self.error_card(e, p),
            Some(Ok(contexts)) => {
                let list = contexts_for(contexts, self.source);
                if list.is_empty() {
                    self.hint(
                        if self.source == Source::Local {
                            "No local cluster contexts found. Start kind, minikube, k3d or Docker / Rancher Desktop's Kubernetes, then Refresh."
                        } else {
                            "The kubeconfig has no contexts."
                        },
                        p.stg,
                        p,
                    )
                } else {
                    div()
                        .flex()
                        .flex_col()
                        .children(list.into_iter().enumerate().map(|(i, k)| {
                            let name = k.name.clone();
                            let provider = k.provider;
                            let label = match k.local_flavor {
                                Some(f) => format!("Local · {f}"),
                                None => provider.label().to_owned(),
                            };
                            let current = contexts.current.as_deref() == Some(k.name.as_str());
                            div()
                                .id(("ctx", i))
                                .flex()
                                .flex_col()
                                .gap(rpx(2.))
                                .px(rpx(10.))
                                .py(rpx(6.))
                                .border_b_1()
                                .border_color(p.line)
                                .cursor_pointer()
                                .hover(|st| st.bg(p.hover))
                                .on_click(cx.listener(move |t, _, window, cx| {
                                    t.use_context(name.clone(), Some(provider), window, cx)
                                }))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(rpx(8.))
                                        .child(
                                            div()
                                                .px(rpx(5.))
                                                .rounded(px(3.))
                                                .border_1()
                                                .border_color(p.bd2)
                                                .text_size(ts::TINY_PLUS)
                                                .text_color(p.fg2)
                                                .child(label),
                                        )
                                        .child(
                                            div()
                                                .font_family(MONO)
                                                .text_size(ts::BODY)
                                                .child(k.name.clone()),
                                        )
                                        .when(current, |d| {
                                            d.child(
                                                div()
                                                    .text_size(ts::SMALL)
                                                    .text_color(p.acc)
                                                    .child("current"),
                                            )
                                        }),
                                )
                                .child(
                                    div()
                                        .text_size(ts::SMALL)
                                        .text_color(p.fg3)
                                        .truncate()
                                        .child(k.server.clone()),
                                )
                                .when_some(
                                    k.exec.clone().filter(|_| k.plugin_missing),
                                    |d, plugin| {
                                        d.child(div().text_size(ts::SMALL).text_color(p.stg).child(
                                            format!(
                                                "{plugin} is missing: {}",
                                                plugin_install_hint(&plugin)
                                            ),
                                        ))
                                    },
                                )
                        }))
                        .into_any_element()
                }
            }
        };
        div()
            .flex()
            .flex_col()
            .gap(rpx(8.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(self.label("Contexts in your kubeconfig", p))
                    .child(div().flex_1())
                    .child(
                        ui::button("ctx-refresh", "Refresh", Kind::Secondary, p)
                            .h(rpx(24.))
                            .on_click(cx.listener(|t, _, _, cx| {
                                t.load_contexts();
                                cx.notify();
                            })),
                    ),
            )
            .child(body)
            .into_any_element()
    }

    fn render_cloud(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity().downgrade();
        let options: Option<AnyElement> = match self.source {
            Source::Gke => Some(
                div()
                    .w(rpx(280.))
                    .child(self.field("Project (optional)", &self.gcp_project, p))
                    .into_any_element(),
            ),
            Source::Eks => {
                let region = self.aws_region.clone();
                let profile = self.aws_profile.clone();
                let profiles = self.aws_profiles.clone();
                let this2 = this.clone();
                Some(
                    div()
                        .flex()
                        .gap(rpx(10.))
                        .items_end()
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(rpx(5.))
                                .child(self.label("Region", p))
                                .child(
                                    Button::new("eks-region")
                                        .outline()
                                        .small()
                                        .label(region.clone())
                                        .dropdown_menu(move |mut menu, _, _| {
                                            for r in AWS_REGIONS {
                                                let this = this.clone();
                                                menu = menu.item(
                                                    PopupMenuItem::new(r)
                                                        .checked(r == region)
                                                        .on_click(move |_, _, cx| {
                                                            let _ = this.update(cx, |t, cx| {
                                                                t.aws_region = r.to_owned();
                                                                cx.notify();
                                                            });
                                                        }),
                                                );
                                            }
                                            menu
                                        }),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(rpx(5.))
                                .child(self.label("AWS profile", p))
                                .child(
                                    Button::new("eks-profile")
                                        .outline()
                                        .small()
                                        .label(
                                            profile
                                                .clone()
                                                .unwrap_or_else(|| "Default chain".into()),
                                        )
                                        .dropdown_menu(move |mut menu, _, _| {
                                            let options = std::iter::once(None)
                                                .chain(profiles.iter().cloned().map(Some));
                                            for pr in options {
                                                let this = this2.clone();
                                                let label = pr
                                                    .clone()
                                                    .unwrap_or_else(|| "Default chain".into());
                                                menu = menu.item(
                                                    PopupMenuItem::new(label)
                                                        .checked(pr == profile)
                                                        .on_click(move |_, _, cx| {
                                                            let pr = pr.clone();
                                                            let _ = this.update(cx, |t, cx| {
                                                                t.aws_profile = pr;
                                                                cx.notify();
                                                            });
                                                        }),
                                                );
                                            }
                                            menu
                                        }),
                                ),
                        )
                        .into_any_element(),
                )
            }
            _ => None,
        };
        let list: AnyElement = match &self.clusters {
            None if self.discover_request.is_some() => ui::shimmer(240., p),
            None => div().into_any_element(),
            Some(Err(e)) => self.error_card(e, p),
            Some(Ok(list)) if list.is_empty() => self.hint(
                "No clusters found for this account, project or region.",
                p.stg,
                p,
            ),
            Some(Ok(list)) => div()
                .flex()
                .flex_col()
                .children(list.iter().enumerate().map(|(i, c)| {
                    let chosen = self.chosen_cluster == Some(i);
                    let place = [
                        c.resource_group.clone(),
                        c.project.clone(),
                        Some(c.location.clone()),
                    ]
                    .into_iter()
                    .flatten()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(" · ");
                    div()
                        .id(("cluster", i))
                        .flex()
                        .items_center()
                        .gap(rpx(10.))
                        .px(rpx(10.))
                        .py(rpx(6.))
                        .border_b_1()
                        .border_color(p.line)
                        .cursor_pointer()
                        .when(chosen, |d| d.bg(p.sel))
                        .when(!chosen, |d| d.hover(|st| st.bg(p.hover)))
                        .on_click(cx.listener(move |t, _, _, cx| {
                            t.chosen_cluster = Some(i);
                            cx.notify();
                        }))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .font_family(MONO)
                                        .text_size(ts::BODY)
                                        .child(c.name.clone()),
                                )
                                .child(
                                    div()
                                        .text_size(ts::SMALL)
                                        .text_color(p.fg3)
                                        .truncate()
                                        .child(place),
                                ),
                        )
                        .when(c.entra, |d| {
                            d.child(
                                div()
                                    .text_size(ts::SMALL)
                                    .text_color(p.fg2)
                                    .child("Entra ID"),
                            )
                        })
                        .child(
                            div()
                                .text_size(ts::SMALL)
                                .text_color(p.fg3)
                                .child(format!("{} {}", c.status, c.version)),
                        )
                }))
                .into_any_element(),
        };
        let chosen = match (&self.clusters, self.chosen_cluster) {
            (Some(Ok(l)), Some(i)) => l.get(i),
            _ => None,
        };
        let aks_admin = chosen
            .filter(|c| c.provider == ClusterProvider::Aks)
            .map(|c| {
                let disabled = c.local_accounts_disabled;
                ui::checkbox(
                    "aks-admin",
                    self.admin && !disabled,
                    if disabled {
                        "Admin credentials (local accounts are disabled on this cluster)"
                    } else {
                        "Admin credentials (--admin, skips Entra sign-in)"
                    },
                    p,
                )
                .cursor_pointer()
                .when(disabled, |d| d.opacity(0.5))
                .when(!disabled, |d| {
                    d.on_click(cx.listener(|t, _, _, cx| {
                        t.admin = !t.admin;
                        cx.notify();
                    }))
                })
                .into_any_element()
            });
        let entra_note = chosen
            .filter(|c| c.provider == ClusterProvider::Aks && c.entra && !self.admin)
            .map(|_| {
                self.hint(
                    "Entra ID cluster: after get-credentials, Switchyard runs kubelogin convert-kubeconfig -l azurecli, so kubectl signs in with your az login.",
                    p.acc,
                    p,
                )
            });
        let cli = self
            .source
            .provider()
            .and_then(|pr| pr.cli())
            .map_or("", |t| t.program());
        div()
            .flex()
            .flex_col()
            .gap(rpx(10.))
            .child(div().text_size(ts::BODY).text_color(p.fg2).child(format!(
                "Clusters are listed with {cli} as you are signed in there; adding one runs its get-credentials into your kubeconfig."
            )))
            .children(options)
            .child(
                div().flex().gap(rpx(8.)).child(
                    ui::button("cloud-list", "List clusters", Kind::Secondary, p)
                        .when(self.discover_request.is_some(), |b| b.opacity(0.5))
                        .on_click(cx.listener(|t, _, _, cx| {
                            if t.discover_request.is_none() {
                                t.discover(cx);
                            }
                        })),
                ),
            )
            .child(list)
            .children(aks_admin)
            .children(entra_note)
            .when_some(self.cred_error.as_ref(), |d, e| d.child(self.error_card(e, p)))
            .into_any_element()
    }

    fn render_rancher(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let list: AnyElement = match &self.rancher_clusters {
            None if self.rancher_request.is_some() => ui::shimmer(240., p),
            None => div().into_any_element(),
            Some(Err(e)) => self.error_card(e, p),
            Some(Ok(list)) if list.is_empty() => {
                self.hint("This token sees no clusters.", p.stg, p)
            }
            Some(Ok(list)) => div()
                .flex()
                .flex_col()
                .children(list.iter().enumerate().map(|(i, c)| {
                    let cluster = c.clone();
                    let usable = c.can_kubeconfig;
                    div()
                        .id(("rancher", i))
                        .flex()
                        .items_center()
                        .gap(rpx(10.))
                        .px(rpx(10.))
                        .py(rpx(6.))
                        .border_b_1()
                        .border_color(p.line)
                        .when(!usable, |d| d.opacity(0.5))
                        .when(usable, |d| {
                            d.cursor_pointer()
                                .hover(|st| st.bg(p.hover))
                                .on_click(cx.listener(move |t, _, window, cx| {
                                    t.use_rancher(cluster.clone(), window, cx)
                                }))
                        })
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .font_family(MONO)
                                .text_size(ts::BODY)
                                .child(c.name.clone()),
                        )
                        .child(
                            div()
                                .text_size(ts::SMALL)
                                .text_color(p.fg3)
                                .child(format!("{} · {} · {}", c.driver, c.state, c.version)),
                        )
                }))
                .into_any_element(),
        };
        div()
            .flex()
            .flex_col()
            .gap(rpx(10.))
            .child(self.field("Server URL", &self.rancher_server, p))
            .child(self.field("API token", &self.rancher_token, p))
            .child(self.field("Trusted certificate (self-signed servers)", &self.rancher_pem, p))
            .child(
                div()
                    .text_size(ts::SMALL)
                    .text_color(p.fg3)
                    .child("The token is kept in the keychain (or vault) when you save. Certificate checks stay on; a PEM given here is trusted for this connection only."),
            )
            .child(
                div().flex().child(
                    ui::button("rancher-list", "List clusters", Kind::Secondary, p)
                        .when(self.rancher_request.is_some(), |b| b.opacity(0.5))
                        .on_click(cx.listener(|t, _, _, cx| {
                            if t.rancher_request.is_none() {
                                t.list_rancher(cx);
                            }
                        })),
                ),
            )
            .child(list)
            .into_any_element()
    }

    fn render_details(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let kube = self.platform == ContainerPlatform::Kubernetes;
        let rancher = self.source == Source::Rancher && self.rancher_cluster.is_some();
        let this = cx.entity().downgrade();
        let host_label = self
            .host
            .as_ref()
            .and_then(|h| self.profiles.host(h))
            .map_or_else(|| "This computer".to_owned(), |h| h.name.clone());
        let hosts: Vec<(Option<ProfileId>, String)> =
            std::iter::once((None, "This computer".to_owned()))
                .chain(
                    self.profiles
                        .hosts()
                        .map(|h| (Some(h.id.clone()), format!("{} · over SSH", h.name))),
                )
                .collect();
        let chosen_host = self.host.clone();
        let host_menu = Button::new("ce-host")
            .outline()
            .small()
            .label(host_label)
            .dropdown_menu(move |mut menu, _, _| {
                for (id, label) in &hosts {
                    let this = this.clone();
                    let id = id.clone();
                    menu = menu.item(
                        PopupMenuItem::new(label.clone())
                            .checked(id == chosen_host)
                            .on_click(move |_, _, cx| {
                                let id = id.clone();
                                let _ = this.update(cx, |t, cx| {
                                    t.host = id;
                                    cx.notify();
                                });
                            }),
                    );
                }
                menu
            });
        let env_picker = div()
            .flex()
            .gap(rpx(6.))
            .children(EnvironmentLabel::ALL.iter().map(|e| {
                let e = *e;
                let active = e == self.env;
                div()
                    .id(SharedString::from(format!("ce-env-{}", e.badge())))
                    .flex_1()
                    .h(rpx(30.))
                    .flex()
                    .items_center()
                    .gap(rpx(8.))
                    .px(rpx(10.))
                    .border_1()
                    .border_color(if active { p.env(e) } else { p.bd2 })
                    .rounded(px(6.))
                    .bg(if active { p.env_bg(e) } else { p.bg })
                    .text_size(ts::UI)
                    .cursor_pointer()
                    .on_click(cx.listener(move |t, _, _, cx| {
                        t.env = e;
                        cx.notify();
                    }))
                    .child(ui::dot(p.env(e), 8.))
                    .child(e.name())
            }));
        let provider_line = self.provider.filter(|_| kube).map(|pr| {
            div()
                .text_size(ts::SMALL)
                .text_color(p.fg3)
                .child(format!("Provider: {}", pr.label()))
        });
        div()
            .flex()
            .flex_col()
            .gap(rpx(12.))
            .child(self.field("Name", &self.name, p))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(rpx(5.))
                    .child(self.label("Environment", p))
                    .child(env_picker),
            )
            .when(rancher, |d| {
                let rc = self.rancher_cluster.as_ref();
                d.child(self.hint(
                    format!(
                        "Rancher cluster {} ({}) on {}",
                        rc.map_or("", |c| c.name.as_str()),
                        rc.map_or("", |c| c.id.as_str()),
                        self.value(&self.rancher_server, cx)
                    ),
                    p.acc,
                    p,
                ))
                .child(self.field("API token", &self.rancher_token, p))
            })
            .when(!rancher, |d| {
                d.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(rpx(5.))
                        .child(self.label(
                            if kube {
                                "Run kubectl on"
                            } else {
                                "Run docker on"
                            },
                            p,
                        ))
                        .child(div().flex().child(host_menu)),
                )
            })
            .child(
                div()
                    .flex()
                    .gap(rpx(10.))
                    .child(div().flex_1().child(self.field(
                        if kube { "Context" } else { "Docker context" },
                        &self.context,
                        p,
                    )))
                    .when(kube, |d| {
                        d.child(
                            div()
                                .flex_1()
                                .child(self.field("Namespace", &self.namespace, p)),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .gap(rpx(10.))
                    .when(kube && !rancher, |d| {
                        d.child(div().flex_1().child(self.field(
                            "Kubeconfig file",
                            &self.kubeconfig,
                            p,
                        )))
                    })
                    .child(div().flex_1().child(self.field(
                        "CLI (optional; e.g. podman)",
                        &self.executable,
                        p,
                    ))),
            )
            .child(
                ui::checkbox(
                    "ce-read-only",
                    self.read_only,
                    "Read-only: browse, logs and shells only (no start, stop, scale or delete)",
                    p,
                )
                .cursor_pointer()
                .on_click(cx.listener(|t, _, _, cx| {
                    t.read_only = !t.read_only;
                    cx.notify();
                })),
            )
            .children(provider_line)
            .into_any_element()
    }
}

impl Render for ContainerEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let kube = self.platform == ContainerPlatform::Kubernetes;
        let title = match (&self.existing, kube, self.step) {
            (Some(c), _, _) => format!("Edit {}", c.name),
            (None, false, _) => "New Docker connection".to_owned(),
            (None, true, Step::Source) => "Add Kubernetes cluster".to_owned(),
            (None, true, _) => format!("Add cluster · {}", self.source.title()),
        };
        let body = match self.step {
            Step::Source => self.render_sources(&p, cx),
            Step::Find => match self.source {
                Source::Existing | Source::Local => self.render_contexts(&p, cx),
                Source::Aks | Source::Gke | Source::Eks => self.render_cloud(&p, cx),
                Source::Rancher => self.render_rancher(&p, cx),
            },
            Step::Details => self.render_details(&p, cx),
        };
        let (test_label, test_color) = match &self.test {
            TestState::Idle => (String::new(), p.fg3),
            TestState::Testing(_) => ("Connecting…".to_owned(), p.acc),
            TestState::Passed(s) => (s.clone(), p.dev),
            TestState::Failed(_) => ("Connection failed".to_owned(), p.prod),
        };
        let failure = match &self.test {
            TestState::Failed(m) => Some(m.clone()),
            _ => None,
        };
        let can_back = match self.step {
            Step::Source => false,
            Step::Find => true,
            Step::Details => self.existing.is_none() && kube,
        };
        let cloud_ready = self.step == Step::Find
            && matches!(self.source, Source::Aks | Source::Gke | Source::Eks)
            && self.chosen_cluster.is_some()
            && matches!(self.clusters, Some(Ok(_)));
        div()
            .id("container-editor")
            .w(rpx(640.))
            .max_h(rpx(640.))
            .flex()
            .flex_col()
            .bg(p.elev)
            .rounded(px(10.))
            .shadow(ui::shadow(&p))
            .overflow_hidden()
            .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
                cx.stop_propagation()
            })
            .child(
                div()
                    .h(rpx(46.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px(rpx(16.))
                    .border_b_1()
                    .border_color(p.bd)
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(ts::TITLE)
                            .child(title),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("cte-close")
                            .px(rpx(8.))
                            .py(rpx(2.))
                            .rounded(px(4.))
                            .text_color(p.fg3)
                            .hover(|s| s.bg(p.hover))
                            .on_click(
                                cx.listener(|_, _, _, cx| cx.emit(ContainerEditorEvent::Close)),
                            )
                            .child("×"),
                    ),
            )
            .child(
                div()
                    .id("cte-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p(rpx(16.))
                    .child(body),
            )
            .when_some(failure, |d, m| {
                d.child(
                    div()
                        .flex_none()
                        .mx(rpx(16.))
                        .mb(rpx(8.))
                        .child(self.hint(m, p.prod, &p)),
                )
            })
            .when_some(self.error.clone(), |d, m| {
                d.child(
                    div()
                        .flex_none()
                        .mx(rpx(16.))
                        .mb(rpx(8.))
                        .text_size(ts::BODY)
                        .text_color(p.prod)
                        .child(m),
                )
            })
            .child(
                div()
                    .h(rpx(52.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(rpx(8.))
                    .px(rpx(16.))
                    .border_t_1()
                    .border_color(p.bd)
                    .when(can_back, |d| {
                        d.child(
                            ui::button("cte-back", "Back", Kind::Ghost, &p)
                                .on_click(cx.listener(|t, _, _, cx| t.back(cx))),
                        )
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(ts::BODY)
                            .text_color(test_color)
                            .child(test_label),
                    )
                    .when(self.step == Step::Details, |d| {
                        d.child(
                            ui::button("cte-test", "Test", Kind::Secondary, &p)
                                .on_click(cx.listener(|t, _, _, cx| t.run_test(cx))),
                        )
                        .child(
                            ui::button("cte-save", "Save", Kind::Primary, &p)
                                .when(self.save_request.is_some(), |b| b.opacity(0.6))
                                .on_click(cx.listener(|t, _, _, cx| t.save(cx))),
                        )
                    })
                    .when(cloud_ready, |d| {
                        d.child(
                            ui::button("cte-add", "Add to kubeconfig", Kind::Primary, &p)
                                .when(self.cred_request.is_some(), |b| b.opacity(0.6))
                                .on_click(cx.listener(|t, _, _, cx| {
                                    if t.cred_request.is_none() {
                                        t.add_credentials(cx);
                                    }
                                })),
                        )
                    }),
            )
    }
}

impl Workspace {
    /// Open the Docker / Kubernetes dialog; `existing` edits a saved connection.
    pub(crate) fn open_container_editor(
        &mut self,
        platform: ContainerPlatform,
        existing: Option<ContainerConnection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let core = self.core.clone();
        let profiles = self.profiles.clone();
        let ed = cx.new(|cx| ContainerEditor::new(core, profiles, platform, existing, window, cx));
        let sub = cx.subscribe_in(
            &ed,
            window,
            |this, _, ev: &ContainerEditorEvent, _, cx| match ev {
                ContainerEditorEvent::Close => {
                    this.overlay = None;
                    cx.notify();
                }
            },
        );
        self._subs.push(sub);
        self.overlay = Some(crate::overlays::Overlay::ContainerEditor(ed));
        cx.notify();
    }

    /// Answers for the dialog's lookups.
    pub(crate) fn on_add_cluster_event(
        &mut self,
        ev: switchyard_core::Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use switchyard_core::Event;
        let request = match &ev {
            Event::KubeContexts { request, .. }
            | Event::ClustersDiscovered { request, .. }
            | Event::ClusterCredentials { request, .. }
            | Event::AwsProfiles { request, .. }
            | Event::RancherClusters { request, .. } => *request,
            _ => return,
        };
        if let Some(crate::overlays::Overlay::ContainerEditor(ed)) = &self.overlay
            && ed.read(cx).owns(request)
        {
            ed.update(cx, |ed, cx| ed.on_event(ev, window, cx));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(name: &str, provider: ClusterProvider) -> KubeContext {
        KubeContext {
            name: name.into(),
            cluster: name.into(),
            user: name.into(),
            namespace: None,
            server: String::new(),
            exec: None,
            provider,
            local_flavor: None,
            plugin_missing: false,
        }
    }

    #[test]
    fn local_source_lists_only_local_contexts() {
        let contexts = Contexts {
            current: None,
            contexts: vec![
                ctx("kind-dev", ClusterProvider::Local),
                ctx("prod-weu", ClusterProvider::Aks),
            ],
        };
        let local: Vec<_> = contexts_for(&contexts, Source::Local)
            .into_iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(local, ["kind-dev"]);
        assert_eq!(contexts_for(&contexts, Source::Existing).len(), 2);
        assert_eq!(blank("  "), None);
        assert_eq!(blank(" a "), Some("a".into()));
    }
}
