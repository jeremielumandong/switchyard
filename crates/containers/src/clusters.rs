//! Where Kubernetes clusters come from: AKS, GKE, EKS, Rancher and local clusters.
//!
//! Switchyard never holds cloud credentials for a cluster: the provider's own CLI
//! (`az`, `gcloud`, `aws`) lists clusters and writes a kubeconfig context, and the
//! context's exec plugin (`kubelogin`, `gke-gcloud-auth-plugin`, `aws eks get-token`)
//! signs in each time kubectl runs. Rancher has no CLI step: its API (called by core
//! with the user's API token from the keychain) hands out a kubeconfig.
//!
//! This module infers a context's provider, builds the provider CLI calls, parses their
//! JSON, and turns kubectl's sign-in failures into hints.

use serde_json::Value;

use crate::{ContainersError, Invocation, Result, Tool};

/// Where a cluster runs, for labels and hints.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ClusterProvider {
    /// Azure Kubernetes Service.
    Aks,
    /// Google Kubernetes Engine.
    Gke,
    /// Amazon Elastic Kubernetes Service.
    Eks,
    /// A cluster managed by Rancher (through its API proxy).
    Rancher,
    /// kind, minikube, k3d, k3s, Docker Desktop, Rancher Desktop, …
    Local,
    /// Anything else.
    #[default]
    Other,
}

impl ClusterProvider {
    /// Every provider, in menu order.
    pub const ALL: [ClusterProvider; 6] = [
        ClusterProvider::Aks,
        ClusterProvider::Gke,
        ClusterProvider::Eks,
        ClusterProvider::Rancher,
        ClusterProvider::Local,
        ClusterProvider::Other,
    ];

    /// Short label (`AKS`).
    pub fn label(self) -> &'static str {
        match self {
            ClusterProvider::Aks => "AKS",
            ClusterProvider::Gke => "GKE",
            ClusterProvider::Eks => "EKS",
            ClusterProvider::Rancher => "Rancher",
            ClusterProvider::Local => "Local",
            ClusterProvider::Other => "Other",
        }
    }

    /// Stored key (`aks`).
    pub fn key(self) -> &'static str {
        match self {
            ClusterProvider::Aks => "aks",
            ClusterProvider::Gke => "gke",
            ClusterProvider::Eks => "eks",
            ClusterProvider::Rancher => "rancher",
            ClusterProvider::Local => "local",
            ClusterProvider::Other => "other",
        }
    }

    /// From [`Self::key`].
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.key() == key)
    }

    /// The provider CLI that discovers its clusters, if any.
    pub fn cli(self) -> Option<Tool> {
        match self {
            ClusterProvider::Aks => Some(Tool::Az),
            ClusterProvider::Gke => Some(Tool::Gcloud),
            ClusterProvider::Eks => Some(Tool::Aws),
            _ => None,
        }
    }
}

/// The host part of a server URL (`https://host:443/path` → `host`).
fn server_host(server: &str) -> String {
    let rest = server
        .split_once("://")
        .map_or(server, |(_, r)| r)
        .split('/')
        .next()
        .unwrap_or_default();
    let host = if let Some(v6) = rest.strip_prefix('[') {
        v6.split(']').next().unwrap_or_default()
    } else {
        rest.rsplit_once(':').map_or(rest, |(h, _)| h)
    };
    host.to_ascii_lowercase()
}

/// The program name of an exec plugin path (`/usr/local/bin/kubelogin` → `kubelogin`).
pub fn plugin_name(command: &str) -> String {
    let base = command.rsplit(['/', '\\']).next().unwrap_or(command);
    base.strip_suffix(".exe").unwrap_or(base).to_owned()
}

/// Which local distribution a context is, from its context / cluster name and server.
pub fn local_flavor(context: &str, cluster: &str, server: &str) -> Option<&'static str> {
    let names = [context.to_ascii_lowercase(), cluster.to_ascii_lowercase()];
    let starts = |p: &str| names.iter().any(|n| n.starts_with(p));
    let is = |p: &str| names.iter().any(|n| n == p);
    if starts("kind-") {
        return Some("kind");
    }
    if is("minikube") || starts("minikube") {
        return Some("minikube");
    }
    if starts("k3d-") {
        return Some("k3d");
    }
    if is("docker-desktop") || is("docker-for-desktop") {
        return Some("Docker Desktop");
    }
    if is("rancher-desktop") {
        return Some("Rancher Desktop");
    }
    if is("colima") {
        return Some("Colima");
    }
    if is("orbstack") {
        return Some("OrbStack");
    }
    if is("microk8s") || is("microk8s-cluster") {
        return Some("MicroK8s");
    }
    let host = server_host(server);
    let loopback = matches!(
        host.as_str(),
        "127.0.0.1"
            | "localhost"
            | "0.0.0.0"
            | "::1"
            | "kubernetes.docker.internal"
            | "host.docker.internal"
    );
    if loopback {
        // k3s writes a context named "default" pointing at 127.0.0.1:6443.
        return Some(if is("default") && server.ends_with(":6443") {
            "k3s"
        } else {
            "local"
        });
    }
    None
}

/// The provider of a kubeconfig context, from its server URL, names and exec plugin.
pub fn infer_provider(
    context: &str,
    cluster: &str,
    server: &str,
    exec: Option<&str>,
    exec_args: &[String],
) -> ClusterProvider {
    let host = server_host(server);
    let plugin = exec.map(plugin_name);
    let arg = |a: &str| exec_args.iter().any(|x| x == a);
    if host.ends_with(".azmk8s.io")
        || host.ends_with(".azmk8s.us")
        || host.ends_with(".azk8s.cn")
        || plugin.as_deref() == Some("kubelogin") && !arg("oidc-login")
    {
        return ClusterProvider::Aks;
    }
    if plugin.as_deref() == Some("gke-gcloud-auth-plugin")
        || context.starts_with("gke_")
        || cluster.starts_with("gke_")
        || host.ends_with(".gke.goog")
    {
        return ClusterProvider::Gke;
    }
    if host.ends_with(".eks.amazonaws.com")
        || host.ends_with(".eks.amazonaws.com.cn")
        || context.starts_with("arn:aws:eks:")
        || cluster.starts_with("arn:aws:eks:")
        || plugin.as_deref() == Some("aws-iam-authenticator")
        || plugin.as_deref() == Some("aws") && arg("eks")
    {
        return ClusterProvider::Eks;
    }
    if server.contains("/k8s/clusters/") {
        return ClusterProvider::Rancher;
    }
    if local_flavor(context, cluster, server).is_some() {
        return ClusterProvider::Local;
    }
    ClusterProvider::Other
}

/// How to install a missing exec plugin.
pub fn plugin_install_hint(plugin: &str) -> String {
    match plugin_name(plugin).as_str() {
        "kubelogin" => "Install kubelogin: `az aks install-cli`, or `brew install Azure/kubelogin/kubelogin`, or winget install Microsoft.Azure.Kubelogin".into(),
        "gke-gcloud-auth-plugin" => "Install it with `gcloud components install gke-gcloud-auth-plugin` (or your package manager's google-cloud-cli-gke-gcloud-auth-plugin)".into(),
        "aws" => "Install the AWS CLI v2 (https://aws.amazon.com/cli/)".into(),
        "aws-iam-authenticator" => "Install aws-iam-authenticator, or run `aws eks update-kubeconfig` to switch the context to `aws eks get-token`".into(),
        "kubectl" | "kubectl-oidc_login" => "Install the kubelogin (oidc-login) kubectl plugin: `kubectl krew install oidc-login`".into(),
        other => format!("Install {other} and make sure it is on PATH"),
    }
}

/// What kubectl's error output says about signing in, as a hint for the user.
pub fn sign_in_hint(message: &str, provider: ClusterProvider) -> Option<String> {
    let m = message.to_ascii_lowercase();
    // A missing plugin: `exec: executable kubelogin not found`.
    if let Some(i) = m.find("exec: executable ") {
        let rest = &message[i + "exec: executable ".len()..];
        let plugin = rest.split_whitespace().next().unwrap_or_default();
        if m[i..].contains("not found") {
            return Some(format!(
                "kubectl needs {plugin} to sign in. {}",
                plugin_install_hint(plugin)
            ));
        }
    }
    let expired = [
        "unauthorized",
        "you must be logged in",
        "provide credentials",
        "token has expired",
        "expired token",
        "expiredtoken",
        "refresh token",
        "aadsts",
        "reauthenticate",
        "re-authenticate",
        "gcloud auth login",
        "sso session",
        "unable to locate credentials",
        "getting credentials: exec",
        "the security token included in the request is invalid",
        "invalid_grant",
    ];
    if !expired.iter().any(|p| m.contains(p)) {
        return None;
    }
    Some(
        match provider {
            ClusterProvider::Aks => {
                "Sign-in expired or missing: run `az login` (Entra clusters use kubelogin with the Azure CLI's sign-in)."
            }
            ClusterProvider::Gke => {
                "Sign-in expired or missing: run `gcloud auth login` (and `gcloud auth application-default login` if your setup uses it)."
            }
            ClusterProvider::Eks => {
                "Sign-in expired or missing: run `aws sso login --profile <profile>` or refresh your AWS credentials."
            }
            ClusterProvider::Rancher => {
                "The Rancher token was refused or expired: create a new API token in Rancher and save it in the connection."
            }
            _ => "Sign-in expired or refused: renew the credentials this context uses.",
        }
        .to_owned(),
    )
}

/// What to ask a provider CLI.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiscoverOptions {
    /// AWS region (EKS lists one region at a time).
    pub aws_region: Option<String>,
    /// AWS CLI profile (`None`: the default chain).
    pub aws_profile: Option<String>,
    /// Google Cloud project (`None`: gcloud's configured one).
    pub gcp_project: Option<String>,
}

/// A cluster a provider CLI listed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloudCluster {
    /// AKS, GKE or EKS.
    pub provider: ClusterProvider,
    /// Cluster name.
    pub name: String,
    /// Region or zone.
    pub location: String,
    /// AKS resource group.
    pub resource_group: Option<String>,
    /// GKE project.
    pub project: Option<String>,
    /// AWS CLI profile it was listed with.
    pub aws_profile: Option<String>,
    /// The cluster's ARN (EKS).
    pub arn: Option<String>,
    /// Kubernetes version.
    pub version: String,
    /// Provider status (`Running`, `RUNNING`, `ACTIVE`, …).
    pub status: String,
    /// AKS with Microsoft Entra ID: needs kubelogin.
    pub entra: bool,
    /// AKS with local accounts disabled (no admin credentials).
    pub local_accounts_disabled: bool,
}

impl CloudCluster {
    /// The kubeconfig context the provider CLI writes for it.
    pub fn context_name(&self, admin: bool) -> String {
        match self.provider {
            ClusterProvider::Aks if admin => format!("{}-admin", self.name),
            ClusterProvider::Gke => format!(
                "gke_{}_{}_{}",
                self.project.as_deref().unwrap_or_default(),
                self.location,
                self.name
            ),
            ClusterProvider::Eks => self.arn.clone().unwrap_or_else(|| self.name.clone()),
            _ => self.name.clone(),
        }
    }
}

fn json(tool: Tool, stdout: &str) -> Result<Value> {
    serde_json::from_str(stdout).map_err(|e| ContainersError::Parse {
        tool,
        message: e.to_string(),
    })
}

fn s(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn opt(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

fn aws_args(opts: &DiscoverOptions) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(r) = opt(opts.aws_region.clone()) {
        v.extend(["--region".to_owned(), r]);
    }
    if let Some(p) = opt(opts.aws_profile.clone()) {
        v.extend(["--profile".to_owned(), p]);
    }
    v.extend(["--output".to_owned(), "json".to_owned()]);
    v
}

/// The call that lists a provider's clusters (`az aks list`, `gcloud container clusters
/// list`, `aws eks list-clusters`).
pub fn list_invocation(provider: ClusterProvider, opts: &DiscoverOptions) -> Option<Invocation> {
    let (tool, args): (Tool, Vec<String>) = match provider {
        ClusterProvider::Aks => (
            Tool::Az,
            ["aks", "list", "-o", "json"].map(str::to_owned).to_vec(),
        ),
        ClusterProvider::Gke => {
            let mut a: Vec<String> = ["container", "clusters", "list", "--format=json"]
                .map(str::to_owned)
                .to_vec();
            if let Some(p) = opt(opts.gcp_project.clone()) {
                a.push(format!("--project={p}"));
            }
            (Tool::Gcloud, a)
        }
        ClusterProvider::Eks => {
            let mut a: Vec<String> = vec!["eks".into(), "list-clusters".into()];
            a.extend(aws_args(opts));
            (Tool::Aws, a)
        }
        _ => return None,
    };
    Some(Invocation { tool, args })
}

/// `aws eks describe-cluster` for one listed name.
pub fn eks_describe_invocation(name: &str, opts: &DiscoverOptions) -> Invocation {
    let mut args: Vec<String> = vec![
        "eks".into(),
        "describe-cluster".into(),
        "--name".into(),
        name.to_owned(),
    ];
    args.extend(aws_args(opts));
    Invocation {
        tool: Tool::Aws,
        args,
    }
}

/// The calls that add a cluster's context to the kubeconfig (`az aks get-credentials`,
/// `gcloud … get-credentials`, `aws eks update-kubeconfig`), then, for an AKS cluster
/// with Entra ID, `kubelogin convert-kubeconfig -l azurecli` so kubectl signs in with
/// the Azure CLI's login. `kubeconfig` writes to that file instead of the default.
pub fn credentials_invocations(
    c: &CloudCluster,
    admin: bool,
    kubeconfig: Option<&str>,
) -> Vec<Invocation> {
    let kubeconfig = kubeconfig.map(str::trim).filter(|k| !k.is_empty());
    match c.provider {
        ClusterProvider::Aks => {
            let mut args: Vec<String> = vec![
                "aks".into(),
                "get-credentials".into(),
                "-g".into(),
                c.resource_group.clone().unwrap_or_default(),
                "-n".into(),
                c.name.clone(),
                "--overwrite-existing".into(),
            ];
            if admin {
                args.push("--admin".into());
            }
            if let Some(k) = kubeconfig {
                args.extend(["--file".into(), k.to_owned()]);
            }
            let mut out = vec![Invocation {
                tool: Tool::Az,
                args,
            }];
            if c.entra && !admin {
                let mut args: Vec<String> = vec![
                    "convert-kubeconfig".into(),
                    "-l".into(),
                    "azurecli".into(),
                    "--context".into(),
                    c.context_name(false),
                ];
                if let Some(k) = kubeconfig {
                    args.extend(["--kubeconfig".into(), k.to_owned()]);
                }
                out.push(Invocation {
                    tool: Tool::Kubelogin,
                    args,
                });
            }
            out
        }
        ClusterProvider::Gke => {
            let location_flag = if is_gcp_zone(&c.location) {
                "--zone"
            } else {
                "--region"
            };
            let mut args: Vec<String> = vec![
                "container".into(),
                "clusters".into(),
                "get-credentials".into(),
                c.name.clone(),
                format!("{location_flag}={}", c.location),
            ];
            if let Some(p) = &c.project {
                args.push(format!("--project={p}"));
            }
            // gcloud writes to $KUBECONFIG; core sets it when `kubeconfig` is given.
            vec![Invocation {
                tool: Tool::Gcloud,
                args,
            }]
        }
        ClusterProvider::Eks => {
            let mut args: Vec<String> = vec![
                "eks".into(),
                "update-kubeconfig".into(),
                "--name".into(),
                c.name.clone(),
                "--region".into(),
                c.location.clone(),
            ];
            if let Some(p) = &c.aws_profile {
                args.extend(["--profile".into(), p.clone()]);
            }
            if let Some(k) = kubeconfig {
                args.extend(["--kubeconfig".into(), k.to_owned()]);
            }
            vec![Invocation {
                tool: Tool::Aws,
                args,
            }]
        }
        _ => Vec::new(),
    }
}

/// `europe-west1-b` is a zone, `europe-west1` a region.
fn is_gcp_zone(location: &str) -> bool {
    location
        .rsplit('-')
        .next()
        .is_some_and(|last| last.len() == 1 && last.chars().all(|c| c.is_ascii_lowercase()))
}

/// The clusters of `az aks list -o json`.
pub fn parse_aks_list(stdout: &str) -> Result<Vec<CloudCluster>> {
    let v = json(Tool::Az, stdout)?;
    let items = v.as_array().map(Vec::as_slice).unwrap_or_default();
    Ok(items
        .iter()
        .map(|c| {
            let aad = c.get("aadProfile").filter(|a| !a.is_null());
            CloudCluster {
                provider: ClusterProvider::Aks,
                name: s(c, "name"),
                location: s(c, "location"),
                resource_group: Some(s(c, "resourceGroup")),
                project: None,
                aws_profile: None,
                arn: None,
                version: match s(c, "currentKubernetesVersion") {
                    v if v.is_empty() => s(c, "kubernetesVersion"),
                    v => v,
                },
                status: c
                    .get("powerState")
                    .map(|p| s(p, "code"))
                    .filter(|p| !p.is_empty())
                    .unwrap_or_else(|| s(c, "provisioningState")),
                entra: aad.is_some(),
                local_accounts_disabled: c
                    .get("disableLocalAccounts")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }
        })
        .collect())
}

/// The project of a GKE `selfLink` (`…/projects/<p>/…`).
fn project_of(self_link: &str) -> Option<String> {
    let mut parts = self_link.split('/');
    parts.find(|p| *p == "projects")?;
    parts.next().map(str::to_owned).filter(|p| !p.is_empty())
}

/// The clusters of `gcloud container clusters list --format=json`.
pub fn parse_gke_list(stdout: &str, project: Option<&str>) -> Result<Vec<CloudCluster>> {
    let v = json(Tool::Gcloud, stdout)?;
    let items = v.as_array().map(Vec::as_slice).unwrap_or_default();
    Ok(items
        .iter()
        .map(|c| CloudCluster {
            provider: ClusterProvider::Gke,
            name: s(c, "name"),
            location: match s(c, "location") {
                l if l.is_empty() => s(c, "zone"),
                l => l,
            },
            resource_group: None,
            project: project_of(&s(c, "selfLink")).or_else(|| project.map(str::to_owned)),
            aws_profile: None,
            arn: None,
            version: s(c, "currentMasterVersion"),
            status: s(c, "status"),
            entra: false,
            local_accounts_disabled: false,
        })
        .collect())
}

/// The names of `aws eks list-clusters`.
pub fn parse_eks_list(stdout: &str) -> Result<Vec<String>> {
    let v = json(Tool::Aws, stdout)?;
    Ok(v.get("clusters")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default())
}

/// One cluster of `aws eks describe-cluster`.
pub fn parse_eks_describe(stdout: &str, opts: &DiscoverOptions) -> Result<CloudCluster> {
    let v = json(Tool::Aws, stdout)?;
    let c = v.get("cluster").ok_or_else(|| ContainersError::Parse {
        tool: Tool::Aws,
        message: "no cluster in the answer".into(),
    })?;
    let arn = s(c, "arn");
    // arn:aws:eks:<region>:<account>:cluster/<name>
    let region = arn
        .split(':')
        .nth(3)
        .map(str::to_owned)
        .filter(|r| !r.is_empty())
        .or_else(|| opt(opts.aws_region.clone()))
        .unwrap_or_default();
    Ok(CloudCluster {
        provider: ClusterProvider::Eks,
        name: s(c, "name"),
        location: region,
        resource_group: None,
        project: None,
        aws_profile: opt(opts.aws_profile.clone()),
        arn: Some(arn).filter(|a| !a.is_empty()),
        version: s(c, "version"),
        status: s(c, "status"),
        entra: false,
        local_accounts_disabled: false,
    })
}

/// AWS CLI profile names from `~/.aws/config` and `~/.aws/credentials` text.
pub fn aws_profiles(config: &str, credentials: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (text, is_config) in [(config, true), (credentials, false)] {
        for line in text.lines() {
            let l = line.trim();
            let Some(inner) = l.strip_prefix('[').and_then(|r| r.strip_suffix(']')) else {
                continue;
            };
            let inner = inner.trim();
            let name = if is_config {
                match inner.strip_prefix("profile ") {
                    Some(p) => p.trim(),
                    None if inner == "default" => "default",
                    // `[sso-session x]`, `[services x]`.
                    None => continue,
                }
            } else {
                inner
            };
            if !name.is_empty() && !out.iter().any(|p| p == name) {
                out.push(name.to_owned());
            }
        }
    }
    out
}

/// Common AWS regions for the EKS picker (any other can be typed).
pub const AWS_REGIONS: [&str; 17] = [
    "us-east-1",
    "us-east-2",
    "us-west-1",
    "us-west-2",
    "ca-central-1",
    "eu-west-1",
    "eu-west-2",
    "eu-west-3",
    "eu-central-1",
    "eu-north-1",
    "eu-south-1",
    "ap-south-1",
    "ap-southeast-1",
    "ap-southeast-2",
    "ap-northeast-1",
    "ap-northeast-2",
    "sa-east-1",
];

/// A cluster Rancher manages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RancherCluster {
    /// Id (`c-m-x7k2p9qz`, `local`).
    pub id: String,
    /// Name.
    pub name: String,
    /// State (`active`, `unavailable`, …).
    pub state: String,
    /// Distribution or driver (`rke2`, `k3s`, `AKS`, `imported`).
    pub driver: String,
    /// Kubernetes version.
    pub version: String,
    /// Rancher can generate a kubeconfig for it.
    pub can_kubeconfig: bool,
}

/// The clusters of Rancher's `GET /v3/clusters`.
pub fn parse_rancher_clusters(body: &str) -> Result<Vec<RancherCluster>> {
    let v = json(Tool::Rancher, body)?;
    let items = v
        .get("data")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut out: Vec<RancherCluster> = items
        .iter()
        .map(|c| RancherCluster {
            id: s(c, "id"),
            name: s(c, "name"),
            state: s(c, "state"),
            driver: match s(c, "provider") {
                p if p.is_empty() => s(c, "driver"),
                p => p,
            },
            version: c
                .get("version")
                .map(|v| s(v, "gitVersion"))
                .unwrap_or_default(),
            can_kubeconfig: c
                .get("actions")
                .and_then(|a| a.get("generateKubeconfig"))
                .is_some(),
        })
        .filter(|c| !c.id.is_empty())
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// The kubeconfig text of Rancher's `?action=generateKubeconfig` answer.
pub fn parse_rancher_kubeconfig(body: &str) -> Result<String> {
    let v = json(Tool::Rancher, body)?;
    let config = s(&v, "config");
    if config.trim().is_empty() {
        return Err(ContainersError::Parse {
            tool: Tool::Rancher,
            message: "the answer holds no kubeconfig".into(),
        });
    }
    Ok(config)
}

/// Rancher's API base for a server URL the user typed (`rancher.example.com` →
/// `https://rancher.example.com`). Only `https` is accepted.
pub fn rancher_base(server: &str) -> Result<String> {
    let s = server.trim().trim_end_matches('/');
    let s = s.strip_suffix("/v3").unwrap_or(s);
    if s.is_empty() {
        return Err(ContainersError::Unsupported(
            "Enter the Rancher server URL".into(),
        ));
    }
    if let Some(rest) = s.strip_prefix("http://") {
        return Err(ContainersError::Unsupported(format!(
            "Rancher must be reached over https (https://{rest})"
        )));
    }
    Ok(if s.starts_with("https://") {
        s.to_owned()
    } else {
        format!("https://{s}")
    })
}

/// Rancher's message for a failed request (`{"message": "…"}`), else the status.
pub fn rancher_error(status: u16, body: &str) -> ContainersError {
    let msg = serde_json::from_str::<Value>(body)
        .ok()
        .map(|v| s(&v, "message"))
        .filter(|m| !m.is_empty());
    let message = match (status, msg) {
        (401, _) => "Rancher refused the API token (expired or revoked): create a new one in Rancher → Account & API Keys".to_owned(),
        (403, Some(m)) => format!("Rancher refused access: {m}"),
        (_, Some(m)) => m,
        (s, None) => format!("Rancher answered HTTP {s}"),
    };
    ContainersError::Failed {
        tool: Tool::Rancher,
        code: Some(i32::from(status)),
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kube::parse_contexts;

    fn ok<T>(r: Result<T>) -> T {
        r.unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn providers_from_a_recorded_kubeconfig() {
        let c = ok(parse_contexts(include_str!(
            "../testdata/kubectl_config_providers.json"
        )));
        let got: Vec<(&str, ClusterProvider, Option<&str>)> = c
            .contexts
            .iter()
            .map(|x| (x.name.as_str(), x.provider, x.local_flavor))
            .collect();
        use ClusterProvider::*;
        assert_eq!(
            got,
            [
                ("shop-aks", Aks, None),
                ("gke_acme-prod_europe-west1_web", Gke, None),
                (
                    "arn:aws:eks:eu-west-1:123456789012:cluster/payments",
                    Eks,
                    None
                ),
                ("edge", Rancher, None),
                ("kind-dev", Local, Some("kind")),
                ("minikube", Local, Some("minikube")),
                ("k3d-demo", Local, Some("k3d")),
                ("docker-desktop", Local, Some("Docker Desktop")),
                ("rancher-desktop", Local, Some("Rancher Desktop")),
                ("default", Local, Some("k3s")),
                ("corp", Other, None),
            ]
        );
        let aks = &c.contexts[0];
        assert_eq!(aks.exec.as_deref(), Some("kubelogin"));
        assert!(aks.server.ends_with(".azmk8s.io:443"));
        assert_eq!(c.contexts[2].exec.as_deref(), Some("aws"));
        assert_eq!(c.contexts[10].exec.as_deref(), Some("kubectl"));
        assert_eq!(c.contexts[4].exec, None);
    }

    #[test]
    fn inference_edge_cases() {
        // A path to the plugin, and an EKS context renamed by the user.
        assert_eq!(
            infer_provider(
                "prod",
                "prod",
                "https://x.example",
                Some("/opt/bin/kubelogin.exe"),
                &[]
            ),
            ClusterProvider::Aks
        );
        assert_eq!(
            infer_provider(
                "payments",
                "payments",
                "https://ABC.yl4.us-east-1.eks.amazonaws.com",
                None,
                &[]
            ),
            ClusterProvider::Eks
        );
        // kubelogin's OIDC mode is not AKS.
        assert_eq!(
            infer_provider(
                "c",
                "c",
                "https://k.example",
                Some("kubelogin"),
                &["oidc-login".to_owned()]
            ),
            ClusterProvider::Other
        );
        assert_eq!(
            infer_provider("dev", "dev", "https://[::1]:6443", None, &[]),
            ClusterProvider::Local
        );
        assert_eq!(server_host("https://[::1]:6443/x"), "::1");
        assert_eq!(server_host("https://Host.Example:443"), "host.example");
        assert_eq!(plugin_name(r"C:\tools\kubelogin.exe"), "kubelogin");
        for p in ClusterProvider::ALL {
            assert_eq!(ClusterProvider::from_key(p.key()), Some(p));
        }
    }

    #[test]
    fn aks_list_and_credentials() {
        let list = ok(parse_aks_list(include_str!("../testdata/az_aks_list.json")));
        assert_eq!(list.len(), 2);
        let shop = &list[0];
        assert_eq!(
            (
                shop.name.as_str(),
                shop.location.as_str(),
                shop.status.as_str()
            ),
            ("shop-aks", "westeurope", "Running")
        );
        assert_eq!(shop.resource_group.as_deref(), Some("shop-rg"));
        assert!(shop.entra && shop.local_accounts_disabled);
        assert_eq!(shop.version, "1.31.2");
        assert!(!list[1].entra);
        assert_eq!(list[1].status, "Stopped");

        let calls = credentials_invocations(shop, false, None);
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0].shell_line("az"),
            "az aks get-credentials -g shop-rg -n shop-aks --overwrite-existing"
        );
        assert_eq!(
            calls[1].shell_line("kubelogin"),
            "kubelogin convert-kubeconfig -l azurecli --context shop-aks"
        );
        let admin = credentials_invocations(&list[1], true, Some("/tmp/k"));
        assert_eq!(admin.len(), 1);
        assert!(admin[0].args.ends_with(&[
            "--admin".to_owned(),
            "--file".to_owned(),
            "/tmp/k".to_owned()
        ]));
        assert_eq!(list[1].context_name(true), "lab-admin");
        assert_eq!(
            list_invocation(ClusterProvider::Aks, &DiscoverOptions::default()).map(|i| i.args),
            Some(vec![
                "aks".into(),
                "list".into(),
                "-o".into(),
                "json".into()
            ])
        );
    }

    #[test]
    fn gke_list_and_credentials() {
        let list = ok(parse_gke_list(
            include_str!("../testdata/gcloud_clusters.json"),
            None,
        ));
        assert_eq!(list.len(), 2);
        let web = &list[0];
        assert_eq!(web.project.as_deref(), Some("acme-prod"));
        assert_eq!(web.location, "europe-west1");
        assert_eq!(web.status, "RUNNING");
        assert_eq!(web.context_name(false), "gke_acme-prod_europe-west1_web");
        assert_eq!(
            credentials_invocations(web, false, None)[0].shell_line("gcloud"),
            "gcloud container clusters get-credentials web --region=europe-west1 --project=acme-prod"
        );
        assert_eq!(
            credentials_invocations(&list[1], false, None)[0].args[4],
            "--zone=us-central1-a"
        );
        let opts = DiscoverOptions {
            gcp_project: Some(" acme-prod ".into()),
            ..DiscoverOptions::default()
        };
        assert_eq!(
            list_invocation(ClusterProvider::Gke, &opts).map(|i| i.shell_line("gcloud")),
            Some("gcloud container clusters list --format=json --project=acme-prod".into())
        );
    }

    #[test]
    fn eks_list_describe_and_credentials() {
        let opts = DiscoverOptions {
            aws_region: Some("eu-west-1".into()),
            aws_profile: Some("prod".into()),
            gcp_project: None,
        };
        assert_eq!(
            list_invocation(ClusterProvider::Eks, &opts).map(|i| i.shell_line("aws")),
            Some("aws eks list-clusters --region eu-west-1 --profile prod --output json".into())
        );
        let names = ok(parse_eks_list(include_str!(
            "../testdata/aws_eks_list_clusters.json"
        )));
        assert_eq!(names, ["payments", "staging"]);
        assert_eq!(
            eks_describe_invocation("payments", &opts).args[..4],
            ["eks", "describe-cluster", "--name", "payments"]
        );
        let c = ok(parse_eks_describe(
            include_str!("../testdata/aws_eks_describe_cluster.json"),
            &opts,
        ));
        assert_eq!(c.location, "eu-west-1");
        assert_eq!(c.status, "ACTIVE");
        assert_eq!(c.aws_profile.as_deref(), Some("prod"));
        assert_eq!(
            c.context_name(false),
            "arn:aws:eks:eu-west-1:123456789012:cluster/payments"
        );
        assert_eq!(
            credentials_invocations(&c, false, None)[0].shell_line("aws"),
            "aws eks update-kubeconfig --name payments --region eu-west-1 --profile prod"
        );
    }

    #[test]
    fn aws_profile_names() {
        let config = "[default]\nregion = eu-west-1\n\n[profile prod]\nsso_session = corp\n[sso-session corp]\nsso_start_url = x\n[profile  dev ]\n";
        let creds = "[default]\naws_access_key_id = X\n[ci]\n";
        assert_eq!(
            aws_profiles(config, creds),
            ["default", "prod", "dev", "ci"]
        );
    }

    #[test]
    fn rancher_answers() {
        let list = ok(parse_rancher_clusters(include_str!(
            "../testdata/rancher_clusters.json"
        )));
        let names: Vec<&str> = list.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["edge", "local", "retired"]);
        assert_eq!(list[0].id, "c-m-x7k2p9qz");
        assert_eq!(list[0].driver, "rke2");
        assert_eq!(list[0].version, "v1.31.3+rke2r1");
        assert!(list[0].can_kubeconfig && !list[2].can_kubeconfig);
        let kc = ok(parse_rancher_kubeconfig(include_str!(
            "../testdata/rancher_kubeconfig.json"
        )));
        assert!(kc.contains("server: \"https://rancher.example.com/k8s/clusters/c-m-x7k2p9qz\""));
        assert!(parse_rancher_kubeconfig("{}").is_err());
        assert_eq!(
            rancher_base("rancher.example.com/"),
            Ok("https://rancher.example.com".into())
        );
        assert_eq!(
            rancher_base("https://r.example/v3"),
            Ok("https://r.example".into())
        );
        assert!(rancher_base("http://r.example").is_err());
        assert!(rancher_error(401, "").to_string().contains("token"));
        assert_eq!(
            rancher_error(404, r#"{"message":"cluster not found"}"#).to_string(),
            "cluster not found"
        );
    }

    #[test]
    fn sign_in_hints() {
        let missing = "Unable to connect to the server: getting credentials: exec: executable kubelogin not found\n\nIt looks like you are trying to use a client-go credential plugin that is not installed.";
        let h = sign_in_hint(missing, ClusterProvider::Aks).unwrap_or_default();
        assert!(h.starts_with("kubectl needs kubelogin to sign in."), "{h}");
        let gke = "E1012 error: You must be logged in to the server (Unauthorized)";
        assert!(
            sign_in_hint(gke, ClusterProvider::Gke)
                .is_some_and(|h| h.contains("gcloud auth login"))
        );
        let aws = "An error occurred (ExpiredToken) when calling the GetCallerIdentity operation: The security token included in the request is expired";
        assert!(
            sign_in_hint(aws, ClusterProvider::Eks).is_some_and(|h| h.contains("aws sso login"))
        );
        let aad = "AADSTS700082: The refresh token has expired due to inactivity.";
        assert!(sign_in_hint(aad, ClusterProvider::Aks).is_some_and(|h| h.contains("az login")));
        assert_eq!(
            sign_in_hint(
                "Error from server (NotFound): pods \"x\" not found",
                ClusterProvider::Aks
            ),
            None
        );
        assert!(
            plugin_install_hint("gke-gcloud-auth-plugin").contains("gcloud components install")
        );
    }
}
