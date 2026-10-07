//! The in-process stand-in for the AgentOps agent-service endpoints the API workbench
//! used over loopback HTTP (`/api/api-workbench/{send,cancel,script,oauth/token}`).
//!
//! [`crate::runtime::transport`] still speaks the same JSON wire format (it is what its
//! tests pin), but [`dispatch`] answers it here: native sends ([`crate::http`]), sandboxed
//! scripts ([`crate::script`]), OAuth token exchanges and cancellation of in-flight
//! operations. Ported from AgentOps's `httpd.rs` (MIT, same owner).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::oneshot;

const MAX_API_WORKBENCH_REQUEST_ID_BYTES: usize = 128;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Operations in flight, shared by every send, script and cancel.
struct State {
    active: Arc<Mutex<HashMap<String, ActiveWorkbenchEntry>>>,
    generation: AtomicU64,
}

fn state() -> &'static State {
    static STATE: OnceLock<State> = OnceLock::new();
    STATE.get_or_init(|| State {
        active: Arc::default(),
        generation: AtomicU64::new(1),
    })
}

fn error(message: &str) -> Value {
    serde_json::json!({"success": false, "error": message})
}

fn send_error(
    message: &str,
    response_version: u32,
    request_id: Option<&str>,
    operation_id: Option<&str>,
) -> Value {
    if response_version != 2 {
        return error(message);
    }
    serde_json::json!({
        "success": false,
        "version": 2,
        "requestId": request_id,
        "operationId": operation_id,
        "error": message,
    })
}

fn cancelled(
    response_version: Option<u32>,
    request_id: Option<&str>,
    operation_id: Option<&str>,
    what: &str,
) -> Value {
    let mut v = serde_json::json!({
        "success": false,
        "requestId": request_id,
        "operationId": operation_id,
        "error": format!("{what} cancelled"),
    });
    if let Some(version) = response_version {
        v["version"] = Value::from(version);
    }
    v
}

/// Answer one transport call: `path` is the AgentOps endpoint, `body` its JSON request.
/// Runs the async work on `runtime`; call it from a blocking thread, never from inside
/// the runtime's async tasks.
pub fn dispatch(runtime: &tokio::runtime::Handle, path: &str, body: &str) -> Value {
    match path {
        "/api/api-workbench/send" => runtime.block_on(send(body.as_bytes())),
        "/api/api-workbench/cancel" => cancel(body.as_bytes()),
        "/api/api-workbench/script" => runtime.block_on(script(body.as_bytes())),
        "/api/api-workbench/oauth/token" => runtime.block_on(oauth(body.as_bytes())),
        other => error(&format!("unknown API workspace operation {other}")),
    }
}

struct ActiveWorkbenchRequest {
    operation_id: String,
    generation: u64,
    requests: Arc<Mutex<HashMap<String, ActiveWorkbenchEntry>>>,
    final_phase: bool,
    completed: bool,
}

struct ActiveWorkbenchEntry {
    generation: u64,
    abort_handle: Option<tokio::task::AbortHandle>,
    cancelled: bool,
    completed: bool,
    touched_at: Instant,
}

const WORKBENCH_OPERATION_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_WORKBENCH_OPERATIONS: usize = 1024;

#[allow(clippy::large_enum_variant)]
enum WorkbenchTaskResult {
    Completed(Result<crate::http::ApiWorkbenchResponse, String>),
    Cancelled,
}

#[allow(clippy::large_enum_variant)]
enum WorkbenchScriptTaskResult {
    Completed(Result<crate::script::ScriptExecutionResult, String>),
    Cancelled,
}

#[derive(Clone, Copy)]
enum WorkbenchPhaseOutcome {
    Succeeded,
    Failed,
    Cancelled,
}

impl ActiveWorkbenchRequest {
    /// Marks this phase inactive and reports whether cancellation won before
    /// the phase result became authoritative.
    fn complete(mut self, outcome: WorkbenchPhaseOutcome) -> bool {
        let mut requests = lock(&self.requests);
        let cancelled = requests
            .get(&self.operation_id)
            .is_some_and(|entry| entry.generation == self.generation && entry.cancelled);
        if requests
            .get(&self.operation_id)
            .is_some_and(|entry| entry.generation == self.generation)
            && let Some(entry) = requests.get_mut(&self.operation_id)
        {
            entry.abort_handle = None;
            match outcome {
                WorkbenchPhaseOutcome::Cancelled => entry.cancelled = true,
                WorkbenchPhaseOutcome::Failed => entry.completed = !entry.cancelled,
                WorkbenchPhaseOutcome::Succeeded => {
                    entry.completed = self.final_phase && !entry.cancelled;
                }
            }
            entry.touched_at = Instant::now();
        }
        self.completed = true;
        cancelled
    }
}

impl Drop for ActiveWorkbenchRequest {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let mut requests = lock(&self.requests);
        if let Some(entry) = requests
            .get_mut(&self.operation_id)
            .filter(|entry| entry.generation == self.generation)
        {
            entry.cancelled = true;
            entry.touched_at = Instant::now();
            if let Some(abort) = entry.abort_handle.take() {
                abort.abort();
            }
        }
    }
}

fn register_workbench_phase(
    requests: &Arc<Mutex<HashMap<String, ActiveWorkbenchEntry>>>,
    operation_id: &str,
    generation: u64,
    abort_handle: tokio::task::AbortHandle,
    starts_operation: bool,
    final_phase: bool,
) -> Result<ActiveWorkbenchRequest, &'static str> {
    let now = Instant::now();
    let mut active = lock(requests);
    active.retain(|_, entry| {
        entry.abort_handle.is_some()
            || now.saturating_duration_since(entry.touched_at) < WORKBENCH_OPERATION_TTL
    });
    match active.get_mut(operation_id) {
        Some(entry) if entry.cancelled => return Err("operation is cancelled"),
        Some(entry) if entry.completed => return Err("operation is completed"),
        Some(entry) if entry.abort_handle.is_some() => return Err("operation phase is active"),
        Some(entry) => {
            entry.generation = generation;
            entry.abort_handle = Some(abort_handle);
            entry.touched_at = now;
        }
        None if starts_operation => {
            if active.len() >= MAX_WORKBENCH_OPERATIONS {
                return Err("too many Workbench operations are active");
            }
            active.insert(
                operation_id.to_string(),
                ActiveWorkbenchEntry {
                    generation,
                    abort_handle: Some(abort_handle),
                    cancelled: false,
                    completed: false,
                    touched_at: now,
                },
            );
        }
        None => return Err("operation has not been started"),
    }
    Ok(ActiveWorkbenchRequest {
        operation_id: operation_id.to_string(),
        generation,
        requests: Arc::clone(requests),
        final_phase,
        completed: false,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CancelWorkbenchResult {
    Cancelled,
    Completed,
    CapacityExceeded,
}

fn cancel_workbench_request(
    requests: &Mutex<HashMap<String, ActiveWorkbenchEntry>>,
    operation_id: &str,
) -> CancelWorkbenchResult {
    let mut requests = lock(requests);
    let now = Instant::now();
    requests.retain(|_, entry| {
        entry.abort_handle.is_some()
            || now.saturating_duration_since(entry.touched_at) < WORKBENCH_OPERATION_TTL
    });
    if !requests.contains_key(operation_id) {
        if requests.len() >= MAX_WORKBENCH_OPERATIONS {
            return CancelWorkbenchResult::CapacityExceeded;
        }
        // Reserve cancellation before the initial phase can register. This
        // closes the parse/body-read -> registration race instead of replying
        // false and allowing the supposedly cancelled operation to start.
        requests.insert(
            operation_id.to_string(),
            ActiveWorkbenchEntry {
                generation: 0,
                abort_handle: None,
                cancelled: true,
                completed: false,
                touched_at: now,
            },
        );
        return CancelWorkbenchResult::Cancelled;
    }
    let Some(entry) = requests.get_mut(operation_id) else {
        return CancelWorkbenchResult::Cancelled;
    };
    if entry.completed {
        return CancelWorkbenchResult::Completed;
    }
    if entry.cancelled {
        return CancelWorkbenchResult::Cancelled;
    }
    entry.cancelled = true;
    entry.touched_at = Instant::now();
    if let Some(abort) = entry.abort_handle.take() {
        abort.abort();
    }
    CancelWorkbenchResult::Cancelled
}

/// The transient native API-client payload. Header pairs stay ordered and
/// repeatable; a JSON object would silently collapse duplicate header names.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiWorkbenchLegacyRequest {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    method: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    headers: Vec<ApiWorkbenchHeader>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    allow_private_network: bool,
    #[serde(default)]
    request_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ApiWorkbenchV2Request {
    #[serde(rename = "version")]
    _version: u32,
    request_id: String,
    operation_id: String,
    starts_operation: bool,
    final_phase: bool,
    method: String,
    url: String,
    headers: Vec<ApiWorkbenchHeader>,
    #[serde(default)]
    body_base64: Option<String>,
    #[serde(default)]
    content_type: Option<String>,
    allow_private_network: bool,
    timeout_ms: u64,
    max_redirects: u8,
    response_limit_bytes: usize,
    #[serde(default, rename = "awsSigV4")]
    aws_sigv4: Option<ApiWorkbenchAwsSigV4>,
}

#[derive(Debug)]
struct ApiWorkbenchRequest {
    response_version: u32,
    request_id: Option<String>,
    operation_id: Option<String>,
    starts_operation: bool,
    final_phase: bool,
    method: String,
    url: String,
    headers: Vec<ApiWorkbenchHeader>,
    body: Option<String>,
    body_base64: Option<String>,
    content_type: Option<String>,
    allow_private_network: bool,
    timeout_ms: u64,
    max_redirects: u8,
    response_limit_bytes: usize,
    aws_sigv4: Option<ApiWorkbenchAwsSigV4>,
}

fn decode_api_workbench_request(body: &[u8]) -> Result<ApiWorkbenchRequest, String> {
    let value: Value = serde_json::from_slice(body).map_err(|error| error.to_string())?;
    let version = value.get("version").and_then(Value::as_u64).unwrap_or(0);
    if version == 2 {
        let request: ApiWorkbenchV2Request =
            serde_json::from_value(value).map_err(|error| error.to_string())?;
        return Ok(ApiWorkbenchRequest {
            response_version: 2,
            request_id: Some(request.request_id),
            operation_id: Some(request.operation_id),
            starts_operation: request.starts_operation,
            final_phase: request.final_phase,
            method: request.method,
            url: request.url,
            headers: request.headers,
            body: None,
            body_base64: request.body_base64,
            content_type: request.content_type,
            allow_private_network: request.allow_private_network,
            timeout_ms: request.timeout_ms,
            max_redirects: request.max_redirects,
            response_limit_bytes: request.response_limit_bytes,
            aws_sigv4: request.aws_sigv4,
        });
    }
    let request: ApiWorkbenchLegacyRequest =
        serde_json::from_value(value).map_err(|error| error.to_string())?;
    if request.version > 1 {
        return Err(format!(
            "Unsupported API workbench version {}",
            request.version
        ));
    }
    Ok(ApiWorkbenchRequest {
        response_version: 1,
        operation_id: request.request_id.clone(),
        request_id: request.request_id,
        starts_operation: true,
        final_phase: true,
        method: request.method,
        url: request.url,
        headers: request.headers,
        body: request.body,
        body_base64: None,
        content_type: None,
        allow_private_network: request.allow_private_network,
        timeout_ms: api_workbench_default_timeout(),
        max_redirects: api_workbench_default_redirects(),
        response_limit_bytes: api_workbench_default_response_limit(),
        aws_sigv4: None,
    })
}

#[derive(Debug, Default, Deserialize)]
struct ApiWorkbenchAwsSigV4 {
    #[serde(default, rename = "accessKey")]
    access_key: String,
    #[serde(default, rename = "secretKey")]
    secret_key: String,
    #[serde(default)]
    region: String,
    #[serde(default)]
    service: String,
    #[serde(default, rename = "sessionToken")]
    session_token: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ApiWorkbenchCancelRequest {
    #[serde(default)]
    version: u32,
    #[serde(default, rename = "requestId")]
    request_id: String,
    #[serde(default, rename = "operationId")]
    operation_id: String,
}

const fn api_workbench_default_timeout() -> u64 {
    30_000
}

const fn api_workbench_default_redirects() -> u8 {
    5
}

const fn api_workbench_default_response_limit() -> usize {
    256 * 1024
}

fn valid_api_workbench_request_id(request_id: &str) -> bool {
    !request_id.is_empty()
        && request_id.len() <= MAX_API_WORKBENCH_REQUEST_ID_BYTES
        && request_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

#[derive(Debug, Default, Deserialize)]
struct ApiWorkbenchHeader {
    #[serde(default)]
    name: String,
    #[serde(default)]
    value: String,
}

async fn send(body: &[u8]) -> Value {
    let wire_context = serde_json::from_slice::<Value>(body).ok();
    let wire_version = wire_context
        .as_ref()
        .and_then(|value| value.get("version"))
        .and_then(Value::as_u64)
        .unwrap_or_default() as u32;
    let wire_request_id = wire_context
        .as_ref()
        .and_then(|value| value.get("requestId"))
        .and_then(Value::as_str);
    let wire_operation_id = wire_context
        .as_ref()
        .and_then(|value| value.get("operationId"))
        .and_then(Value::as_str);
    let request = match decode_api_workbench_request(body) {
        Ok(request) => request,
        Err(e) => {
            return send_error(
                &format!("Invalid JSON: {e}"),
                wire_version,
                wire_request_id,
                wire_operation_id,
            );
        }
    };
    if request
        .request_id
        .as_deref()
        .is_some_and(|id| !valid_api_workbench_request_id(id))
        || request
            .operation_id
            .as_deref()
            .is_some_and(|id| !valid_api_workbench_request_id(id))
    {
        return send_error(
            "requestId and operationId must be 1-128 ASCII letters, digits, '.', '_', ':', or '-'",
            request.response_version,
            request.request_id.as_deref(),
            request.operation_id.as_deref(),
        );
    }
    let request_id = request.request_id.clone();
    let operation_id = request.operation_id.clone();
    let response_version = request.response_version;
    let starts_operation = request.starts_operation;
    let final_phase = request.final_phase;
    let headers = request
        .headers
        .into_iter()
        .map(|header| (header.name, header.value))
        .collect();
    let body = match request.body_base64.as_deref() {
        Some(encoded) => match base64::engine::general_purpose::STANDARD.decode(encoded) {
            Ok(body) => Some(body),
            Err(e) => {
                return send_error(
                    &format!("Invalid base64 request body: {e}"),
                    response_version,
                    request_id.as_deref(),
                    operation_id.as_deref(),
                );
            }
        },
        None => request.body.map(String::into_bytes),
    };
    let (start_tx, start_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        if start_rx.await.is_err() {
            return WorkbenchTaskResult::Cancelled;
        }
        let result = crate::http::send_api_workbench_request(crate::http::ApiWorkbenchRequest {
            method: request.method,
            url: request.url,
            headers,
            body,
            content_type: request.content_type,
            allow_private_network: request.allow_private_network,
            timeout_ms: request.timeout_ms,
            max_redirects: request.max_redirects,
            response_limit_bytes: request.response_limit_bytes,
            aws_sigv4: request
                .aws_sigv4
                .map(|signing| crate::http::ApiWorkbenchAwsSigV4 {
                    access_key: signing.access_key,
                    secret_key: signing.secret_key,
                    region: signing.region,
                    service: signing.service,
                    session_token: signing.session_token,
                }),
        })
        .await;
        WorkbenchTaskResult::Completed(result)
    });
    let abort_handle = task.abort_handle();
    let generation = state().generation.fetch_add(1, Ordering::Relaxed);
    let registration = if let Some(operation_id) = operation_id.as_deref() {
        match register_workbench_phase(
            &state().active,
            operation_id,
            generation,
            abort_handle,
            starts_operation,
            final_phase,
        ) {
            Ok(registration) => Some(registration),
            Err("operation is cancelled") => {
                task.abort();
                return cancelled(
                    Some(response_version),
                    request_id.as_deref(),
                    Some(operation_id),
                    "API operation",
                );
            }
            Err(e) => {
                task.abort();
                return send_error(
                    e,
                    response_version,
                    request_id.as_deref(),
                    Some(operation_id),
                );
            }
        }
    } else {
        None
    };
    let _ = start_tx.send(());
    let result = task.await;
    let outcome = match &result {
        Ok(WorkbenchTaskResult::Completed(Ok(_))) => WorkbenchPhaseOutcome::Succeeded,
        Ok(WorkbenchTaskResult::Completed(Err(_))) => WorkbenchPhaseOutcome::Failed,
        Ok(WorkbenchTaskResult::Cancelled) => WorkbenchPhaseOutcome::Cancelled,
        Err(e) if e.is_cancelled() => WorkbenchPhaseOutcome::Cancelled,
        Err(_) => WorkbenchPhaseOutcome::Failed,
    };
    let operation_cancelled = registration.is_some_and(|guard| guard.complete(outcome));
    let result = if operation_cancelled {
        Ok(WorkbenchTaskResult::Cancelled)
    } else {
        result
    };
    match result {
        Ok(WorkbenchTaskResult::Completed(Ok(response))) => {
            let mut value = serde_json::json!({
                "success": true,
                "requestId": request_id,
                "operationId": operation_id,
                "status": response.status,
                "reason": response.reason,
                "headers": response.headers.into_iter().map(|(name, value)| {
                    serde_json::json!({"name": name, "value": value})
                }).collect::<Vec<_>>(),
                "setCookies": response.set_cookies,
                "cookieMutations": response.cookie_mutations.into_iter().map(|mutation| {
                    serde_json::json!({
                        "sourceUrl": mutation.source_url,
                        "header": mutation.header,
                    })
                }).collect::<Vec<_>>(),
                "body": if response_version == 1 {
                    Some(String::from_utf8_lossy(&response.body).into_owned())
                } else {
                    std::str::from_utf8(&response.body).ok().map(str::to_string)
                },
                "bodyBase64": base64::engine::general_purpose::STANDARD.encode(&response.body),
                "finalUrl": response.final_url,
                "httpVersion": response.http_version,
                "receivedBytes": response.received_bytes,
                "storedBytes": response.stored_bytes,
                "full_body_sha256": response.full_body_sha256,
                "timings": {
                    "dnsMs": response.timings.dns_ms,
                    // reqwest does not expose distinct connect/TLS events.
                    "connectMs": Value::Null,
                    "tlsMs": Value::Null,
                    "firstByteMs": response.timings.first_byte_ms,
                    "downloadMs": response.timings.download_ms,
                },
                "cookies": response.cookies.into_iter().map(|cookie| serde_json::json!({
                    "name": cookie.name,
                    "domain": cookie.domain,
                    "path": cookie.path,
                    "secure": cookie.secure,
                    "httpOnly": cookie.http_only,
                    "sameSite": cookie.same_site,
                    "expiresAt": cookie.expires_at,
                })).collect::<Vec<_>>(),
                "durationMs": response.duration_ms,
                "truncated": response.truncated,
                "redirects": response.redirects.into_iter().map(|redirect| serde_json::json!({
                    "status": redirect.status,
                    "from": redirect.from,
                    "to": redirect.to,
                    "method": redirect.method,
                    "crossOrigin": redirect.cross_origin,
                })).collect::<Vec<_>>(),
            });
            if let Some(object) = value.as_object_mut() {
                if response_version == 1 {
                    for key in [
                        "version",
                        "operationId",
                        "bodyBase64",
                        "full_body_sha256",
                        "cookieMutations",
                    ] {
                        object.remove(key);
                    }
                } else {
                    object.insert("version".into(), Value::from(2));
                }
            }
            value
        }
        Ok(WorkbenchTaskResult::Completed(Err(e))) => send_error(
            &e,
            response_version,
            request_id.as_deref(),
            operation_id.as_deref(),
        ),
        Ok(WorkbenchTaskResult::Cancelled) => cancelled(
            Some(response_version),
            request_id.as_deref(),
            operation_id.as_deref(),
            "API request",
        ),
        Err(e) if e.is_cancelled() => cancelled(
            Some(response_version),
            request_id.as_deref(),
            operation_id.as_deref(),
            "API request",
        ),
        Err(e) => send_error(
            &format!("API request task failed: {e}"),
            response_version,
            request_id.as_deref(),
            operation_id.as_deref(),
        ),
    }
}

fn cancel(body: &[u8]) -> Value {
    let request: ApiWorkbenchCancelRequest = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(e) => return error(&format!("Invalid JSON: {e}")),
    };
    if request.version > 1 {
        return error(&format!(
            "Unsupported API workbench cancel version {}",
            request.version
        ));
    }
    let operation_id = if request.operation_id.is_empty() {
        request.request_id.clone()
    } else {
        request.operation_id.clone()
    };
    if !valid_api_workbench_request_id(&operation_id) {
        return error(
            "operationId (or legacy requestId) must be 1-128 ASCII letters, digits, '.', '_', ':', or '-'",
        );
    }
    let cancellation = cancel_workbench_request(&state().active, &operation_id);
    if cancellation == CancelWorkbenchResult::CapacityExceeded {
        return error("too many API operations are active");
    }
    serde_json::json!({
        "success": true,
        "version": 1,
        "requestId": request.request_id,
        "operationId": operation_id,
        "cancelled": cancellation == CancelWorkbenchResult::Cancelled,
        "completed": cancellation == CancelWorkbenchResult::Completed,
    })
}

fn script_result(
    result: Result<crate::script::ScriptExecutionResult, String>,
    request_id: Option<&str>,
) -> Value {
    match result {
        Ok(result) => serde_json::json!({
            "success": true,
            "version": 1,
            "requestId": request_id,
            "result": result,
        }),
        Err(e) => error(&e),
    }
}

async fn script(body: &[u8]) -> Value {
    let request: crate::script::ScriptExecutionRequest = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(e) => return error(&format!("Invalid script JSON: {e}")),
    };
    let request_id = request.request_id.clone();
    let operation_id = request.operation_id.clone();
    let starts_operation = request.starts_operation;
    let final_phase = request.final_phase;
    if request_id.is_none() && operation_id.is_none() {
        return script_result(crate::script::execute_isolated(request).await, None);
    }
    if request_id
        .as_deref()
        .is_some_and(|id| !valid_api_workbench_request_id(id))
        || operation_id
            .as_deref()
            .is_some_and(|id| !valid_api_workbench_request_id(id))
    {
        return error(
            "requestId and operationId must be 1-128 ASCII letters, digits, '.', '_', ':', or '-'",
        );
    }
    let (start_tx, start_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        if start_rx.await.is_err() {
            return WorkbenchScriptTaskResult::Cancelled;
        }
        WorkbenchScriptTaskResult::Completed(crate::script::execute_isolated(request).await)
    });
    let abort_handle = task.abort_handle();
    let generation = state().generation.fetch_add(1, Ordering::Relaxed);
    let phase_id = operation_id.as_deref().or(request_id.as_deref());
    let registration = if let Some(phase_id) = phase_id {
        match register_workbench_phase(
            &state().active,
            phase_id,
            generation,
            abort_handle,
            operation_id.is_none() || starts_operation,
            operation_id.is_none() || final_phase,
        ) {
            Ok(registration) => Some(registration),
            Err("operation is cancelled") => {
                task.abort();
                return cancelled(
                    None,
                    request_id.as_deref(),
                    operation_id.as_deref(),
                    "Script",
                );
            }
            Err(e) => {
                task.abort();
                return error(e);
            }
        }
    } else {
        None
    };
    let _ = start_tx.send(());
    let result = task.await;
    let outcome = match &result {
        Ok(WorkbenchScriptTaskResult::Completed(Ok(_))) => WorkbenchPhaseOutcome::Succeeded,
        Ok(WorkbenchScriptTaskResult::Completed(Err(_))) => WorkbenchPhaseOutcome::Failed,
        Ok(WorkbenchScriptTaskResult::Cancelled) => WorkbenchPhaseOutcome::Cancelled,
        Err(e) if e.is_cancelled() => WorkbenchPhaseOutcome::Cancelled,
        Err(_) => WorkbenchPhaseOutcome::Failed,
    };
    let operation_cancelled = registration.is_some_and(|guard| guard.complete(outcome));
    match (operation_cancelled, result) {
        (false, Ok(WorkbenchScriptTaskResult::Completed(result))) => {
            script_result(result, request_id.as_deref())
        }
        (_, Err(e)) if !e.is_cancelled() => error(&format!("Script task failed: {e}")),
        _ => cancelled(
            None,
            request_id.as_deref(),
            operation_id.as_deref(),
            "Script",
        ),
    }
}

async fn oauth(body: &[u8]) -> Value {
    let request: crate::http::WorkbenchOAuthRequest = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(e) => return error(&format!("Invalid OAuth JSON: {e}")),
    };
    match crate::http::exchange_workbench_oauth_token(request).await {
        Ok(result) => serde_json::json!({"success": true, "version": 1, "token": result}),
        Err(e) => error(&e),
    }
}
