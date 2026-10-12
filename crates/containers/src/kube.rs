//! Kubernetes listings: `kubectl config view -o json` and `kubectl get … -o json`.
//!
//! Parsed leniently from `serde_json::Value` (a missing field reads as empty), so
//! objects from older or newer API versions still list.

use serde_json::Value;

use crate::clusters::{ClusterProvider, infer_provider, local_flavor, plugin_name};
use crate::{ContainersError, Result, Tool, parse_rfc3339};

/// `kubectl config view -o json` arguments.
pub fn contexts_args() -> Vec<String> {
    vec!["config".into(), "view".into(), "-o".into(), "json".into()]
}

/// `kubectl get <resource> -o json` in `namespace` (`None`: all namespaces, unless the
/// resource is cluster-wide).
pub fn get_args(resource: &str, namespace: Option<&str>, cluster_wide: bool) -> Vec<String> {
    let mut v = vec![
        "get".to_owned(),
        resource.to_owned(),
        "-o".to_owned(),
        "json".to_owned(),
    ];
    if !cluster_wide {
        match namespace.map(str::trim).filter(|n| !n.is_empty()) {
            Some(ns) => v.extend(["-n".to_owned(), ns.to_owned()]),
            None => v.push("-A".to_owned()),
        }
    }
    v
}

/// One kubeconfig context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Context {
    /// Name.
    pub name: String,
    /// Cluster.
    pub cluster: String,
    /// User.
    pub user: String,
    /// Its default namespace.
    pub namespace: Option<String>,
    /// The cluster's API server URL.
    pub server: String,
    /// The user's exec credential plugin (`kubelogin`, `gke-gcloud-auth-plugin`, `aws`),
    /// as a program name.
    pub exec: Option<String>,
    /// Where the cluster runs, inferred from the server, names and plugin.
    pub provider: ClusterProvider,
    /// The local distribution (`kind`, `minikube`, …) of a local cluster.
    pub local_flavor: Option<&'static str>,
    /// The exec plugin is not installed (checked by the caller where it runs).
    pub plugin_missing: bool,
}

/// The kubeconfig's contexts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Contexts {
    /// The current context.
    pub current: Option<String>,
    /// All of them, in file order.
    pub contexts: Vec<Context>,
}

/// One namespace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Namespace {
    /// Name.
    pub name: String,
    /// Phase (`Active`, `Terminating`).
    pub phase: String,
}

/// One container port of a pod.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerPort {
    /// Container.
    pub container: String,
    /// Port number.
    pub port: u16,
    /// Port name.
    pub name: Option<String>,
    /// Protocol.
    pub protocol: String,
}

/// One pod, with the columns of `kubectl get pods`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pod {
    /// Namespace.
    pub namespace: String,
    /// Name.
    pub name: String,
    /// Status as kubectl shows it (`Running`, `CrashLoopBackOff`, `Init:1/2`, …).
    pub status: String,
    /// Phase (`Pending`, `Running`, `Succeeded`, `Failed`, `Unknown`).
    pub phase: String,
    /// Ready containers.
    pub ready: u32,
    /// Containers.
    pub total: u32,
    /// Restarts of all containers.
    pub restarts: u32,
    /// Node it runs on.
    pub node: String,
    /// Pod IP.
    pub ip: String,
    /// Created, Unix seconds.
    pub created: Option<i64>,
    /// Container names (the log and shell pickers).
    pub containers: Vec<String>,
    /// Declared container ports (port-forward suggestions).
    pub ports: Vec<ContainerPort>,
}

impl Pod {
    /// Whether the status reads as healthy (green), failing (red) or neither.
    pub fn health(&self) -> Health {
        health_of(&self.status)
    }
}

/// How a status reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    /// Running or completed.
    Good,
    /// Starting, terminating, pending.
    Pending,
    /// Failing.
    Bad,
}

/// How a status text reads.
pub fn health_of(status: &str) -> Health {
    match status {
        "Running" | "Completed" | "Succeeded" | "Active" => Health::Good,
        "Pending" | "ContainerCreating" | "PodInitializing" | "Terminating" | "NotReady" => {
            Health::Pending
        }
        s if s.starts_with("Init:") && !s.contains("Err") && !s.contains("BackOff") => {
            Health::Pending
        }
        _ => Health::Bad,
    }
}

/// One deployment, with the columns of `kubectl get deployments`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deployment {
    /// Namespace.
    pub namespace: String,
    /// Name.
    pub name: String,
    /// Desired replicas.
    pub replicas: u32,
    /// Ready replicas.
    pub ready: u32,
    /// Up-to-date replicas.
    pub up_to_date: u32,
    /// Available replicas.
    pub available: u32,
    /// Images of its pod template.
    pub images: Vec<String>,
    /// Created, Unix seconds.
    pub created: Option<i64>,
}

/// One service port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServicePort {
    /// Name.
    pub name: Option<String>,
    /// Service port.
    pub port: u16,
    /// Node port.
    pub node_port: Option<u16>,
    /// Protocol.
    pub protocol: String,
}

/// One service, with the columns of `kubectl get services`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Service {
    /// Namespace.
    pub namespace: String,
    /// Name.
    pub name: String,
    /// Type (`ClusterIP`, `NodePort`, `LoadBalancer`, `ExternalName`).
    pub kind: String,
    /// Cluster IP (`None` for headless).
    pub cluster_ip: String,
    /// External IPs or host names.
    pub external: Vec<String>,
    /// Ports.
    pub ports: Vec<ServicePort>,
    /// Created, Unix seconds.
    pub created: Option<i64>,
}

impl Service {
    /// The PORT(S) column: `80:30080/TCP,443/TCP`.
    pub fn ports_text(&self) -> String {
        self.ports
            .iter()
            .map(|p| match p.node_port {
                Some(n) => format!("{}:{n}/{}", p.port, p.protocol),
                None => format!("{}/{}", p.port, p.protocol),
            })
            .collect::<Vec<_>>()
            .join(",")
    }
}

fn parse_json(stdout: &str) -> Result<Value> {
    serde_json::from_str(stdout).map_err(|e| ContainersError::Parse {
        tool: Tool::Kubectl,
        message: e.to_string(),
    })
}

fn items(v: &Value) -> &[Value] {
    v.get("items")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn s<'a>(v: &'a Value, path: &[&str]) -> &'a str {
    path.iter()
        .try_fold(v, |v, k| v.get(*k))
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn n(v: &Value, path: &[&str]) -> Option<u64> {
    path.iter().try_fold(v, |v, k| v.get(*k))?.as_u64()
}

fn arr<'a>(v: &'a Value, path: &[&str]) -> &'a [Value] {
    path.iter()
        .try_fold(v, |v, k| v.get(*k))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn count(v: Option<u64>) -> u32 {
    v.map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX))
}

fn port(v: Option<u64>) -> Option<u16> {
    v.and_then(|n| u16::try_from(n).ok())
}

fn created(item: &Value) -> Option<i64> {
    parse_rfc3339(s(item, &["metadata", "creationTimestamp"]))
}

/// The contexts of `kubectl config view -o json`.
pub fn parse_contexts(stdout: &str) -> Result<Contexts> {
    let v = parse_json(stdout)?;
    let current = Some(s(&v, &["current-context"]).to_owned()).filter(|c| !c.is_empty());
    let named = |list: &str, name: &str, field: &str| -> Option<Value> {
        arr(&v, &[list])
            .iter()
            .find(|x| s(x, &["name"]) == name)
            .and_then(|x| x.get(field))
            .cloned()
    };
    let contexts = arr(&v, &["contexts"])
        .iter()
        .map(|c| {
            let name = s(c, &["name"]).to_owned();
            let cluster = s(c, &["context", "cluster"]).to_owned();
            let user = s(c, &["context", "user"]).to_owned();
            let server = named("clusters", &cluster, "cluster")
                .map(|x| s(&x, &["server"]).to_owned())
                .unwrap_or_default();
            let exec = named("users", &user, "user").and_then(|u| u.get("exec").cloned());
            let exec_cmd = exec
                .as_ref()
                .map(|e| s(e, &["command"]).to_owned())
                .filter(|c| !c.is_empty());
            let exec_args: Vec<String> = exec
                .as_ref()
                .map(|e| {
                    arr(e, &["args"])
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            let provider =
                infer_provider(&name, &cluster, &server, exec_cmd.as_deref(), &exec_args);
            Context {
                local_flavor: if provider == ClusterProvider::Local {
                    local_flavor(&name, &cluster, &server)
                } else {
                    None
                },
                namespace: Some(s(c, &["context", "namespace"]).to_owned())
                    .filter(|n| !n.is_empty()),
                exec: exec_cmd.map(|c| plugin_name(&c)),
                plugin_missing: false,
                provider,
                server,
                name,
                cluster,
                user,
            }
        })
        .filter(|c| !c.name.is_empty())
        .collect();
    Ok(Contexts { current, contexts })
}

/// The namespaces of `kubectl get namespaces -o json`, by name.
pub fn parse_namespaces(stdout: &str) -> Result<Vec<Namespace>> {
    let v = parse_json(stdout)?;
    let mut out: Vec<Namespace> = items(&v)
        .iter()
        .map(|i| Namespace {
            name: s(i, &["metadata", "name"]).to_owned(),
            phase: s(i, &["status", "phase"]).to_owned(),
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// The STATUS column, following kubectl's `printPod`.
fn pod_status(item: &Value) -> String {
    let status = &item["status"];
    let mut reason = match s(status, &["reason"]) {
        "" => s(status, &["phase"]).to_owned(),
        r => r.to_owned(),
    };
    let exit_reason = |t: &Value| -> String {
        match s(t, &["reason"]) {
            "" => match n(t, &["signal"]) {
                Some(sig) if sig != 0 => format!("Signal:{sig}"),
                _ => format!("ExitCode:{}", t["exitCode"].as_i64().unwrap_or_default()),
            },
            r => r.to_owned(),
        }
    };
    let inits = arr(status, &["initContainerStatuses"]);
    let mut initializing = false;
    for (i, c) in inits.iter().enumerate() {
        let state = &c["state"];
        if let Some(t) = state.get("terminated") {
            if t["exitCode"].as_i64() == Some(0) {
                continue;
            }
            reason = format!("Init:{}", exit_reason(t));
        } else if let Some(w) = state.get("waiting")
            && !matches!(s(w, &["reason"]), "" | "PodInitializing")
        {
            reason = format!("Init:{}", s(w, &["reason"]));
        } else {
            reason = format!("Init:{i}/{}", inits.len());
        }
        initializing = true;
        break;
    }
    if !initializing {
        let mut has_running = false;
        for c in arr(status, &["containerStatuses"]).iter().rev() {
            let state = &c["state"];
            if let Some(w) = state.get("waiting")
                && !s(w, &["reason"]).is_empty()
            {
                reason = s(w, &["reason"]).to_owned();
            } else if let Some(t) = state.get("terminated") {
                reason = exit_reason(t);
            } else if state.get("running").is_some() && c["ready"].as_bool() == Some(true) {
                has_running = true;
            }
        }
        if reason == "Completed" && has_running {
            reason = "Running".into();
        }
    }
    if item["metadata"].get("deletionTimestamp").is_some() {
        reason = if s(status, &["reason"]) == "NodeLost" {
            "Unknown".into()
        } else {
            "Terminating".into()
        };
    }
    reason
}

/// The pods of `kubectl get pods -o json`, by namespace and name.
pub fn parse_pods(stdout: &str) -> Result<Vec<Pod>> {
    let v = parse_json(stdout)?;
    let mut out: Vec<Pod> = items(&v)
        .iter()
        .map(|i| {
            let statuses = arr(i, &["status", "containerStatuses"]);
            let containers: Vec<String> = arr(i, &["spec", "containers"])
                .iter()
                .map(|c| s(c, &["name"]).to_owned())
                .collect();
            let ports = arr(i, &["spec", "containers"])
                .iter()
                .flat_map(|c| {
                    let name = s(c, &["name"]).to_owned();
                    arr(c, &["ports"]).iter().filter_map(move |p| {
                        Some(ContainerPort {
                            container: name.clone(),
                            port: port(n(p, &["containerPort"]))?,
                            name: Some(s(p, &["name"]).to_owned()).filter(|n| !n.is_empty()),
                            protocol: match s(p, &["protocol"]) {
                                "" => "TCP".to_owned(),
                                p => p.to_owned(),
                            },
                        })
                    })
                })
                .collect();
            let restarts = statuses
                .iter()
                .chain(arr(i, &["status", "initContainerStatuses"]))
                .map(|c| count(n(c, &["restartCount"])))
                .fold(0u32, u32::saturating_add);
            Pod {
                namespace: s(i, &["metadata", "namespace"]).to_owned(),
                name: s(i, &["metadata", "name"]).to_owned(),
                status: pod_status(i),
                phase: s(i, &["status", "phase"]).to_owned(),
                ready: count(Some(
                    statuses
                        .iter()
                        .filter(|c| c["ready"].as_bool() == Some(true))
                        .count() as u64,
                )),
                total: count(Some(containers.len() as u64)),
                restarts,
                node: s(i, &["spec", "nodeName"]).to_owned(),
                ip: s(i, &["status", "podIP"]).to_owned(),
                created: created(i),
                containers,
                ports,
            }
        })
        .collect();
    out.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    Ok(out)
}

/// The deployments of `kubectl get deployments -o json`.
pub fn parse_deployments(stdout: &str) -> Result<Vec<Deployment>> {
    let v = parse_json(stdout)?;
    let mut out: Vec<Deployment> = items(&v)
        .iter()
        .map(|i| Deployment {
            namespace: s(i, &["metadata", "namespace"]).to_owned(),
            name: s(i, &["metadata", "name"]).to_owned(),
            // The API's default is one replica.
            replicas: count(n(i, &["spec", "replicas"]).or(Some(1))),
            ready: count(n(i, &["status", "readyReplicas"])),
            up_to_date: count(n(i, &["status", "updatedReplicas"])),
            available: count(n(i, &["status", "availableReplicas"])),
            images: arr(i, &["spec", "template", "spec", "containers"])
                .iter()
                .map(|c| s(c, &["image"]).to_owned())
                .collect(),
            created: created(i),
        })
        .collect();
    out.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    Ok(out)
}

/// The services of `kubectl get services -o json`.
pub fn parse_services(stdout: &str) -> Result<Vec<Service>> {
    let v = parse_json(stdout)?;
    let mut out: Vec<Service> = items(&v)
        .iter()
        .map(|i| {
            let mut external: Vec<String> = arr(i, &["status", "loadBalancer", "ingress"])
                .iter()
                .map(|g| match s(g, &["ip"]) {
                    "" => s(g, &["hostname"]).to_owned(),
                    ip => ip.to_owned(),
                })
                .filter(|e| !e.is_empty())
                .collect();
            external.extend(
                arr(i, &["spec", "externalIPs"])
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned),
            );
            if let Some(name) = Some(s(i, &["spec", "externalName"])).filter(|n| !n.is_empty()) {
                external.push(name.to_owned());
            }
            Service {
                namespace: s(i, &["metadata", "namespace"]).to_owned(),
                name: s(i, &["metadata", "name"]).to_owned(),
                kind: match s(i, &["spec", "type"]) {
                    "" => "ClusterIP".to_owned(),
                    t => t.to_owned(),
                },
                cluster_ip: s(i, &["spec", "clusterIP"]).to_owned(),
                external,
                ports: arr(i, &["spec", "ports"])
                    .iter()
                    .filter_map(|p| {
                        Some(ServicePort {
                            name: Some(s(p, &["name"]).to_owned()).filter(|n| !n.is_empty()),
                            port: port(n(p, &["port"]))?,
                            node_port: port(n(p, &["nodePort"])),
                            protocol: match s(p, &["protocol"]) {
                                "" => "TCP".to_owned(),
                                p => p.to_owned(),
                            },
                        })
                    })
                    .collect(),
                created: created(i),
            }
        })
        .collect();
    out.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok<T>(r: Result<T>) -> T {
        r.unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn contexts() {
        let c = ok(parse_contexts(include_str!(
            "../testdata/kubectl_config_view.json"
        )));
        assert_eq!(c.current.as_deref(), Some("kind-dev"));
        assert_eq!(c.contexts.len(), 2);
        assert_eq!(c.contexts[1].name, "prod-eu");
        assert_eq!(c.contexts[1].namespace.as_deref(), Some("shop"));
        assert_eq!(c.contexts[0].namespace, None);
        // An empty kubeconfig lists nothing.
        let empty = ok(parse_contexts(
            r#"{"kind":"Config","contexts":null,"current-context":""}"#,
        ));
        assert_eq!(empty, Contexts::default());
    }

    #[test]
    fn namespaces() {
        let ns = ok(parse_namespaces(include_str!(
            "../testdata/kubectl_namespaces.json"
        )));
        let names: Vec<&str> = ns.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["default", "kube-system", "old", "shop"]);
        assert_eq!(ns[2].phase, "Terminating");
    }

    #[test]
    fn pods_read_like_kubectl() {
        let pods = ok(parse_pods(include_str!("../testdata/kubectl_pods.json")));
        let row = |name: &str| {
            pods.iter()
                .find(|p| p.name == name)
                .unwrap_or_else(|| panic!("{name} missing"))
        };
        let web = row("web-7d9f8-abcde");
        assert_eq!(web.status, "Running");
        assert_eq!((web.ready, web.total, web.restarts), (2, 2, 3));
        assert_eq!(web.node, "kind-dev-worker");
        assert_eq!(web.containers, ["nginx", "envoy"]);
        assert_eq!(web.ports.len(), 2);
        assert_eq!(web.ports[0].port, 80);
        assert_eq!(web.ports[0].name.as_deref(), Some("http"));
        assert_eq!(web.created, parse_rfc3339("2026-10-10T09:00:00Z"));
        assert_eq!(web.health(), Health::Good);

        let worker = row("worker-5c6b-xyz12");
        assert_eq!(worker.status, "CrashLoopBackOff");
        assert_eq!((worker.ready, worker.restarts), (0, 14));
        assert_eq!(worker.health(), Health::Bad);

        assert_eq!(row("migrate-8k2j4").status, "Init:1/2");
        assert_eq!(row("migrate-8k2j4").health(), Health::Pending);
        assert_eq!(row("backup-28812").status, "Completed");
        assert_eq!(row("web-old-1").status, "Terminating");
        assert_eq!(row("pending-q").status, "Pending");
        assert_eq!(row("pending-q").node, "");

        // Sorted by namespace, then name.
        assert_eq!(pods[0].namespace, "ops");
    }

    #[test]
    fn init_failures_and_exit_codes() {
        let pod = r#"{"items":[{"metadata":{"name":"p","namespace":"n"},
            "spec":{"containers":[{"name":"c"}]},
            "status":{"phase":"Pending","initContainerStatuses":[
              {"name":"i","state":{"terminated":{"exitCode":3}}}]}}]}"#;
        assert_eq!(ok(parse_pods(pod))[0].status, "Init:ExitCode:3");
        let pod = r#"{"items":[{"metadata":{"name":"p","namespace":"n"},
            "spec":{"containers":[{"name":"c"}]},
            "status":{"phase":"Running","containerStatuses":[
              {"name":"c","state":{"terminated":{"exitCode":137,"signal":9}}}]}}]}"#;
        assert_eq!(ok(parse_pods(pod))[0].status, "Signal:9");
        let pod = r#"{"items":[{"metadata":{"name":"p","namespace":"n"},
            "spec":{"containers":[{"name":"c"}]},
            "status":{"phase":"Pending","initContainerStatuses":[
              {"name":"i","state":{"waiting":{"reason":"ImagePullBackOff"}}}]}}]}"#;
        let p = ok(parse_pods(pod));
        assert_eq!(p[0].status, "Init:ImagePullBackOff");
        assert_eq!(p[0].health(), Health::Bad);
    }

    #[test]
    fn deployments() {
        let d = ok(parse_deployments(include_str!(
            "../testdata/kubectl_deployments.json"
        )));
        assert_eq!(d.len(), 2);
        assert_eq!(
            (
                d[0].name.as_str(),
                d[0].ready,
                d[0].replicas,
                d[0].up_to_date,
                d[0].available
            ),
            ("web", 2, 3, 3, 2)
        );
        assert_eq!(d[0].images, ["nginx:1.27", "envoyproxy/envoy:v1.31"]);
        assert_eq!((d[1].replicas, d[1].ready), (0, 0));
    }

    #[test]
    fn services() {
        let s = ok(parse_services(include_str!(
            "../testdata/kubectl_services.json"
        )));
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].name, "kubernetes");
        assert_eq!(s[0].ports_text(), "443/TCP");
        let web = s
            .iter()
            .find(|s| s.name == "web")
            .unwrap_or_else(|| panic!("web missing"));
        assert_eq!(web.kind, "LoadBalancer");
        assert_eq!(web.external, ["203.0.113.10", "web.example.com"]);
        assert_eq!(web.ports_text(), "80:30080/TCP,9901:30901/TCP");
        let db = s
            .iter()
            .find(|s| s.name == "db")
            .unwrap_or_else(|| panic!("db missing"));
        assert_eq!(db.cluster_ip, "None");
        assert!(db.external.is_empty());
    }

    #[test]
    fn get_args_scope() {
        assert_eq!(
            get_args("pods", Some(" shop "), false),
            ["get", "pods", "-o", "json", "-n", "shop"]
        );
        assert_eq!(
            get_args("pods", Some(""), false).last().map(String::as_str),
            Some("-A")
        );
        assert_eq!(
            get_args("namespaces", None, true),
            ["get", "namespaces", "-o", "json"]
        );
    }

    #[test]
    fn not_json_is_a_parse_error() {
        assert!(matches!(
            parse_pods("error: You must be logged in"),
            Err(ContainersError::Parse { .. })
        ));
    }
}
