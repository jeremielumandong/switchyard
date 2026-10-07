//! Typed, transient transport for the API Workbench.
//!
//! The service transport lives in the GPUI-free core runtime and is
//! re-exported here; this module keeps only what needs the desktop process:
//! the UI method picker, the OpenAPI import resolver and the loopback PKCE
//! browser flow. It never persists request data.

use base64::Engine as _;
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::net::{IpAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use switchyard_api::{
    HttpMethod, ImportOrigin, ImportReferenceResolver, PreparedBody, PreparedRequest, RequestId,
    RequestSettings,
};

pub use switchyard_api::runtime::transport::{
    FileCapabilities, NativeWorkbenchTransport, OAuthTokenRequest, Response, ScriptScopes,
    WorkbenchTransport, exchange_oauth_token, response_from_snapshot, response_snapshot,
};
#[cfg(test)]
pub use switchyard_api::runtime::transport::{
    OAuthTokenResponse, OperationPhase, ScriptRequestView, ScriptResponseView, ScriptResult,
};

/// Reference resolver used by OpenAPI imports. HTTPS references traverse the
/// authenticated native outbound policy; file references are confined to the
/// explicitly selected document's directory.
pub struct ProtectedImportResolver {
    local_root: Option<PathBuf>,
}

impl ProtectedImportResolver {
    pub fn for_origin(origin: &ImportOrigin) -> Self {
        let local_root = url::Url::parse(&origin.uri)
            .ok()
            .filter(|url| url.scheme() == "file")
            .and_then(|url| url.to_file_path().ok())
            .and_then(|path| path.parent().map(Path::to_path_buf))
            .and_then(|path| std::fs::canonicalize(path).ok());
        Self { local_root }
    }
}

impl ImportReferenceResolver for ProtectedImportResolver {
    fn resolve(&self, canonical_uri: &str) -> Result<Vec<u8>, String> {
        let url = url::Url::parse(canonical_uri)
            .map_err(|error| format!("invalid import reference URI: {error}"))?;
        match url.scheme() {
            "https" => Ok(fetch_https_import(canonical_uri)?.into_bytes()),
            "file" => {
                let root = self.local_root.as_ref().ok_or_else(|| {
                    "file references require an explicitly selected local import".to_string()
                })?;
                let path = url
                    .to_file_path()
                    .map_err(|_| "invalid file reference URI".to_string())?;
                let canonical = std::fs::canonicalize(path)
                    .map_err(|error| format!("resolve import reference: {error}"))?;
                if !canonical.starts_with(root) {
                    return Err(
                        "file import reference escapes the selected document directory".into(),
                    );
                }
                let metadata = std::fs::metadata(&canonical)
                    .map_err(|error| format!("inspect import reference: {error}"))?;
                if !metadata.is_file() || metadata.len() > 5 * 1024 * 1024 {
                    return Err(
                        "import reference must be a regular file no larger than 5 MiB".into(),
                    );
                }
                std::fs::read(canonical).map_err(|error| format!("read import reference: {error}"))
            }
            _ => Err("import references must use file: or https:".into()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
    Trace,
    Custom,
}

impl Method {
    pub const ALL: [Self; 9] = [
        Self::Get,
        Self::Post,
        Self::Put,
        Self::Patch,
        Self::Delete,
        Self::Head,
        Self::Options,
        Self::Trace,
        Self::Custom,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Head => "HEAD",
            Self::Options => "OPTIONS",
            Self::Trace => "TRACE",
            Self::Custom => "CUSTOM",
        }
    }

    pub fn from_label(value: &str) -> Option<Self> {
        Self::ALL[..Self::ALL.len() - 1]
            .iter()
            .copied()
            .find(|method| method.label().eq_ignore_ascii_case(value.trim()))
    }

    pub fn forbids_body(self) -> bool {
        matches!(self, Self::Get | Self::Head)
    }
}

pub struct PkceAuthorization {
    pub authorization_url: String,
    listener: TcpListener,
    expected_state: String,
    redirect_path: String,
    code_verifier: String,
}

/// Bind the configured loopback callback before opening the provider. State
/// and PKCE material are UUID-v4 backed and live only in this transient flow.
pub fn begin_pkce_authorization(
    authorization_endpoint: &str,
    client_id: &str,
    scopes: &[String],
    redirect_uri: &str,
) -> Result<PkceAuthorization, String> {
    let redirect = url::Url::parse(redirect_uri)
        .map_err(|error| format!("invalid OAuth redirect URI: {error}"))?;
    if redirect.scheme() != "http" || redirect.username() != "" || redirect.password().is_some() {
        return Err("OAuth PKCE redirect URI must be a credential-free http loopback URL".into());
    }
    let host = redirect
        .host_str()
        .ok_or_else(|| "OAuth redirect URI needs a host".to_string())?;
    let ip = match host {
        "localhost" => IpAddr::from([127, 0, 0, 1]),
        value => value
            .parse::<IpAddr>()
            .map_err(|_| "OAuth redirect URI must use localhost or a loopback IP".to_string())?,
    };
    if !ip.is_loopback() {
        return Err("OAuth redirect URI must use a loopback address".into());
    }
    let port = redirect
        .port()
        .ok_or_else(|| "OAuth redirect URI needs an explicit callback port".to_string())?;
    let listener = TcpListener::bind((ip, port))
        .map_err(|error| format!("bind OAuth callback {redirect_uri}: {error}"))?;
    listener
        .set_nonblocking(false)
        .map_err(|error| format!("configure OAuth callback: {error}"))?;

    let random = || RequestId::new().to_string().replace('-', "");
    let expected_state = random();
    let code_verifier = format!("{}{}", random(), random());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(code_verifier.as_bytes()));
    let mut authorize = url::Url::parse(authorization_endpoint)
        .map_err(|error| format!("invalid OAuth authorization endpoint: {error}"))?;
    if !matches!(authorize.scheme(), "http" | "https")
        || authorize.username() != ""
        || authorize.password().is_some()
    {
        return Err("OAuth authorization endpoint must be credential-free HTTP(S)".into());
    }
    authorize
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("state", &expected_state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    if !scopes.is_empty() {
        authorize
            .query_pairs_mut()
            .append_pair("scope", &scopes.join(" "));
    }
    Ok(PkceAuthorization {
        authorization_url: authorize.into(),
        listener,
        expected_state,
        redirect_path: redirect.path().to_string(),
        code_verifier,
    })
}

impl PkceAuthorization {
    pub fn expected_state(&self) -> &str {
        &self.expected_state
    }

    pub fn wait_for_callback(self, timeout: Duration) -> Result<(String, String, String), String> {
        self.listener
            .set_nonblocking(true)
            .map_err(|error| format!("configure OAuth callback: {error}"))?;
        let deadline = Instant::now() + timeout;
        let (mut stream, _) = loop {
            match self.listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err("OAuth callback timed out".into());
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => return Err(format!("accept OAuth callback: {error}")),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .map_err(|error| format!("configure OAuth callback socket: {error}"))?;
        let mut bytes = [0_u8; 8192];
        let read = stream
            .read(&mut bytes)
            .map_err(|error| format!("read OAuth callback: {error}"))?;
        let request = std::str::from_utf8(&bytes[..read])
            .map_err(|_| "OAuth callback was not valid HTTP text".to_string())?;
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .ok_or_else(|| "OAuth callback request line is malformed".to_string())?;
        let callback = url::Url::parse(&format!("http://localhost{target}"))
            .map_err(|error| format!("invalid OAuth callback URL: {error}"))?;
        let mut query = callback
            .query_pairs()
            .into_owned()
            .collect::<BTreeMap<_, _>>();
        let callback_state = query.remove("state").unwrap_or_default();
        let result = if callback.path() != self.redirect_path {
            Err("OAuth callback path did not match the configured redirect URI".into())
        } else if callback_state != self.expected_state {
            Err("OAuth callback state did not match; authorization was refused".into())
        } else if let Some(error) = query.remove("error") {
            Err(format!("OAuth provider refused authorization: {error}"))
        } else {
            query
                .remove("code")
                .filter(|code| !code.is_empty())
                .map(|code| (code, self.code_verifier, callback_state))
                .ok_or_else(|| "OAuth callback did not contain an authorization code".into())
        };
        let (status, message) = if result.is_ok() {
            (
                "200 OK",
                "Authorization received. You can return to AgentOps.",
            )
        } else {
            (
                "400 Bad Request",
                "Authorization was refused. Return to AgentOps for details.",
            )
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{message}",
            message.len()
        );
        let _ = stream.write_all(response.as_bytes());
        result
    }
}

/// Load an HTTPS import through the authenticated service transport. This
/// deliberately inherits the service's DNS pinning and private-network policy
/// instead of allowing the desktop process to fetch arbitrary URLs directly.
pub fn fetch_https_import(url: &str) -> Result<String, String> {
    let parsed = url::Url::parse(url).map_err(|error| format!("invalid import URL: {error}"))?;
    if parsed.scheme() != "https" {
        return Err("remote imports must use HTTPS".into());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("remote import URLs cannot contain credentials".into());
    }
    let request = PreparedRequest {
        request_id: RequestId::new(),
        method: HttpMethod::get(),
        url: parsed.to_string(),
        headers: Vec::new(),
        body: PreparedBody::None,
        settings: RequestSettings::default(),
        aws_sigv4: None,
        redactions: Vec::new(),
    };
    let response = NativeWorkbenchTransport::from_env().send_prepared_with_limit(
        &request,
        &FileCapabilities::default(),
        5 * 1024 * 1024,
        None,
    )?;
    if !(200..300).contains(&response.status) {
        return Err(format!("remote import returned HTTP {}", response.status));
    }
    if response.truncated {
        return Err("remote import exceeds the 5 MiB response limit".into());
    }
    if response.binary {
        return Err("remote import must be UTF-8 text".into());
    }
    let final_url = url::Url::parse(&response.final_url)
        .map_err(|error| format!("invalid final import URL: {error}"))?;
    if final_url.scheme() != "https" {
        return Err("remote import redirected away from HTTPS".into());
    }
    Ok(response.body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_import_rejects_insecure_or_credentialed_urls_before_network_io() {
        assert_eq!(
            fetch_https_import("http://example.test/openapi.json").unwrap_err(),
            "remote imports must use HTTPS"
        );
        assert_eq!(
            fetch_https_import("https://user:secret@example.test/openapi.json").unwrap_err(),
            "remote import URLs cannot contain credentials"
        );
    }

    #[test]
    fn pkce_callback_requires_the_exact_transient_state() {
        let reserve = TcpListener::bind((IpAddr::from([127, 0, 0, 1]), 0)).unwrap();
        let port = reserve.local_addr().unwrap().port();
        drop(reserve);
        let redirect = format!("http://127.0.0.1:{port}/oauth/callback");
        let flow = begin_pkce_authorization(
            "https://identity.example.test/authorize",
            "native-client",
            &["read".into(), "write".into()],
            &redirect,
        )
        .unwrap();
        assert!(
            flow.authorization_url
                .contains("code_challenge_method=S256")
        );
        assert!(flow.authorization_url.contains("scope=read+write"));
        let state = flow.expected_state.clone();
        let sent_state = state.clone();
        let client = std::thread::spawn(move || {
            let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            write!(
                stream,
                "GET /oauth/callback?code=issued&state={sent_state} HTTP/1.1\r\nHost: localhost\r\n\r\n"
            )
            .unwrap();
        });
        let (code, verifier, callback_state) =
            flow.wait_for_callback(Duration::from_secs(2)).unwrap();
        client.join().unwrap();
        assert_eq!(code, "issued");
        assert_eq!(callback_state, state);
        assert!((43..=128).contains(&verifier.len()));
    }

    #[test]
    fn pkce_callback_rejects_state_substitution() {
        let reserve = TcpListener::bind((IpAddr::from([127, 0, 0, 1]), 0)).unwrap();
        let port = reserve.local_addr().unwrap().port();
        drop(reserve);
        let flow = begin_pkce_authorization(
            "https://identity.example.test/authorize",
            "native-client",
            &[],
            &format!("http://127.0.0.1:{port}/callback"),
        )
        .unwrap();
        let client = std::thread::spawn(move || {
            let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            write!(
                stream,
                "GET /callback?code=stolen&state=attacker HTTP/1.1\r\nHost: localhost\r\n\r\n"
            )
            .unwrap();
        });
        assert!(
            flow.wait_for_callback(Duration::from_secs(2))
                .unwrap_err()
                .contains("state did not match")
        );
        client.join().unwrap();
    }
}
