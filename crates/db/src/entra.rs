//! Microsoft Entra ID (Azure AD) access tokens for Azure SQL.
//!
//! Protocol only: the OAuth 2.0 endpoints of the Microsoft identity platform over `reqwest`.
//! Core decides when to sign in, shows the prompts and keeps refresh tokens in the keychain.
//!
//! - Interactive: authorization code with PKCE. The system browser shows Microsoft's own
//!   sign-in page (MFA, conditional access, security keys) and redirects to a one-shot
//!   listener on `http://localhost:<port>`.
//! - Device code: the user enters a short code at microsoft.com/devicelogin on any device.
//! - Password (ROPC, no MFA) and client credentials (service principal) for automation.

use std::fmt;
use std::time::{Duration, Instant};

use data_encoding::BASE64URL_NOPAD;
use ring::digest::{SHA256, digest};
use ring::rand::{SecureRandom as _, SystemRandom};
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value as Json;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tracing::debug;

use crate::error::{DbError, Result};

/// Azure SQL's resource scope (the double slash is how Azure SQL's resource id is written).
pub const SQL_SCOPE: &str = "https://database.windows.net//.default";

/// Switchyard's multi-tenant public client, set at build time with
/// `SWITCHYARD_ENTRA_CLIENT_ID` (see `docs/entra-app.md`).
pub const BUILTIN_CLIENT_ID: Option<&str> = option_env!("SWITCHYARD_ENTRA_CLIENT_ID");

/// The Microsoft identity platform.
pub const LOGIN_BASE: &str = "https://login.microsoftonline.com";

/// Tenant used when a connection names none: any work or school account.
pub const ANY_ORGANIZATION: &str = "organizations";

const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Which application signs in, and to which directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntraApp {
    /// Directory id or domain (`contoso.onmicrosoft.com`), or `organizations`.
    pub tenant: String,
    /// Application (client) id.
    pub client_id: String,
}

impl EntraApp {
    /// The app for a connection: its own client id or Switchyard's, its tenant or any
    /// organization.
    pub fn resolve(tenant: Option<&str>, client_id: Option<&str>) -> Result<Self> {
        fn nonempty(s: Option<&str>) -> Option<&str> {
            s.map(str::trim).filter(|s| !s.is_empty())
        }
        let client_id = nonempty(client_id)
            .or(nonempty(BUILTIN_CLIENT_ID))
            .ok_or_else(|| {
                DbError::Unsupported(
                    "this build has no Microsoft Entra app registration; set an application \
                     (client) id in the connection's advanced settings"
                        .into(),
                )
            })?
            .to_owned();
        let tenant = nonempty(tenant).unwrap_or(ANY_ORGANIZATION).to_owned();
        Ok(Self { tenant, client_id })
    }
}

/// Tokens from one sign-in or refresh.
pub struct Token {
    /// Access token for Azure SQL.
    pub access: SecretString,
    /// Refresh token, when the flow issues one.
    pub refresh: Option<SecretString>,
    /// When the access token stops working.
    pub expires_at: Instant,
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Token")
            .field("refresh", &self.refresh.is_some())
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl Token {
    /// Whether the access token is still good for at least `margin`.
    pub fn fresh_for(&self, margin: Duration) -> bool {
        self.expires_at > Instant::now() + margin
    }
}

/// A device-code sign-in waiting for the user.
pub struct DeviceCode {
    device_code: SecretString,
    /// Code to type at [`DeviceCode::verification_uri`].
    pub user_code: String,
    /// Where to enter it (`https://microsoft.com/devicelogin`).
    pub verification_uri: String,
    /// Microsoft's instruction text.
    pub message: String,
    interval: Duration,
    expires_at: Instant,
}

impl fmt::Debug for DeviceCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceCode")
            .field("user_code", &self.user_code)
            .field("verification_uri", &self.verification_uri)
            .finish_non_exhaustive()
    }
}

/// An interactive sign-in: open [`Interactive::url`] in the browser, then
/// [`Entra::finish_interactive`].
pub struct Interactive {
    /// Microsoft's sign-in page for this attempt.
    pub url: String,
    listener: TcpListener,
    /// The same port on `::1`, when IPv6 loopback is available: browsers often resolve
    /// `localhost` to `::1` first.
    listener_v6: Option<TcpListener>,
    redirect_uri: String,
    verifier: SecretString,
    state: String,
}

impl fmt::Debug for Interactive {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Interactive")
            .field("redirect_uri", &self.redirect_uri)
            .finish_non_exhaustive()
    }
}

/// Client for the identity platform.
#[derive(Clone, Debug)]
pub struct Entra {
    http: reqwest::Client,
    base: String,
}

impl Entra {
    /// A client for `login.microsoftonline.com`.
    pub fn new() -> Result<Self> {
        Self::with_base(LOGIN_BASE)
    }

    /// A client for another identity endpoint (tests, sovereign clouds).
    pub fn with_base(base: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .tls_backend_preconfigured(crate::tls::client_config()?)
            .timeout(HTTP_TIMEOUT)
            .user_agent(format!("Switchyard/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| DbError::Connect(e.to_string()))?;
        Ok(Self {
            http,
            base: base.into().trim_end_matches('/').to_owned(),
        })
    }

    fn endpoint(&self, app: &EntraApp, path: &str) -> String {
        format!("{}/{}/oauth2/v2.0/{path}", self.base, app.tenant)
    }

    async fn post(&self, url: &str, form: &[(&str, &str)]) -> Result<(u16, Json)> {
        let resp = self
            .http
            .post(url)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(form_encode(form))
            .send()
            .await
            .map_err(|e| DbError::Connect(format!("Microsoft sign-in unreachable: {e}")))?;
        let status = resp.status().as_u16();
        let body = resp
            .bytes()
            .await
            .map_err(|e| DbError::Connect(e.to_string()))?;
        let json = serde_json::from_slice(&body).unwrap_or(Json::Null);
        Ok((status, json))
    }

    async fn token(&self, app: &EntraApp, form: &[(&str, &str)]) -> Result<Token> {
        let (status, json) = self.post(&self.endpoint(app, "token"), form).await?;
        if status != 200 {
            return Err(DbError::Connect(error_message(&json, status)));
        }
        parse_token(&json)
    }

    /// A new access token from a refresh token.
    pub async fn refresh(&self, app: &EntraApp, refresh: &SecretString) -> Result<Token> {
        self.token(
            app,
            &[
                ("client_id", &app.client_id),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh.expose_secret()),
                ("scope", &user_scope()),
            ],
        )
        .await
    }

    /// User name and password (fails for accounts that need MFA).
    pub async fn password(
        &self,
        app: &EntraApp,
        user: &str,
        password: &SecretString,
    ) -> Result<Token> {
        self.token(
            app,
            &[
                ("client_id", &app.client_id),
                ("grant_type", "password"),
                ("username", user),
                ("password", password.expose_secret()),
                ("scope", &user_scope()),
            ],
        )
        .await
    }

    /// Service principal: `app.client_id` with a client secret.
    pub async fn client_credentials(&self, app: &EntraApp, secret: &SecretString) -> Result<Token> {
        self.token(
            app,
            &[
                ("client_id", &app.client_id),
                ("grant_type", "client_credentials"),
                ("client_secret", secret.expose_secret()),
                ("scope", SQL_SCOPE),
            ],
        )
        .await
    }

    /// Start a device-code sign-in.
    pub async fn device_code(&self, app: &EntraApp) -> Result<DeviceCode> {
        let (status, json) = self
            .post(
                &self.endpoint(app, "devicecode"),
                &[("client_id", &app.client_id), ("scope", &user_scope())],
            )
            .await?;
        if status != 200 {
            return Err(DbError::Connect(error_message(&json, status)));
        }
        let text = |k: &str| json.get(k).and_then(Json::as_str).map(str::to_owned);
        let device_code = text("device_code")
            .ok_or_else(|| DbError::Protocol("device code response without device_code".into()))?;
        let user_code = text("user_code").unwrap_or_default();
        let verification_uri =
            text("verification_uri").unwrap_or_else(|| "https://microsoft.com/devicelogin".into());
        let message = text("message").unwrap_or_else(|| {
            format!("To sign in, open {verification_uri} and enter the code {user_code}.")
        });
        Ok(DeviceCode {
            device_code: SecretString::from(device_code),
            user_code,
            verification_uri,
            message,
            interval: Duration::from_secs(seconds(&json, "interval").unwrap_or(5).clamp(1, 30)),
            expires_at: Instant::now()
                + Duration::from_secs(seconds(&json, "expires_in").unwrap_or(900)),
        })
    }

    /// Wait until the user finishes the device-code sign-in (or it expires).
    pub async fn finish_device_code(&self, app: &EntraApp, dc: &DeviceCode) -> Result<Token> {
        let mut interval = dc.interval;
        loop {
            tokio::time::sleep(interval).await;
            if Instant::now() > dc.expires_at {
                return Err(DbError::Connect("the sign-in code expired".into()));
            }
            let (status, json) = self
                .post(
                    &self.endpoint(app, "token"),
                    &[
                        ("client_id", &app.client_id),
                        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                        ("device_code", dc.device_code.expose_secret()),
                    ],
                )
                .await?;
            if status == 200 {
                return parse_token(&json);
            }
            match json.get("error").and_then(Json::as_str) {
                Some("authorization_pending") => {}
                Some("slow_down") => interval += Duration::from_secs(5),
                Some("authorization_declined") => {
                    return Err(DbError::Connect("sign-in was declined".into()));
                }
                Some("expired_token") => {
                    return Err(DbError::Connect("the sign-in code expired".into()));
                }
                _ => return Err(DbError::Connect(error_message(&json, status))),
            }
        }
    }

    /// Start an interactive sign-in: listen for the redirect and build the sign-in URL.
    pub async fn begin_interactive(
        &self,
        app: &EntraApp,
        login_hint: Option<&str>,
    ) -> Result<Interactive> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| DbError::Connect(format!("could not listen for the sign-in: {e}")))?;
        let port = listener
            .local_addr()
            .map_err(|e| DbError::Connect(e.to_string()))?
            .port();
        // `localhost` is what the app registration allows with any port. Browsers may try
        // `::1` before `127.0.0.1`, so listen on both where the system has IPv6 loopback.
        let listener_v6 = TcpListener::bind(("::1", port)).await.ok();
        let redirect_uri = format!("http://localhost:{port}");
        let verifier = random_token(48)?;
        let state = random_token(16)?;
        let challenge = BASE64URL_NOPAD.encode(digest(&SHA256, verifier.as_bytes()).as_ref());
        let mut query = vec![
            ("client_id", app.client_id.as_str()),
            ("response_type", "code"),
            ("redirect_uri", redirect_uri.as_str()),
            ("response_mode", "query"),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("state", state.as_str()),
            ("prompt", "select_account"),
        ];
        let scope = user_scope();
        query.push(("scope", &scope));
        if let Some(hint) = login_hint.filter(|h| !h.trim().is_empty()) {
            query.push(("login_hint", hint));
        }
        let url = format!(
            "{}?{}",
            self.endpoint(app, "authorize"),
            form_encode(&query)
        );
        Ok(Interactive {
            url,
            listener,
            listener_v6,
            redirect_uri,
            verifier: SecretString::from(verifier),
            state,
        })
    }

    /// Wait for the browser to come back, then trade the code for tokens.
    pub async fn finish_interactive(&self, app: &EntraApp, flow: Interactive) -> Result<Token> {
        let code = loop {
            let accepted = match &flow.listener_v6 {
                Some(v6) => tokio::select! {
                    a = flow.listener.accept() => a,
                    a = v6.accept() => a,
                },
                None => flow.listener.accept().await,
            };
            let (mut sock, _) = accepted.map_err(|e| DbError::Connect(e.to_string()))?;
            let Some(target) = read_request_target(&mut sock).await else {
                continue;
            };
            let params = parse_query(&target);
            let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
            // Anything else (favicon, a stale tab) does not end the wait.
            if get("state") != Some(flow.state.as_str()) {
                let _ = respond(&mut sock, 404, "Not found").await;
                continue;
            }
            if let Some(err) = get("error") {
                let detail = get("error_description").unwrap_or(err);
                let _ = respond(&mut sock, 200, &page(false, detail)).await;
                return Err(DbError::Connect(first_line(detail)));
            }
            let Some(code) = get("code") else {
                let _ = respond(&mut sock, 400, "Missing code").await;
                continue;
            };
            let _ = respond(&mut sock, 200, &page(true, "")).await;
            break SecretString::from(code.to_owned());
        };
        debug!("entra authorization code received");
        self.token(
            app,
            &[
                ("client_id", &app.client_id),
                ("grant_type", "authorization_code"),
                ("code", code.expose_secret()),
                ("redirect_uri", &flow.redirect_uri),
                ("code_verifier", flow.verifier.expose_secret()),
                ("scope", &user_scope()),
            ],
        )
        .await
    }
}

/// Delegated scope: Azure SQL plus a refresh token.
fn user_scope() -> String {
    format!("{SQL_SCOPE} offline_access")
}

fn seconds(json: &Json, key: &str) -> Option<u64> {
    match json.get(key)? {
        Json::Number(n) => n.as_u64(),
        Json::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn parse_token(json: &Json) -> Result<Token> {
    let access = json
        .get("access_token")
        .and_then(Json::as_str)
        .ok_or_else(|| DbError::Protocol("token response without access_token".into()))?;
    let refresh = json
        .get("refresh_token")
        .and_then(Json::as_str)
        .map(|r| SecretString::from(r.to_owned()));
    Ok(Token {
        access: SecretString::from(access.to_owned()),
        refresh,
        expires_at: Instant::now()
            + Duration::from_secs(seconds(json, "expires_in").unwrap_or(3600)),
    })
}

/// `AADSTS50076: Due to a configuration change ... \r\nTrace ID: ...` → the first line.
fn error_message(json: &Json, status: u16) -> String {
    match json.get("error_description").and_then(Json::as_str) {
        Some(d) => first_line(d),
        None => match json.get("error").and_then(Json::as_str) {
            Some(e) => e.to_owned(),
            None => format!("Microsoft sign-in answered HTTP {status}"),
        },
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or(s).trim().to_owned()
}

fn random_token(bytes: usize) -> Result<String> {
    let mut buf = vec![0u8; bytes];
    SystemRandom::new()
        .fill(&mut buf)
        .map_err(|_| DbError::Protocol("no system random source".into()))?;
    Ok(BASE64URL_NOPAD.encode(&buf))
}

/// `application/x-www-form-urlencoded` (also used for the authorize query string).
fn form_encode(pairs: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        percent_encode(k, &mut out);
        out.push('=');
        percent_encode(v, &mut out);
    }
    out
}

fn percent_encode(s: &str, out: &mut String) {
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => {
                        out.push(v);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `/path?a=1&b=x%20y` → `[("a","1"),("b","x y")]`.
fn parse_query(target: &str) -> Vec<(String, String)> {
    let Some((_, q)) = target.split_once('?') else {
        return Vec::new();
    };
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

/// The request target of a `GET` from the browser, or None for anything unreadable.
async fn read_request_target(sock: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    let read = async {
        loop {
            let n = sock.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 16 * 1024 {
                return Some(());
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(10), read)
        .await
        .ok()??;
    let head = String::from_utf8_lossy(&buf);
    let line = head.lines().next()?;
    let mut parts = line.split_whitespace();
    (parts.next()? == "GET").then_some(())?;
    parts.next().map(str::to_owned)
}

async fn respond(sock: &mut tokio::net::TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let msg = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    sock.write_all(msg.as_bytes()).await?;
    sock.shutdown().await
}

fn page(ok: bool, detail: &str) -> String {
    let (title, text) = if ok {
        (
            "Signed in",
            "Switchyard is connecting. You can close this tab.".to_owned(),
        )
    } else {
        ("Sign-in failed", html_escape(&first_line(detail)))
    };
    format!(
        "<!doctype html><meta charset=utf-8><title>Switchyard · {title}</title>\
         <body style=\"font:15px system-ui;margin:15vh auto;max-width:28rem;color:#222\">\
         <h2>{title}</h2><p>{text}</p></body>"
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn encoding_round_trips() {
        let s = form_encode(&[(
            "scope",
            "https://database.windows.net//.default offline_access",
        )]);
        assert_eq!(
            s,
            "scope=https%3A%2F%2Fdatabase.windows.net%2F%2F.default%20offline_access"
        );
        let q = parse_query("/?code=a%2Bb+c&state=x&empty");
        assert_eq!(
            q,
            [
                ("code".into(), "a+b c".into()),
                ("state".into(), "x".into()),
                ("empty".into(), String::new())
            ]
        );
        assert_eq!(percent_decode("bad%zz%4"), "bad%zz%4");
    }

    #[test]
    fn resolve_prefers_the_connection_client_id() {
        let app = EntraApp::resolve(Some(" contoso.com "), Some("abc")).unwrap();
        assert_eq!(app.tenant, "contoso.com");
        assert_eq!(app.client_id, "abc");
        let app = EntraApp::resolve(None, Some("abc")).unwrap();
        assert_eq!(app.tenant, ANY_ORGANIZATION);
    }

    #[test]
    fn error_text_is_the_first_line() {
        let j = serde_json::json!({
            "error": "invalid_grant",
            "error_description": "AADSTS50076: MFA required.\r\nTrace ID: 1"
        });
        assert_eq!(error_message(&j, 400), "AADSTS50076: MFA required.");
    }

    /// A stand-in identity endpoint: answers each POST with the next canned response and
    /// records the form bodies.
    async fn mock(responses: Vec<(u16, Json)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            for (status, body) in responses {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = sock.read(&mut chunk).await.unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                        let len = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if rest.len() >= len {
                            log.lock()
                                .unwrap()
                                .push(format!("{} {rest}", head.lines().next().unwrap()));
                            break;
                        }
                    }
                }
                let body = body.to_string();
                let msg = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                sock.write_all(msg.as_bytes()).await.unwrap();
            }
        });
        (format!("http://{addr}"), seen)
    }

    fn app() -> EntraApp {
        EntraApp {
            tenant: "contoso.com".into(),
            client_id: "client-1".into(),
        }
    }

    #[tokio::test]
    async fn interactive_flow_checks_state_and_sends_the_pkce_verifier() {
        let (base, seen) = mock(vec![(
            200,
            serde_json::json!({"access_token": "AT", "refresh_token": "RT", "expires_in": 3599}),
        )])
        .await;
        let entra = Entra::with_base(&base).unwrap();
        let flow = entra
            .begin_interactive(&app(), Some("ana@contoso.com"))
            .await
            .unwrap();
        assert!(flow.url.starts_with(&format!(
            "{base}/contoso.com/oauth2/v2.0/authorize?client_id=client-1&response_type=code"
        )));
        let q = parse_query(&flow.url);
        let get = |k: &str| q.iter().find(|(n, _)| n == k).unwrap().1.clone();
        assert_eq!(get("login_hint"), "ana@contoso.com");
        let redirect = get("redirect_uri");
        let (state, challenge) = (get("state"), get("code_challenge"));
        let port: u16 = redirect.rsplit(':').next().unwrap().parse().unwrap();

        // The browser: a stray request first, then the real redirect over IPv6 loopback
        // when the system has it (browsers often resolve `localhost` to `::1` first).
        let real_host = if std::net::TcpListener::bind(("::1", 0)).is_ok() {
            "::1"
        } else {
            "127.0.0.1"
        };
        let browser = tokio::spawn(async move {
            let get = |path: String, host: &'static str| async move {
                let mut s = tokio::net::TcpStream::connect((host, port)).await.unwrap();
                s.write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
                    .await
                    .unwrap();
                let mut out = String::new();
                s.read_to_string(&mut out).await.unwrap();
                out
            };
            let stray = get("/favicon.ico".into(), "127.0.0.1").await;
            assert!(stray.starts_with("HTTP/1.1 404"));
            let page = get(format!("/?code=CODE%2F1&state={state}"), real_host).await;
            assert!(page.contains("You can close this tab"));
        });
        let token = entra.finish_interactive(&app(), flow).await.unwrap();
        browser.await.unwrap();
        assert_eq!(token.access.expose_secret(), "AT");
        assert_eq!(token.refresh.as_ref().unwrap().expose_secret(), "RT");
        assert!(token.fresh_for(Duration::from_secs(3000)));

        let posts = seen.lock().unwrap().clone();
        assert_eq!(posts.len(), 1);
        assert!(posts[0].starts_with("POST /contoso.com/oauth2/v2.0/token "));
        let form = parse_query(&format!(
            "?{}",
            posts[0]
                .split_once(' ')
                .unwrap()
                .1
                .split_once(' ')
                .unwrap()
                .1
        ));
        let field = |k: &str| form.iter().find(|(n, _)| n == k).unwrap().1.clone();
        assert_eq!(field("code"), "CODE/1");
        assert_eq!(field("grant_type"), "authorization_code");
        let verifier = field("code_verifier");
        assert_eq!(
            BASE64URL_NOPAD.encode(digest(&SHA256, verifier.as_bytes()).as_ref()),
            challenge
        );
    }

    #[tokio::test]
    async fn interactive_error_from_microsoft_is_reported() {
        let entra = Entra::with_base("http://127.0.0.1:9").unwrap();
        let flow = entra.begin_interactive(&app(), None).await.unwrap();
        let q = parse_query(&flow.url);
        let get = |k: &str| q.iter().find(|(n, _)| n == k).unwrap().1.clone();
        let (state, redirect) = (get("state"), get("redirect_uri"));
        let port: u16 = redirect.rsplit(':').next().unwrap().parse().unwrap();
        tokio::spawn(async move {
            let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            let path = format!(
                "/?error=access_denied&error_description=AADSTS65004%3A+User+declined.&state={state}"
            );
            s.write_all(format!("GET {path} HTTP/1.1\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut out = String::new();
            let _ = s.read_to_string(&mut out).await;
        });
        let err = entra.finish_interactive(&app(), flow).await.unwrap_err();
        assert!(
            err.to_string().contains("AADSTS65004: User declined."),
            "{err}"
        );
    }

    #[tokio::test]
    async fn device_code_polls_until_signed_in() {
        let (base, seen) = mock(vec![
            (
                200,
                serde_json::json!({
                    "device_code": "DC", "user_code": "ABCD-EFGH",
                    "verification_uri": "https://microsoft.com/devicelogin",
                    "expires_in": 900, "interval": 1,
                    "message": "To sign in, use a web browser to open the page https://microsoft.com/devicelogin and enter the code ABCD-EFGH to authenticate."
                }),
            ),
            (400, serde_json::json!({"error": "authorization_pending"})),
            (400, serde_json::json!({"error": "authorization_pending"})),
            (200, serde_json::json!({"access_token": "AT2", "expires_in": "3600"})),
        ])
        .await;
        let entra = Entra::with_base(&base).unwrap();
        let dc = entra.device_code(&app()).await.unwrap();
        assert_eq!(dc.user_code, "ABCD-EFGH");
        assert!(dc.message.contains("ABCD-EFGH"));
        let token = entra.finish_device_code(&app(), &dc).await.unwrap();
        assert_eq!(token.access.expose_secret(), "AT2");
        assert!(token.refresh.is_none());
        let posts = seen.lock().unwrap().clone();
        assert_eq!(posts.len(), 4);
        assert!(posts[0].starts_with("POST /contoso.com/oauth2/v2.0/devicecode "));
        assert!(posts[3].contains("device_code=DC"));
    }

    #[tokio::test]
    async fn refresh_failure_carries_the_aadsts_text() {
        let (base, seen) = mock(vec![(
            400,
            serde_json::json!({"error": "invalid_grant", "error_description": "AADSTS700082: The refresh token has expired.\r\nTrace ID: x"}),
        )])
        .await;
        let entra = Entra::with_base(&base).unwrap();
        let err = entra
            .refresh(&app(), &SecretString::from("old".to_owned()))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, DbError::Connect(m) if m == "AADSTS700082: The refresh token has expired."),
            "{err}"
        );
        assert!(seen.lock().unwrap()[0].contains("grant_type=refresh_token"));
    }

    #[tokio::test]
    async fn service_principal_uses_the_default_scope_only() {
        let (base, seen) = mock(vec![(
            200,
            serde_json::json!({"access_token": "SP", "expires_in": 3599}),
        )])
        .await;
        let entra = Entra::with_base(&base).unwrap();
        let t = entra
            .client_credentials(&app(), &SecretString::from("s3cret".to_owned()))
            .await
            .unwrap();
        assert_eq!(t.access.expose_secret(), "SP");
        let post = seen.lock().unwrap()[0].clone();
        assert!(post.contains("grant_type=client_credentials"));
        assert!(post.ends_with("scope=https%3A%2F%2Fdatabase.windows.net%2F%2F.default"));
    }
}
