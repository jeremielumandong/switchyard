//! Adding Kubernetes clusters: AKS, GKE and EKS through the provider's own CLI (which
//! writes a kubeconfig context and keeps the cloud sign-in), Rancher through its API
//! with the user's token (kept in the keychain or vault) into a Switchyard-managed
//! kubeconfig, and local clusters from the user's kubeconfig. Switchyard never stores
//! cloud credentials; the Rancher token and generated kubeconfigs never reach logs or
//! history.

use std::path::PathBuf;
use std::time::Duration;

use secrecy::{ExposeSecret as _, SecretString};
use switchyard_containers::clusters::{self, CloudCluster, ClusterProvider, DiscoverOptions};
use switchyard_containers::{self as ct, ContainersError, Target, Tool, local};
use switchyard_store::{ContainerConnection, ContainerPlatform, ProfileId};
use tracing::info;

use super::Service;
use super::containers::{classify, mark_missing_plugins};
use crate::bus::{Event, RequestId};

type CtResult<T> = Result<T, ContainersError>;

/// Limits for provider CLIs (they may open a browser or refresh a token first).
const CLI_TIMEOUT: Duration = Duration::from_secs(180);
/// Limit for each Rancher request.
const RANCHER_TIMEOUT: Duration = Duration::from_secs(30);
/// EKS clusters described per listing.
const EKS_MAX: usize = 50;

fn io<E: std::fmt::Display>(e: E) -> ContainersError {
    ContainersError::Io(e.to_string())
}

/// A provider CLI failure that is about signing in, as [`ContainersError::SignIn`].
fn cli_classify(provider: ClusterProvider, e: ContainersError) -> ContainersError {
    match e {
        ContainersError::Failed {
            tool,
            code,
            message,
        } => match clusters::sign_in_hint(&message, provider) {
            Some(hint) => ContainersError::SignIn { message, hint },
            None => ContainersError::Failed {
                tool,
                code,
                message,
            },
        },
        e => e,
    }
}

/// Run a provider CLI call on this computer.
async fn run_cli(inv: &ct::Invocation) -> CtResult<String> {
    let program = local::find_program(inv.tool, None).ok_or(ContainersError::Missing(inv.tool))?;
    local::run(&program, inv, CLI_TIMEOUT).await
}

/// A trusted certificate given as PEM text or as the path of a PEM file.
async fn resolve_pem(input: Option<&str>) -> CtResult<Option<String>> {
    let Some(v) = input.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    if v.starts_with("-----BEGIN") {
        return Ok(Some(v.to_owned()));
    }
    tokio::fs::read_to_string(local::expand_home(v))
        .await
        .map(Some)
        .map_err(|e| ContainersError::Io(format!("trusted certificate {v}: {e}")))
}

fn rancher_client(pem: Option<&str>) -> CtResult<reqwest::Client> {
    let tls = switchyard_db::tls::client_config_with(pem).map_err(io)?;
    reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .timeout(RANCHER_TIMEOUT)
        .build()
        .map_err(io)
}

/// Read a Rancher answer: the body, or Rancher's error.
async fn rancher_body(resp: reqwest::Response) -> CtResult<String> {
    let status = resp.status();
    let body = resp.text().await.map_err(io)?;
    if status.is_success() {
        Ok(body)
    } else {
        Err(clusters::rancher_error(status.as_u16(), &body))
    }
}

/// A request error without the URL's query (no secrets are in URLs, but keep it short).
fn rancher_send_error(e: reqwest::Error) -> ContainersError {
    let msg = if e.is_connect() {
        "could not reach the Rancher server".to_owned()
    } else if e.is_timeout() {
        "the Rancher server did not answer in time".to_owned()
    } else {
        e.without_url().to_string()
    };
    ContainersError::Failed {
        tool: Tool::Rancher,
        code: None,
        message: format!(
            "{msg}. For a self-signed server, add its certificate (PEM) to the connection."
        ),
    }
}

impl Service {
    fn kubeconfig_path(&self, id: &ProfileId) -> PathBuf {
        let safe: String =
            id.0.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
        self.data_dir
            .join("kubeconfigs")
            .join(format!("{safe}.yaml"))
    }

    /// Remove a connection's managed kubeconfig (it is generated again when needed).
    pub(super) async fn drop_managed_kubeconfig(&self, id: &ProfileId) {
        let _ = tokio::fs::remove_file(self.kubeconfig_path(id)).await;
    }

    async fn rancher_token(
        &self,
        typed: Option<SecretString>,
        connection: Option<&ContainerConnection>,
    ) -> CtResult<SecretString> {
        let saved = connection.and_then(|c| c.secret.clone());
        self.secret_value(typed, saved)
            .await
            .map_err(io)?
            .ok_or_else(|| ContainersError::Unsupported("Enter the Rancher API token".into()))
    }

    /// The managed kubeconfig of a Rancher cluster, generated with the API token when it
    /// is missing (or `refresh`). The file is only readable by the user.
    pub(super) async fn rancher_kubeconfig(
        &self,
        c: &ContainerConnection,
        typed: Option<SecretString>,
        refresh: bool,
    ) -> CtResult<PathBuf> {
        let path = self.kubeconfig_path(&c.id);
        if !refresh && tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(path);
        }
        let src = c
            .rancher
            .as_ref()
            .ok_or_else(|| ContainersError::Unsupported("not a Rancher cluster".into()))?;
        if !src
            .cluster_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.')
        {
            return Err(ContainersError::Unsupported(
                "invalid Rancher cluster id".into(),
            ));
        }
        let token = self.rancher_token(typed, Some(c)).await?;
        let pem = resolve_pem(src.trusted_ca_pem.as_deref()).await?;
        let base = clusters::rancher_base(&src.server)?;
        let client = rancher_client(pem.as_deref())?;
        let resp = client
            .post(format!(
                "{base}/v3/clusters/{}?action=generateKubeconfig",
                src.cluster_id
            ))
            .bearer_auth(token.expose_secret())
            .send()
            .await
            .map_err(rancher_send_error)?;
        let config = clusters::parse_rancher_kubeconfig(&rancher_body(resp).await?)?;
        write_private(&path, config.as_bytes()).await?;
        info!(connection = %c.name, "rancher kubeconfig generated");
        Ok(path)
    }

    /// [`Command::RancherClusters`](crate::Command::RancherClusters).
    pub(super) async fn rancher_clusters(
        &self,
        request: RequestId,
        server: String,
        token: Option<SecretString>,
        connection: Option<ProfileId>,
        trusted_ca_pem: Option<String>,
    ) {
        let result = async {
            let conn = match &connection {
                Some(id) => self.saved_container(id).await,
                None => None,
            };
            let token = self.rancher_token(token, conn.as_ref()).await?;
            let pem = resolve_pem(trusted_ca_pem.as_deref()).await?;
            let base = clusters::rancher_base(&server)?;
            let resp = rancher_client(pem.as_deref())?
                .get(format!("{base}/v3/clusters"))
                .bearer_auth(token.expose_secret())
                .send()
                .await
                .map_err(rancher_send_error)?;
            clusters::parse_rancher_clusters(&rancher_body(resp).await?)
        }
        .await;
        self.emit(Event::RancherClusters { request, result });
    }

    async fn saved_container(&self, id: &ProfileId) -> Option<ContainerConnection> {
        let id = id.clone();
        match self.with_store(move |s| s.profile(&id)).await {
            Ok(Some(switchyard_store::Profile::Containers(c))) => Some(c),
            _ => None,
        }
    }

    /// [`Command::KubeContexts`](crate::Command::KubeContexts).
    pub(super) async fn kube_contexts(&self, request: RequestId, kubeconfig: Option<String>) {
        let result = async {
            let target = Target {
                kubeconfig,
                ..Target::default()
            };
            let q = ct::Query::Contexts;
            let program = local::find_program(Tool::Kubectl, None)
                .ok_or(ContainersError::Missing(Tool::Kubectl))?;
            let out = local::run(&program, &q.invocation(&target), CLI_TIMEOUT).await?;
            let mut list = ct::kube::parse_contexts(&out)?;
            mark_missing_plugins(&mut list);
            Ok(list)
        }
        .await;
        self.emit(Event::KubeContexts { request, result });
    }

    /// [`Command::DiscoverClusters`](crate::Command::DiscoverClusters).
    pub(super) async fn discover_clusters(
        &self,
        request: RequestId,
        provider: ClusterProvider,
        options: DiscoverOptions,
    ) {
        let result = async {
            let inv = clusters::list_invocation(provider, &options).ok_or_else(|| {
                ContainersError::Unsupported(format!(
                    "{} clusters are not listed by a cloud CLI",
                    provider.label()
                ))
            })?;
            match provider {
                ClusterProvider::Aks => clusters::parse_aks_list(&run_cli(&inv).await?),
                ClusterProvider::Gke => clusters::parse_gke_list(
                    &run_cli(&inv).await?,
                    options
                        .gcp_project
                        .as_deref()
                        .map(str::trim)
                        .filter(|p| !p.is_empty()),
                ),
                _ => {
                    if options
                        .aws_region
                        .as_deref()
                        .is_none_or(|r| r.trim().is_empty())
                    {
                        return Err(ContainersError::Unsupported(
                            "Choose a region: EKS lists the clusters of one region".into(),
                        ));
                    }
                    let names = clusters::parse_eks_list(&run_cli(&inv).await?)?;
                    let mut out = Vec::new();
                    for name in names.iter().take(EKS_MAX) {
                        let d = clusters::eks_describe_invocation(name, &options);
                        out.push(clusters::parse_eks_describe(&run_cli(&d).await?, &options)?);
                    }
                    Ok(out)
                }
            }
        }
        .await
        .map_err(|e| cli_classify(provider, e));
        self.emit(Event::ClustersDiscovered { request, result });
    }

    /// [`Command::AddClusterCredentials`](crate::Command::AddClusterCredentials).
    pub(super) async fn add_cluster_credentials(
        &self,
        request: RequestId,
        cluster: CloudCluster,
        admin: bool,
    ) {
        let result = async {
            for inv in clusters::credentials_invocations(&cluster, admin, None) {
                run_cli(&inv).await?;
            }
            info!(
                provider = cluster.provider.label(),
                cluster = %cluster.name,
                "kubeconfig context added"
            );
            Ok(cluster.context_name(admin))
        }
        .await
        .map_err(|e| cli_classify(cluster.provider, e));
        self.emit(Event::ClusterCredentials { request, result });
    }

    /// [`Command::AwsProfiles`](crate::Command::AwsProfiles).
    pub(super) async fn aws_profiles(&self, request: RequestId) {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_default();
        let file = |var: &str, name: &str| {
            std::env::var_os(var)
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".aws").join(name))
        };
        let config = tokio::fs::read_to_string(file("AWS_CONFIG_FILE", "config"))
            .await
            .unwrap_or_default();
        let creds = tokio::fs::read_to_string(file("AWS_SHARED_CREDENTIALS_FILE", "credentials"))
            .await
            .unwrap_or_default();
        self.emit(Event::AwsProfiles {
            request,
            profiles: clusters::aws_profiles(&config, &creds),
        });
    }

    /// [`Command::TestContainers`](crate::Command::TestContainers).
    pub(super) async fn test_containers(
        &self,
        c: ContainerConnection,
        secret: Option<SecretString>,
    ) -> CtResult<String> {
        if c.rancher.is_some() {
            // A fresh kubeconfig with the token as typed (or saved).
            self.rancher_kubeconfig(&c, secret, true).await?;
        }
        let target = self.container_target(&c, None, false).await?;
        let (query, noun) = match c.platform {
            ContainerPlatform::Docker => {
                (ct::Query::Containers { all: false }, "running containers")
            }
            ContainerPlatform::Kubernetes => (ct::Query::Namespaces, "namespaces"),
        };
        let out = self
            .container_run(&c, &query.invocation(&target), Duration::from_secs(60))
            .await
            .map_err(|e| classify(&c, e))?;
        let n = match query.parse(&out)? {
            ct::Listing::Containers(v) => v.len(),
            ct::Listing::Namespaces(v) => v.len(),
            _ => 0,
        };
        Ok(format!("Connected · {n} {noun}"))
    }
}

/// Write `bytes` to `path` (parents created), readable by the user only.
async fn write_private(path: &std::path::Path, bytes: &[u8]) -> CtResult<()> {
    use tokio::io::AsyncWriteExt as _;
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await.map_err(io)?;
    }
    let mut opts = tokio::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let mut f = opts.open(path).await.map_err(io)?;
    f.write_all(bytes).await.map_err(io)?;
    f.flush().await.map_err(io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_cli_errors_become_sign_in_hints() {
        let e = cli_classify(
            ClusterProvider::Aks,
            ContainersError::Failed {
                tool: Tool::Az,
                code: Some(1),
                message: "ERROR: AADSTS70043: The refresh token has expired".into(),
            },
        );
        assert!(matches!(e, ContainersError::SignIn { hint, .. } if hint.contains("az login")));
        let other = ContainersError::Failed {
            tool: Tool::Az,
            code: Some(1),
            message: "ResourceGroupNotFound".into(),
        };
        assert_eq!(cli_classify(ClusterProvider::Aks, other.clone()), other);
    }

    #[tokio::test]
    async fn pem_text_or_file() {
        assert_eq!(resolve_pem(None).await, Ok(None));
        assert_eq!(resolve_pem(Some("  ")).await, Ok(None));
        let pem = "-----BEGIN CERTIFICATE-----\nAAA\n-----END CERTIFICATE-----";
        assert_eq!(resolve_pem(Some(pem)).await, Ok(Some(pem.to_owned())));
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let f = dir.path().join("ca.pem");
        std::fs::write(&f, pem).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            resolve_pem(Some(&f.display().to_string())).await,
            Ok(Some(pem.to_owned()))
        );
        assert!(resolve_pem(Some("/nonexistent/ca.pem")).await.is_err());
    }

    #[tokio::test]
    async fn private_files() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let p = dir.path().join("a/b.yaml");
        write_private(&p, b"x")
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&p)
                .map(|m| m.permissions().mode())
                .unwrap_or_default();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
