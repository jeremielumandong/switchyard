//! Infra tools on the runtime: Terraform state, cloud inventory, network checks and log
//! tails ([`crate::infra`]). Read-only throughout.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::{AbortHandle, Abortable};
use secrecy::SecretString;
use switchyard_cloud::auth::StaticToken;
use switchyard_cloud::cloudflare::CloudflareApi;
use switchyard_cloud::inventory::aws::AwsInventory;
use switchyard_cloud::inventory::azure::{ARM_SCOPE, AzureInventory};
use switchyard_cloud::inventory::gcp::{GcloudToken, GcpInventory, default_project};
use switchyard_cloud::inventory::{self, CloudResource};
use switchyard_cloud::logs::{CloudWatchLogs, LOG_ANALYTICS_SCOPE, LogAnalytics};
use switchyard_cloud::tfcloud::TfCloud;
use switchyard_cloud::{CloudError, tfstate};
use switchyard_remote::ssh::CommandChunk;
use switchyard_store::{CloudAuth, CloudConnection, CloudProvider, ProfileId};
use tracing::{debug, info};

use super::Service;
use crate::bus::{Event, RequestId};
use crate::error::{CoreError, Result};
use crate::infra::logline::{self, Level, LineSplitter, LogLine};
use crate::infra::netcheck::{self, CheckKind, CheckResult, NetCheck};
use crate::infra::{
    InfraOp, InfraUpdate, InventoryAccount, LogSource, TfSource, TfVersion, tf_token_key,
};

/// Most lines sent in one update.
const LOG_BATCH: usize = 500;
/// How often a batch is flushed while lines trickle in.
const LOG_FLUSH: Duration = Duration::from_millis(150);
/// How often cloud logs are polled when following.
const POLL_EVERY: Duration = Duration::from_secs(5);
/// Longest a Host command for a check may run.
const CHECK_TIMEOUT: Duration = Duration::from_secs(40);
/// Most regions one AWS account is listed in.
const MAX_REGIONS: usize = 32;

fn s(e: impl std::fmt::Display) -> String {
    e.to_string()
}

impl Service {
    fn infra_emit(&self, request: RequestId, update: InfraUpdate) {
        self.emit(Event::Infra { request, update });
    }

    /// Run `op`, stoppable with [`Service::infra_cancel`].
    pub(super) async fn infra(&self, request: RequestId, op: InfraOp) {
        let (handle, reg) = AbortHandle::new_pair();
        self.infra_tasks.insert(request, handle);
        let tail = matches!(op, InfraOp::Tail(_));
        let done = Abortable::new(self.infra_run(request, op), reg).await;
        self.infra_tasks.remove(request);
        if done.is_err() && tail {
            self.infra_emit(request, InfraUpdate::LogEnded(None));
        }
    }

    /// Stop a running infra request (a tail, an inventory).
    pub(super) fn infra_cancel(&self, request: RequestId) {
        if self.infra_tasks.cancel(request) {
            debug!(request, "infra request stopped");
        }
    }

    async fn infra_run(&self, request: RequestId, op: InfraOp) {
        match op {
            InfraOp::TfLoad(src) => {
                let label = src.label();
                let result = self.tf_load(&src).await.map_err(s);
                self.infra_emit(request, InfraUpdate::TfState { label, result });
            }
            InfraOp::TfVersions(src) => {
                let r = self.tf_versions(&src).await.map_err(s);
                self.infra_emit(request, InfraUpdate::TfVersions(r));
            }
            InfraOp::TfSaveToken { host, token } => {
                let key = tf_token_key(&host);
                let r = self
                    .with_secrets(move |st| st.set(&key, &token))
                    .await
                    .map_err(s);
                self.infra_emit(request, InfraUpdate::Done(r));
            }
            InfraOp::TfHasToken { host } => {
                let key = tf_token_key(&host);
                let has = self
                    .with_secrets(move |st| st.get(&key))
                    .await
                    .ok()
                    .flatten()
                    .is_some();
                self.infra_emit(request, InfraUpdate::TfToken(has));
            }
            InfraOp::AwsProfiles => {
                let names = tokio::task::spawn_blocking(switchyard_cloud::aws::profile_names)
                    .await
                    .unwrap_or_default();
                self.infra_emit(request, InfraUpdate::AwsProfiles(names));
            }
            InfraOp::Inventory(accounts) => {
                let jobs = accounts.into_iter().map(|a| async move {
                    let (account, items, errors) = self.inventory_account(&a).await;
                    self.infra_emit(
                        request,
                        InfraUpdate::Inventory {
                            account,
                            items,
                            errors,
                        },
                    );
                });
                futures::future::join_all(jobs).await;
                self.infra_emit(request, InfraUpdate::Done(Ok(())));
            }
            InfraOp::Check(check) => {
                let started = Instant::now();
                let mut r = self.run_check(&check).await;
                if r.elapsed_ms == 0 {
                    r.elapsed_ms = started.elapsed().as_millis() as u64;
                }
                self.infra_emit(request, InfraUpdate::Check(r));
            }
            InfraOp::LogGroups {
                conn,
                region,
                prefix,
            } => {
                let r = async {
                    let logs = self.cloudwatch(&conn, &region).await?;
                    Ok::<_, CoreError>(logs.groups(&prefix).await?)
                }
                .await
                .map_err(s);
                self.infra_emit(request, InfraUpdate::LogGroups(r));
            }
            InfraOp::Workspaces { conn } => {
                let r = async {
                    let token = self.arm_token(conn.as_ref()).await?;
                    let inv = AzureInventory::new(token, String::new(), None)?;
                    Ok::<_, CoreError>(inv.log_workspaces().await?)
                }
                .await
                .map_err(s);
                self.infra_emit(request, InfraUpdate::Workspaces(r));
            }
            InfraOp::Tail(src) => {
                let r = self.tail(request, src).await.err().map(s);
                self.infra_emit(request, InfraUpdate::LogEnded(r));
            }
        }
    }

    // ------------------------------------------------------------- terraform

    async fn tf_cloud(&self, host: &str) -> Result<TfCloud> {
        let key = tf_token_key(host);
        let token = self
            .with_secrets(move |st| st.get(&key))
            .await?
            .ok_or_else(|| {
                CoreError::Cloud(CloudError::Auth(
                    "no API token saved for this host: add one (User settings → Tokens in HCP Terraform)".into(),
                ))
            })?;
        Ok(TfCloud::new(host, Arc::new(StaticToken(token)))?)
    }

    async fn tf_bytes(&self, src: &TfSource) -> Result<Vec<u8>> {
        match src {
            TfSource::File(path) => {
                let path = path.clone();
                tokio::fs::read(&path).await.map_err(|e| {
                    CoreError::Cloud(CloudError::Invalid(format!("{}: {e}", path.display())))
                })
            }
            TfSource::S3 {
                conn,
                bucket,
                key,
                version,
            } => {
                let c = self.cloud_profile(conn.clone()).await?;
                let secret = self.cloud_secret(&c, None).await?;
                let fs = self.cloud_s3(&c, secret).await?;
                Ok(fs
                    .get_object(bucket.trim(), key.trim(), version.as_deref())
                    .await?)
            }
            TfSource::Blob {
                conn,
                container,
                blob,
                version,
            } => {
                let c = self.cloud_profile(conn.clone()).await?;
                let secret = self.cloud_secret(&c, None).await?;
                let fs = self.cloud_blob(&c, secret)?;
                Ok(fs
                    .get_blob(container.trim(), blob.trim(), version.as_deref())
                    .await?)
            }
            TfSource::Cloud {
                host,
                organization,
                workspace,
                version,
            } => {
                let api = self.tf_cloud(host).await?;
                let v = match version {
                    None => api.current(organization, workspace).await?,
                    Some(id) => api
                        .state_versions(organization, workspace)
                        .await?
                        .into_iter()
                        .find(|v| v.id == *id)
                        .ok_or_else(|| CoreError::NotFound(format!("state version {id}")))?,
                };
                if v.download_url.is_empty() {
                    return Err(CoreError::Unsupported(
                        "this state version has no downloadable state (the token may lack read-state access)".into(),
                    ));
                }
                Ok(api.download(&v.download_url).await?)
            }
        }
    }

    async fn tf_load(&self, src: &TfSource) -> Result<Arc<tfstate::TfState>> {
        let bytes = self.tf_bytes(src).await?;
        info!(source = %src.label(), bytes = bytes.len(), "terraform state read");
        tokio::task::spawn_blocking(move || tfstate::parse(&bytes))
            .await
            .map_err(|e| CoreError::Internal(e.to_string()))?
            .map(Arc::new)
            .map_err(CoreError::from)
    }

    async fn tf_versions(&self, src: &TfSource) -> Result<Vec<TfVersion>> {
        let from_objects = |v: Vec<switchyard_cloud::s3::ObjectVersion>| {
            v.into_iter()
                .map(|o| TfVersion {
                    id: o.version_id,
                    modified_ms: o.modified_ms,
                    size: Some(o.size),
                    serial: None,
                    latest: o.latest,
                })
                .filter(|v| !v.id.is_empty() && v.id != "null")
                .collect()
        };
        match src {
            TfSource::File(_) => Ok(Vec::new()),
            TfSource::S3 {
                conn, bucket, key, ..
            } => {
                let c = self.cloud_profile(conn.clone()).await?;
                let secret = self.cloud_secret(&c, None).await?;
                let fs = self.cloud_s3(&c, secret).await?;
                Ok(from_objects(
                    fs.object_versions(bucket.trim(), key.trim()).await?,
                ))
            }
            TfSource::Blob {
                conn,
                container,
                blob,
                ..
            } => {
                let c = self.cloud_profile(conn.clone()).await?;
                let secret = self.cloud_secret(&c, None).await?;
                let fs = self.cloud_blob(&c, secret)?;
                Ok(from_objects(
                    fs.blob_versions(container.trim(), blob.trim()).await?,
                ))
            }
            TfSource::Cloud {
                host,
                organization,
                workspace,
                ..
            } => {
                let api = self.tf_cloud(host).await?;
                let list = api.state_versions(organization, workspace).await?;
                Ok(list
                    .into_iter()
                    .enumerate()
                    .map(|(i, v)| TfVersion {
                        id: v.id,
                        modified_ms: v.created_ms,
                        size: None,
                        serial: Some(v.serial),
                        latest: i == 0,
                    })
                    .collect())
            }
        }
    }

    // ------------------------------------------------------------- inventory

    /// Bearer tokens for Azure Resource Manager–style scopes from a saved connection, or
    /// the Azure CLI when `conn` is `None`.
    async fn azure_token_for(
        &self,
        conn: Option<&ProfileId>,
        scope: &'static str,
    ) -> Result<Arc<dyn switchyard_cloud::auth::TokenSource>> {
        match conn {
            None => Ok(Arc::new(switchyard_cloud::azure::AzureCliToken::new(
                scope, None,
            ))),
            Some(id) => {
                let c = self.cloud_profile(id.clone()).await?;
                let secret = self.cloud_secret(&c, None).await?;
                self.azure_tokens(&c, scope, secret)
            }
        }
    }

    async fn arm_token(
        &self,
        conn: Option<&ProfileId>,
    ) -> Result<Arc<dyn switchyard_cloud::auth::TokenSource>> {
        self.azure_token_for(conn, ARM_SCOPE).await
    }

    async fn aws_inventory(
        &self,
        c: Option<&CloudConnection>,
        profile: Option<&str>,
    ) -> Result<(AwsInventory, String)> {
        let (auth, region, label) = match (c, profile) {
            (Some(c), _) => {
                if c.service.provider() != CloudProvider::Aws {
                    return Err(CoreError::Unsupported(format!(
                        "{} is not an AWS connection",
                        c.name
                    )));
                }
                let secret = self.cloud_secret(c, None).await?;
                let (auth, region) = self.aws_auth(c, secret).await?;
                (auth, region, c.name.clone())
            }
            (None, Some(p)) => {
                let p = p.trim().to_owned();
                let name = if p.is_empty() {
                    "default".to_owned()
                } else {
                    p
                };
                let n = name.clone();
                let region =
                    tokio::task::spawn_blocking(move || switchyard_cloud::aws::profile_region(&n))
                        .await
                        .ok()
                        .flatten()
                        .unwrap_or_else(|| "us-east-1".into());
                (
                    Arc::new(switchyard_cloud::aws::AwsAuth::new(
                        switchyard_cloud::aws::AwsSource::Profile(name.clone()),
                    )),
                    region,
                    format!("profile {name}"),
                )
            }
            (None, None) => return Err(CoreError::Internal("no AWS account".into())),
        };
        let inv = AwsInventory::new(auth, label, None)?;
        Ok((inv, region))
    }

    async fn aws_account_items(
        &self,
        inv: AwsInventory,
        default_region: &str,
        regions: &[String],
    ) -> (String, Vec<CloudResource>, Vec<String>) {
        let mut errors = Vec::new();
        let label = match inv.account_id().await {
            Ok(id) => format!("{} ({id})", inv.account()),
            Err(e) => {
                debug!(error = %e, "sts get-caller-identity failed");
                inv.account().to_owned()
            }
        };
        let inv = inv.with_account(label.clone());
        let regions: Vec<String> = if regions.iter().any(|r| r.eq_ignore_ascii_case("all")) {
            match inv.regions(default_region).await {
                Ok(r) => r,
                Err(e) => {
                    errors.push(format!("regions: {e}"));
                    vec![default_region.to_owned()]
                }
            }
        } else if regions.is_empty() {
            vec![default_region.to_owned()]
        } else {
            regions.to_vec()
        };
        let mut items = Vec::new();
        let per_region = regions.iter().take(MAX_REGIONS).map(|region| {
            let inv = &inv;
            async move {
                let (a, b, c, d) = futures::join!(
                    inv.instances(region),
                    inv.databases(region),
                    inv.load_balancers(region),
                    inv.clusters(region),
                );
                let mut items = Vec::new();
                let mut errors = Vec::new();
                for (what, r) in [("EC2", a), ("RDS", b), ("ELB", c), ("EKS", d)] {
                    match r {
                        Ok(v) => items.extend(v),
                        Err(e) => errors.push(format!("{region}: {what}: {e}")),
                    }
                }
                (items, errors)
            }
        });
        for (i, e) in futures::future::join_all(per_region).await {
            items.extend(i);
            errors.extend(e);
        }
        match inv.buckets().await {
            Ok(b) => items.extend(b),
            Err(e) => errors.push(format!("S3: {e}")),
        }
        (label, items, errors)
    }

    /// One account's resources: (account label, resources, failures).
    async fn inventory_account(
        &self,
        a: &InventoryAccount,
    ) -> (String, Vec<CloudResource>, Vec<String>) {
        match a {
            InventoryAccount::AwsProfile { profile, regions } => {
                match self.aws_inventory(None, Some(profile)).await {
                    Ok((inv, region)) => self.aws_account_items(inv, &region, regions).await,
                    Err(e) => (format!("profile {profile}"), Vec::new(), vec![s(e)]),
                }
            }
            InventoryAccount::AzureCli => match self.azure_items(None, "Azure CLI").await {
                Ok(items) => ("Azure CLI".into(), items, Vec::new()),
                Err(e) => ("Azure CLI".into(), Vec::new(), vec![s(e)]),
            },
            InventoryAccount::Gcp { project } => {
                let project = match project.trim() {
                    "" => match default_project().await {
                        Ok(p) => p,
                        Err(e) => return ("Google Cloud".into(), Vec::new(), vec![s(e)]),
                    },
                    p => p.to_owned(),
                };
                match GcpInventory::new(Box::new(GcloudToken::default()), project.clone()) {
                    Ok(g) => {
                        let (items, errors) = g.resources().await;
                        (project, items, errors)
                    }
                    Err(e) => (project, Vec::new(), vec![s(e)]),
                }
            }
            InventoryAccount::Connection { conn, regions } => {
                let c = match self.cloud_profile(conn.clone()).await {
                    Ok(c) => c,
                    Err(e) => return ("?".into(), Vec::new(), vec![s(e)]),
                };
                let name = c.name.clone();
                match c.service.provider() {
                    CloudProvider::Aws => match self.aws_inventory(Some(&c), None).await {
                        Ok((inv, region)) => self.aws_account_items(inv, &region, regions).await,
                        Err(e) => (name, Vec::new(), vec![s(e)]),
                    },
                    CloudProvider::Azure => match self.azure_items(Some(conn), &name).await {
                        Ok(items) => (name, items, Vec::new()),
                        Err(e) => (name, Vec::new(), vec![s(e)]),
                    },
                    CloudProvider::Cloudflare => match self.cloudflare_items(&c).await {
                        Ok((items, errors)) => (name, items, errors),
                        Err(e) => (name, Vec::new(), vec![s(e)]),
                    },
                }
            }
        }
    }

    async fn azure_items(
        &self,
        conn: Option<&ProfileId>,
        label: &str,
    ) -> Result<Vec<CloudResource>> {
        let token = self.arm_token(conn).await?;
        let inv = AzureInventory::new(token, label.to_owned(), None)?;
        Ok(inv.resources().await?)
    }

    async fn cloudflare_items(
        &self,
        c: &CloudConnection,
    ) -> Result<(Vec<CloudResource>, Vec<String>)> {
        if c.auth != CloudAuth::ApiToken {
            return Err(CoreError::Unsupported(format!(
                "{} signs in with R2 access keys; the inventory needs a Cloudflare API token connection",
                c.name
            )));
        }
        let token: SecretString = self
            .cloud_secret(c, None)
            .await?
            .ok_or_else(|| CoreError::NotFound("API token".into()))?;
        let api = CloudflareApi::new(Arc::new(StaticToken(token)), None)?;
        let account = (!c.endpoint.trim().is_empty()).then_some(c.endpoint.trim());
        Ok(inventory::cloudflare::resources(&api, account).await?)
    }

    // ------------------------------------------------------------- checks

    async fn host_command(&self, host: &ProfileId, command: &str) -> Result<String, String> {
        let target = self.ssh_target(host).await.map_err(s)?;
        let conn = self.ssh.session(&target).await.map_err(s)?;
        let out = conn
            .run_command(command, 256 * 1024, CHECK_TIMEOUT)
            .await
            .map_err(s)?;
        if out.timed_out {
            return Err(format!("no answer in {} s", CHECK_TIMEOUT.as_secs()));
        }
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        Ok(text)
    }

    async fn host_name(&self, id: &ProfileId) -> String {
        let id = id.clone();
        match self.with_store(move |st| st.profile(&id)).await {
            Ok(Some(p)) => p.name().to_owned(),
            _ => "Host".into(),
        }
    }

    /// Run one check here or on its Host.
    pub(super) async fn run_check(&self, c: &NetCheck) -> CheckResult {
        let now = switchyard_store::now_ms();
        let fail = |m: String| CheckResult {
            verdict: netcheck::Verdict::Fail,
            summary: m,
            details: Vec::new(),
            elapsed_ms: 0,
            expires_in_days: None,
        };
        match c.kind {
            CheckKind::Http => {
                let url = c.target.trim();
                let url = if url.contains("://") {
                    url.to_owned()
                } else {
                    format!("https://{url}")
                };
                let report = match &c.via {
                    None => netcheck::http_local(&url).await,
                    Some(h) => match self.host_command(h, &netcheck::http_command(&url)).await {
                        Ok(out) => netcheck::parse_curl(&out, &url),
                        Err(e) => return fail(format!("{}: {e}", self.host_name(h).await)),
                    },
                };
                netcheck::format_http(&report)
            }
            kind => {
                let default_port = if kind == CheckKind::Tls { 443 } else { 0 };
                let (host, port) = match netcheck::split_host_port(&c.target, default_port) {
                    Ok(hp) => hp,
                    Err(e) => return fail(e),
                };
                if !netcheck::valid_host(&host) {
                    return fail(format!("{host:?} is not a host name or address"));
                }
                if kind == CheckKind::Tcp && port == 0 {
                    return fail("TCP checks need host:port".into());
                }
                match (kind, &c.via) {
                    (CheckKind::Dns, None) => {
                        let record = c.record.as_deref().unwrap_or("A/AAAA");
                        netcheck::format_dns(&dns_local(&host, record).await)
                    }
                    (CheckKind::Dns, Some(h)) => {
                        let record = c.record.as_deref().unwrap_or("A/AAAA");
                        let name = self.host_name(h).await;
                        match self
                            .host_command(h, &netcheck::dns_command(&host, record))
                            .await
                        {
                            Ok(out) => {
                                let records = if matches!(record, "A/AAAA" | "") {
                                    netcheck::parse_getent(&out)
                                } else {
                                    netcheck::parse_dig(&out)
                                };
                                let status = out
                                    .lines()
                                    .find_map(|l| l.strip_prefix("SWY_DNS "))
                                    .map(str::to_owned)
                                    .filter(|_| records.is_empty())
                                    .map(|m| {
                                        if m == "none" {
                                            "not found".to_owned()
                                        } else {
                                            m
                                        }
                                    });
                                netcheck::format_dns(&netcheck::DnsReport {
                                    name: host.clone(),
                                    resolver: format!("resolver of {name}"),
                                    records,
                                    status,
                                })
                            }
                            Err(e) => fail(format!("{name}: {e}")),
                        }
                    }
                    (CheckKind::Tls, None) => {
                        netcheck::format_tls(&netcheck::tls_local(&host, port).await, now)
                    }
                    (CheckKind::Tls, Some(h)) => {
                        let hq = netcheck::shell_quote(&host);
                        let cmd = format!(
                            "echo | timeout 15 openssl s_client -connect {hq}:{port} -servername {hq} -showcerts 2>&1"
                        );
                        match self.host_command(h, &cmd).await {
                            Ok(out) => netcheck::format_tls(
                                &netcheck::parse_s_client(&out, &format!("{host}:{port}")),
                                now,
                            ),
                            Err(e) => fail(format!("{}: {e}", self.host_name(h).await)),
                        }
                    }
                    (_, None) => netcheck::format_tcp(&netcheck::tcp_local(&host, port).await),
                    (_, Some(h)) => match self
                        .host_command(h, &netcheck::tcp_command(&host, port))
                        .await
                    {
                        Ok(out) => netcheck::format_tcp(&netcheck::parse_tcp_output(
                            &out,
                            &format!("{host}:{port}"),
                        )),
                        Err(e) => fail(format!("{}: {e}", self.host_name(h).await)),
                    },
                }
            }
        }
    }

    // ------------------------------------------------------------- logs

    async fn cloudwatch(&self, conn: &ProfileId, region: &str) -> Result<CloudWatchLogs> {
        let c = self.cloud_profile(conn.clone()).await?;
        if c.service.provider() != CloudProvider::Aws {
            return Err(CoreError::Unsupported(format!(
                "{} is not an AWS connection",
                c.name
            )));
        }
        let secret = self.cloud_secret(&c, None).await?;
        let (auth, default_region) = self.aws_auth(&c, secret).await?;
        let region = match region.trim() {
            "" => default_region,
            r => r.to_owned(),
        };
        Ok(CloudWatchLogs::new(auth, &region, None)?)
    }

    /// Follow `src`, emitting batches of lines until it ends or is cancelled.
    async fn tail(&self, request: RequestId, src: LogSource) -> Result<(), String> {
        match src {
            LogSource::HostFile { host, path, lines } => {
                let source = path.rsplit('/').next().unwrap_or(path.as_str()).to_owned();
                let cmd = logline::tail_command(&path, lines);
                self.tail_host(request, &host, &cmd, move |l| {
                    logline::parse_text(l, &source)
                })
                .await
            }
            LogSource::Journal { host, unit, lines } => {
                let cmd = logline::journal_command(unit.as_deref(), lines);
                self.tail_host(request, &host, &cmd, |l| {
                    logline::parse_journal(l)
                        .unwrap_or_else(|| logline::parse_text(l, "journalctl"))
                })
                .await
            }
            LogSource::CloudWatch {
                conn,
                region,
                group,
                pattern,
                since_mins,
                follow,
            } => {
                let logs = self.cloudwatch(&conn, &region).await.map_err(s)?;
                self.tail_cloudwatch(request, &logs, &group, &pattern, since_mins, follow)
                    .await
            }
            LogSource::LogAnalytics {
                conn,
                workspace,
                query,
                since_mins,
                follow,
            } => {
                let token = self
                    .azure_token_for(conn.as_ref(), LOG_ANALYTICS_SCOPE)
                    .await
                    .map_err(s)?;
                let la = LogAnalytics::new(token, None).map_err(s)?;
                self.tail_log_analytics(request, &la, &workspace, &query, since_mins, follow)
                    .await
            }
        }
    }

    async fn tail_host(
        &self,
        request: RequestId,
        host: &ProfileId,
        command: &str,
        parse: impl Fn(&str) -> LogLine,
    ) -> Result<(), String> {
        let name = self.host_name(host).await;
        let target = self.ssh_target(host).await.map_err(s)?;
        let conn = self.ssh.session(&target).await.map_err(s)?;
        let mut rx = conn.stream_command(command).await.map_err(s)?;
        self.infra_emit(
            request,
            InfraUpdate::LogStatus(format!("Following on {name}")),
        );
        let mut out = LineSplitter::default();
        let mut err = LineSplitter::default();
        let mut batch: Vec<LogLine> = Vec::new();
        let mut last_flush = Instant::now();
        loop {
            let exit = match tokio::time::timeout(LOG_FLUSH, rx.recv()).await {
                Ok(Some(CommandChunk::Stdout(b))) => {
                    batch.extend(out.push(&b).iter().map(|l| parse(l)));
                    None
                }
                Ok(Some(CommandChunk::Stderr(b))) => {
                    batch.extend(err.push(&b).iter().map(|l| LogLine {
                        level: Level::Warn,
                        source: "stderr".into(),
                        message: l.clone(),
                        ts_ms: None,
                    }));
                    None
                }
                Ok(Some(CommandChunk::Exit(code))) => Some(code),
                Ok(None) => Some(None),
                // Nothing new for a while: flush what is waiting.
                Err(_) => None,
            };
            if let Some(code) = exit {
                if let Some(l) = out.finish() {
                    batch.push(parse(&l));
                }
                if !batch.is_empty() {
                    self.infra_emit(request, InfraUpdate::LogLines(std::mem::take(&mut batch)));
                }
                return match code {
                    Some(0) | None => Ok(()),
                    Some(c) => Err(format!("the command ended with status {c}")),
                };
            }
            if !batch.is_empty() && (batch.len() >= LOG_BATCH || last_flush.elapsed() >= LOG_FLUSH)
            {
                self.infra_emit(request, InfraUpdate::LogLines(std::mem::take(&mut batch)));
                last_flush = Instant::now();
            }
        }
    }

    async fn tail_cloudwatch(
        &self,
        request: RequestId,
        logs: &CloudWatchLogs,
        group: &str,
        pattern: &str,
        since_mins: u32,
        follow: bool,
    ) -> Result<(), String> {
        let mut start = switchyard_store::now_ms() - i64::from(since_mins.max(1)) * 60_000;
        // Events at the last timestamp already shown (polls overlap by that millisecond).
        let mut seen_at_start: Vec<String> = Vec::new();
        self.infra_emit(
            request,
            InfraUpdate::LogStatus(if follow {
                format!("Polling {group} every {} s", POLL_EVERY.as_secs())
            } else {
                format!("Reading {group}")
            }),
        );
        loop {
            let mut token: Option<String> = None;
            let mut newest = start;
            let mut at_newest: Vec<String> = Vec::new();
            for _ in 0..20 {
                let page = logs
                    .filter(group, pattern, start, None, token.as_deref())
                    .await
                    .map_err(s)?;
                let lines: Vec<LogLine> = page
                    .events
                    .iter()
                    .filter(|e| !(e.ts_ms == start && seen_at_start.contains(&e.id)))
                    .map(|e| {
                        if e.ts_ms > newest {
                            newest = e.ts_ms;
                            at_newest.clear();
                        }
                        if e.ts_ms == newest {
                            at_newest.push(e.id.clone());
                        }
                        let mut l = logline::parse_text(&e.message, &e.stream);
                        l.ts_ms = Some(e.ts_ms);
                        l
                    })
                    .collect();
                for chunk in lines.chunks(LOG_BATCH) {
                    self.infra_emit(request, InfraUpdate::LogLines(chunk.to_vec()));
                }
                token = page.next;
                if token.is_none() {
                    break;
                }
            }
            if newest > start {
                start = newest;
                seen_at_start = at_newest;
            } else {
                seen_at_start.extend(at_newest);
            }
            if !follow {
                return Ok(());
            }
            tokio::time::sleep(POLL_EVERY).await;
        }
    }

    async fn tail_log_analytics(
        &self,
        request: RequestId,
        la: &LogAnalytics,
        workspace: &str,
        query: &str,
        since_mins: u32,
        follow: bool,
    ) -> Result<(), String> {
        let mut from = switchyard_store::now_ms() - i64::from(since_mins.max(1)) * 60_000;
        self.infra_emit(
            request,
            InfraUpdate::LogStatus(if follow {
                format!("Re-running the query every {} s", POLL_EVERY.as_secs() * 3)
            } else {
                "Running the query".into()
            }),
        );
        loop {
            let to = switchyard_store::now_ms();
            let span = format!("{}/{}", logline::iso(from), logline::iso(to));
            let table = la.query(workspace, query, &span).await.map_err(s)?;
            let t = &table;
            let time_col = t.column(&["TimeGenerated", "timestamp", "Timestamp"]);
            let msg_col = t.column(&[
                "Message",
                "message",
                "SyslogMessage",
                "RenderedDescription",
                "LogEntry",
                "ResultDescription",
                "Log_s",
            ]);
            let level_col = t.column(&["SeverityLevel", "Level", "SeverityText", "severity"]);
            let src_col = t.column(&["Computer", "_ResourceId", "Category", "Type"]);
            let text = |v: &serde_json::Value| match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Null => String::new(),
                other => other.to_string(),
            };
            let mut lines: Vec<LogLine> = t
                .rows
                .iter()
                .map(|row| {
                    let get = |i: Option<usize>| i.and_then(|i| row.get(i)).map(text);
                    let message = match get(msg_col) {
                        Some(m) => m,
                        None => t
                            .columns
                            .iter()
                            .zip(row.iter())
                            .filter(|(c, _)| {
                                Some(c.as_str())
                                    != time_col.and_then(|i| t.columns.get(i)).map(String::as_str)
                            })
                            .map(|(c, v)| format!("{c}={}", text(v)))
                            .collect::<Vec<_>>()
                            .join(" "),
                    };
                    LogLine {
                        ts_ms: get(time_col).and_then(|v| {
                            switchyard_cloud::parse_utc_ms(v.get(0..19).unwrap_or(v.as_str()))
                        }),
                        level: get(level_col).map_or_else(
                            || logline::level_in_text(&message),
                            |l| Level::from_name(&l),
                        ),
                        source: get(src_col).unwrap_or_default(),
                        message,
                    }
                })
                .collect();
            lines.sort_by_key(|l| l.ts_ms);
            for chunk in lines.chunks(LOG_BATCH) {
                self.infra_emit(request, InfraUpdate::LogLines(chunk.to_vec()));
            }
            if !follow {
                return Ok(());
            }
            from = to;
            tokio::time::sleep(POLL_EVERY * 3).await;
        }
    }
}

/// A DNS lookup from this computer: the system resolver for addresses, DNS over HTTPS for
/// other record types.
async fn dns_local(name: &str, record: &str) -> netcheck::DnsReport {
    let mut report = netcheck::DnsReport {
        name: name.to_owned(),
        resolver: String::new(),
        records: Vec::new(),
        status: None,
    };
    match record {
        "A/AAAA" | "A" | "AAAA" | "" => {
            report.resolver = "system resolver".into();
            match tokio::time::timeout(netcheck::STEP_TIMEOUT, tokio::net::lookup_host((name, 0)))
                .await
            {
                Ok(Ok(addrs)) => {
                    for a in addrs {
                        let ip = a.ip();
                        let rtype = if ip.is_ipv4() { "A" } else { "AAAA" };
                        if (record == "A" && rtype != "A") || (record == "AAAA" && rtype != "AAAA")
                        {
                            continue;
                        }
                        let value = ip.to_string();
                        if !report.records.iter().any(|r| r.value == value) {
                            report.records.push(netcheck::DnsRecord {
                                rtype: rtype.into(),
                                value,
                                ttl: None,
                            });
                        }
                    }
                }
                Ok(Err(e)) => report.status = Some(e.to_string()),
                Err(_) => report.status = Some("timed out".into()),
            }
        }
        t => {
            report.resolver = "DNS over HTTPS (cloudflare-dns.com)".into();
            let r = async {
                let tls = switchyard_db::tls::client_config().map_err(s)?;
                let http = reqwest::Client::builder()
                    .tls_backend_preconfigured(tls)
                    .timeout(netcheck::STEP_TIMEOUT)
                    .build()
                    .map_err(s)?;
                let mut url = url::Url::parse(netcheck::DOH_URL).map_err(s)?;
                url.query_pairs_mut()
                    .append_pair("name", name)
                    .append_pair("type", t);
                let resp = http
                    .get(url)
                    .header("accept", "application/dns-json")
                    .send()
                    .await
                    .map_err(s)?;
                let json: serde_json::Value = resp.json().await.map_err(s)?;
                Ok::<_, String>(json)
            }
            .await;
            match r {
                Ok(json) => {
                    let (records, status) = netcheck::parse_doh(&json);
                    report.records = records;
                    report.status = status;
                }
                Err(e) => report.status = Some(e),
            }
        }
    }
    report
}
