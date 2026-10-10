//! Cloud connections: object storage as file systems (Files tab, transfers) and the key /
//! value tools ([`Command::CloudOpen`] and friends).
//!
//! [`Command::CloudOpen`]: crate::bus::Command::CloudOpen

use std::sync::Arc;
use std::time::Instant;

use futures::future::BoxFuture;
use secrecy::SecretString;
use switchyard_cloud::auth::{StaticToken, TokenSource};
use switchyard_cloud::aws::{AwsAuth, AwsSource};
use switchyard_cloud::azure::{
    APP_CONFIG_SCOPE, AzureAuth, AzureCliToken, KEY_VAULT_SCOPE, STORAGE_SCOPE,
};
use switchyard_cloud::cloudflare::{CloudflareApi, r2_credentials};
use switchyard_cloud::kv::ReadOnlyKv;
use switchyard_cloud::sigv4::AwsCredentials;
use switchyard_cloud::{CloudError, KvService, appconfig, blob, keyvault, s3};
use switchyard_remote::RemoteFs;
use switchyard_store::{CloudAuth, CloudConnection, CloudService, Profile, ProfileId};
use tracing::info;

use super::{Service, lock};
use crate::bus::{CloudEdit, CloudInfo, Event, RequestId, SessionId};
use crate::entra::EntraSignIn;
use crate::error::{CoreError, Result};

/// An open key / value tool.
pub(super) struct CloudSlot {
    connection: CloudConnection,
    svc: Arc<dyn KvService>,
}

/// Entra tokens for one connection and resource, signing in when needed.
struct EntraSource {
    entra: Arc<EntraSignIn>,
    connection: CloudConnection,
    scope: &'static str,
    secret: Option<SecretString>,
}

impl TokenSource for EntraSource {
    fn token(&self) -> BoxFuture<'_, switchyard_cloud::Result<SecretString>> {
        Box::pin(async move {
            self.entra
                .cloud_token(&self.connection, self.scope, self.secret.clone())
                .await
                .map_err(|e| CloudError::Auth(format!("Microsoft sign-in: {e}")))
        })
    }
}

fn missing(what: &str) -> CoreError {
    CoreError::Cloud(CloudError::Auth(format!(
        "the {what} is not saved for this connection; edit it and enter it again"
    )))
}

fn url(s: &str, what: &str) -> Result<url::Url> {
    url::Url::parse(s.trim())
        .map_err(|e| CoreError::Cloud(CloudError::Invalid(format!("{what}: {e}"))))
}

impl Service {
    /// The secret typed in the editor, else the stored one.
    async fn cloud_secret(
        &self,
        c: &CloudConnection,
        typed: Option<SecretString>,
    ) -> Result<Option<SecretString>> {
        if typed.is_some() {
            return Ok(typed);
        }
        match c.secret.clone() {
            Some(key) => self.with_secrets(move |s| s.get(&key)).await,
            None => Ok(None),
        }
    }

    /// Bearer tokens for an Azure `scope`, per the connection's sign-in method.
    fn azure_tokens(
        &self,
        c: &CloudConnection,
        scope: &'static str,
        secret: Option<SecretString>,
    ) -> Result<Arc<dyn TokenSource>> {
        Ok(match c.auth {
            CloudAuth::AzureCli => Arc::new(AzureCliToken::new(scope, c.tenant.clone())),
            a if a.is_entra() => Arc::new(EntraSource {
                entra: self.entra.clone(),
                connection: c.clone(),
                scope,
                secret,
            }),
            other => {
                return Err(CoreError::Unsupported(format!(
                    "{} has no bearer tokens",
                    other.label()
                )));
            }
        })
    }

    /// AWS credentials and the region to use.
    async fn aws_auth(
        &self,
        c: &CloudConnection,
        secret: Option<SecretString>,
    ) -> Result<(Arc<AwsAuth>, String)> {
        let region = c.region.trim();
        match c.auth {
            CloudAuth::AwsProfile => {
                let profile = if c.user.trim().is_empty() {
                    "default".to_owned()
                } else {
                    c.user.trim().to_owned()
                };
                let region = if region.is_empty() {
                    let p = profile.clone();
                    tokio::task::spawn_blocking(move || switchyard_cloud::aws::profile_region(&p))
                        .await
                        .ok()
                        .flatten()
                        .unwrap_or_else(|| "us-east-1".into())
                } else {
                    region.to_owned()
                };
                Ok((Arc::new(AwsAuth::new(AwsSource::Profile(profile))), region))
            }
            CloudAuth::AccessKey => {
                let secret = secret.ok_or_else(|| missing("secret access key"))?;
                let creds = AwsCredentials {
                    access_key_id: c.user.trim().to_owned(),
                    secret_access_key: secret,
                    session_token: None,
                    expires_ms: None,
                };
                let region = match (c.service, region) {
                    (CloudService::R2, _) => "auto".to_owned(),
                    (_, "") => "us-east-1".to_owned(),
                    (_, r) => r.to_owned(),
                };
                Ok((Arc::new(AwsAuth::new(AwsSource::Keys(creds))), region))
            }
            CloudAuth::ApiToken if c.service == CloudService::R2 => {
                let token = secret.ok_or_else(|| missing("API token"))?;
                let api = CloudflareApi::new(Arc::new(StaticToken(token.clone())), None)?;
                let id = api.token_id(&c.endpoint).await?;
                Ok((
                    Arc::new(AwsAuth::new(AwsSource::Keys(r2_credentials(&id, &token)))),
                    "auto".into(),
                ))
            }
            other => Err(CoreError::Unsupported(format!(
                "{} cannot sign AWS requests",
                other.label()
            ))),
        }
    }

    /// Object storage of a cloud connection as a file system.
    pub(super) async fn cloud_fs(
        &self,
        c: &CloudConnection,
        typed: Option<SecretString>,
    ) -> Result<Arc<dyn RemoteFs>> {
        let secret = self.cloud_secret(c, typed).await?;
        let home = c.default_path.clone().filter(|p| !p.trim().is_empty());
        match c.service {
            CloudService::S3 | CloudService::R2 => {
                let (auth, region) = self.aws_auth(c, secret).await?;
                let mut cfg = if c.service == CloudService::R2 {
                    s3::S3Config::r2(&c.name, &c.endpoint)?
                } else {
                    s3::S3Config {
                        name: c.name.clone(),
                        endpoint: match c.endpoint.trim() {
                            "" => None,
                            e => Some(url(e, "endpoint")?),
                        },
                        region: region.clone(),
                        home: None,
                        read_only: false,
                        service: "S3",
                    }
                };
                cfg.home = home;
                cfg.read_only = c.read_only;
                Ok(Arc::new(s3::S3Fs::new(cfg, auth)?))
            }
            CloudService::AzureBlob => {
                let (endpoint, auth) = match c.auth {
                    CloudAuth::ConnectionString => {
                        let cs = secret.ok_or_else(|| missing("connection string"))?;
                        let (endpoint, _, auth) =
                            switchyard_cloud::azure::storage_connection_string(&cs)?;
                        (endpoint, auth)
                    }
                    CloudAuth::SharedKey => {
                        let endpoint = switchyard_cloud::azure::blob_endpoint(&c.endpoint)?;
                        let account = switchyard_cloud::azure::account_from_endpoint(&endpoint)
                            .unwrap_or_else(|| c.endpoint.trim().to_owned());
                        let key = secret.ok_or_else(|| missing("account key"))?;
                        (endpoint, AzureAuth::SharedKey { account, key })
                    }
                    CloudAuth::Sas => {
                        let endpoint = switchyard_cloud::azure::blob_endpoint(&c.endpoint)?;
                        let sas = secret.ok_or_else(|| missing("SAS token"))?;
                        let sas = secrecy::ExposeSecret::expose_secret(&sas)
                            .trim()
                            .trim_start_matches('?')
                            .to_owned();
                        (endpoint, AzureAuth::Sas(SecretString::from(sas)))
                    }
                    _ => (
                        switchyard_cloud::azure::blob_endpoint(&c.endpoint)?,
                        AzureAuth::Bearer(self.azure_tokens(c, STORAGE_SCOPE, secret)?),
                    ),
                };
                Ok(Arc::new(blob::BlobFs::new(
                    blob::BlobConfig {
                        name: c.name.clone(),
                        endpoint,
                        home,
                        read_only: c.read_only,
                    },
                    auth,
                )?))
            }
            other => Err(CoreError::Unsupported(format!(
                "{} is not file storage",
                other.display_name()
            ))),
        }
    }

    /// The key / value service of a cloud connection.
    pub(super) async fn cloud_kv(
        &self,
        c: &CloudConnection,
        typed: Option<SecretString>,
    ) -> Result<Arc<dyn KvService>> {
        let secret = self.cloud_secret(c, typed).await?;
        let svc: Box<dyn KvService> = match c.service {
            CloudService::AppConfig => {
                let (endpoint, auth) = match c.auth {
                    CloudAuth::ConnectionString => {
                        let cs = secret.ok_or_else(|| missing("connection string"))?;
                        appconfig::from_connection_string(&cs)?
                    }
                    _ => (
                        appconfig::endpoint_url(&c.endpoint)?,
                        appconfig::AppConfigAuth::Bearer(self.azure_tokens(
                            c,
                            APP_CONFIG_SCOPE,
                            secret,
                        )?),
                    ),
                };
                Box::new(appconfig::AppConfig::new(endpoint, auth)?)
            }
            CloudService::KeyVault => Box::new(keyvault::KeyVault::new(
                keyvault::vault_url(&c.endpoint)?,
                self.azure_tokens(c, KEY_VAULT_SCOPE, secret)?,
            )?),
            CloudService::SecretsManager | CloudService::ParameterStore => {
                let (auth, region) = self.aws_auth(c, secret).await?;
                let endpoint = match c.endpoint.trim() {
                    "" => None,
                    e => Some(url(e, "endpoint")?),
                };
                if c.service == CloudService::SecretsManager {
                    Box::new(switchyard_cloud::secrets_manager::SecretsManager::new(
                        auth,
                        &region,
                        endpoint,
                        c.read_only,
                    )?)
                } else {
                    Box::new(switchyard_cloud::parameters::ParameterStore::new(
                        auth,
                        &region,
                        endpoint,
                        c.read_only,
                    )?)
                }
            }
            CloudService::WorkersKv => {
                let token = secret.ok_or_else(|| missing("API token"))?;
                let api = Arc::new(CloudflareApi::new(Arc::new(StaticToken(token)), None)?);
                Box::new(switchyard_cloud::workers_kv::WorkersKv::new(
                    api,
                    &c.endpoint,
                    c.read_only,
                )?)
            }
            other => {
                return Err(CoreError::Unsupported(format!(
                    "{} opens in the Files tab",
                    other.display_name()
                )));
            }
        };
        Ok(if c.read_only {
            Arc::new(ReadOnlyKv(svc))
        } else {
            Arc::from(svc)
        })
    }

    /// Test connection: sign in and list something.
    pub(super) async fn test_cloud(
        &self,
        c: CloudConnection,
        secret: Option<SecretString>,
    ) -> Result<String> {
        let started = Instant::now();
        let ms = || started.elapsed().as_millis();
        if c.service.is_storage() {
            let fs = self.cloud_fs(&c, secret).await?;
            let home = fs.home();
            let listed = fs.list(&home).await.map_err(|e| {
                CoreError::Unsupported(if home.as_os_str() == "/" {
                    format!(
                        "Signed in, but listing buckets / containers failed: {e}. If the \
                         credentials only reach one bucket, set it as the default path"
                    )
                } else {
                    format!("Listing {}: {e}", home.display())
                })
            })?;
            return Ok(format!(
                "Connected · {} entries in {} · {} ms",
                listed.len(),
                home.display(),
                ms()
            ));
        }
        let svc = self.cloud_kv(&c, secret).await?;
        let scopes = svc.scopes().await?;
        if let Some(label) = svc.caps().scope_label {
            return Ok(format!(
                "Connected · {} {}{} · {} ms",
                scopes.len(),
                label.to_lowercase(),
                if scopes.len() == 1 { "" } else { "s" },
                ms()
            ));
        }
        let page = svc.list(&switchyard_cloud::KvQuery::default()).await?;
        Ok(format!(
            "Connected · {}{} items on the first page · {} ms",
            page.items.len(),
            if page.next.is_some() { "+" } else { "" },
            ms()
        ))
    }

    async fn cloud_profile(&self, id: ProfileId) -> Result<CloudConnection> {
        match self.with_store(move |s| s.profile(&id)).await? {
            Some(Profile::Cloud(c)) => Ok(c),
            _ => Err(CoreError::NotFound("cloud connection".into())),
        }
    }

    /// The object storage file system of a saved cloud connection.
    pub(super) async fn cloud_fs_by_id(&self, id: &ProfileId) -> Result<Arc<dyn RemoteFs>> {
        let c = self.cloud_profile(id.clone()).await?;
        self.cloud_fs(&c, None).await
    }

    pub(super) async fn cloud_open(&self, session: SessionId, id: ProfileId) -> Result<CloudInfo> {
        let c = self.cloud_profile(id).await?;
        info!(connection = %c.name, service = ?c.service, "cloud tool open");
        let svc = self.cloud_kv(&c, None).await?;
        let scopes = svc.scopes().await?;
        let info = CloudInfo {
            service: c.service,
            caps: svc.caps(),
            scopes,
            read_only: c.read_only,
        };
        lock(&self.cloud).insert(session, Arc::new(CloudSlot { connection: c, svc }));
        Ok(info)
    }

    fn cloud_slot(&self, session: SessionId) -> Result<Arc<CloudSlot>> {
        lock(&self.cloud)
            .get(&session)
            .cloned()
            .ok_or_else(|| CoreError::NotFound("cloud session".into()))
    }

    pub(super) async fn cloud_list(
        &self,
        session: SessionId,
        request: RequestId,
        query: switchyard_cloud::KvQuery,
    ) {
        let result = async {
            let slot = self.cloud_slot(session)?;
            Ok::<_, CoreError>(slot.svc.list(&query).await?)
        }
        .await;
        self.emit(Event::CloudItems {
            session,
            request,
            result: result.map_err(|e| e.to_string()),
        });
    }

    pub(super) async fn cloud_get(
        &self,
        session: SessionId,
        request: RequestId,
        scope: Option<String>,
        key: String,
        label: Option<String>,
    ) {
        let result = async {
            let slot = self.cloud_slot(session)?;
            Ok::<_, CoreError>(
                slot.svc
                    .get(scope.as_deref(), &key, label.as_deref())
                    .await?,
            )
        }
        .await;
        self.emit(Event::CloudItem {
            session,
            request,
            result: result.map_err(|e| e.to_string()),
        });
    }

    pub(super) async fn cloud_edit(&self, session: SessionId, request: RequestId, edit: CloudEdit) {
        let result = async {
            let slot = self.cloud_slot(session)?;
            if slot.connection.read_only {
                return Err(CoreError::Cloud(CloudError::ReadOnly));
            }
            let svc = &slot.svc;
            let name = slot.connection.name.as_str();
            Ok::<_, CoreError>(match edit {
                CloudEdit::Put(w) => {
                    svc.put(&w).await?;
                    info!(connection = name, create = w.create, "cloud item saved");
                    if w.create {
                        format!("Created {}", w.key)
                    } else {
                        format!("Saved {}", w.key)
                    }
                }
                CloudEdit::Delete { scope, key, label } => {
                    svc.delete(scope.as_deref(), &key, label.as_deref()).await?;
                    info!(connection = name, "cloud item deleted");
                    format!("Deleted {key}")
                }
                CloudEdit::Lock { key, label, locked } => {
                    svc.set_locked(&key, label.as_deref(), locked).await?;
                    format!("{} {key}", if locked { "Locked" } else { "Unlocked" })
                }
            })
        }
        .await;
        self.emit(Event::CloudEdited {
            session,
            request,
            result: result.map_err(|e| e.to_string()),
        });
    }
}
