//! Named secrets (`{{vault.name}}`): the catalog commands and resolution for connection
//! settings and the API workbench.
//!
//! A local named secret is a keychain item; a cloud one is read through a saved cloud
//! connection (Azure Key Vault, AWS Secrets Manager, Parameter Store) when it is used and
//! kept in memory for [`CACHE_TTL`], so a rotated secret is picked up within a minute.
//! Values never reach the UI, the store, history or logs.

use std::sync::Arc;
use std::time::{Duration, Instant};

use secrecy::{ExposeSecret as _, SecretString};
use switchyard_store::named_secrets::{NamedSecret, NamedSecretSource, parse_expression};
use switchyard_store::{CloudService, Profile, ProfileId, SecretRef, StoreError};
use tracing::info;

use super::{Service, lock};
use crate::bus::{Event, RequestId};
use crate::error::{CoreError, Result};

/// How long a value read from a cloud store is reused.
const CACHE_TTL: Duration = Duration::from_secs(60);

/// Longest wait for a cloud read from a blocking caller (the API workbench).
const BLOCKING_TIMEOUT: Duration = Duration::from_secs(90);

fn not_found(name: &str) -> CoreError {
    CoreError::NotFound(format!("Secret '{name}' in the secret vault"))
}

/// The field `field` of a JSON object value, as text.
fn json_field(value: &str, field: &str) -> Result<String> {
    let invalid = || {
        CoreError::Unsupported(format!(
            "the secret is not a JSON object with a '{field}' field"
        ))
    };
    let json: serde_json::Value = serde_json::from_str(value).map_err(|_| invalid())?;
    match json.get(field).ok_or_else(invalid)? {
        serde_json::Value::String(s) => Ok(s.clone()),
        serde_json::Value::Null => Err(invalid()),
        other => Ok(other.to_string()),
    }
}

impl Service {
    /// Emit the catalog ([`Event::NamedSecrets`]).
    pub(super) async fn emit_named_secrets(&self) {
        match self.with_store(|s| s.named_secrets()).await {
            Ok(list) => self.emit(Event::NamedSecrets(list)),
            Err(e) => self.error("Secret vault", e),
        }
    }

    /// [`Command::SaveNamedSecret`](crate::bus::Command::SaveNamedSecret).
    pub(super) async fn save_named_secret(
        &self,
        request: RequestId,
        secret: NamedSecret,
        value: Option<SecretString>,
        previous: Option<String>,
    ) {
        let fail = |field, message: String| Event::NamedSecretError {
            request,
            field,
            message,
        };
        if let Err(e) = secret.validate() {
            self.emit(fail(Some(e.field), e.message));
            return;
        }
        let name = secret.name.clone();
        let renamed = previous.clone().filter(|p| *p != name);
        let local = secret.source == NamedSecretSource::Local;
        let result = async {
            if local {
                let key = SecretRef::for_named(&name);
                match (value, &renamed) {
                    (Some(v), _) => self.with_secrets(move |s| s.set(&key, &v)).await?,
                    // A renamed local secret keeps its value under the new name.
                    (None, Some(old)) => {
                        let old_key = SecretRef::for_named(old);
                        if let Some(v) = self.with_secrets(move |s| s.get(&old_key)).await? {
                            self.with_secrets(move |s| s.set(&key, &v)).await?;
                        }
                    }
                    (None, None) => {}
                }
            }
            let (s2, p2) = (secret.clone(), previous.clone());
            self.with_store(move |s| s.save_named_secret(&s2, p2.as_deref()))
                .await?;
            // A secret that moved to the cloud, or was renamed, drops its old local value.
            let mut stale = renamed.clone();
            if !local {
                stale.get_or_insert_with(|| name.clone());
            }
            if let Some(old) = stale {
                let key = SecretRef::for_named(&old);
                self.with_secrets(move |s| s.delete(&key)).await?;
            }
            Ok::<_, CoreError>(())
        }
        .await;
        match result {
            Ok(()) => {
                self.forget_named(&name);
                if let Some(old) = &renamed {
                    self.forget_named(old);
                }
                info!(name = %name, "named secret saved");
                self.emit_named_secrets().await;
            }
            Err(CoreError::Store(StoreError::Validation(v))) => {
                self.emit(fail(Some(v.field), v.message))
            }
            Err(e) => {
                self.emit(fail(None, e.to_string()));
                self.emit_secret_backend();
            }
        }
    }

    /// [`Command::DeleteNamedSecret`](crate::bus::Command::DeleteNamedSecret).
    pub(super) async fn delete_named_secret(&self, name: String) {
        let n = name.clone();
        match self.with_store(move |s| s.delete_named_secret(&n)).await {
            Ok(_) => {
                let key = SecretRef::for_named(&name);
                if let Err(e) = self.with_secrets(move |s| s.delete(&key)).await {
                    self.error("Secret vault", e);
                }
                self.forget_named(&name);
                self.emit_named_secrets().await;
            }
            Err(e) => self.error("Secret vault", e),
        }
    }

    /// [`Command::TestNamedSecret`](crate::bus::Command::TestNamedSecret): read it and say
    /// how long it is and where it came from. Bypasses the cache.
    pub(super) async fn test_named_secret(&self, request: RequestId, name: String) {
        self.forget_named(&name);
        let result = match self.resolve_named(&name).await {
            Ok(v) => {
                let n = v.expose_secret().chars().count();
                let from = match self.named_entry(&name).await {
                    Ok(Some(NamedSecret {
                        source: NamedSecretSource::Cloud { connection, .. },
                        ..
                    })) => self
                        .cloud_name(&connection)
                        .await
                        .unwrap_or_else(|| "the cloud store".into()),
                    _ => "the keychain".into(),
                };
                Ok(format!(
                    "Read {n} character{} from {from}",
                    if n == 1 { "" } else { "s" }
                ))
            }
            Err(e) => Err(e.to_string()),
        };
        self.emit(Event::NamedSecretTested {
            request,
            name,
            result,
        });
    }

    async fn named_entry(&self, name: &str) -> Result<Option<NamedSecret>> {
        let n = name.to_owned();
        self.with_store(move |s| s.named_secret(&n)).await
    }

    async fn cloud_name(&self, id: &ProfileId) -> Option<String> {
        let id = id.clone();
        match self.with_store(move |s| s.profile(&id)).await {
            Ok(Some(p)) => Some(p.name().to_owned()),
            _ => None,
        }
    }

    fn forget_named(&self, name: &str) {
        lock(&self.named_cache).remove(name);
    }

    /// The value of a named secret, from the keychain or its cloud store.
    pub(crate) async fn resolve_named(&self, name: &str) -> Result<SecretString> {
        if let Some((at, v)) = lock(&self.named_cache).get(name)
            && at.elapsed() < CACHE_TTL
        {
            return Ok(v.clone());
        }
        match self.named_entry(name).await? {
            Some(NamedSecret {
                source:
                    NamedSecretSource::Cloud {
                        connection,
                        key,
                        field,
                    },
                ..
            }) => {
                let value = self.read_cloud_secret(&connection, &key).await?;
                let value = match field.as_deref() {
                    Some(f) => json_field(value.expose_secret(), f.trim())?,
                    None => value.expose_secret().to_owned(),
                };
                let value = SecretString::from(value);
                lock(&self.named_cache).insert(name.to_owned(), (Instant::now(), value.clone()));
                info!(name, "named secret read from its cloud store");
                Ok(value)
            }
            // Local, or a value the API workbench stored before the catalog existed.
            _ => self.resolve_local_named(name).await,
        }
    }

    async fn resolve_local_named(&self, name: &str) -> Result<SecretString> {
        let key = SecretRef::for_named(name);
        self.with_secrets(move |s| s.get(&key))
            .await?
            .ok_or_else(|| not_found(name))
    }

    /// Read `key` through the cloud connection `id`. Its own saved secret may be a local
    /// named secret, never a cloud one (no chains, no loops).
    async fn read_cloud_secret(&self, id: &ProfileId, key: &str) -> Result<SecretString> {
        let pid = id.clone();
        let c = match self.with_store(move |s| s.profile(&pid)).await? {
            Some(Profile::Cloud(c)) => c,
            _ => return Err(CoreError::NotFound(format!("Cloud connection {id}"))),
        };
        if !matches!(
            c.service,
            CloudService::KeyVault | CloudService::SecretsManager | CloudService::ParameterStore
        ) {
            return Err(CoreError::Unsupported(format!(
                "{} is a {}; secrets come from Key Vault, Secrets Manager or Parameter Store",
                c.name,
                c.service.display_name()
            )));
        }
        let secret = match c.secret.clone() {
            Some(r) => match r.named_secret() {
                Some(n) => {
                    let n = n.to_owned();
                    match self.named_entry(&n).await? {
                        Some(NamedSecret {
                            source: NamedSecretSource::Cloud { .. },
                            ..
                        }) => {
                            return Err(CoreError::Unsupported(format!(
                                "{}'s own secret is '{n}', which is also read from the \
                                 cloud; store it locally",
                                c.name
                            )));
                        }
                        _ => Some(self.resolve_local_named(&n).await?),
                    }
                }
                None => self.with_secrets(move |s| s.get(&r)).await?,
            },
            None => None,
        };
        let svc = self.cloud_kv_resolved(&c, secret).await?;
        let item = svc.get(None, key, None).await?;
        match item.value {
            Some(v) => Ok(SecretString::from(v)),
            None => Err(CoreError::Unsupported(format!(
                "'{key}' in {} has no value",
                c.name
            ))),
        }
    }

    /// The secret to use for a connection: what was typed in the editor, else the saved
    /// one. Either may be a named secret: `{{vault.name}}` typed, or a saved reference.
    pub(super) async fn secret_value(
        &self,
        typed: Option<SecretString>,
        saved: Option<SecretRef>,
    ) -> Result<Option<SecretString>> {
        if let Some(t) = typed {
            return match parse_expression(t.expose_secret()) {
                Some(name) => self.resolve_named(name).await.map(Some),
                None => Ok(Some(t)),
            };
        }
        match saved {
            Some(r) => match r.named_secret() {
                Some(name) => self.resolve_named(name).await.map(Some),
                None => self.with_secrets(move |s| s.get(&r)).await,
            },
            None => Ok(None),
        }
    }
}

/// Resolves named secrets for callers outside the runtime (the API workbench, which runs on
/// the blocking pool): the read runs on the runtime and the caller waits for it.
#[derive(Clone)]
pub struct NamedSecrets {
    service: std::sync::Weak<Service>,
    handle: tokio::runtime::Handle,
}

impl NamedSecrets {
    pub(crate) fn new(service: &Arc<Service>, handle: tokio::runtime::Handle) -> Self {
        Self {
            service: Arc::downgrade(service),
            handle,
        }
    }

    /// The value of `name`, waiting for a cloud read. Call from a blocking thread, never
    /// from the UI thread.
    pub fn resolve_blocking(&self, name: &str) -> Result<SecretString> {
        let service = self
            .service
            .upgrade()
            .ok_or_else(|| CoreError::Internal("the runtime is gone".into()))?;
        let (tx, rx) = std::sync::mpsc::channel();
        let name = name.to_owned();
        self.handle.spawn(async move {
            let _ = tx.send(service.resolve_named(&name).await);
        });
        rx.recv_timeout(BLOCKING_TIMEOUT)
            .map_err(|_| CoreError::Internal("reading the secret timed out".into()))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_fields() {
        let v = r#"{"username":"app","password":"p@ss","port":5432,"none":null}"#;
        assert_eq!(json_field(v, "password").unwrap(), "p@ss");
        assert_eq!(json_field(v, "port").unwrap(), "5432");
        assert!(json_field(v, "none").is_err());
        assert!(json_field(v, "missing").is_err());
        let e = json_field("not json p@ss", "password")
            .unwrap_err()
            .to_string();
        assert!(!e.contains("p@ss"));
    }
}
