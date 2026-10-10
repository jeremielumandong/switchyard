//! Azure credentials: Shared Key and SAS for storage accounts, connection strings, bearer
//! tokens (Microsoft Entra through `core`, or the Azure CLI's `az account get-access-token`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use data_encoding::BASE64;
use futures::future::BoxFuture;
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value as Json;

use crate::auth::{TokenSource, run_cli};
use crate::error::{CloudError, Result};
use crate::http::{Req, hmac256};

/// Azure Storage's Entra scope.
pub const STORAGE_SCOPE: &str = "https://storage.azure.com/.default";
/// Azure App Configuration's Entra scope (all regions).
pub const APP_CONFIG_SCOPE: &str = "https://azconfig.io/.default";
/// Azure Key Vault's Entra scope.
pub const KEY_VAULT_SCOPE: &str = "https://vault.azure.net/.default";

/// How requests to an Azure service are authorized.
#[derive(Clone)]
pub enum AzureAuth {
    /// Storage account name and key (base64, as the portal shows it).
    SharedKey {
        /// Account name.
        account: String,
        /// Account key.
        key: SecretString,
    },
    /// A shared access signature (the query string, without `?`).
    Sas(SecretString),
    /// Bearer tokens.
    Bearer(Arc<dyn TokenSource>),
}

impl std::fmt::Debug for AzureAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            AzureAuth::SharedKey { .. } => "SharedKey",
            AzureAuth::Sas(_) => "Sas",
            AzureAuth::Bearer(_) => "Bearer",
        })
    }
}

/// `Key=Value;Key=Value` settings, keys lower-cased. Values may contain `=` (keys, SAS).
pub fn parse_connection_string(s: &str) -> HashMap<String, String> {
    s.split(';')
        .filter_map(|part| {
            let (k, v) = part.split_once('=')?;
            Some((k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        })
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

/// Azurite's well-known development account key (public, documented by Microsoft).
const DEV_ACCOUNT: &str = "devstoreaccount1";
const DEV_KEY: &str =
    "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";

/// The blob endpoint, account name and credential of a storage connection string.
pub fn storage_connection_string(s: &SecretString) -> Result<(url::Url, String, AzureAuth)> {
    let m = parse_connection_string(s.expose_secret());
    if m.get("usedevelopmentstorage").map(String::as_str) == Some("true") {
        let url = url::Url::parse(&format!("http://127.0.0.1:10000/{DEV_ACCOUNT}"))
            .map_err(|e| CloudError::Invalid(e.to_string()))?;
        return Ok((
            url,
            DEV_ACCOUNT.into(),
            AzureAuth::SharedKey {
                account: DEV_ACCOUNT.into(),
                key: SecretString::from(DEV_KEY.to_owned()),
            },
        ));
    }
    let account = m.get("accountname").cloned().unwrap_or_default();
    let endpoint = match m.get("blobendpoint") {
        Some(e) => e.clone(),
        None if !account.is_empty() => format!(
            "{}://{account}.blob.{}",
            m.get("defaultendpointsprotocol")
                .map(String::as_str)
                .unwrap_or("https"),
            m.get("endpointsuffix")
                .map(String::as_str)
                .unwrap_or("core.windows.net")
        ),
        None => {
            return Err(CloudError::Invalid(
                "the connection string names no AccountName or BlobEndpoint".into(),
            ));
        }
    };
    let url = url::Url::parse(&endpoint)
        .map_err(|e| CloudError::Invalid(format!("BlobEndpoint: {e}")))?;
    let account = if account.is_empty() {
        account_from_endpoint(&url).unwrap_or_default()
    } else {
        account
    };
    let auth = if let Some(key) = m.get("accountkey") {
        AzureAuth::SharedKey {
            account: account.clone(),
            key: SecretString::from(key.clone()),
        }
    } else if let Some(sas) = m.get("sharedaccesssignature") {
        AzureAuth::Sas(SecretString::from(sas.trim_start_matches('?').to_owned()))
    } else {
        return Err(CloudError::Invalid(
            "the connection string has no AccountKey or SharedAccessSignature".into(),
        ));
    };
    Ok((url, account, auth))
}

/// `myaccount` from `https://myaccount.blob.core.windows.net`, or from the first path
/// segment of an emulator URL (`http://127.0.0.1:10000/devstoreaccount1`).
pub fn account_from_endpoint(url: &url::Url) -> Option<String> {
    let host = url.host_str()?;
    if host.parse::<std::net::IpAddr>().is_ok() || host == "localhost" {
        return url
            .path_segments()?
            .next()
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
    }
    host.split('.').next().map(str::to_owned)
}

/// The blob endpoint for a storage account name or URL.
pub fn blob_endpoint(account_or_url: &str) -> Result<url::Url> {
    let s = account_or_url.trim().trim_end_matches('/');
    let s = if s.contains("://") {
        s.to_owned()
    } else {
        format!("https://{s}.blob.core.windows.net")
    };
    url::Url::parse(&s).map_err(|e| CloudError::Invalid(format!("storage account: {e}")))
}

/// Sign a storage request with Shared Key (`x-ms-date` and `x-ms-version` must be set).
pub(crate) fn sign_shared_key(req: &mut Req, account: &str, key: &SecretString) -> Result<()> {
    let key = BASE64
        .decode(key.expose_secret().trim().as_bytes())
        .map_err(|_| CloudError::Invalid("the account key is not valid base64".into()))?;
    let h = |name: &str| {
        req.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .unwrap_or("")
    };
    let len = if req.body.is_empty() {
        String::new()
    } else {
        req.body.len().to_string()
    };
    let mut ms: Vec<(String, String)> = req
        .headers
        .iter()
        .filter(|(k, _)| k.starts_with("x-ms-"))
        .map(|(k, v)| (k.clone(), v.trim().to_owned()))
        .collect();
    ms.sort();
    let canonical_headers: String = ms.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let mut resource = format!("/{account}{}", req.full_path());
    let mut q: Vec<(String, String)> = req
        .query
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
        .collect();
    q.sort();
    let mut grouped: Vec<(String, Vec<String>)> = Vec::new();
    for (k, v) in q {
        match grouped.last_mut() {
            Some((lk, vs)) if *lk == k => vs.push(v),
            _ => grouped.push((k, vec![v])),
        }
    }
    for (k, vs) in grouped {
        resource.push_str(&format!("\n{k}:{}", vs.join(",")));
    }
    let to_sign = format!(
        "{}\n{}\n{}\n{}\n{}\n{}\n\n{}\n{}\n{}\n{}\n{}\n{}{}",
        req.method.as_str(),
        h("content-encoding"),
        h("content-language"),
        len,
        h("content-md5"),
        h("content-type"),
        h("if-modified-since"),
        h("if-match"),
        h("if-none-match"),
        h("if-unmodified-since"),
        h("range"),
        canonical_headers,
        resource
    );
    let sig = BASE64.encode(&hmac256(&key, to_sign.as_bytes()));
    req.set_header("authorization", format!("SharedKey {account}:{sig}"));
    Ok(())
}

/// Tokens from the Azure CLI's sign-in (`az login`), cached until shortly before expiry.
pub struct AzureCliToken {
    resource: String,
    tenant: Option<String>,
    cache: tokio::sync::Mutex<Option<(SecretString, i64)>>,
}

impl AzureCliToken {
    /// Tokens for `scope` (`https://storage.azure.com/.default`), optionally in `tenant`.
    pub fn new(scope: &str, tenant: Option<String>) -> Self {
        Self {
            resource: scope
                .trim_end_matches(".default")
                .trim_end_matches('/')
                .to_owned(),
            tenant: tenant.filter(|t| !t.trim().is_empty()),
            cache: tokio::sync::Mutex::new(None),
        }
    }

    async fn fetch(&self) -> Result<(SecretString, i64)> {
        let mut args = vec![
            "account",
            "get-access-token",
            "--resource",
            &self.resource,
            "--output",
            "json",
        ];
        if let Some(t) = &self.tenant {
            args.push("--tenant");
            args.push(t);
        }
        let out = run_cli("az", &args).await.map_err(|e| {
            CloudError::Auth(format!(
                "Azure CLI: {e}. Sign in with `az login` (or pick another sign-in method)"
            ))
        })?;
        let json: Json = serde_json::from_slice(&out)
            .map_err(|_| CloudError::Auth("the Azure CLI printed no token".into()))?;
        let token = json
            .get("accessToken")
            .and_then(Json::as_str)
            .ok_or_else(|| CloudError::Auth("the Azure CLI printed no token".into()))?;
        let expires = json
            .get("expires_on")
            .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()))
            .map(|s| s * 1000)
            .unwrap_or_else(|| now_ms() + 30 * 60 * 1000);
        Ok((SecretString::from(token.to_owned()), expires))
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

impl TokenSource for AzureCliToken {
    fn token(&self) -> BoxFuture<'_, Result<SecretString>> {
        Box::pin(async move {
            let mut cache = self.cache.lock().await;
            if let Some((t, exp)) = cache.as_ref()
                && *exp > now_ms() + 5 * 60 * 1000
            {
                return Ok(t.clone());
            }
            let fresh = self.fetch().await?;
            let t = fresh.0.clone();
            *cache = Some(fresh);
            Ok(t)
        })
    }
}

/// Apply `auth` to a storage request (after its `x-ms-*` headers are set).
pub(crate) async fn authorize(req: &mut Req, auth: &AzureAuth) -> Result<()> {
    match auth {
        AzureAuth::SharedKey { account, key } => sign_shared_key(req, account, key),
        AzureAuth::Sas(sas) => {
            for pair in sas.expose_secret().split('&') {
                if let Some((k, v)) = pair.split_once('=') {
                    let v = url::form_urlencoded::parse(format!("x={v}").as_bytes())
                        .next()
                        .map(|(_, v)| v.into_owned())
                        .unwrap_or_default();
                    req.query.push((k.to_owned(), v));
                }
            }
            Ok(())
        }
        AzureAuth::Bearer(src) => {
            let t = src.token().await?;
            req.set_header("authorization", format!("Bearer {}", t.expose_secret()));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_strings() {
        let (url, account, auth) = storage_connection_string(&SecretString::from(
            "DefaultEndpointsProtocol=https;AccountName=acme;AccountKey=a2V5;EndpointSuffix=core.windows.net"
                .to_owned(),
        ))
        .unwrap();
        assert_eq!(url.as_str(), "https://acme.blob.core.windows.net/");
        assert_eq!(account, "acme");
        assert!(matches!(auth, AzureAuth::SharedKey { .. }));

        let (url, account, auth) = storage_connection_string(&SecretString::from(
            "BlobEndpoint=https://acme.blob.core.windows.net/;SharedAccessSignature=sv=2022&sig=a%2Bb="
                .to_owned(),
        ))
        .unwrap();
        assert_eq!(account, "acme");
        assert_eq!(url.host_str(), Some("acme.blob.core.windows.net"));
        assert!(matches!(auth, AzureAuth::Sas(_)));

        let (url, account, _) =
            storage_connection_string(&SecretString::from("UseDevelopmentStorage=true".to_owned()))
                .unwrap();
        assert_eq!(account, "devstoreaccount1");
        assert_eq!(
            account_from_endpoint(&url).as_deref(),
            Some("devstoreaccount1")
        );
        assert!(storage_connection_string(&SecretString::from("x=y".to_owned())).is_err());
    }

    #[test]
    fn endpoints() {
        assert_eq!(
            blob_endpoint("acme").unwrap().as_str(),
            "https://acme.blob.core.windows.net/"
        );
        assert_eq!(
            blob_endpoint("http://127.0.0.1:10000/devstoreaccount1/")
                .unwrap()
                .as_str(),
            "http://127.0.0.1:10000/devstoreaccount1"
        );
    }

    /// Shared Key string-to-sign layout, checked against a signature computed with the
    /// documented algorithm in Python (`hmac`, `hashlib`, `base64`).
    #[test]
    fn shared_key() {
        let base = url::Url::parse("https://acme.blob.core.windows.net").unwrap();
        let mut req = Req::new(reqwest::Method::GET, &base, "/photos")
            .query("restype", "container")
            .query("comp", "list")
            .header("x-ms-date", "Sun, 30 Aug 2015 12:36:00 GMT")
            .header("x-ms-version", "2023-11-03");
        sign_shared_key(&mut req, "acme", &SecretString::from("a2V5".to_owned())).unwrap();
        let auth = req
            .headers
            .iter()
            .find(|(k, _)| k == "authorization")
            .map(|(_, v)| v.clone())
            .unwrap();
        assert_eq!(auth, format!("SharedKey acme:{}", SHARED_KEY_EXPECTED));
    }

    const SHARED_KEY_EXPECTED: &str = "KAZXYGyH2Fl4fzXrwnpEWOjDq7WxtXI2AhNuMbg4dyE=";
}
