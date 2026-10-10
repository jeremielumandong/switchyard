//! AWS credentials: access keys, or a named profile from `~/.aws/config` and
//! `~/.aws/credentials` (static keys, `credential_process`, and IAM Identity Center (SSO) /
//! assumed roles through the AWS CLI's `aws configure export-credentials`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use secrecy::SecretString;
use serde_json::Value as Json;
use tracing::debug;

use crate::auth::{run_cli, run_shell};
use crate::error::{CloudError, Result};
use crate::http::{Req, Resp};
use crate::sigv4::{AwsCredentials, sign};

/// Refresh temporary credentials this long before they expire.
const EXPIRY_MARGIN_MS: i64 = 5 * 60 * 1000;

/// Where credentials come from.
#[derive(Clone, Debug)]
pub enum AwsSource {
    /// Saved access keys.
    Keys(AwsCredentials),
    /// A profile in the AWS CLI's config files.
    Profile(String),
}

/// Credentials for one connection, refreshed when temporary ones expire.
pub struct AwsAuth {
    source: AwsSource,
    cache: tokio::sync::Mutex<Option<AwsCredentials>>,
}

impl AwsAuth {
    /// From a source.
    pub fn new(source: AwsSource) -> Self {
        Self {
            source,
            cache: tokio::sync::Mutex::new(None),
        }
    }

    /// Current credentials.
    pub async fn credentials(&self) -> Result<AwsCredentials> {
        let profile = match &self.source {
            AwsSource::Keys(c) => return Ok(c.clone()),
            AwsSource::Profile(p) => p,
        };
        let mut cache = self.cache.lock().await;
        if let Some(c) = cache.as_ref()
            && c.expires_ms.is_none_or(|e| e > now_ms() + EXPIRY_MARGIN_MS)
        {
            return Ok(c.clone());
        }
        let c = profile_credentials(profile).await?;
        *cache = Some(c.clone());
        Ok(c)
    }

    /// Forget cached credentials (after the service refused them).
    pub async fn reset(&self) {
        *self.cache.lock().await = None;
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn config_path() -> Option<PathBuf> {
    std::env::var_os("AWS_CONFIG_FILE")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".aws").join("config")))
}

fn credentials_path() -> Option<PathBuf> {
    std::env::var_os("AWS_SHARED_CREDENTIALS_FILE")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".aws").join("credentials")))
}

/// INI sections: name → keys. `config` names are `profile x` (except `default`).
pub(crate) fn parse_ini(text: &str) -> HashMap<String, HashMap<String, String>> {
    let mut out: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut section: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let name = name.trim().to_owned();
            out.entry(name.clone()).or_default();
            section = Some(name);
            continue;
        }
        if let (Some(s), Some((k, v))) = (&section, line.split_once('=')) {
            out.entry(s.clone())
                .or_default()
                .insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
        }
    }
    out
}

/// Profiles named in the AWS CLI's config and credentials files, `default` first.
pub fn profile_names() -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for (path, is_config) in [(config_path(), true), (credentials_path(), false)] {
        let Some(text) = path.and_then(|p| std::fs::read_to_string(p).ok()) else {
            continue;
        };
        for name in parse_ini(&text).into_keys() {
            let name = if is_config {
                match name.strip_prefix("profile ") {
                    Some(n) => n.trim().to_owned(),
                    None if name == "default" => name,
                    None => continue,
                }
            } else {
                name
            };
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names.sort_by_key(|n| (n != "default", n.clone()));
    names
}

/// One profile's settings, credentials file first (it wins for keys).
fn profile_settings(name: &str) -> HashMap<String, String> {
    let mut merged = HashMap::new();
    if let Some(text) = config_path().and_then(|p| std::fs::read_to_string(p).ok()) {
        let ini = parse_ini(&text);
        let key = if name == "default" {
            "default".to_owned()
        } else {
            format!("profile {name}")
        };
        if let Some(s) = ini.get(&key).or_else(|| ini.get(name)) {
            merged.extend(s.clone());
        }
    }
    if let Some(text) = credentials_path().and_then(|p| std::fs::read_to_string(p).ok())
        && let Some(s) = parse_ini(&text).get(name)
    {
        merged.extend(s.clone());
    }
    merged
}

/// The profile's default region, if it names one.
pub fn profile_region(name: &str) -> Option<String> {
    profile_settings(name)
        .get("region")
        .filter(|r| !r.is_empty())
        .cloned()
}

/// Credentials in the `credential_process` JSON format.
pub(crate) fn parse_process_output(out: &[u8]) -> Result<AwsCredentials> {
    let json: Json = serde_json::from_slice(out)
        .map_err(|_| CloudError::Auth("the credential process printed no JSON".into()))?;
    let s = |k: &str| json.get(k).and_then(Json::as_str).map(str::to_owned);
    let (Some(id), Some(secret)) = (s("AccessKeyId"), s("SecretAccessKey")) else {
        return Err(CloudError::Auth(
            "the credential process printed no AccessKeyId / SecretAccessKey".into(),
        ));
    };
    Ok(AwsCredentials {
        access_key_id: id,
        secret_access_key: SecretString::from(secret),
        session_token: s("SessionToken").map(SecretString::from),
        expires_ms: s("Expiration").and_then(|e| crate::time::parse_iso_ms(&e)),
    })
}

async fn profile_credentials(name: &str) -> Result<AwsCredentials> {
    let name = if name.trim().is_empty() {
        "default"
    } else {
        name.trim()
    };
    let s = profile_settings(name);
    if let (Some(id), Some(secret)) = (s.get("aws_access_key_id"), s.get("aws_secret_access_key")) {
        return Ok(AwsCredentials {
            access_key_id: id.clone(),
            secret_access_key: SecretString::from(secret.clone()),
            session_token: s.get("aws_session_token").cloned().map(SecretString::from),
            expires_ms: None,
        });
    }
    if let Some(line) = s.get("credential_process") {
        debug!(profile = name, "aws credential_process");
        return parse_process_output(&run_shell(line).await?);
    }
    // SSO, assumed roles, MFA caches: the AWS CLI v2 resolves them all.
    debug!(profile = name, "aws configure export-credentials");
    let out = run_cli(
        "aws",
        &[
            "configure",
            "export-credentials",
            "--profile",
            name,
            "--format",
            "process",
        ],
    )
    .await
    .map_err(|e| {
        let sso = s.contains_key("sso_session") || s.contains_key("sso_start_url");
        let hint = if sso {
            format!(" Sign in with `aws sso login --profile {name}` and try again.")
        } else if s.is_empty() {
            format!(" No profile named \"{name}\" in ~/.aws/config or ~/.aws/credentials.")
        } else {
            String::new()
        };
        CloudError::Auth(format!("AWS profile {name}: {e}.{hint}"))
    })?;
    parse_process_output(&out)
}

/// A client for an AWS JSON 1.1 API (Secrets Manager, Systems Manager).
pub(crate) struct AwsJson {
    http: reqwest::Client,
    auth: Arc<AwsAuth>,
    region: String,
    service: &'static str,
    target: &'static str,
    endpoint: url::Url,
    read_only: bool,
}

impl AwsJson {
    /// `service` signs (`secretsmanager`, `ssm`); `target` prefixes actions
    /// (`secretsmanager`, `AmazonSSM`); `endpoint` overrides `https://<service>.<region>…`.
    pub(crate) fn new(
        auth: Arc<AwsAuth>,
        region: &str,
        service: &'static str,
        target: &'static str,
        endpoint: Option<url::Url>,
        read_only: bool,
    ) -> Result<Self> {
        let region = if region.trim().is_empty() {
            "us-east-1".to_owned()
        } else {
            region.trim().to_owned()
        };
        let endpoint = match endpoint {
            Some(e) => e,
            None => url::Url::parse(&format!("https://{service}.{region}.amazonaws.com"))
                .map_err(|e| CloudError::Invalid(e.to_string()))?,
        };
        Ok(Self {
            http: crate::http::client()?,
            auth,
            region,
            service,
            target,
            endpoint,
            read_only,
        })
    }

    /// Call `action` with a JSON body.
    pub(crate) async fn call(&self, action: &str, body: Json) -> Result<Json> {
        let writes = !(action.starts_with("Get")
            || action.starts_with("List")
            || action.starts_with("Describe")
            || action.starts_with("BatchGet"));
        if self.read_only && writes {
            return Err(CloudError::ReadOnly);
        }
        let mut req = Req::new(reqwest::Method::POST, &self.endpoint, "/")
            .header("content-type", "application/x-amz-json-1.1")
            .header("x-amz-target", format!("{}.{action}", self.target))
            .body(serde_json::to_vec(&body).map_err(|e| CloudError::Invalid(e.to_string()))?);
        let creds = self.auth.credentials().await?;
        sign(
            &mut req,
            &creds,
            &self.region,
            self.service,
            SystemTime::now(),
            false,
        );
        let resp = Resp::read(req.send(&self.http).await?).await?;
        let json = resp.json().unwrap_or(Json::Null);
        if resp.ok() {
            return Ok(json);
        }
        let code = json
            .get("__type")
            .and_then(Json::as_str)
            .map(|t| t.rsplit('#').next().unwrap_or(t).to_owned());
        if resp.status == 403 || code.as_deref() == Some("ExpiredTokenException") {
            self.auth.reset().await;
        }
        let status = match code.as_deref() {
            Some(c) if c.contains("NotFound") => 404,
            _ => resp.status,
        };
        Err(CloudError::api(
            status,
            code,
            crate::kv::json_message(&json).unwrap_or_default(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ini() {
        let ini = parse_ini(
            "# c\n[default]\nregion = eu-west-1\n\n[profile dev]\nsso_session = corp\n\
             credential_process = /bin/creds --x\n",
        );
        assert_eq!(ini["default"]["region"], "eu-west-1");
        assert_eq!(ini["profile dev"]["credential_process"], "/bin/creds --x");
    }

    #[test]
    fn process_output() {
        let c = parse_process_output(
            br#"{"Version":1,"AccessKeyId":"AKIA1","SecretAccessKey":"topsecret","SessionToken":"t","Expiration":"2030-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(c.access_key_id, "AKIA1");
        assert!(c.session_token.is_some());
        assert_eq!(c.expires_ms, Some(1_893_456_000_000));
        assert!(parse_process_output(b"nope").is_err());
        // Debug never shows the secret.
        assert!(!format!("{c:?}").contains("topsecret"));
    }
}
