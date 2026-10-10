//! HTTP plumbing shared by the services: one client per service, encoding, hashing, and
//! requests whose exact bytes are known before sending (so they can be signed).

use std::sync::Arc;
use std::time::Duration;

use data_encoding::{BASE64, HEXLOWER};
use ring::{digest, hmac};

use crate::error::{CloudError, Result};

/// Timeout for one request's headers and small bodies; streams set their own pace.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const READ_TIMEOUT: Duration = Duration::from_secs(120);

/// An HTTPS client verifying servers against the system roots (and `SSL_CERT_FILE`).
pub(crate) fn client() -> Result<reqwest::Client> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
    let tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| CloudError::Network(e.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .user_agent(format!("Switchyard/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| CloudError::Network(e.to_string()))
}

/// Percent-encode everything but RFC 3986 unreserved characters (and `/` when kept).
pub(crate) fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `k=v&k2=v2` with both sides encoded, in the given order.
pub(crate) fn query_string(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", uri_encode(k, false), uri_encode(v, false)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Hex SHA-256.
pub(crate) fn sha256_hex(data: &[u8]) -> String {
    HEXLOWER.encode(digest::digest(&digest::SHA256, data).as_ref())
}

/// Base64 SHA-256.
pub(crate) fn sha256_b64(data: &[u8]) -> String {
    BASE64.encode(digest::digest(&digest::SHA256, data).as_ref())
}

/// HMAC-SHA256.
pub(crate) fn hmac256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let k = hmac::Key::new(hmac::HMAC_SHA256, key);
    hmac::sign(&k, data).as_ref().to_vec()
}

/// A request built up front, so its signature covers exactly what is sent.
#[derive(Debug)]
pub(crate) struct Req {
    pub(crate) method: reqwest::Method,
    /// Scheme, host, port and base path (no query).
    pub(crate) base: url::Url,
    /// Encoded path appended to `base`'s path (starts with `/` or is empty).
    pub(crate) path: String,
    /// Query parameters, unencoded.
    pub(crate) query: Vec<(String, String)>,
    /// Header names are lower case.
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl Req {
    pub(crate) fn new(method: reqwest::Method, base: &url::Url, path: impl Into<String>) -> Self {
        Self {
            method,
            base: base.clone(),
            path: path.into(),
            query: Vec::new(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub(crate) fn query(mut self, k: &str, v: impl Into<String>) -> Self {
        self.query.push((k.to_owned(), v.into()));
        self
    }

    pub(crate) fn header(mut self, k: &str, v: impl Into<String>) -> Self {
        self.set_header(k, v);
        self
    }

    pub(crate) fn set_header(&mut self, k: &str, v: impl Into<String>) {
        let k = k.to_ascii_lowercase();
        self.headers.retain(|(n, _)| *n != k);
        self.headers.push((k, v.into()));
    }

    pub(crate) fn body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    /// The full encoded path: the base's path plus [`Req::path`].
    pub(crate) fn full_path(&self) -> String {
        let base = self.base.path().trim_end_matches('/');
        let p = format!("{base}{}", self.path);
        if p.is_empty() { "/".into() } else { p }
    }

    /// `host` or `host:port` as sent in the Host header.
    pub(crate) fn host(&self) -> String {
        let host = self.base.host_str().unwrap_or_default();
        match self.base.port() {
            Some(p) => format!("{host}:{p}"),
            None => host.to_owned(),
        }
    }

    pub(crate) fn url(&self) -> Result<url::Url> {
        let mut u = self.base.clone();
        u.set_path(&self.full_path());
        let q = query_string(&self.query);
        u.set_query((!q.is_empty()).then_some(q.as_str()));
        Ok(u)
    }

    /// Send and return the response, whatever its status.
    pub(crate) async fn send(self, client: &reqwest::Client) -> Result<reqwest::Response> {
        let url = self.url()?;
        let mut b = client.request(self.method.clone(), url);
        for (k, v) in &self.headers {
            if k == "host" {
                continue;
            }
            b = b.header(k.as_str(), v.as_str());
        }
        if !self.body.is_empty()
            || matches!(self.method, reqwest::Method::PUT | reqwest::Method::POST)
        {
            b = b.body(self.body);
        }
        b.send().await.map_err(|e| {
            CloudError::Network(format!(
                "{} unreachable: {}",
                self.base.host_str().unwrap_or("service"),
                root_cause(&e)
            ))
        })
    }
}

/// The innermost error's message (reqwest wraps hyper wraps io).
pub(crate) fn root_cause(e: &(dyn std::error::Error + 'static)) -> String {
    let mut cur = e;
    while let Some(next) = cur.source() {
        cur = next;
    }
    cur.to_string()
}

/// A finished response: status, headers and body.
pub(crate) struct Resp {
    pub(crate) status: u16,
    pub(crate) headers: reqwest::header::HeaderMap,
    pub(crate) body: Vec<u8>,
}

impl Resp {
    pub(crate) async fn read(r: reqwest::Response) -> Result<Self> {
        let status = r.status().as_u16();
        let headers = r.headers().clone();
        let body = r
            .bytes()
            .await
            .map_err(|e| CloudError::Network(root_cause(&e)))?
            .to_vec();
        Ok(Self {
            status,
            headers,
            body,
        })
    }

    pub(crate) fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    pub(crate) fn json(&self) -> Result<serde_json::Value> {
        if self.body.is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_slice(&self.body)
            .map_err(|e| CloudError::Network(format!("unreadable reply: {e}")))
    }
}
