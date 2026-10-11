//! Azure App Configuration: key-values with labels, content types, tags and locks, and
//! the feature flags stored among them (`.appconfig.featureflag/<name>`).
//!
//! Auth is a connection string (`Endpoint=…;Id=…;Secret=…`, HMAC-SHA256 request signing)
//! or a Microsoft Entra token for `https://azconfig.io` (App Configuration Data Reader /
//! Owner roles).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::SystemTime;

use data_encoding::BASE64;
use futures::future::BoxFuture;
use reqwest::Method;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value as Json, json};

use crate::auth::TokenSource;
use crate::azure::parse_connection_string;
use crate::error::{CloudError, Result};
use crate::http::{Req, Resp, client, hmac256, sha256_b64, uri_encode};
use crate::kv::{KvCaps, KvItem, KvPage, KvQuery, KvService, KvWrite, json_message};
use crate::time::http_date;

/// Data-plane API version.
pub const API_VERSION: &str = "1.0";
/// Most revisions listed for one setting.
const MAX_REVISIONS: usize = 200;
/// Key prefix of feature flags.
pub const FEATURE_FLAG_PREFIX: &str = ".appconfig.featureflag/";
/// Content type of feature flags.
pub const FEATURE_FLAG_CONTENT_TYPE: &str =
    "application/vnd.microsoft.appconfig.ff+json;charset=utf-8";
/// Content type of Key Vault references.
pub const KEY_VAULT_REF_CONTENT_TYPE: &str =
    "application/vnd.microsoft.appconfig.keyvaultref+json;charset=utf-8";

/// How requests are authorized.
pub enum AppConfigAuth {
    /// Access key id and secret (from a connection string).
    Hmac {
        /// Credential id.
        id: String,
        /// Base64 secret.
        secret: SecretString,
    },
    /// Microsoft Entra bearer token.
    Bearer(Arc<dyn TokenSource>),
}

/// An App Configuration store.
pub struct AppConfig {
    endpoint: url::Url,
    auth: AppConfigAuth,
    http: reqwest::Client,
}

/// Endpoint and HMAC credentials from a connection string.
pub fn from_connection_string(s: &SecretString) -> Result<(url::Url, AppConfigAuth)> {
    let m = parse_connection_string(s.expose_secret());
    let (Some(endpoint), Some(id), Some(secret)) =
        (m.get("endpoint"), m.get("id"), m.get("secret"))
    else {
        return Err(CloudError::Invalid(
            "an App Configuration connection string has Endpoint, Id and Secret".into(),
        ));
    };
    Ok((
        endpoint_url(endpoint)?,
        AppConfigAuth::Hmac {
            id: id.clone(),
            secret: SecretString::from(secret.clone()),
        },
    ))
}

/// `https://<name>.azconfig.io` from a store name or URL.
pub fn endpoint_url(name_or_url: &str) -> Result<url::Url> {
    let s = name_or_url.trim().trim_end_matches('/');
    let s = if s.contains("://") {
        s.to_owned()
    } else {
        format!("https://{s}.azconfig.io")
    };
    url::Url::parse(&s).map_err(|e| CloudError::Invalid(format!("App Configuration endpoint: {e}")))
}

/// Sign with the App Configuration HMAC scheme.
pub(crate) fn sign_hmac(
    req: &mut Req,
    id: &str,
    secret: &SecretString,
    now: SystemTime,
) -> Result<()> {
    let key = BASE64
        .decode(secret.expose_secret().trim().as_bytes())
        .map_err(|_| CloudError::Invalid("the connection string's Secret is not base64".into()))?;
    let date = http_date(now);
    let hash = sha256_b64(&req.body);
    let host = req.host();
    let url = req.url()?;
    let path_query = match url.query() {
        Some(q) => format!("{}?{q}", url.path()),
        None => url.path().to_owned(),
    };
    let to_sign = format!(
        "{}\n{path_query}\n{date};{host};{hash}",
        req.method.as_str()
    );
    let sig = BASE64.encode(&hmac256(&key, to_sign.as_bytes()));
    req.set_header("x-ms-date", date);
    req.set_header("x-ms-content-sha256", hash);
    req.set_header(
        "authorization",
        format!(
            "HMAC-SHA256 Credential={id}&SignedHeaders=x-ms-date;host;x-ms-content-sha256&Signature={sig}"
        ),
    );
    Ok(())
}

/// Add the `label` parameter of a single-setting request; the null label is no parameter
/// (`\0` is only for list filters).
fn with_label(req: Req, label: Option<&str>) -> Req {
    match label {
        Some(l) if !l.is_empty() => req.query("label", l),
        _ => req,
    }
}

fn item_from(j: &Json) -> KvItem {
    let s = |k: &str| j.get(k).and_then(Json::as_str).map(str::to_owned);
    KvItem {
        key: s("key").unwrap_or_default(),
        label: s("label"),
        value: s("value"),
        content_type: s("content_type").filter(|c| !c.is_empty()),
        enabled: None,
        locked: j.get("locked").and_then(Json::as_bool),
        modified_ms: s("last_modified").and_then(|t| crate::time::parse_iso_ms(&t)),
        etag: s("etag"),
        tags: j
            .get("tags")
            .and_then(Json::as_object)
            .map(|o| {
                o.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned()))
                    .collect()
            })
            .unwrap_or_default(),
        ..KvItem::default()
    }
}

impl AppConfig {
    /// A client for the store at `endpoint`.
    pub fn new(endpoint: url::Url, auth: AppConfigAuth) -> Result<Self> {
        Ok(Self {
            endpoint,
            auth,
            http: client()?,
        })
    }

    async fn send(&self, mut req: Req) -> Result<Resp> {
        req = req.query("api-version", API_VERSION);
        match &self.auth {
            AppConfigAuth::Hmac { id, secret } => {
                sign_hmac(&mut req, id, secret, SystemTime::now())?
            }
            AppConfigAuth::Bearer(t) => {
                let t = t.token().await?;
                req.set_header("authorization", format!("Bearer {}", t.expose_secret()));
            }
        }
        let resp = Resp::read(req.send(&self.http).await?).await?;
        if !resp.ok() {
            let json = resp.json().unwrap_or(Json::Null);
            let msg = json_message(&json).unwrap_or_default();
            let msg = match resp.status {
                403 if matches!(self.auth, AppConfigAuth::Bearer(_)) => format!(
                    "Signed in, but this account has no data role on the store (App \
                     Configuration Data Reader or Owner). {msg}"
                ),
                409 => format!("The setting is locked or changed meanwhile. {msg}"),
                412 => "Changed by someone else since it was loaded; reload it first".into(),
                _ => msg,
            };
            return Err(CloudError::api(resp.status, None, msg));
        }
        Ok(resp)
    }

    fn kv_path(key: &str) -> String {
        format!("/kv/{}", uri_encode(key, false))
    }

    /// Every page of a list request, up to `max` items.
    async fn all_pages(&self, mut req: Req, max: usize) -> Result<Vec<Json>> {
        let mut out = Vec::new();
        loop {
            let path = req.path.clone();
            let json = self
                .send(req.header(
                    "accept",
                    "application/vnd.microsoft.appconfig.kvset+json, application/problem+json",
                ))
                .await?
                .json()?;
            if let Some(a) = json.get("items").and_then(Json::as_array) {
                out.extend(a.iter().cloned());
            }
            let next = json.get("@nextLink").and_then(Json::as_str);
            match next {
                Some(link) if out.len() < max => req = self.next_page(link, &path)?,
                _ => break,
            }
        }
        out.truncate(max);
        Ok(out)
    }

    /// A request for `@nextLink` (a path and query relative to the endpoint).
    fn next_page(&self, link: &str, path: &str) -> Result<Req> {
        let u = self
            .endpoint
            .join(link)
            .map_err(|e| CloudError::Invalid(e.to_string()))?;
        let mut r = Req::new(Method::GET, &self.endpoint, path);
        r.query = u
            .query_pairs()
            .filter(|(k, _)| k != "api-version")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        Ok(r)
    }
}

/// A key or label matched exactly in a filter: `\`, `*` and `,` are escaped.
fn exact_filter(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '*' | ',') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// App Configuration's key filter: `prefix*` unless it already has wildcards.
fn key_filter(key: &str) -> String {
    let k = key.trim();
    if k.is_empty() {
        "*".into()
    } else if k.contains('*') || k.contains(',') {
        k.to_owned()
    } else {
        format!("{k}*")
    }
}

impl KvService for AppConfig {
    fn caps(&self) -> KvCaps {
        KvCaps {
            labels: true,
            content_type: true,
            locks: true,
            feature_flags: true,
            tags: true,
            values_in_list: true,
            history: true,
            filter_hint: "Key filter: prefix, or * wildcards (app:*,db:*)",
            ..KvCaps::default()
        }
    }

    fn list<'a>(&'a self, q: &'a KvQuery) -> BoxFuture<'a, Result<KvPage>> {
        Box::pin(async move {
            let req = match &q.cursor {
                // `@nextLink` is a path and query relative to the endpoint.
                Some(link) => self.next_page(link, "/kv")?,
                None => {
                    let label = match q.label.trim() {
                        "" => "*".to_owned(),
                        "\\0" | "(no label)" => "\0".to_owned(),
                        l => l.to_owned(),
                    };
                    Req::new(Method::GET, &self.endpoint, "/kv")
                        .query("key", key_filter(&q.key))
                        .query("label", label)
                }
            };
            let resp = self
                .send(req.header(
                    "accept",
                    "application/vnd.microsoft.appconfig.kvset+json, application/problem+json",
                ))
                .await?;
            let json = resp.json()?;
            let items = json
                .get("items")
                .and_then(Json::as_array)
                .map(|a| a.iter().map(item_from).collect())
                .unwrap_or_default();
            Ok(KvPage {
                items,
                next: json
                    .get("@nextLink")
                    .and_then(Json::as_str)
                    .map(str::to_owned),
            })
        })
    }

    fn get<'a>(
        &'a self,
        _scope: Option<&'a str>,
        key: &'a str,
        label: Option<&'a str>,
    ) -> BoxFuture<'a, Result<KvItem>> {
        Box::pin(async move {
            let resp = self
                .send(with_label(
                    Req::new(Method::GET, &self.endpoint, Self::kv_path(key)),
                    label,
                ))
                .await?;
            let mut item = item_from(&resp.json()?);
            if item.etag.is_none() {
                item.etag = resp.header("etag").map(|e| e.trim_matches('"').to_owned());
            }
            Ok(item)
        })
    }

    fn put<'a>(&'a self, w: &'a KvWrite) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let body = json!({
                "value": w.value,
                "content_type": w.content_type.clone().unwrap_or_default(),
                "tags": w.tags,
            });
            let mut req = with_label(
                Req::new(Method::PUT, &self.endpoint, Self::kv_path(&w.key)),
                w.label.as_deref(),
            )
            .header(
                "content-type",
                "application/vnd.microsoft.appconfig.kv+json",
            )
            .body(serde_json::to_vec(&body).map_err(|e| CloudError::Invalid(e.to_string()))?);
            if w.create {
                req.set_header("if-none-match", "\"*\"");
            } else if let Some(e) = &w.etag {
                req.set_header("if-match", format!("\"{e}\""));
            }
            match self.send(req).await {
                Err(CloudError::Api { status: 412, .. }) if w.create => Err(CloudError::api(
                    412,
                    None,
                    format!("{} already exists with this label", w.key),
                )),
                r => r.map(|_| ()),
            }
        })
    }

    fn delete<'a>(
        &'a self,
        _scope: Option<&'a str>,
        key: &'a str,
        label: Option<&'a str>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.send(with_label(
                Req::new(Method::DELETE, &self.endpoint, Self::kv_path(key)),
                label,
            ))
            .await?;
            Ok(())
        })
    }

    fn set_locked<'a>(
        &'a self,
        key: &'a str,
        label: Option<&'a str>,
        locked: bool,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let method = if locked { Method::PUT } else { Method::DELETE };
            self.send(with_label(
                Req::new(
                    method,
                    &self.endpoint,
                    format!("/locks/{}", uri_encode(key, false)),
                ),
                label,
            ))
            .await?;
            Ok(())
        })
    }

    fn revisions<'a>(
        &'a self,
        key: &'a str,
        label: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<KvItem>>> {
        Box::pin(async move {
            let label = match label {
                Some(l) if !l.is_empty() => exact_filter(l),
                _ => "\0".to_owned(),
            };
            let req = Req::new(Method::GET, &self.endpoint, "/revisions")
                .query("key", exact_filter(key))
                .query("label", label);
            let mut items: Vec<KvItem> = self
                .all_pages(req, MAX_REVISIONS)
                .await?
                .iter()
                .map(item_from)
                .collect();
            items.sort_by_key(|a| std::cmp::Reverse(a.modified_ms));
            Ok(items)
        })
    }

    fn labels(&self) -> BoxFuture<'_, Result<Vec<Option<String>>>> {
        Box::pin(async move {
            let req = Req::new(Method::GET, &self.endpoint, "/labels").query("name", "*");
            let mut labels: Vec<Option<String>> = self
                .all_pages(req, 1000)
                .await?
                .iter()
                .map(|i| i.get("name").and_then(Json::as_str).map(str::to_owned))
                .collect();
            labels.sort();
            labels.dedup();
            Ok(labels)
        })
    }
}

/// A feature flag's editable parts; everything else in its JSON is kept as is.
#[derive(Clone, Debug, PartialEq)]
pub struct FeatureFlag {
    /// Flag id (the key without [`FEATURE_FLAG_PREFIX`]).
    pub id: String,
    /// On or off.
    pub enabled: bool,
    /// Description.
    pub description: String,
    /// Number of client filters (targeting, time window, percentage).
    pub filters: usize,
}

/// Whether a setting is a feature flag.
pub fn is_feature_flag(key: &str) -> bool {
    key.starts_with(FEATURE_FLAG_PREFIX)
}

/// Read a feature flag's value.
pub fn parse_flag(key: &str, value: &str) -> Option<FeatureFlag> {
    let j: Json = serde_json::from_str(value).ok()?;
    Some(FeatureFlag {
        id: j
            .get("id")
            .and_then(Json::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| key.trim_start_matches(FEATURE_FLAG_PREFIX).to_owned()),
        enabled: j.get("enabled").and_then(Json::as_bool).unwrap_or(false),
        description: j
            .get("description")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_owned(),
        filters: j
            .pointer("/conditions/client_filters")
            .and_then(Json::as_array)
            .map_or(0, Vec::len),
    })
}

/// A flag's value with `enabled` (and `description`, when given) changed, keeping
/// conditions and any other fields.
pub fn update_flag(value: &str, enabled: bool, description: Option<&str>) -> Result<String> {
    let mut j: Json = serde_json::from_str(value)
        .map_err(|e| CloudError::Invalid(format!("the flag's value is not JSON: {e}")))?;
    let o = j
        .as_object_mut()
        .ok_or_else(|| CloudError::Invalid("the flag's value is not a JSON object".into()))?;
    o.insert("enabled".into(), Json::Bool(enabled));
    if let Some(d) = description {
        o.insert("description".into(), Json::String(d.to_owned()));
    }
    serde_json::to_string(&j).map_err(|e| CloudError::Invalid(e.to_string()))
}

/// A new flag `id`: its key, value and content type.
pub fn new_flag(id: &str, enabled: bool, description: &str) -> (String, String) {
    let value = json!({
        "id": id,
        "description": description,
        "enabled": enabled,
        "conditions": { "client_filters": [] },
    });
    (format!("{FEATURE_FLAG_PREFIX}{id}"), value.to_string())
}

/// Whether a setting is a Key Vault reference (by its content type).
pub fn is_key_vault_ref(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|c| {
        c.trim()
            .to_ascii_lowercase()
            .starts_with("application/vnd.microsoft.appconfig.keyvaultref+json")
    })
}

/// The secret URI of a Key Vault reference's value (`{"uri": "https://…"}`).
pub fn key_vault_ref_uri(value: &str) -> Option<String> {
    let j: Json = serde_json::from_str(value).ok()?;
    j.get("uri")
        .and_then(Json::as_str)
        .map(|u| u.trim().to_owned())
        .filter(|u| !u.is_empty())
}

/// The value of a Key Vault reference to `uri`.
pub fn key_vault_ref(uri: &str) -> String {
    json!({ "uri": uri.trim() }).to_string()
}

/// Tags written as `a=1; b=2`.
pub fn format_tags(tags: &BTreeMap<String, String>) -> String {
    tags.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Parse `a=1; b=2`.
pub fn parse_tags(s: &str) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for part in s.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let (k, v) = part
            .split_once('=')
            .ok_or_else(|| CloudError::Invalid(format!("\"{part}\" is not name=value")))?;
        out.insert(k.trim().to_owned(), v.trim().to_owned());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn connection_string() {
        let (url, auth) = from_connection_string(&SecretString::from(
            "Endpoint=https://acme.azconfig.io;Id=abc;Secret=c2VjcmV0".to_owned(),
        ))
        .unwrap();
        assert_eq!(url.as_str(), "https://acme.azconfig.io/");
        assert!(matches!(auth, AppConfigAuth::Hmac { .. }));
        assert!(from_connection_string(&SecretString::from("Endpoint=x".to_owned())).is_err());
        assert_eq!(
            endpoint_url("acme").unwrap().as_str(),
            "https://acme.azconfig.io/"
        );
    }

    /// Checked against the documented algorithm computed in Python.
    #[test]
    fn hmac_signature() {
        let base = url::Url::parse("https://acme.azconfig.io").unwrap();
        let mut req = Req::new(Method::GET, &base, "/kv")
            .query("key", "app:*")
            .query("label", "\0")
            .query("api-version", "1.0");
        let now = UNIX_EPOCH + Duration::from_secs(1_440_938_160);
        sign_hmac(
            &mut req,
            "abc",
            &SecretString::from("c2VjcmV0".to_owned()),
            now,
        )
        .unwrap();
        let auth = &req
            .headers
            .iter()
            .find(|(k, _)| k == "authorization")
            .unwrap()
            .1;
        assert_eq!(
            req.url().unwrap().as_str(),
            "https://acme.azconfig.io/kv?key=app%3A%2A&label=%00&api-version=1.0"
        );
        assert_eq!(
            auth,
            &format!(
                "HMAC-SHA256 Credential=abc&SignedHeaders=x-ms-date;host;x-ms-content-sha256&Signature={HMAC_EXPECTED}"
            )
        );
    }

    const HMAC_EXPECTED: &str = "tKR3LjF34Wvb78A+qbVxsPQrqjkLYx5WwsJt0X038Vg=";

    #[test]
    fn flags() {
        let (key, value) = new_flag("Beta", false, "New checkout");
        assert_eq!(key, ".appconfig.featureflag/Beta");
        let f = parse_flag(&key, &value).unwrap();
        assert!(!f.enabled);
        assert_eq!(f.id, "Beta");
        let with_filter = r#"{"id":"Beta","enabled":false,"conditions":{"client_filters":[{"name":"Microsoft.Percentage","parameters":{"Value":50}}]},"x":1}"#;
        let on = update_flag(with_filter, true, None).unwrap();
        let f = parse_flag(&key, &on).unwrap();
        assert!(f.enabled);
        assert_eq!(f.filters, 1);
        assert!(on.contains("\"x\":1"));
        assert!(update_flag("nope", true, None).is_err());
    }

    #[test]
    fn filters_and_tags() {
        assert_eq!(key_filter(""), "*");
        assert_eq!(key_filter("app:"), "app:*");
        assert_eq!(key_filter("a*,b*"), "a*,b*");
        let t = parse_tags("env=prod; team = web").unwrap();
        assert_eq!(format_tags(&t), "env=prod; team=web");
        assert!(parse_tags("oops").is_err());
    }
}
