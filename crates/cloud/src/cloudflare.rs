//! The Cloudflare API (`api.cloudflare.com/client/v4`) with an API token, and R2's S3
//! credentials derived from that token.

use std::sync::Arc;

use reqwest::Method;
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value as Json;

use crate::auth::TokenSource;
use crate::error::{CloudError, Result};
use crate::http::{Req, Resp, client, sha256_hex};
use crate::sigv4::AwsCredentials;

/// The API's base URL.
pub const API_BASE: &str = "https://api.cloudflare.com/client/v4";

/// A Cloudflare API client.
pub struct CloudflareApi {
    base: url::Url,
    token: Arc<dyn TokenSource>,
    http: reqwest::Client,
}

/// `errors[0].message` (and code) from an API envelope.
fn api_error(status: u16, json: &Json) -> CloudError {
    let first = json.pointer("/errors/0");
    let code = first
        .and_then(|e| e.get("code"))
        .map(|c| c.to_string().trim_matches('"').to_owned());
    let msg = first
        .and_then(|e| e.get("message"))
        .and_then(Json::as_str)
        .unwrap_or_default();
    let msg = match status {
        401 | 403 if msg.is_empty() || msg.contains("Authentication") => {
            format!("The API token was refused or lacks this permission. {msg}")
        }
        _ => msg.to_owned(),
    };
    CloudError::api(status, code, msg)
}

impl CloudflareApi {
    /// A client using `token`; `base` overrides the API URL (tests).
    pub fn new(token: Arc<dyn TokenSource>, base: Option<url::Url>) -> Result<Self> {
        let base = match base {
            Some(b) => b,
            None => url::Url::parse(API_BASE).map_err(|e| CloudError::Invalid(e.to_string()))?,
        };
        Ok(Self {
            base,
            token,
            http: client()?,
        })
    }

    /// A request to `path` (encoded, under `/client/v4`).
    pub(crate) fn req(&self, method: Method, path: impl Into<String>) -> Req {
        Req::new(method, &self.base, path)
    }

    /// Send and return the raw response, failures turned into errors.
    pub(crate) async fn send_raw(&self, mut req: Req) -> Result<Resp> {
        let t = self.token.token().await?;
        req.set_header("authorization", format!("Bearer {}", t.expose_secret()));
        let resp = Resp::read(req.send(&self.http).await?).await?;
        if !resp.ok() {
            let json = resp.json().unwrap_or(Json::Null);
            return Err(api_error(resp.status, &json));
        }
        Ok(resp)
    }

    /// Send and return the JSON envelope (checking `success`).
    pub(crate) async fn send(&self, req: Req) -> Result<Json> {
        let resp = self.send_raw(req).await?;
        let json = resp.json()?;
        if json.get("success").and_then(Json::as_bool) == Some(false) {
            return Err(api_error(resp.status, &json));
        }
        Ok(json)
    }

    /// The token's id, from `/user/tokens/verify` (user tokens) or
    /// `/accounts/<id>/tokens/verify` (account-owned tokens).
    pub async fn token_id(&self, account_id: &str) -> Result<String> {
        let user = self
            .send(self.req(Method::GET, "/user/tokens/verify"))
            .await;
        let json = match user {
            Ok(j) => j,
            Err(_) if !account_id.trim().is_empty() => {
                self.send(self.req(
                    Method::GET,
                    format!("/accounts/{}/tokens/verify", account_id.trim()),
                ))
                .await?
            }
            Err(e) => return Err(e),
        };
        json.pointer("/result/id")
            .and_then(Json::as_str)
            .map(str::to_owned)
            .ok_or_else(|| CloudError::Auth("Cloudflare did not return the token's id".into()))
    }
}

/// R2's S3 credentials for an API token: the token's id is the access key id and the
/// SHA-256 of the token is the secret (Cloudflare's documented derivation).
pub fn r2_credentials(token_id: &str, token: &SecretString) -> AwsCredentials {
    AwsCredentials {
        access_key_id: token_id.to_owned(),
        secret_access_key: SecretString::from(sha256_hex(token.expose_secret().as_bytes())),
        session_token: None,
        expires_ms: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derivation() {
        let c = r2_credentials("tid", &SecretString::from("abc".to_owned()));
        assert_eq!(c.access_key_id, "tid");
        assert_eq!(
            c.secret_access_key.expose_secret(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn errors() {
        let e = api_error(
            403,
            &serde_json::json!({"success":false,"errors":[{"code":10000,"message":"Authentication error"}]}),
        );
        assert!(e.to_string().contains("token was refused"));
    }
}
