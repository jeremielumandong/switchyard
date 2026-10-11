//! Azure Key Vault secrets (Microsoft Entra only: Key Vault has no keys of its own).
//! Deleting a secret soft-deletes it; it stays recoverable for the vault's retention period.

use std::sync::Arc;

use futures::future::BoxFuture;
use reqwest::Method;
use secrecy::ExposeSecret as _;
use serde_json::{Value as Json, json};

use crate::auth::TokenSource;
use crate::error::{CloudError, Result};
use crate::http::{Req, Resp, client, uri_encode};
use crate::kv::{KvCaps, KvItem, KvPage, KvQuery, KvService, KvWrite, json_message};

/// Key Vault API version.
pub const API_VERSION: &str = "7.4";

/// A vault's secrets.
pub struct KeyVault {
    vault: url::Url,
    token: Arc<dyn TokenSource>,
    http: reqwest::Client,
}

/// `https://<name>.vault.azure.net` from a vault name or URL.
pub fn vault_url(name_or_url: &str) -> Result<url::Url> {
    let s = name_or_url.trim().trim_end_matches('/');
    let s = if s.contains("://") {
        s.to_owned()
    } else {
        format!("https://{s}.vault.azure.net")
    };
    url::Url::parse(&s).map_err(|e| CloudError::Invalid(format!("vault: {e}")))
}

/// A secret's address: `https://<vault>.vault.azure.net/secrets/<name>[/<version>]`, as
/// App Configuration's Key Vault references hold it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretUri {
    /// The vault (scheme and host).
    pub vault: url::Url,
    /// Secret name.
    pub name: String,
    /// A pinned version (`None`: the current one).
    pub version: Option<String>,
}

/// Parse a secret URI.
pub fn parse_secret_uri(uri: &str) -> Result<SecretUri> {
    let bad = || CloudError::Invalid(format!("not a Key Vault secret URI: {uri}"));
    let u = url::Url::parse(uri.trim()).map_err(|_| bad())?;
    if u.scheme() != "https" && u.scheme() != "http" {
        return Err(bad());
    }
    let mut parts = u.path_segments().ok_or_else(bad)?.filter(|s| !s.is_empty());
    if parts.next() != Some("secrets") {
        return Err(bad());
    }
    let name = parts.next().ok_or_else(bad)?.to_owned();
    let version = parts.next().map(str::to_owned);
    let mut vault = u.clone();
    vault.set_path("");
    vault.set_query(None);
    vault.set_fragment(None);
    Ok(SecretUri {
        vault,
        name,
        version,
    })
}

/// Secret name from its id (`https://v.vault.azure.net/secrets/<name>[/<version>]`).
fn name_of(id: &str) -> String {
    id.split("/secrets/")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or(id)
        .to_owned()
}

fn item_from(j: &Json) -> KvItem {
    let s = |p: &str| j.pointer(p).and_then(Json::as_str).map(str::to_owned);
    let secs = |p: &str| j.pointer(p).and_then(Json::as_i64).map(|s| s * 1000);
    KvItem {
        key: s("/id").map(|i| name_of(&i)).unwrap_or_default(),
        value: s("/value"),
        content_type: s("/contentType"),
        enabled: j.pointer("/attributes/enabled").and_then(Json::as_bool),
        modified_ms: secs("/attributes/updated"),
        tags: j
            .get("tags")
            .and_then(Json::as_object)
            .map(|o| {
                o.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned()))
                    .collect()
            })
            .unwrap_or_default(),
        kind: match (secs("/deletedDate"), secs("/scheduledPurgeDate")) {
            (Some(d), Some(p)) => Some(format!(
                "deleted {} · purged {}",
                crate::time::display_ms(d),
                crate::time::display_ms(p)
            )),
            (Some(d), None) => Some(format!("deleted {}", crate::time::display_ms(d))),
            _ => secs("/attributes/exp").map(|e| format!("expires {}", crate::time::display_ms(e))),
        },
        version: s("/id").and_then(|i| version_of(&i)),
        not_before_ms: secs("/attributes/nbf"),
        expires_ms: secs("/attributes/exp"),
        ..KvItem::default()
    }
}

/// Version from a secret id (`…/secrets/<name>/<version>`).
fn version_of(id: &str) -> Option<String> {
    id.split("/secrets/")
        .nth(1)?
        .split('/')
        .nth(1)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

/// Most versions listed for one secret.
const MAX_VERSIONS: usize = 200;

impl KeyVault {
    /// A client for the vault at `vault`.
    pub fn new(vault: url::Url, token: Arc<dyn TokenSource>) -> Result<Self> {
        Ok(Self {
            vault,
            token,
            http: client()?,
        })
    }

    async fn send(&self, mut req: Req) -> Result<Resp> {
        if !req.query.iter().any(|(k, _)| k == "api-version") {
            req = req.query("api-version", API_VERSION);
        }
        let t = self.token.token().await?;
        req.set_header("authorization", format!("Bearer {}", t.expose_secret()));
        let resp = Resp::read(req.send(&self.http).await?).await?;
        if !resp.ok() {
            let json = resp.json().unwrap_or(Json::Null);
            let code = json
                .pointer("/error/code")
                .and_then(Json::as_str)
                .map(str::to_owned);
            let msg = json_message(&json).unwrap_or_default();
            let msg = if resp.status == 403 {
                format!(
                    "Signed in, but this account may not use the vault's secrets (needs Key Vault \
                     Secrets User / Officer, or an access policy). {msg}"
                )
            } else {
                msg
            };
            return Err(CloudError::api(resp.status, code, msg));
        }
        Ok(resp)
    }
}

impl KeyVault {
    /// The value of secret `name`, at `version` or the current one.
    pub async fn secret_value(&self, name: &str, version: Option<&str>) -> Result<String> {
        let path = format!(
            "/secrets/{}/{}",
            uri_encode(name, false),
            uri_encode(version.unwrap_or(""), false)
        );
        let item = item_from(
            &self
                .send(Req::new(Method::GET, &self.vault, path))
                .await?
                .json()?,
        );
        if item.enabled == Some(false) {
            return Err(CloudError::Invalid(format!(
                "the secret {name} is disabled"
            )));
        }
        item.value
            .ok_or_else(|| CloudError::Invalid(format!("the secret {name} has no value")))
    }
}

impl KvService for KeyVault {
    fn caps(&self) -> KvCaps {
        KvCaps {
            content_type: true,
            tags: true,
            enabled: true,
            secret_values: true,
            history: true,
            dates: true,
            recoverable: true,
            filter_hint: "Name prefix",
            ..KvCaps::default()
        }
    }

    fn list<'a>(&'a self, q: &'a KvQuery) -> BoxFuture<'a, Result<KvPage>> {
        Box::pin(async move {
            let req = match &q.cursor {
                Some(link) => {
                    let u =
                        url::Url::parse(link).map_err(|e| CloudError::Invalid(e.to_string()))?;
                    if u.host_str() != self.vault.host_str() {
                        return Err(CloudError::Invalid("unexpected next page link".into()));
                    }
                    let mut r = Req::new(Method::GET, &self.vault, u.path().to_owned());
                    r.query = u
                        .query_pairs()
                        .map(|(k, v)| (k.into_owned(), v.into_owned()))
                        .collect();
                    r
                }
                None => Req::new(
                    Method::GET,
                    &self.vault,
                    if q.deleted {
                        "/deletedsecrets"
                    } else {
                        "/secrets"
                    },
                )
                .query("maxresults", "25"),
            };
            let json = self.send(req).await?.json()?;
            let prefix = q.key.trim().to_lowercase();
            let items = json
                .get("value")
                .and_then(Json::as_array)
                .map(|a| {
                    a.iter()
                        .map(item_from)
                        // The API has no name filter: filter each page here.
                        .filter(|i| i.key.to_lowercase().starts_with(&prefix))
                        .collect()
                })
                .unwrap_or_default();
            Ok(KvPage {
                items,
                next: json
                    .get("nextLink")
                    .and_then(Json::as_str)
                    .filter(|l| !l.is_empty())
                    .map(str::to_owned),
            })
        })
    }

    fn get<'a>(
        &'a self,
        _scope: Option<&'a str>,
        key: &'a str,
        _label: Option<&'a str>,
    ) -> BoxFuture<'a, Result<KvItem>> {
        Box::pin(async move {
            let resp = self
                .send(Req::new(
                    Method::GET,
                    &self.vault,
                    format!("/secrets/{}/", uri_encode(key, false)),
                ))
                .await?;
            Ok(item_from(&resp.json()?))
        })
    }

    fn put<'a>(&'a self, w: &'a KvWrite) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if w.create
                && let Ok(existing) = self.get(None, &w.key, None).await
            {
                return Err(CloudError::api(
                    409,
                    None,
                    format!("{} already exists", existing.key),
                ));
            }
            let mut body = json!({ "value": w.value, "tags": w.tags });
            if let Some(ct) = w.content_type.as_ref().filter(|c| !c.is_empty()) {
                body["contentType"] = json!(ct);
            }
            let mut attributes = serde_json::Map::new();
            if let Some(e) = w.enabled {
                attributes.insert("enabled".into(), json!(e));
            }
            if let Some(nbf) = w.not_before_ms {
                attributes.insert("nbf".into(), json!(nbf / 1000));
            }
            if let Some(exp) = w.expires_ms {
                attributes.insert("exp".into(), json!(exp / 1000));
            }
            if !attributes.is_empty() {
                body["attributes"] = Json::Object(attributes);
            }
            self.send(
                Req::new(
                    Method::PUT,
                    &self.vault,
                    format!("/secrets/{}", uri_encode(&w.key, false)),
                )
                .header("content-type", "application/json")
                .body(serde_json::to_vec(&body).map_err(|e| CloudError::Invalid(e.to_string()))?),
            )
            .await?;
            Ok(())
        })
    }

    fn delete<'a>(
        &'a self,
        _scope: Option<&'a str>,
        key: &'a str,
        _label: Option<&'a str>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.send(Req::new(
                Method::DELETE,
                &self.vault,
                format!("/secrets/{}", uri_encode(key, false)),
            ))
            .await?;
            Ok(())
        })
    }

    fn revisions<'a>(
        &'a self,
        key: &'a str,
        _label: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<KvItem>>> {
        Box::pin(async move {
            let mut req = Req::new(
                Method::GET,
                &self.vault,
                format!("/secrets/{}/versions", uri_encode(key, false)),
            )
            .query("maxresults", "25");
            let mut items = Vec::new();
            loop {
                let json = self.send(req).await?.json()?;
                if let Some(a) = json.get("value").and_then(Json::as_array) {
                    items.extend(a.iter().map(item_from));
                }
                match json.get("nextLink").and_then(Json::as_str) {
                    Some(link) if !link.is_empty() && items.len() < MAX_VERSIONS => {
                        let u = url::Url::parse(link)
                            .map_err(|e| CloudError::Invalid(e.to_string()))?;
                        if u.host_str() != self.vault.host_str() {
                            return Err(CloudError::Invalid("unexpected next page link".into()));
                        }
                        req = Req::new(Method::GET, &self.vault, u.path().to_owned());
                        req.query = u
                            .query_pairs()
                            .map(|(k, v)| (k.into_owned(), v.into_owned()))
                            .collect();
                    }
                    _ => break,
                }
            }
            items.sort_by_key(|a| std::cmp::Reverse(a.modified_ms));
            Ok(items)
        })
    }

    fn get_version<'a>(&'a self, key: &'a str, version: &'a str) -> BoxFuture<'a, Result<KvItem>> {
        Box::pin(async move {
            let resp = self
                .send(Req::new(
                    Method::GET,
                    &self.vault,
                    format!(
                        "/secrets/{}/{}",
                        uri_encode(key, false),
                        uri_encode(version, false)
                    ),
                ))
                .await?;
            Ok(item_from(&resp.json()?))
        })
    }

    fn recover<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.send(Req::new(
                Method::POST,
                &self.vault,
                format!("/deletedsecrets/{}/recover", uri_encode(key, false)),
            ))
            .await?;
            Ok(())
        })
    }

    fn purge<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.send(Req::new(
                Method::DELETE,
                &self.vault,
                format!("/deletedsecrets/{}", uri_encode(key, false)),
            ))
            .await?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_uris() {
        let u = parse_secret_uri("https://kv-a.vault.azure.net/secrets/Db--Conn").unwrap();
        assert_eq!(u.vault.as_str(), "https://kv-a.vault.azure.net/");
        assert_eq!(u.name, "Db--Conn");
        assert_eq!(u.version, None);
        let v = parse_secret_uri("https://kv-a.vault.azure.net/secrets/x/abc123/").unwrap();
        assert_eq!(v.version.as_deref(), Some("abc123"));
        assert!(parse_secret_uri("https://kv-a.vault.azure.net/keys/x").is_err());
        assert!(parse_secret_uri("not a uri").is_err());
    }

    #[test]
    fn items_carry_versions_dates_and_deletion() {
        let j: Json = serde_json::from_str(
            r#"{"id":"https://v.vault.azure.net/secrets/Db/0a1b","attributes":{"enabled":true,"nbf":1700000000,"exp":1800000000,"updated":1700000100}}"#,
        )
        .unwrap();
        let i = item_from(&j);
        assert_eq!(i.key, "Db");
        assert_eq!(i.version.as_deref(), Some("0a1b"));
        assert_eq!(i.not_before_ms, Some(1_700_000_000_000));
        assert_eq!(i.expires_ms, Some(1_800_000_000_000));
        let d: Json = serde_json::from_str(
            r#"{"recoveryId":"https://v.vault.azure.net/deletedsecrets/Db","id":"https://v.vault.azure.net/secrets/Db","deletedDate":1700000000,"scheduledPurgeDate":1707776000,"attributes":{}}"#,
        )
        .unwrap();
        let i = item_from(&d);
        assert_eq!(i.key, "Db");
        assert_eq!(i.version, None);
        assert!(i.kind.unwrap().starts_with("deleted "));
    }

    #[test]
    fn items() {
        let i = item_from(&json!({
            "id": "https://v.vault.azure.net/secrets/db-password/abc123",
            "value": "s3cret",
            "attributes": {"enabled": true, "updated": 1_440_938_160},
            "tags": {"env": "prod"}
        }));
        assert_eq!(i.key, "db-password");
        assert_eq!(i.enabled, Some(true));
        assert_eq!(i.modified_ms, Some(1_440_938_160_000));
        assert_eq!(i.tags["env"], "prod");
        assert!(!format!("{i:?}").contains("s3cret"));
        assert_eq!(
            vault_url("v").unwrap().as_str(),
            "https://v.vault.azure.net/"
        );
    }
}
