//! HCP Terraform (Terraform Cloud) and Terraform Enterprise: a workspace's state versions
//! and their state files, read with an API token. Read-only.

use std::sync::Arc;

use reqwest::Method;
use secrecy::ExposeSecret as _;
use serde_json::Value as Json;

use crate::auth::TokenSource;
use crate::error::{CloudError, Result};
use crate::http::{Req, Resp, client};

/// HCP Terraform's host name.
pub const DEFAULT_HOST: &str = "app.terraform.io";

/// One state version of a workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TfcStateVersion {
    /// `sv-…`.
    pub id: String,
    /// The state's serial.
    pub serial: u64,
    /// When it was written, ms since the epoch.
    pub created_ms: Option<i64>,
    /// Where the state file is downloaded from.
    pub download_url: String,
}

/// A client for one HCP Terraform / Terraform Enterprise host.
pub struct TfCloud {
    base: url::Url,
    token: Arc<dyn TokenSource>,
    http: reqwest::Client,
}

fn api_error(status: u16, json: &Json) -> CloudError {
    let first = json.pointer("/errors/0");
    let msg = first
        .and_then(|e| e.get("detail").or_else(|| e.get("title")))
        .and_then(Json::as_str)
        .or_else(|| first.and_then(Json::as_str))
        .unwrap_or_default();
    let msg = match status {
        401 => "The API token was refused (401)".to_owned(),
        404 if msg.is_empty() || msg == "not found" => {
            "Not found (404): check the organization and workspace names, and that the token can read state".to_owned()
        }
        _ => msg.to_owned(),
    };
    CloudError::api(status, None, msg)
}

/// State versions in a `GET /state-versions` (or one `current-state-version`) reply.
pub fn parse_state_versions(json: &Json) -> Vec<TfcStateVersion> {
    let one = |d: &Json| -> Option<TfcStateVersion> {
        let a = d.get("attributes")?;
        Some(TfcStateVersion {
            id: d.get("id")?.as_str()?.to_owned(),
            serial: a.get("serial").and_then(Json::as_u64).unwrap_or(0),
            created_ms: a
                .get("created-at")
                .and_then(Json::as_str)
                .and_then(crate::time::parse_iso_ms),
            download_url: a
                .get("hosted-state-download-url")
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_owned(),
        })
    };
    match json.get("data") {
        Some(Json::Array(items)) => items.iter().filter_map(one).collect(),
        Some(d @ Json::Object(_)) => one(d).into_iter().collect(),
        _ => Vec::new(),
    }
}

impl TfCloud {
    /// A client for `host` (empty: HCP Terraform), signing in with `token`.
    pub fn new(host: &str, token: Arc<dyn TokenSource>) -> Result<Self> {
        let host = host.trim().trim_end_matches('/');
        let host = if host.is_empty() { DEFAULT_HOST } else { host };
        let base = if host.starts_with("http://") || host.starts_with("https://") {
            url::Url::parse(host)
        } else {
            url::Url::parse(&format!("https://{host}"))
        }
        .map_err(|e| CloudError::Invalid(format!("host: {e}")))?;
        Ok(Self {
            base,
            token,
            http: client()?,
        })
    }

    async fn get(&self, req: Req) -> Result<Json> {
        let mut req = req.header("content-type", "application/vnd.api+json");
        let t = self.token.token().await?;
        req.set_header("authorization", format!("Bearer {}", t.expose_secret()));
        let resp = Resp::read(req.send(&self.http).await?).await?;
        let json = resp.json().unwrap_or(Json::Null);
        if !resp.ok() {
            return Err(api_error(resp.status, &json));
        }
        Ok(json)
    }

    /// The workspace's state versions, newest first (up to 100).
    pub async fn state_versions(
        &self,
        organization: &str,
        workspace: &str,
    ) -> Result<Vec<TfcStateVersion>> {
        let req = Req::new(Method::GET, &self.base, "/api/v2/state-versions")
            .query("filter[workspace][name]", workspace.trim())
            .query("filter[organization][name]", organization.trim())
            .query("page[size]", "100");
        let mut out = parse_state_versions(&self.get(req).await?);
        out.sort_by(|a, b| b.serial.cmp(&a.serial));
        Ok(out)
    }

    /// The workspace's current state version.
    pub async fn current(&self, organization: &str, workspace: &str) -> Result<TfcStateVersion> {
        let ws = self
            .get(Req::new(
                Method::GET,
                &self.base,
                format!(
                    "/api/v2/organizations/{}/workspaces/{}",
                    crate::http::uri_encode(organization.trim(), false),
                    crate::http::uri_encode(workspace.trim(), false)
                ),
            ))
            .await?;
        let id = ws
            .pointer("/data/id")
            .and_then(Json::as_str)
            .ok_or_else(|| CloudError::Invalid("the workspace has no id".into()))?;
        let cur = self
            .get(Req::new(
                Method::GET,
                &self.base,
                format!("/api/v2/workspaces/{id}/current-state-version"),
            ))
            .await?;
        parse_state_versions(&cur)
            .into_iter()
            .next()
            .ok_or_else(|| CloudError::api(404, None, "The workspace has no state yet"))
    }

    /// Download a state file. The token goes only to the API's own host; other hosts
    /// (signed archive links) get the request without it.
    pub async fn download(&self, url: &str) -> Result<Vec<u8>> {
        let u = url::Url::parse(url).map_err(|e| CloudError::Invalid(e.to_string()))?;
        let mut b = self.http.get(u.clone());
        if u.host_str() == self.base.host_str() {
            let t = self.token.token().await?;
            b = b.bearer_auth(t.expose_secret());
        }
        let resp = b.send().await.map_err(|e| {
            CloudError::Network(format!(
                "{} unreachable: {}",
                u.host_str().unwrap_or("archive"),
                crate::http::root_cause(&e)
            ))
        })?;
        let resp = Resp::read(resp).await?;
        if !resp.ok() {
            return Err(CloudError::api(resp.status, None, ""));
        }
        Ok(resp.body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_version_list() {
        let json: Json = serde_json::from_str(
            r#"{"data":[
              {"id":"sv-new","type":"state-versions","attributes":{"serial":9,"created-at":"2026-10-01T10:00:00.000Z","hosted-state-download-url":"https://archivist.terraform.io/v1/object/abc"}},
              {"id":"sv-old","type":"state-versions","attributes":{"serial":7,"created-at":"2026-09-01T10:00:00.000Z","hosted-state-download-url":"https://archivist.terraform.io/v1/object/def"}}
            ],"links":{}}"#,
        )
        .unwrap();
        let v = parse_state_versions(&json);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].id, "sv-new");
        assert_eq!(v[0].serial, 9);
        assert!(v[0].created_ms.is_some());
        let one: Json = serde_json::from_str(
            r#"{"data":{"id":"sv-x","attributes":{"serial":3,"hosted-state-download-url":"u"}}}"#,
        )
        .unwrap();
        assert_eq!(parse_state_versions(&one)[0].serial, 3);
    }

    #[test]
    fn errors_name_the_problem() {
        let e = api_error(401, &Json::Null).to_string();
        assert!(e.contains("token"), "{e}");
        let e = api_error(
            404,
            &serde_json::json!({"errors":[{"status":"404","title":"not found"}]}),
        )
        .to_string();
        assert!(e.contains("workspace"), "{e}");
    }
}
