//! Local mock servers: a collection's saved examples served over HTTP on
//! `127.0.0.1`, so a front end or a test suite can run against an API that
//! does not exist yet (or is down, or costs money per call).
//!
//! Each saved request with at least one example becomes a [`MockRoute`]: its
//! method and the path of its URL template, where `:id`, `{id}` and `{{id}}`
//! segments match anything. A call picks the most specific route, then an
//! example the way Postman's mock servers do: the `x-mock-response-name` or
//! `x-mock-response-code` header when sent, else the first 2xx example, else
//! the first one. Every answer carries permissive CORS headers and preflights
//! succeed, so browser apps can call the mock directly.
//!
//! Two switches make it more than a canned-answer server ([`MockOptions`]):
//! *chaos* adds latency and a share of injected 500s to matched calls, so a
//! client's timeouts, spinners and retries can be exercised on purpose; and
//! *record* forwards calls no example matches to a real upstream, answers with
//! what it returned and keeps the exchange ([`MockRecording`]) so it can be
//! saved as a new request and example: the mock grows by being used.
//!
//! The server is a deliberately small HTTP/1.1 responder (one request per
//! connection, `Connection: close`) on the caller's tokio runtime.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::{
    CollectionId, Example, ExampleId, HttpMethod, RequestId, ResponseSnapshot, SavedRequest,
};

/// Hits kept in [`MockServer::hits`].
const MAX_HITS: usize = 200;
/// Longest request head the server reads.
const MAX_HEAD_BYTES: usize = 64 * 1024;
/// Longest request body the server reads (and discards).
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
/// How long a client may take to send its request.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Response headers the server sets itself instead of copying from an example.
const HOP_HEADERS: [&str; 6] = [
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "content-encoding",
    "access-control-allow-origin",
];

/// One segment of a route's path.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Segment {
    Literal(String),
    Param,
}

/// A saved example the mock can answer with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MockResponse {
    pub name: String,
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// A saved request served by the mock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MockRoute {
    pub request_id: RequestId,
    pub request_name: String,
    pub method: String,
    /// The path as shown to people: `/users/:id`.
    pub path: String,
    segments: Vec<Segment>,
    pub responses: Vec<MockResponse>,
}

impl MockRoute {
    fn literals(&self) -> usize {
        self.segments
            .iter()
            .filter(|segment| matches!(segment, Segment::Literal(_)))
            .count()
    }

    fn matches_path(&self, path: &[&str]) -> bool {
        self.segments.len() == path.len()
            && self
                .segments
                .iter()
                .zip(path)
                .all(|(segment, part)| match segment {
                    Segment::Param => true,
                    Segment::Literal(literal) => literal == part,
                })
    }
}

/// The path of a request URL template, without scheme, host, a leading
/// `{{base_url}}`-style variable, query or fragment: `/users/:id`.
pub fn url_path_template(url: &str) -> String {
    let url = url.trim();
    let url = url.split(['?', '#']).next().unwrap_or_default();
    let rest = if url.starts_with("{{") {
        match url.find("}}") {
            Some(end) => &url[end + 2..],
            None => url,
        }
    } else if let Some(scheme_end) = url.find("://") {
        let after = &url[scheme_end + 3..];
        after.find('/').map(|slash| &after[slash..]).unwrap_or("")
    } else {
        url
    };
    let parts: Vec<&str> = rest.split('/').filter(|part| !part.is_empty()).collect();
    format!("/{}", parts.join("/"))
}

fn segments(path: &str) -> Vec<Segment> {
    path.split('/')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let param = part.starts_with(':')
                || part.contains("{{")
                || (part.starts_with('{') && part.ends_with('}'));
            if param {
                Segment::Param
            } else {
                Segment::Literal(part.to_string())
            }
        })
        .collect()
}

fn mock_response(example: &Example) -> MockResponse {
    MockResponse {
        name: example.name.clone(),
        status: if example.response.status == 0 {
            200
        } else {
            example.response.status
        },
        reason: example.response.reason.clone(),
        headers: example
            .response
            .headers
            .iter()
            .filter(|(name, _)| !HOP_HEADERS.contains(&name.to_ascii_lowercase().as_str()))
            .cloned()
            .collect(),
        body: base64::engine::general_purpose::STANDARD
            .decode(example.response.body_base64.as_bytes())
            .unwrap_or_default(),
    }
}

/// The routes `requests` and their `examples` make, most specific first.
/// Requests without an example are not served.
pub fn mock_routes(requests: &[SavedRequest], examples: &[Example]) -> Vec<MockRoute> {
    let mut routes: Vec<MockRoute> = requests
        .iter()
        .filter_map(|request| {
            let mut own: Vec<&Example> = examples
                .iter()
                .filter(|example| example.request_id == request.id)
                .collect();
            if own.is_empty() {
                return None;
            }
            own.sort_by_key(|example| example.sort_key);
            let path = url_path_template(&request.url);
            Some(MockRoute {
                request_id: request.id.clone(),
                request_name: request.name.clone(),
                method: request.method.as_str().to_ascii_uppercase(),
                segments: segments(&path),
                path,
                responses: own.into_iter().map(mock_response).collect(),
            })
        })
        .collect();
    routes.sort_by(|a, b| {
        b.literals()
            .cmp(&a.literals())
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.method.cmp(&b.method))
    });
    routes
}

/// What a call to the mock resolved to.
#[derive(Debug, PartialEq, Eq)]
pub enum MockMatch<'a> {
    Found {
        route: &'a MockRoute,
        response: &'a MockResponse,
    },
    /// The path is served, but not for this method.
    MethodNotAllowed,
    NotFound,
}

/// Resolve `method path` (query allowed) against `routes`, honouring the
/// `x-mock-response-name` / `x-mock-response-code` headers.
pub fn match_request<'a>(
    routes: &'a [MockRoute],
    method: &str,
    path: &str,
    headers: &[(String, String)],
) -> MockMatch<'a> {
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    let mut path_matched = false;
    for route in routes.iter().filter(|route| route.matches_path(&parts)) {
        path_matched = true;
        if !route.method.eq_ignore_ascii_case(method)
            && !(method.eq_ignore_ascii_case("HEAD") && route.method == "GET")
        {
            continue;
        }
        if let Some(response) = pick_response(route, headers) {
            return MockMatch::Found { route, response };
        }
    }
    if path_matched {
        MockMatch::MethodNotAllowed
    } else {
        MockMatch::NotFound
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim())
}

fn pick_response<'a>(
    route: &'a MockRoute,
    headers: &[(String, String)],
) -> Option<&'a MockResponse> {
    if let Some(name) = header(headers, "x-mock-response-name")
        && let Some(found) = route
            .responses
            .iter()
            .find(|response| response.name.eq_ignore_ascii_case(name))
    {
        return Some(found);
    }
    if let Some(code) = header(headers, "x-mock-response-code").and_then(|v| v.parse::<u16>().ok())
        && let Some(found) = route.responses.iter().find(|r| r.status == code)
    {
        return Some(found);
    }
    route
        .responses
        .iter()
        .find(|response| (200..300).contains(&response.status))
        .or_else(|| route.responses.first())
}

/// Saved requests and examples for the recorded calls no existing route
/// serves, one request per method and path (the first recording wins), filed
/// in `collection` with URLs under `upstream`.
pub fn recordings_to_requests(
    recordings: &[MockRecording],
    collection: &CollectionId,
    upstream: &str,
    existing: &[MockRoute],
) -> (Vec<SavedRequest>, Vec<Example>) {
    let mut requests: Vec<SavedRequest> = Vec::new();
    let mut examples = Vec::new();
    let base = upstream.trim_end_matches('/');
    for recording in recordings {
        let parts: Vec<&str> = recording
            .path
            .split('/')
            .filter(|p| !p.is_empty())
            .collect();
        let served = existing
            .iter()
            .any(|route| route.method == recording.method && route.matches_path(&parts));
        let duplicate = requests.iter().any(|request| {
            request.method.as_str() == recording.method
                && url_path_template(&request.url) == recording.path
        });
        if served || duplicate {
            continue;
        }
        let Ok(method) = HttpMethod::new(recording.method.clone()) else {
            continue;
        };
        let request = SavedRequest {
            id: RequestId::new(),
            collection_id: collection.clone(),
            folder_id: None,
            name: format!("{} {}", recording.method, recording.path),
            method,
            url: format!("{base}{}", recording.path),
            params: Vec::new(),
            headers: Vec::new(),
            auth: Default::default(),
            body: Default::default(),
            variables: Vec::new(),
            scripts: Default::default(),
            settings: Default::default(),
            extensions: Default::default(),
            sort_key: 0,
        };
        examples.push(Example {
            id: ExampleId::new(),
            request_id: request.id.clone(),
            name: format!("Recorded {}", recording.status),
            request: None,
            response: ResponseSnapshot {
                status: recording.status,
                reason: recording.reason.clone(),
                headers: recording.headers.clone(),
                body_base64: base64::engine::general_purpose::STANDARD.encode(&recording.body),
                received_bytes: recording.body.len() as u64,
                stored_bytes: recording.body.len() as u64,
                ..ResponseSnapshot::default()
            },
            extensions: Default::default(),
            sort_key: 0,
        });
        requests.push(request);
    }
    (requests, examples)
}

/// One call the mock answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MockHit {
    /// Unix milliseconds.
    pub at_ms: i64,
    pub method: String,
    pub path: String,
    pub status: u16,
    /// The example that answered; `None` for 404 / 405 / preflight.
    pub example: Option<String>,
}

/// How a mock server behaves beyond answering with examples.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MockOptions {
    /// `127.0.0.1:port`; `0` picks a free port.
    pub port: u16,
    /// Extra delay before each answer, uniform between the two bounds (ms).
    pub latency_ms: (u64, u64),
    /// Share of matched calls answered with an injected 500 (0–100).
    pub failure_percent: f64,
    /// Calls no example matches go here (`https://api.example.com/v1`) and
    /// are recorded; `None` answers them 404 / 405.
    pub upstream: Option<String>,
}

impl MockOptions {
    /// Why these options cannot start a server, if they cannot.
    pub fn validate(&self) -> Result<(), String> {
        if self.latency_ms.0 > self.latency_ms.1 {
            return Err("The minimum latency is above the maximum.".into());
        }
        if self.latency_ms.1 > MAX_LATENCY_MS {
            return Err(format!("Latency is capped at {MAX_LATENCY_MS} ms."));
        }
        if !(0.0..=100.0).contains(&self.failure_percent) {
            return Err("The failure rate must be between 0 and 100%.".into());
        }
        if let Some(upstream) = &self.upstream {
            let url = url::Url::parse(upstream)
                .map_err(|_| format!("Record upstream {upstream:?} is not a URL."))?;
            if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                return Err("The record upstream must be an http(s) URL.".into());
            }
        }
        Ok(())
    }
}

/// Most latency chaos can add to one answer.
pub const MAX_LATENCY_MS: u64 = 60_000;
/// Recordings kept per server.
const MAX_RECORDINGS: usize = 500;
/// Response headers never kept in a recording.
const UNRECORDED_HEADERS: [&str; 2] = ["set-cookie", "date"];

/// A call forwarded to the upstream and what it answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MockRecording {
    pub method: String,
    /// The path without query, as called: `/users/42`.
    pub path: String,
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

struct Shared {
    routes: Vec<MockRoute>,
    options: MockOptions,
    upstream: Option<reqwest::Client>,
    hits: Mutex<VecDeque<MockHit>>,
    recordings: Mutex<Vec<MockRecording>>,
    total: AtomicU64,
}

/// A running mock server. Dropping it stops the server.
pub struct MockServer {
    address: SocketAddr,
    shared: Arc<Shared>,
    task: tokio::task::JoinHandle<()>,
}

impl MockServer {
    /// Serve `routes` on `127.0.0.1:<options.port>` on `runtime`.
    pub fn start(
        runtime: &tokio::runtime::Handle,
        routes: Vec<MockRoute>,
        options: MockOptions,
    ) -> Result<Self, String> {
        options.validate()?;
        let port = options.port;
        let upstream = match &options.upstream {
            Some(_) => Some(
                reqwest::Client::builder()
                    .tls_backend_preconfigured(crate::tls::client_config()?)
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(Duration::from_secs(60))
                    .build()
                    .map_err(|error| format!("Could not prepare recording: {error}"))?,
            ),
            None => None,
        };
        let listener = std::net::TcpListener::bind(("127.0.0.1", port)).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AddrInUse {
                format!("Port {port} is already in use.")
            } else {
                format!("Could not listen on 127.0.0.1:{port}: {error}")
            }
        })?;
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("Could not start the mock server: {error}"))?;
        let address = listener
            .local_addr()
            .map_err(|error| format!("Could not start the mock server: {error}"))?;
        let listener = {
            let _entered = runtime.enter();
            TcpListener::from_std(listener)
                .map_err(|error| format!("Could not start the mock server: {error}"))?
        };
        let shared = Arc::new(Shared {
            routes,
            options,
            upstream,
            hits: Mutex::new(VecDeque::new()),
            recordings: Mutex::new(Vec::new()),
            total: AtomicU64::new(0),
        });
        let task = runtime.spawn(accept_loop(listener, shared.clone()));
        tracing::info!(%address, "API mock server started");
        Ok(Self {
            address,
            shared,
            task,
        })
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// `http://127.0.0.1:<port>`.
    pub fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    pub fn routes(&self) -> &[MockRoute] {
        &self.shared.routes
    }

    /// Recent calls, newest first.
    pub fn hits(&self) -> Vec<MockHit> {
        lock(&self.shared.hits).iter().rev().cloned().collect()
    }

    pub fn options(&self) -> &MockOptions {
        &self.shared.options
    }

    /// Calls forwarded to the upstream so far, oldest first.
    pub fn recordings(&self) -> Vec<MockRecording> {
        lock(&self.shared.recordings).clone()
    }

    /// Calls answered since the server started.
    pub fn total_hits(&self) -> u64 {
        self.shared.total.load(Ordering::Relaxed)
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.task.abort();
        tracing::info!(address = %self.address, "API mock server stopped");
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

async fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    // Connections live in a set owned by this task, so stopping the server
    // (aborting this task) also ends every connection it is serving.
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let shared = shared.clone();
                    connections.spawn(async move {
                        if let Err(error) = serve(stream, &shared).await {
                            tracing::debug!(%error, "mock connection ended");
                        }
                    });
                }
                Err(error) => {
                    tracing::warn!(%error, "mock server accept failed");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}

/// A parsed request head.
struct Head {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn parse_head(head: &[u8]) -> Option<Head> {
    let text = std::str::from_utf8(head).ok()?;
    let mut lines = text.split("\r\n");
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_string();
    let target = request_line.next()?.to_string();
    let headers = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_string(), value.trim().to_string()))
        })
        .collect();
    Some(Head {
        method,
        target,
        headers,
        body: Vec::new(),
    })
}

fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

async fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<Head>> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(end) = find_head_end(&buffer) {
            break end;
        }
        if buffer.len() > MAX_HEAD_BYTES {
            return Ok(None);
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let Some(mut head) = parse_head(&buffer[..head_end]) else {
        return Ok(None);
    };
    // Read the body too: closing a socket with unread data sends a reset,
    // which can destroy the response before the client reads it.
    let mut body = buffer.split_off(head_end + 4);
    let chunked = header(&head.headers, "transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"));
    let length = header(&head.headers, "content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0)
        .min(MAX_BODY_BYTES);
    loop {
        let done = if chunked {
            body.ends_with(b"0\r\n\r\n")
        } else {
            body.len() >= length
        };
        if done || body.len() > MAX_BODY_BYTES {
            break;
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    head.body = if chunked {
        dechunk(&body)
    } else {
        body.truncate(length);
        body
    };
    Ok(Some(head))
}

/// The payload of a chunked body (extensions and trailers dropped).
fn dechunk(mut raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(line_end) = raw.windows(2).position(|w| w == b"\r\n") {
        let size = std::str::from_utf8(&raw[..line_end])
            .ok()
            .and_then(|line| usize::from_str_radix(line.split(';').next()?.trim(), 16).ok());
        let Some(size) = size.filter(|size| *size > 0) else {
            break;
        };
        let start = line_end + 2;
        let Some(chunk) = raw.get(start..start + size) else {
            break;
        };
        out.extend_from_slice(chunk);
        raw = raw.get(start + size + 2..).unwrap_or_default();
    }
    out
}

/// A number in `0..bound` that is good enough for chaos.
fn random_below(bound: u64) -> u64 {
    if bound == 0 {
        return 0;
    }
    (uuid::Uuid::new_v4().as_u128() % u128::from(bound)) as u64
}

/// Answer a call no example matched from the upstream, and keep it.
async fn forward(shared: &Shared, head: &Head) -> Option<Answer> {
    let (client, base) = (shared.upstream.as_ref()?, shared.options.upstream.as_ref()?);
    let url = format!("{}{}", base.trim_end_matches('/'), head.target);
    let method = reqwest::Method::from_bytes(head.method.as_bytes()).ok()?;
    let mut request = client.request(method, &url);
    for (name, value) in &head.headers {
        let skip = matches!(
            name.to_ascii_lowercase().as_str(),
            "host"
                | "content-length"
                | "connection"
                | "transfer-encoding"
                | "accept-encoding"
                | "keep-alive"
                | "x-mock-response-name"
                | "x-mock-response-code"
        );
        if !skip {
            request = request.header(name, value);
        }
    }
    if !head.body.is_empty() {
        request = request.body(head.body.clone());
    }
    let answer = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            return Some(json_answer(
                502,
                "Bad Gateway",
                serde_json::json!({ "error": format!("Recording upstream failed: {error}") }),
            ));
        }
    };
    let status = answer.status();
    let headers: Vec<(String, String)> = answer
        .headers()
        .iter()
        .filter_map(|(name, value)| Some((name.to_string(), value.to_str().ok()?.to_string())))
        .filter(|(name, _)| !HOP_HEADERS.contains(&name.to_ascii_lowercase().as_str()))
        .collect();
    let body = answer.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
    let reason = status.canonical_reason().unwrap_or("Status").to_string();
    {
        let mut recordings = lock(&shared.recordings);
        if recordings.len() < MAX_RECORDINGS {
            recordings.push(MockRecording {
                method: head.method.to_ascii_uppercase(),
                path: url_path_template(&head.target),
                status: status.as_u16(),
                reason: reason.clone(),
                headers: headers
                    .iter()
                    .filter(|(name, _)| {
                        !UNRECORDED_HEADERS.contains(&name.to_ascii_lowercase().as_str())
                    })
                    .cloned()
                    .collect(),
                body: body.clone(),
            });
        }
    }
    let mut headers = headers;
    headers.push(("X-Switchyard-Mock".into(), "recorded".into()));
    Some((
        status.as_u16(),
        reason,
        headers,
        body,
        Some(RECORDED.into()),
    ))
}

/// [`MockHit::example`] for a call answered by the upstream.
pub const RECORDED: &str = "recorded from upstream";
/// [`MockHit::example`] for an injected failure.
pub const INJECTED_FAILURE: &str = "chaos: injected 500";

async fn answer(shared: &Shared, head: &Head) -> Answer {
    let preflight = head.method.eq_ignore_ascii_case("OPTIONS")
        && header(&head.headers, "access-control-request-method").is_some();
    let matched = !preflight
        && matches!(
            match_request(&shared.routes, &head.method, &head.target, &head.headers),
            MockMatch::Found { .. }
        );
    if !preflight
        && !matched
        && let Some(answer) = forward(shared, head).await
    {
        return answer;
    }
    if matched {
        let (min, max) = shared.options.latency_ms;
        if max > 0 {
            let delay = min + random_below(max - min + 1);
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        let failure = shared.options.failure_percent;
        if failure > 0.0 && (random_below(10_000) as f64) < failure * 100.0 {
            let mut answer = json_answer(
                500,
                "Internal Server Error",
                serde_json::json!({ "error": "Injected failure (Switchyard mock chaos)" }),
            );
            answer.4 = Some(INJECTED_FAILURE.into());
            return answer;
        }
    }
    respond(&shared.routes, head)
}

async fn serve(mut stream: TcpStream, shared: &Shared) -> std::io::Result<()> {
    let Ok(head) = tokio::time::timeout(READ_TIMEOUT, read_request(&mut stream)).await else {
        return Ok(());
    };
    let Some(head) = head? else {
        return Ok(());
    };
    let (status, reason, headers, body, example) = answer(shared, &head).await;
    let mut out = format!("HTTP/1.1 {status} {reason}\r\n");
    for (name, value) in &headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str(&format!(
        "Content-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Expose-Headers: *\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    stream.write_all(out.as_bytes()).await?;
    if !head.method.eq_ignore_ascii_case("HEAD") {
        stream.write_all(&body).await?;
    }
    stream.flush().await?;
    let _ = stream.shutdown().await;
    record_hit(shared, &head, status, example);
    Ok(())
}

type Answer = (u16, String, Vec<(String, String)>, Vec<u8>, Option<String>);

fn json_answer(status: u16, reason: &str, body: serde_json::Value) -> Answer {
    (
        status,
        reason.into(),
        vec![("Content-Type".into(), "application/json".into())],
        body.to_string().into_bytes(),
        None,
    )
}

fn respond(routes: &[MockRoute], head: &Head) -> Answer {
    if head.method.eq_ignore_ascii_case("OPTIONS")
        && header(&head.headers, "access-control-request-method").is_some()
    {
        let allow_headers = header(&head.headers, "access-control-request-headers")
            .unwrap_or("*")
            .to_string();
        return (
            204,
            "No Content".into(),
            vec![
                (
                    "Access-Control-Allow-Methods".into(),
                    "GET, POST, PUT, PATCH, DELETE, HEAD, OPTIONS".into(),
                ),
                ("Access-Control-Allow-Headers".into(), allow_headers),
                ("Access-Control-Max-Age".into(), "600".into()),
            ],
            Vec::new(),
            None,
        );
    }
    let path = head.target.split(['?', '#']).next().unwrap_or("/");
    match match_request(routes, &head.method, &head.target, &head.headers) {
        MockMatch::Found { response, .. } => {
            let mut headers = response.headers.clone();
            headers.push(("X-Switchyard-Mock".into(), response.name.clone()));
            let reason = if response.reason.is_empty() {
                default_reason(response.status).to_string()
            } else {
                response.reason.clone()
            };
            (
                response.status,
                reason,
                headers,
                response.body.clone(),
                Some(response.name.clone()),
            )
        }
        MockMatch::MethodNotAllowed => json_answer(
            405,
            "Method Not Allowed",
            serde_json::json!({
                "error": format!("{} {path} has no saved example for this method", head.method),
                "allowed": routes
                    .iter()
                    .filter(|route| route.matches_path(&path.split('/').filter(|p| !p.is_empty()).collect::<Vec<_>>()))
                    .map(|route| route.method.clone())
                    .collect::<Vec<_>>(),
            }),
        ),
        MockMatch::NotFound => json_answer(
            404,
            "Not Found",
            serde_json::json!({
                "error": format!("No saved example matches {} {path}", head.method),
                "routes": routes
                    .iter()
                    .take(50)
                    .map(|route| format!("{} {}", route.method, route.path))
                    .collect::<Vec<_>>(),
            }),
        ),
    }
}

fn default_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

fn record_hit(shared: &Shared, head: &Head, status: u16, example: Option<String>) {
    shared.total.fetch_add(1, Ordering::Relaxed);
    let mut hits = lock(&shared.hits);
    if hits.len() >= MAX_HITS {
        hits.pop_front();
    }
    hits.push_back(MockHit {
        at_ms: chrono::Utc::now().timestamp_millis(),
        method: head.method.clone(),
        path: head.target.clone(),
        status,
        example,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, url: &str) -> SavedRequest {
        SavedRequest {
            id: RequestId::new(),
            collection_id: CollectionId::new(),
            folder_id: None,
            name: format!("{method} {url}"),
            method: HttpMethod::new(method).unwrap(),
            url: url.into(),
            params: Vec::new(),
            headers: Vec::new(),
            auth: Default::default(),
            body: Default::default(),
            variables: Vec::new(),
            scripts: Default::default(),
            settings: Default::default(),
            extensions: Default::default(),
            sort_key: 0,
        }
    }

    fn example(request: &SavedRequest, name: &str, status: u16, body: &str) -> Example {
        Example {
            id: ExampleId::new(),
            request_id: request.id.clone(),
            name: name.into(),
            request: None,
            response: ResponseSnapshot {
                status,
                headers: vec![
                    ("Content-Type".into(), "application/json".into()),
                    ("Content-Length".into(), "999".into()),
                ],
                body_base64: base64::engine::general_purpose::STANDARD.encode(body),
                ..ResponseSnapshot::default()
            },
            extensions: Default::default(),
            sort_key: 0,
        }
    }

    #[test]
    fn path_templates_drop_hosts_variables_and_queries() {
        assert_eq!(url_path_template("{{baseUrl}}/users/:id?x=1"), "/users/:id");
        assert_eq!(
            url_path_template("https://api.example.com/v1/users/{{id}}"),
            "/v1/users/{{id}}"
        );
        assert_eq!(url_path_template("https://api.example.com"), "/");
        assert_eq!(url_path_template("/health/"), "/health");
        assert_eq!(url_path_template("{{base}}"), "/");
    }

    #[test]
    fn the_most_specific_route_and_method_win() {
        let by_id = request("GET", "{{base}}/users/:id");
        let me = request("GET", "{{base}}/users/me");
        let create = request("POST", "{{base}}/users");
        let examples = vec![
            example(&by_id, "One user", 200, r#"{"id":7}"#),
            example(&me, "Me", 200, r#"{"id":"me"}"#),
            example(&create, "Created", 201, "{}"),
        ];
        let routes = mock_routes(
            &[by_id, me, create, request("GET", "/no-example")],
            &examples,
        );
        assert_eq!(routes.len(), 3, "requests without examples are not served");
        let name = |m: MockMatch<'_>| match m {
            MockMatch::Found { response, .. } => response.name.clone(),
            other => format!("{other:?}"),
        };
        assert_eq!(name(match_request(&routes, "GET", "/users/me", &[])), "Me");
        assert_eq!(
            name(match_request(&routes, "GET", "/users/42?expand=1", &[])),
            "One user"
        );
        assert_eq!(
            name(match_request(&routes, "HEAD", "/users/42", &[])),
            "One user"
        );
        assert_eq!(
            name(match_request(&routes, "post", "/users", &[])),
            "Created"
        );
        assert_eq!(
            match_request(&routes, "DELETE", "/users/42", &[]),
            MockMatch::MethodNotAllowed
        );
        assert_eq!(
            match_request(&routes, "GET", "/orders", &[]),
            MockMatch::NotFound
        );
    }

    #[test]
    fn headers_pick_an_example_and_success_is_the_default() {
        let get = request("GET", "/items");
        let examples = vec![
            example(&get, "Server error", 500, "{}"),
            example(&get, "Empty list", 200, "[]"),
            example(&get, "Not allowed", 403, "{}"),
        ];
        let routes = mock_routes(std::slice::from_ref(&get), &examples);
        let pick = |headers: &[(&str, &str)]| {
            let headers: Vec<(String, String)> = headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            match match_request(&routes, "GET", "/items", &headers) {
                MockMatch::Found { response, .. } => response.name.clone(),
                other => format!("{other:?}"),
            }
        };
        assert_eq!(pick(&[]), "Empty list");
        assert_eq!(pick(&[("X-Mock-Response-Code", "403")]), "Not allowed");
        assert_eq!(
            pick(&[("x-mock-response-name", "server error")]),
            "Server error"
        );
        assert_eq!(pick(&[("x-mock-response-name", "missing")]), "Empty list");
        assert!(
            routes[0].responses[0]
                .headers
                .iter()
                .all(|(name, _)| !name.eq_ignore_ascii_case("content-length")),
            "the server sets Content-Length itself"
        );
    }

    async fn call(address: SocketAddr, raw: &str) -> String {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream.write_all(raw.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        stream.read_to_end(&mut out).await.unwrap();
        String::from_utf8(out).unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_server_answers_with_examples_and_records_hits() {
        let get = request("GET", "{{base}}/users/:id");
        let post = request("POST", "{{base}}/users");
        let examples = vec![
            example(&get, "One user", 200, r#"{"id":7}"#),
            example(&post, "Created", 201, r#"{"id":8}"#),
        ];
        let routes = mock_routes(&[get, post], &examples);
        let server = MockServer::start(
            &tokio::runtime::Handle::current(),
            routes,
            MockOptions::default(),
        )
        .unwrap();
        let address = server.address();

        let found = call(address, "GET /users/7 HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(found.starts_with("HTTP/1.1 200 OK\r\n"), "{found}");
        assert!(found.contains("Content-Length: 8\r\n"), "{found}");
        assert!(found.contains("X-Switchyard-Mock: One user\r\n"));
        assert!(found.contains("Access-Control-Allow-Origin: *\r\n"));
        assert!(found.ends_with(r#"{"id":7}"#));

        let body = r#"{"name":"new"}"#;
        let created = call(
            address,
            &format!(
                "POST /users HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        )
        .await;
        assert!(created.starts_with("HTTP/1.1 201 Created\r\n"), "{created}");

        let preflight = call(
            address,
            "OPTIONS /users HTTP/1.1\r\nOrigin: http://localhost:3000\r\nAccess-Control-Request-Method: POST\r\nAccess-Control-Request-Headers: content-type\r\n\r\n",
        )
        .await;
        assert!(preflight.starts_with("HTTP/1.1 204"), "{preflight}");
        assert!(preflight.contains("Access-Control-Allow-Headers: content-type\r\n"));

        let missing = call(address, "GET /nope HTTP/1.1\r\n\r\n").await;
        assert!(missing.starts_with("HTTP/1.1 404"), "{missing}");
        assert!(missing.contains("GET /users/:id"), "404 lists the routes");

        assert_eq!(server.total_hits(), 4);
        let hits = server.hits();
        assert_eq!(hits[0].path, "/nope");
        assert_eq!(hits[3].example.as_deref(), Some("One user"));

        drop(server);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            TcpStream::connect(address).await.is_err(),
            "dropping the server stops listening"
        );
    }

    #[test]
    fn a_taken_port_is_reported_plainly() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let first =
            MockServer::start(runtime.handle(), Vec::new(), MockOptions::default()).unwrap();
        let error = match MockServer::start(
            runtime.handle(),
            Vec::new(),
            MockOptions {
                port: first.address().port(),
                ..MockOptions::default()
            },
        ) {
            Ok(_) => panic!("the port is taken"),
            Err(error) => error,
        };
        assert!(error.contains("already in use"), "{error}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn chaos_delays_and_fails_matched_calls() {
        let get = request("GET", "/items");
        let routes = mock_routes(
            std::slice::from_ref(&get),
            &[example(&get, "List", 200, "[]")],
        );
        let server = MockServer::start(
            &tokio::runtime::Handle::current(),
            routes,
            MockOptions {
                latency_ms: (120, 120),
                failure_percent: 100.0,
                ..MockOptions::default()
            },
        )
        .unwrap();
        let started = std::time::Instant::now();
        let answer = call(server.address(), "GET /items HTTP/1.1\r\n\r\n").await;
        assert!(started.elapsed() >= Duration::from_millis(120));
        assert!(answer.starts_with("HTTP/1.1 500"), "{answer}");
        assert_eq!(server.hits()[0].example.as_deref(), Some(INJECTED_FAILURE));
        let missing = call(server.address(), "GET /other HTTP/1.1\r\n\r\n").await;
        assert!(
            missing.starts_with("HTTP/1.1 404"),
            "chaos only touches matched calls"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn record_mode_forwards_misses_and_keeps_them() {
        let handle = tokio::runtime::Handle::current();
        let get = request("GET", "/users/:id");
        let upstream = MockServer::start(
            &handle,
            mock_routes(
                std::slice::from_ref(&get),
                &[example(&get, "User", 200, r#"{"id":5}"#)],
            ),
            MockOptions::default(),
        )
        .unwrap();
        let health = request("GET", "/health");
        let local_routes = mock_routes(
            std::slice::from_ref(&health),
            &[example(&health, "Up", 200, "ok")],
        );
        let recorder = MockServer::start(
            &handle,
            local_routes.clone(),
            MockOptions {
                upstream: Some(format!("{}/", upstream.url())),
                ..MockOptions::default()
            },
        )
        .unwrap();
        let local = call(recorder.address(), "GET /health HTTP/1.1\r\n\r\n").await;
        assert!(local.ends_with("ok"), "matched calls stay local: {local}");
        let forwarded = call(recorder.address(), "GET /users/5?full=1 HTTP/1.1\r\n\r\n").await;
        assert!(forwarded.starts_with("HTTP/1.1 200"), "{forwarded}");
        assert!(forwarded.contains("X-Switchyard-Mock: recorded"));
        assert!(forwarded.ends_with(r#"{"id":5}"#));
        let recordings = recorder.recordings();
        assert_eq!(recordings.len(), 1);
        assert_eq!(recordings[0].path, "/users/5");
        assert_eq!(upstream.total_hits(), 1);

        let collection = CollectionId::new();
        let (requests, examples) = recordings_to_requests(
            &[recordings[0].clone(), recordings[0].clone()],
            &collection,
            "https://api.example.test/",
            &local_routes,
        );
        assert_eq!(requests.len(), 1, "one request per method and path");
        assert_eq!(requests[0].url, "https://api.example.test/users/5");
        assert_eq!(examples[0].request_id, requests[0].id);
        assert_eq!(examples[0].response.status, 200);
    }

    #[test]
    fn options_are_checked_before_listening() {
        let bad = [
            MockOptions {
                latency_ms: (10, 5),
                ..MockOptions::default()
            },
            MockOptions {
                failure_percent: 101.0,
                ..MockOptions::default()
            },
            MockOptions {
                upstream: Some("ftp://x".into()),
                ..MockOptions::default()
            },
            MockOptions {
                upstream: Some("not a url".into()),
                ..MockOptions::default()
            },
        ];
        for options in bad {
            assert!(options.validate().is_err(), "{options:?}");
        }
        assert_eq!(dechunk(b"3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n"), b"abcde");
    }
}
