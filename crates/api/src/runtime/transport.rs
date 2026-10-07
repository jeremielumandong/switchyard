//! Typed, transient transport for the API Workbench.
//!
//! Shared by the GPUI panel (which schedules these blocking loopback calls on
//! its background executor) and the headless `agentops-workbench` sidecar.
//! This module never persists request data and never logs secret values.

use base64::Engine as _;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::{
    PreparedBody, PreparedMultipartValue, PreparedRequest, RequestId, ResponseCookie,
    ResponseSnapshot, ResponseTimings,
};

const MAX_AUTHORIZED_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Wire value that asks the native service to retain the complete response.
/// Explicitly bounded callers such as remote imports pass a positive limit.
const COMPLETE_RESPONSE: usize = 0;
const UPLOAD_CAPABILITY_PREFIX: &str = "agentops-upload-capability:";
const UPLOAD_CAPABILITY_TTL: Duration = Duration::from_secs(10 * 60);
/// Environment override for the directory holding the service capability
/// file. The GUI never sets it; the headless sidecar may.
pub const USER_DATA_DIR_ENV: &str = "SWITCHYARD_API_DATA_DIR";

/// One-use upload grants created only by an explicit user action.
///
/// Saved/imported requests may carry display paths for interoperability, but a
/// path is never a grant key and is never opened while sending. The native file
/// picker snapshots a selected file and keeps its opaque grant entirely in this
/// transient state. The editor receives only a display filename. Consuming the
/// bound grant removes it, so imported text can neither nominate nor replay a
/// local file.
#[derive(Clone, Default)]
pub struct FileCapabilities {
    state: Arc<Mutex<FileCapabilityState>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PickerFileGrant {
    pub filename: String,
}

#[derive(Default)]
struct FileCapabilityState {
    grants: BTreeMap<String, AuthorizedFile>,
    bindings: BTreeMap<(String, usize), String>,
}

#[derive(Clone, Debug)]
struct AuthorizedFile {
    request_id: String,
    slot: usize,
    expires_at: Instant,
    filename: String,
    bytes: Vec<u8>,
}

impl FileCapabilities {
    /// Issue a one-use grant from a path returned by GPUI's native picker.
    ///
    /// Do not call this with editor/import text. That text is deliberately not
    /// accepted by [`consume`], and the panel's only production call site is
    /// the native picker completion callback.
    pub fn issue_picker_selection(
        &self,
        path: PathBuf,
        request_id: &RequestId,
        slot: usize,
    ) -> Result<PickerFileGrant, String> {
        self.issue_picker_selection_with_ttl(path, request_id, slot, UPLOAD_CAPABILITY_TTL)
    }

    fn issue_picker_selection_with_ttl(
        &self,
        path: PathBuf,
        request_id: &RequestId,
        slot: usize,
        ttl: Duration,
    ) -> Result<PickerFileGrant, String> {
        let canonical = std::fs::canonicalize(&path)
            .map_err(|error| format!("authorize selected upload file: {error}"))?;
        let metadata = std::fs::metadata(&canonical)
            .map_err(|error| format!("inspect selected upload file: {error}"))?;
        if !metadata.is_file() {
            return Err("upload selection is not a regular file".into());
        }
        if metadata.len() > MAX_AUTHORIZED_FILE_BYTES {
            return Err(format!(
                "upload selection exceeds the {} MiB safety limit",
                MAX_AUTHORIZED_FILE_BYTES / 1024 / 1024
            ));
        }
        let bytes = std::fs::read(&canonical)
            .map_err(|error| format!("authorize selected upload file: {error}"))?;
        let filename = canonical
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("upload.bin")
            .to_string();
        // RequestId::new is UUID-v4 backed. The random token never enters an
        // editable or serializable model; only this transient map can resolve
        // it, and resolution additionally checks the request and body slot.
        let token = format!("{UPLOAD_CAPABILITY_PREFIX}{}", RequestId::new());
        let key = (request_id.as_str().to_string(), slot);
        let mut state = self
            .state
            .lock()
            .map_err(|_| "upload authorization store is unavailable".to_string())?;
        if let Some(previous) = state.bindings.insert(key, token.clone()) {
            state.grants.remove(&previous);
        }
        state.grants.insert(
            token,
            AuthorizedFile {
                request_id: request_id.as_str().to_string(),
                slot,
                expires_at: Instant::now() + ttl,
                filename: filename.clone(),
                bytes,
            },
        );
        Ok(PickerFileGrant { filename })
    }

    fn consume(&self, request_id: &str, slot: usize) -> Result<AuthorizedFile, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "upload authorization store is unavailable".to_string())?;
        let key = (request_id.to_string(), slot);
        let token = state.bindings.remove(&key).ok_or_else(|| {
            "file upload is not picker-authorized for this request and body slot; choose the file before sending".to_string()
        })?;
        let grant = state.grants.remove(&token).ok_or_else(|| {
            "file upload capability is missing, expired, or already used".to_string()
        })?;
        if grant.request_id != request_id
            || grant.slot != slot
            || Instant::now() >= grant.expires_at
        {
            return Err(
                "file upload capability is expired or belongs to another request/body slot".into(),
            );
        }
        Ok(grant)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub console: Vec<crate::ConsoleEntry>,
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    /// Raw Set-Cookie fields are transient input to the native cookie jar and
    /// are never folded into inspectable/durable response headers.
    pub set_cookies: Vec<String>,
    pub cookie_mutations: Vec<CookieMutation>,
    pub body: String,
    pub body_base64: String,
    pub binary: bool,
    pub final_url: String,
    pub http_version: String,
    pub received_bytes: u64,
    pub stored_bytes: u64,
    pub full_body_sha256: Option<String>,
    pub timings: ResponseTimings,
    pub cookies: Vec<ResponseCookie>,
    pub duration_ms: u64,
    pub truncated: bool,
    pub redirects: Vec<Redirect>,
    /// Typed assertions returned by the authenticated JavaScript runtime.
    pub test_results: Vec<crate::TestResult>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CookieMutation {
    pub source_url: String,
    pub header: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    pub status: u16,
    pub from: String,
    pub to: String,
    pub method: String,
    pub cross_origin: bool,
}

/// One logical cancelable workflow spanning pre-script, network send and
/// post-response script. `requestId` remains wire-response correlation; this
/// identity is what the service tombstones when Cancel wins between phases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationPhase {
    pub operation_id: String,
    pub starts_operation: bool,
    pub final_phase: bool,
}

/// Injectable shipping boundary. Production uses the authenticated loopback
/// implementation below; tests (GPUI window tests and the headless runner's
/// unit tests alike) substitute a deterministic transport.
pub trait WorkbenchTransport: Send + Sync {
    fn send(
        &self,
        request: &PreparedRequest,
        files: &FileCapabilities,
        phase: &OperationPhase,
    ) -> Result<Response, String>;

    fn run_script(
        &self,
        script: &str,
        scopes: ScriptScopes,
        request: ScriptRequestView,
        response: Option<ScriptResponseView>,
        request_id: Option<&str>,
        phase: Option<&OperationPhase>,
    ) -> Result<ScriptResult, String>;

    fn cancel(&self, operation_id: &str) -> Result<bool, String>;

    /// Exchange at the token endpoint through the same service the sends
    /// use. The default reaches the service like the desktop app does;
    /// deterministic test transports override it.
    fn exchange_oauth_token(
        &self,
        request: OAuthTokenRequest,
    ) -> Result<OAuthTokenResponse, String> {
        exchange_oauth_token(request)
    }
}

/// The native transport: sends, scripts, OAuth exchanges and cancellation run in this
/// process ([`crate::service`]) on a tokio runtime. AgentOps reached the same handlers over
/// an authenticated loopback HTTP hop to its agent service; the JSON wire format stays.
///
/// Its methods block: call them from a blocking thread (core's `spawn_blocking`), never
/// from inside an async task.
#[derive(Clone, Debug)]
pub struct NativeWorkbenchTransport {
    user_data_dir: Option<PathBuf>,
    runtime: Option<tokio::runtime::Handle>,
}

/// A runtime for callers outside any tokio runtime (tests, the CLI before it starts one).
fn fallback_runtime() -> Result<tokio::runtime::Handle, String> {
    static RUNTIME: std::sync::OnceLock<Result<tokio::runtime::Runtime, String>> =
        std::sync::OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("swy-api")
                .enable_all()
                .build()
                .map_err(|e| format!("start the API runtime: {e}"))
        })
        .as_ref()
        .map(|rt| rt.handle().clone())
        .map_err(Clone::clone)
}

impl Default for NativeWorkbenchTransport {
    fn default() -> Self {
        Self::from_env()
    }
}

impl NativeWorkbenchTransport {
    /// Use an explicit user data directory (the service's `--data` path).
    pub fn new(user_data_dir: impl Into<PathBuf>) -> Self {
        Self {
            user_data_dir: Some(user_data_dir.into()),
            runtime: tokio::runtime::Handle::try_current().ok(),
        }
    }

    /// Use `runtime` for the async work (core passes its own).
    pub fn with_runtime(mut self, runtime: tokio::runtime::Handle) -> Self {
        self.runtime = Some(runtime);
        self
    }

    /// The data directory from [`USER_DATA_DIR_ENV`] (none otherwise), and the current
    /// tokio runtime if there is one.
    pub fn from_env() -> Self {
        Self {
            user_data_dir: default_user_data_dir(),
            runtime: tokio::runtime::Handle::try_current().ok(),
        }
    }

    pub fn user_data_dir(&self) -> Option<&Path> {
        self.user_data_dir.as_deref()
    }

    /// Execute the canonical compiled request. Callers must never rebuild a
    /// wire request themselves: compilation owns variable precedence, auth,
    /// redaction, body encodings and snippets.
    pub fn send_prepared(
        &self,
        request: &PreparedRequest,
        file_capabilities: &FileCapabilities,
    ) -> Result<Response, String> {
        let phase = OperationPhase {
            operation_id: request.request_id.as_str().to_string(),
            starts_operation: true,
            final_phase: true,
        };
        self.send_prepared_in_operation(request, file_capabilities, &phase)
    }

    pub fn send_prepared_in_operation(
        &self,
        request: &PreparedRequest,
        file_capabilities: &FileCapabilities,
        phase: &OperationPhase,
    ) -> Result<Response, String> {
        self.send_prepared_with_limit(request, file_capabilities, COMPLETE_RESPONSE, Some(phase))
    }

    /// [`send_prepared_in_operation`](Self::send_prepared_in_operation) with
    /// an explicit response-body limit and optional operation phase. Zero
    /// retains the complete response; a positive value deliberately bounds it.
    pub fn send_prepared_with_limit(
        &self,
        request: &PreparedRequest,
        file_capabilities: &FileCapabilities,
        response_limit_bytes: usize,
        phase: Option<&OperationPhase>,
    ) -> Result<Response, String> {
        let request_id = request.request_id.as_str();
        let (body_bytes, content_type) =
            prepared_body(&request.body, request_id, file_capabilities)?;
        let body = json!({
            "version": 2,
            "requestId": request_id,
            "operationId": phase.map(|phase| phase.operation_id.as_str()).unwrap_or(request_id),
            "startsOperation": phase.map(|phase| phase.starts_operation).unwrap_or(true),
            "finalPhase": phase.map(|phase| phase.final_phase).unwrap_or(true),
            "method": request.method.as_str(),
            "url": request.url,
            "headers": request.headers.iter().map(|(name, value)| {
                json!({"name": name, "value": value})
            }).collect::<Vec<_>>(),
            "bodyBase64": body_bytes.as_ref().map(|body| {
                base64::engine::general_purpose::STANDARD.encode(body)
            }),
            "contentType": content_type,
            "allowPrivateNetwork": request.settings.allow_private_network,
            "timeoutMs": request.settings.timeout_ms,
            "maxRedirects": if request.settings.follow_redirects {
                request.settings.max_redirects
            } else {
                0
            },
            "responseLimitBytes": response_limit_bytes,
            "awsSigV4": request.aws_sigv4.as_ref().map(|signing| json!({
                "accessKey": signing.access_key,
                "secretKey": signing.secret_key,
                "sessionToken": signing.session_token,
                "region": signing.region,
                "service": signing.service,
            })),
        })
        .to_string();
        let value = self.call_with_capability(
            "/api/api-workbench/send",
            &body,
            Duration::from_millis(request.settings.timeout_ms.saturating_add(5_000)),
        )?;
        decode_response(
            value,
            Some(request_id),
            Some(
                phase
                    .map(|phase| phase.operation_id.as_str())
                    .unwrap_or(request_id),
            ),
        )
    }
    /// Cancel the upstream socket owned by the service, not only the caller
    /// task that is waiting for it.
    pub fn cancel(&self, request_id: &str) -> Result<bool, String> {
        let body = json!({
            "version": 1,
            "operationId": request_id,
            // Kept for rolling updates with services predating operation IDs.
            "requestId": request_id,
        })
        .to_string();
        let value =
            self.call_with_capability("/api/api-workbench/cancel", &body, Duration::from_secs(10))?;
        if value.get("success").and_then(Value::as_bool) != Some(true) {
            return Err(value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("the agent service refused cancellation")
                .into());
        }
        Ok(value
            .get("cancelled")
            .and_then(Value::as_bool)
            .unwrap_or(false))
    }
    pub fn exchange_oauth_token(
        &self,
        request: OAuthTokenRequest,
    ) -> Result<OAuthTokenResponse, String> {
        let body = serde_json::to_string(&json!({
            "version": 1,
            "flow": request.flow,
            "tokenUrl": request.token_url,
            "headers": request.headers,
            "clientId": request.client_id,
            "clientSecret": request.client_secret,
            "scope": request.scope,
            "code": request.code,
            "redirectUri": request.redirect_uri,
            "codeVerifier": request.code_verifier,
            "refreshToken": request.refresh_token,
            "username": request.username,
            "password": request.password,
            "expectedState": request.expected_state,
            "callbackState": request.callback_state,
            "allowPrivateNetwork": request.allow_private_network,
            "timeoutMs": 30_000,
        }))
        .map_err(|error| format!("encode OAuth token request: {error}"))?;
        let value = self.call_with_capability(
            "/api/api-workbench/oauth/token",
            &body,
            Duration::from_secs(35),
        )?;
        if value.get("success").and_then(Value::as_bool) != Some(true) {
            return Err(value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("the agent service refused the OAuth token exchange")
                .into());
        }
        let token = value.get("token").unwrap_or(&value);
        Ok(OAuthTokenResponse {
            access_token: token
                .get("accessToken")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "OAuth token response did not contain an access token".to_string())?
                .to_string(),
            expires_in: token.get("expiresIn").and_then(Value::as_u64),
            refresh_token: token
                .get("refreshToken")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }
    /// Execute imported/user script text only inside the authenticated service
    /// sandbox. This process deliberately has no local JavaScript fallback: if
    /// the service is unavailable, the request/run fails closed and visibly.
    /// The request id is registered by the service before the isolated worker
    /// starts, so the same cancel route can stop scripting or network I/O
    /// without a blind window.
    pub fn run_script_for_request(
        &self,
        script: &str,
        scopes: ScriptScopes,
        request: ScriptRequestView,
        response: Option<ScriptResponseView>,
        request_id: Option<&str>,
        phase: Option<&OperationPhase>,
    ) -> Result<ScriptResult, String> {
        if script.trim().is_empty() {
            return Ok(ScriptResult {
                globals: scopes.globals,
                environment_name: scopes.environment_name,
                next_request: scopes.next_request,
                variables: scopes.local.clone(),
                environment: scopes.environment,
                collection_variables: scopes.collection,
                local_variables: scopes.local,
                cookies: scopes.cookies,
                request,
                tests: Vec::new(),
                console: Vec::new(),
            });
        }
        let variables = scopes.local.clone();
        let body = serde_json::to_string(&json!({
            "version": 1,
            "script": script,
            "variables": variables,
            "globals": scopes.globals,
            "environmentName": scopes.environment_name,
            "nextRequest": scopes.next_request,
            "environment": scopes.environment,
            "collectionVariables": scopes.collection,
            "localVariables": scopes.local,
            "iterationData": scopes.iteration_data,
            "vault": scopes.vault,
            "cookies": scopes.cookies,
            "allowPrivateNetwork": scopes.allow_private_network,
            "assertionsOnly": scopes.assertions_only,
            "request": request,
            "response": response,
            "requestId": request_id,
            "operationId": phase.map(|phase| phase.operation_id.as_str()).or(request_id),
            "startsOperation": phase.map(|phase| phase.starts_operation).unwrap_or(true),
            "finalPhase": phase.map(|phase| phase.final_phase).unwrap_or(true),
        }))
        .map_err(|error| format!("encode Workbench script: {error}"))?;
        decode_script_result(self.call_with_capability(
            "/api/api-workbench/script",
            &body,
            Duration::from_secs(10),
        )?)
    }
    fn call_with_capability(
        &self,
        path: &str,
        body: &str,
        _read_timeout: Duration,
    ) -> Result<Value, String> {
        let runtime = match &self.runtime {
            Some(rt) => rt.clone(),
            None => fallback_runtime()?,
        };
        Ok(crate::service::dispatch(&runtime, path, body))
    }
}

/// The API workspace's data directory from [`USER_DATA_DIR_ENV`], when set. The app
/// passes its own (under Switchyard's data directory) with [`NativeWorkbenchTransport::new`].
pub fn default_user_data_dir() -> Option<PathBuf> {
    std::env::var_os(USER_DATA_DIR_ENV)
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

impl WorkbenchTransport for NativeWorkbenchTransport {
    fn send(
        &self,
        request: &PreparedRequest,
        files: &FileCapabilities,
        phase: &OperationPhase,
    ) -> Result<Response, String> {
        self.send_prepared_in_operation(request, files, phase)
    }

    fn run_script(
        &self,
        script: &str,
        scopes: ScriptScopes,
        request: ScriptRequestView,
        response: Option<ScriptResponseView>,
        request_id: Option<&str>,
        phase: Option<&OperationPhase>,
    ) -> Result<ScriptResult, String> {
        self.run_script_for_request(script, scopes, request, response, request_id, phase)
    }

    fn cancel(&self, operation_id: &str) -> Result<bool, String> {
        NativeWorkbenchTransport::cancel(self, operation_id)
    }

    fn exchange_oauth_token(
        &self,
        request: OAuthTokenRequest,
    ) -> Result<OAuthTokenResponse, String> {
        NativeWorkbenchTransport::exchange_oauth_token(self, request)
    }
}

/// Free-function forms resolving the user data directory like the desktop app.
pub fn send_prepared(
    request: &PreparedRequest,
    file_capabilities: &FileCapabilities,
) -> Result<Response, String> {
    NativeWorkbenchTransport::from_env().send_prepared(request, file_capabilities)
}

pub fn send_prepared_in_operation(
    request: &PreparedRequest,
    file_capabilities: &FileCapabilities,
    phase: &OperationPhase,
) -> Result<Response, String> {
    NativeWorkbenchTransport::from_env().send_prepared_in_operation(
        request,
        file_capabilities,
        phase,
    )
}

pub fn run_script_for_request(
    script: &str,
    scopes: ScriptScopes,
    request: ScriptRequestView,
    response: Option<ScriptResponseView>,
    request_id: Option<&str>,
    phase: Option<&OperationPhase>,
) -> Result<ScriptResult, String> {
    NativeWorkbenchTransport::from_env()
        .run_script_for_request(script, scopes, request, response, request_id, phase)
}

pub fn exchange_oauth_token(request: OAuthTokenRequest) -> Result<OAuthTokenResponse, String> {
    NativeWorkbenchTransport::from_env().exchange_oauth_token(request)
}

pub fn cancel(request_id: &str) -> Result<bool, String> {
    NativeWorkbenchTransport::from_env().cancel(request_id)
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptRequestView {
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub body: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptResponseView {
    pub code: u16,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub response_time_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptTestResult {
    pub name: String,
    pub passed: bool,
    #[serde(default)]
    pub skipped: bool,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScriptScopes {
    pub globals: BTreeMap<String, String>,
    pub environment_name: Option<String>,
    pub next_request: Option<ScriptNextRequest>,
    pub environment: BTreeMap<String, String>,
    pub collection: BTreeMap<String, String>,
    pub local: BTreeMap<String, String>,
    /// Read-only runner row. The service exposes this as `pm.iterationData`
    /// and never returns it as mutable script state.
    pub iteration_data: BTreeMap<String, String>,
    /// Named vault templates used by this request's scripts. Read-only and
    /// transient; excluded from script results and saved variable state.
    pub vault: BTreeMap<String, String>,
    pub cookies: BTreeMap<String, String>,
    pub allow_private_network: bool,
    /// Disable network subrequests for response-only assertion evaluation.
    pub assertions_only: bool,
}

impl ScriptScopes {
    pub fn resolved(&self) -> BTreeMap<String, String> {
        let mut values = self.globals.clone();
        values.extend(self.collection.clone());
        values.extend(self.environment.clone());
        values.extend(self.iteration_data.clone());
        values.extend(self.local.clone());
        values
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptConsoleEntry {
    pub level: String,
    pub message: String,
}

/// Include transient secrets even when they are never used in an HTTP field.
pub(super) fn script_scope_redactions(scopes: &ScriptScopes) -> Vec<String> {
    let mut values: Vec<String> = scopes
        .vault
        .values()
        .chain(scopes.cookies.values())
        .cloned()
        .collect();
    for scope in [
        &scopes.globals,
        &scopes.environment,
        &scopes.collection,
        &scopes.local,
        &scopes.iteration_data,
    ] {
        values.extend(
            scope
                .iter()
                .filter(|(key, _)| crate::persistence_safety::sensitive_name(key))
                .map(|(_, value)| value.clone()),
        );
    }
    values
}

pub(super) fn script_request_redactions(request: &ScriptRequestView) -> Vec<String> {
    let mut values = Vec::new();
    for (name, value) in &request.headers {
        if matches!(
            name.to_ascii_lowercase().as_str(),
            "authorization" | "proxy-authorization"
        ) {
            values.push(value.clone());
            if let Some((_, credential)) = value.split_once(char::is_whitespace) {
                values.push(credential.trim().into());
            }
        } else if name.eq_ignore_ascii_case("cookie") {
            values.push(value.clone());
            values.extend(value.split(';').filter_map(|cookie| {
                cookie
                    .split_once('=')
                    .map(|(_, value)| value.trim().to_string())
            }));
        } else if crate::persistence_safety::sensitive_name(name) {
            values.push(value.clone());
        }
    }
    if let Ok(url) = url::Url::parse(&request.url) {
        if let Some(password) = url.password() {
            values.push(password.into());
        }
        values.extend(
            url.query_pairs()
                .filter(|(key, _)| crate::persistence_safety::sensitive_name(key))
                .map(|(_, value)| value.into_owned()),
        );
    }
    fn visit(value: &Value, values: &mut Vec<String>) {
        match value {
            Value::Object(object) => {
                for (key, value) in object {
                    if crate::persistence_safety::sensitive_name(key)
                        && let Some(value) = value.as_str()
                    {
                        values.push(value.into());
                    }
                    visit(value, values);
                }
            }
            Value::Array(array) => {
                for value in array {
                    visit(value, values);
                }
            }
            _ => {}
        }
    }
    if let Ok(body) = serde_json::from_str(&request.body) {
        visit(&body, &mut values);
    }
    values.retain(|value| !value.contains("{{"));
    values
}

pub(super) fn diagnostic_text(text: &str, redactions: &[String]) -> String {
    diagnostic_redactor(redactions)(text)
}

pub(super) fn diagnostic_redactor(redactions: &[String]) -> impl Fn(&str) -> String + use<> {
    let expanded = redactions
        .iter()
        .flat_map(|value| crate::compile::secret_redaction_variants(value))
        .collect::<Vec<_>>();
    let redactor = crate::compile::Redactor::new(&expanded);
    move |text| bounded_diagnostic(redactor.redact(text))
}

fn bounded_diagnostic(safe: String) -> String {
    // Mask before truncating: a secret split by the display limit must not leak.
    if safe.len() <= 4096 {
        return safe;
    }
    let end = safe
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= 4096)
        .last()
        .unwrap_or(0);
    format!("{}… [truncated]", &safe[..end])
}

pub(super) fn script_diagnostics(
    tests: &[ScriptTestResult],
    messages: &[ScriptConsoleEntry],
    phase: &str,
    redactions: &[String],
) -> (Vec<crate::TestResult>, Vec<crate::ConsoleEntry>) {
    let redact = diagnostic_redactor(redactions);
    let tests = tests
        .iter()
        .map(|test| crate::TestResult {
            name: format!("[{phase}] {}", redact(&test.name)),
            passed: test.passed,
            skipped: test.skipped,
            error: test.error.as_ref().map(|error| redact(error)),
        })
        .collect();
    let mut console: Vec<_> = messages
        .iter()
        .take(100)
        .map(|entry| crate::ConsoleEntry {
            phase: phase.into(),
            level: match entry.level.as_str() {
                "log" | "info" | "warn" | "error" | "debug" => entry.level.clone(),
                _ => "log".into(),
            },
            message: redact(&entry.message),
        })
        .collect();
    if messages.len() > 100 {
        console.push(crate::ConsoleEntry {
            phase: phase.into(),
            level: "warn".into(),
            message: "Console output truncated after 100 entries.".into(),
        });
    }
    (tests, console)
}

pub(super) fn script_error_console(
    phase: &str,
    error: &str,
    redactions: &[String],
) -> crate::ConsoleEntry {
    crate::ConsoleEntry {
        phase: phase.into(),
        level: "error".into(),
        message: diagnostic_text(error, redactions),
    }
}

#[test]
fn console_bounds_mask_complete_secrets_before_utf8_truncation() {
    let secret = "sensitive".repeat(25);
    let message = format!("{}{}tail", "ü".repeat(2000), secret);
    let entries = vec![
        ScriptConsoleEntry {
            level: "untrusted".into(),
            message
        };
        150
    ];
    let (_, console) = script_diagnostics(&[], &entries, "Pre-request", &[secret]);
    assert_eq!(console.len(), 101);
    assert_eq!(console[0].level, "log");
    assert!(console[0].message.contains("<redacted>"));
    assert!(!console[0].message.contains("sensitive"));
    assert!(console[0].message.len() < 4200);
    assert_eq!(console[100].level, "warn");
    let truncated = diagnostic_text(&"ü".repeat(3000), &[]);
    assert!(truncated.ends_with("… [truncated]"));
    assert!(truncated.len() < 4200);
    let secret = "private\"quoted\nvalue";
    let encoded = serde_json::to_string(secret).unwrap();
    assert!(!diagnostic_text(&encoded, &[secret.into()]).contains("private"));
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptResult {
    #[serde(default)]
    pub globals: BTreeMap<String, String>,
    #[serde(default)]
    pub environment_name: Option<String>,
    #[serde(default)]
    pub next_request: Option<ScriptNextRequest>,
    #[serde(default)]
    pub variables: BTreeMap<String, String>,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default)]
    pub collection_variables: BTreeMap<String, String>,
    #[serde(default)]
    pub local_variables: BTreeMap<String, String>,
    #[serde(default)]
    pub cookies: BTreeMap<String, String>,
    pub request: ScriptRequestView,
    pub tests: Vec<ScriptTestResult>,
    #[serde(default)]
    pub console: Vec<ScriptConsoleEntry>,
}

impl ScriptResult {
    pub fn scopes(&self) -> ScriptScopes {
        ScriptScopes {
            globals: self.globals.clone(),
            environment_name: self.environment_name.clone(),
            next_request: self.next_request.clone(),
            environment: self.environment.clone(),
            collection: self.collection_variables.clone(),
            local: if self.local_variables.is_empty() {
                self.variables.clone()
            } else {
                self.local_variables.clone()
            },
            iteration_data: BTreeMap::new(),
            vault: BTreeMap::new(),
            cookies: self.cookies.clone(),
            allow_private_network: false,
            assertions_only: false,
        }
    }

    pub fn resolved_variables(&self) -> BTreeMap<String, String> {
        self.scopes().resolved()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OAuthTokenRequest {
    pub flow: String,
    pub token_url: String,
    pub headers: Vec<(String, String)>,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub scope: Option<String>,
    pub code: Option<String>,
    pub redirect_uri: Option<String>,
    pub code_verifier: Option<String>,
    pub refresh_token: Option<String>,
    /// Resource-owner credentials — only the `password` grant sends them.
    pub username: Option<String>,
    pub password: Option<String>,
    pub expected_state: Option<String>,
    pub callback_state: Option<String>,
    pub allow_private_network: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OAuthTokenResponse {
    pub access_token: String,
    pub expires_in: Option<u64>,
    pub refresh_token: Option<String>,
}

fn decode_script_result(value: Value) -> Result<ScriptResult, String> {
    if value.get("success").and_then(Value::as_bool) != Some(true) {
        return Err(value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("the agent service refused the Workbench script")
            .to_string());
    }
    serde_json::from_value(
        value
            .get("result")
            .cloned()
            .ok_or_else(|| "the agent service returned no Workbench script result".to_string())?,
    )
    .map_err(|error| format!("decode Workbench script result: {error}"))
}

pub fn response_snapshot(response: &Response) -> ResponseSnapshot {
    ResponseSnapshot {
        console: response.console.clone(),
        status: response.status,
        reason: response.reason.clone(),
        headers: response.headers.clone(),
        body_base64: response.body_base64.clone(),
        duration_ms: response.duration_ms,
        truncated: response.truncated,
        final_url: response.final_url.clone(),
        redirects: response
            .redirects
            .iter()
            .map(|redirect| crate::RedirectSnapshot {
                status: redirect.status,
                from_url: redirect.from.clone(),
                to_url: redirect.to.clone(),
                method: redirect.method.clone(),
            })
            .collect(),
        http_version: response.http_version.clone(),
        received_bytes: response.received_bytes,
        stored_bytes: response.stored_bytes,
        full_body_sha256: response.full_body_sha256.clone(),
        timings: response.timings.clone(),
        cookies: response.cookies.clone(),
        test_results: response.test_results.clone(),
        ..ResponseSnapshot::default()
    }
}

pub fn response_from_snapshot(response: &ResponseSnapshot) -> Response {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&response.body_base64)
        .unwrap_or_default();
    let (body, binary) = match String::from_utf8(bytes) {
        Ok(body) => (body, false),
        Err(error) => (
            format!("<binary response: {} bytes>", error.as_bytes().len()),
            true,
        ),
    };
    Response {
        console: response.console.clone(),
        status: response.status,
        reason: response.reason.clone(),
        headers: response.headers.clone(),
        set_cookies: Vec::new(),
        cookie_mutations: Vec::new(),
        body,
        body_base64: response.body_base64.clone(),
        binary,
        final_url: response.final_url.clone(),
        http_version: response.http_version.clone(),
        received_bytes: response.received_bytes,
        stored_bytes: response.stored_bytes,
        full_body_sha256: response.full_body_sha256.clone(),
        timings: response.timings.clone(),
        cookies: response.cookies.clone(),
        duration_ms: response.duration_ms,
        truncated: response.truncated,
        redirects: response
            .redirects
            .iter()
            .map(|redirect| Redirect {
                status: redirect.status,
                from: redirect.from_url.clone(),
                to: redirect.to_url.clone(),
                method: redirect.method.clone(),
                cross_origin: url::Url::parse(&redirect.from_url)
                    .ok()
                    .zip(url::Url::parse(&redirect.to_url).ok())
                    .is_some_and(|(from, to)| from.origin() != to.origin()),
            })
            .collect(),
        test_results: response.test_results.clone(),
    }
}

fn prepared_body(
    body: &PreparedBody,
    request_id: &str,
    file_capabilities: &FileCapabilities,
) -> Result<(Option<Vec<u8>>, Option<String>), String> {
    match body {
        PreparedBody::None => Ok((None, None)),
        PreparedBody::Bytes {
            content_type,
            bytes,
        } => Ok((Some(bytes.clone()), Some(content_type.clone()))),
        PreparedBody::File(_) => {
            let file = file_capabilities.consume(request_id, 0)?;
            Ok((Some(file.bytes), Some("application/octet-stream".into())))
        }
        PreparedBody::Multipart(parts) => {
            let boundary = format!("agentops-{request_id}");
            let mut output = Vec::new();
            let mut file_slot = 0;
            for part in parts {
                output.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
                match &part.value {
                    PreparedMultipartValue::Text(value) => {
                        output.extend_from_slice(
                            format!(
                                "Content-Disposition: form-data; name=\"{}\"\r\n\r\n{}\r\n",
                                multipart_token(&part.name)?,
                                value
                            )
                            .as_bytes(),
                        );
                    }
                    PreparedMultipartValue::File(_) => {
                        let file = file_capabilities.consume(request_id, file_slot)?;
                        file_slot += 1;
                        output.extend_from_slice(
                            format!(
                                "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\nContent-Type: application/octet-stream\r\n\r\n",
                                multipart_token(&part.name)?,
                                multipart_token(&file.filename)?,
                            )
                            .as_bytes(),
                        );
                        output.extend_from_slice(&file.bytes);
                        output.extend_from_slice(b"\r\n");
                    }
                }
            }
            output.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
            Ok((
                Some(output),
                Some(format!("multipart/form-data; boundary={boundary}")),
            ))
        }
    }
}

fn multipart_token(value: &str) -> Result<&str, String> {
    if value.is_empty() || value.chars().any(|ch| matches!(ch, '\r' | '\n' | '"')) {
        Err("multipart names and filenames cannot contain quotes or newlines".into())
    } else {
        Ok(value)
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WireHeader {
    name: String,
    value: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WireRedirect {
    status: u16,
    from: String,
    to: String,
    method: String,
    cross_origin: bool,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WireCookie {
    name: String,
    domain: String,
    path: String,
    secure: bool,
    http_only: bool,
    same_site: Option<String>,
    expires_at: Option<i64>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WireTimings {
    dns_ms: Option<u64>,
    connect_ms: Option<u64>,
    tls_ms: Option<u64>,
    first_byte_ms: Option<u64>,
    download_ms: Option<u64>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyResponse {
    success: bool,
    status: u16,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    headers: Vec<WireHeader>,
    #[serde(default)]
    set_cookies: Vec<String>,
    #[serde(default)]
    body: String,
    #[serde(default)]
    final_url: String,
    #[serde(default)]
    duration_ms: u64,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct V2Response {
    success: bool,
    version: u8,
    request_id: String,
    operation_id: String,
    status: u16,
    reason: String,
    headers: Vec<WireHeader>,
    set_cookies: Vec<String>,
    #[serde(default)]
    cookie_mutations: Vec<WireCookieMutation>,
    body: Option<String>,
    body_base64: String,
    final_url: String,
    http_version: String,
    received_bytes: u64,
    stored_bytes: u64,
    #[serde(rename = "full_body_sha256")]
    full_body_sha256: Option<String>,
    timings: WireTimings,
    cookies: Vec<WireCookie>,
    duration_ms: u64,
    truncated: bool,
    redirects: Vec<WireRedirect>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WireCookieMutation {
    source_url: String,
    header: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct V2ErrorResponse {
    success: bool,
    version: u8,
    request_id: String,
    operation_id: String,
    error: String,
}

fn decode_response(
    value: Value,
    expected_request_id: Option<&str>,
    expected_operation_id: Option<&str>,
) -> Result<Response, String> {
    let success = value
        .get("success")
        .and_then(Value::as_bool)
        .ok_or_else(|| "the agent service returned no success marker".to_string())?;
    match value.get("version") {
        None if success => decode_legacy_response(value),
        None => Err(value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("the agent service refused the API request")
            .to_string()),
        Some(Value::Number(version)) if version.as_u64() == Some(2) && success => {
            decode_v2_response(value, expected_request_id, expected_operation_id)
        }
        Some(Value::Number(version)) if version.as_u64() == Some(2) => {
            decode_v2_error(value, expected_request_id, expected_operation_id)
        }
        Some(_) => Err("the agent service returned an unsupported response version".into()),
    }
}

fn validate_correlation(
    request_id: &str,
    operation_id: &str,
    expected_request_id: Option<&str>,
    expected_operation_id: Option<&str>,
) -> Result<(), String> {
    if request_id.trim().is_empty() || operation_id.trim().is_empty() {
        return Err("the agent service returned empty v2 correlation metadata".into());
    }
    if expected_request_id.is_some_and(|expected| expected != request_id) {
        return Err("the agent service returned a response for another request".into());
    }
    if expected_operation_id.is_some_and(|expected| expected != operation_id) {
        return Err("the agent service returned a response for another operation".into());
    }
    Ok(())
}

fn decode_v2_error(
    value: Value,
    expected_request_id: Option<&str>,
    expected_operation_id: Option<&str>,
) -> Result<Response, String> {
    let value: V2ErrorResponse = serde_json::from_value(value)
        .map_err(|error| format!("the agent service returned a malformed v2 error: {error}"))?;
    if value.success || value.version != 2 || value.error.trim().is_empty() {
        return Err("the agent service returned an invalid v2 error marker".into());
    }
    validate_correlation(
        &value.request_id,
        &value.operation_id,
        expected_request_id,
        expected_operation_id,
    )?;
    Err(value.error)
}

fn decode_legacy_response(value: Value) -> Result<Response, String> {
    let value: LegacyResponse = serde_json::from_value(value)
        .map_err(|error| format!("the agent service returned a malformed v1 response: {error}"))?;
    if !value.success {
        return Err("the agent service returned an unsuccessful v1 response".into());
    }
    Ok(Response {
        console: Vec::new(),
        status: value.status,
        reason: value.reason,
        headers: value
            .headers
            .into_iter()
            .map(|header| (header.name, header.value))
            .collect(),
        set_cookies: value.set_cookies,
        cookie_mutations: Vec::new(),
        body: value.body.clone(),
        body_base64: base64::engine::general_purpose::STANDARD.encode(value.body.as_bytes()),
        binary: false,
        final_url: value.final_url,
        http_version: String::new(),
        received_bytes: value.body.len() as u64,
        stored_bytes: value.body.len() as u64,
        full_body_sha256: None,
        timings: ResponseTimings::default(),
        cookies: Vec::new(),
        duration_ms: value.duration_ms,
        truncated: false,
        redirects: Vec::new(),
        test_results: Vec::new(),
    })
}

fn decode_v2_response(
    value: Value,
    expected_request_id: Option<&str>,
    expected_operation_id: Option<&str>,
) -> Result<Response, String> {
    for field in [
        "success",
        "version",
        "requestId",
        "operationId",
        "status",
        "reason",
        "headers",
        "setCookies",
        "body",
        "bodyBase64",
        "finalUrl",
        "httpVersion",
        "receivedBytes",
        "storedBytes",
        "full_body_sha256",
        "timings",
        "cookies",
        "durationMs",
        "truncated",
        "redirects",
    ] {
        if value.get(field).is_none() {
            return Err(format!(
                "the agent service returned a malformed v2 response: missing {field}"
            ));
        }
    }
    let timings = value
        .get("timings")
        .and_then(Value::as_object)
        .ok_or_else(|| "the agent service returned malformed v2 timings".to_string())?;
    for field in ["dnsMs", "connectMs", "tlsMs", "firstByteMs", "downloadMs"] {
        if !timings.contains_key(field) {
            return Err(format!(
                "the agent service returned malformed v2 timings: missing {field}"
            ));
        }
    }
    if let Some(cookies) = value.get("cookies").and_then(Value::as_array) {
        for cookie in cookies {
            for field in [
                "name",
                "domain",
                "path",
                "secure",
                "httpOnly",
                "sameSite",
                "expiresAt",
            ] {
                if cookie.get(field).is_none() {
                    return Err(format!(
                        "the agent service returned a malformed v2 cookie row: missing {field}"
                    ));
                }
            }
        }
    }
    if let Some(mutations) = value.get("cookieMutations").and_then(Value::as_array) {
        for mutation in mutations {
            let source = mutation
                .get("sourceUrl")
                .and_then(Value::as_str)
                .and_then(|source| url::Url::parse(source).ok())
                .filter(|source| {
                    matches!(source.scheme(), "http" | "https")
                        && source.username().is_empty()
                        && source.password().is_none()
                })
                .ok_or_else(|| {
                    "the agent service returned an invalid cookie mutation source".to_string()
                })?;
            let _ = source;
            let header = mutation
                .get("header")
                .and_then(Value::as_str)
                .filter(|header| {
                    !header.is_empty()
                        && header.len() <= 16 * 1024
                        && !header.contains(['\r', '\n'])
                })
                .ok_or_else(|| {
                    "the agent service returned an invalid cookie mutation header".to_string()
                })?;
            let _ = header;
        }
    }
    let value: V2Response = serde_json::from_value(value)
        .map_err(|error| format!("the agent service returned a malformed v2 response: {error}"))?;
    if !value.success || value.version != 2 {
        return Err("the agent service returned an invalid v2 response marker".into());
    }
    validate_correlation(
        &value.request_id,
        &value.operation_id,
        expected_request_id,
        expected_operation_id,
    )?;
    if let Some(digest) = &value.full_body_sha256
        && (digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
    {
        return Err("the agent service returned an invalid full-body SHA-256 digest".into());
    }
    let body_base64 = value.body_base64;
    let decoded = (!body_base64.is_empty())
        .then(|| base64::engine::general_purpose::STANDARD.decode(&body_base64))
        .transpose()
        .map_err(|error| format!("the agent service returned invalid response base64: {error}"))?;
    let (body, binary) = match decoded {
        Some(bytes) => match String::from_utf8(bytes) {
            Ok(text) => (text, false),
            Err(error) => (
                format!("<binary response: {} bytes>", error.as_bytes().len()),
                true,
            ),
        },
        None => (value.body.unwrap_or_default(), false),
    };
    Ok(Response {
        console: Vec::new(),
        status: value.status,
        reason: value.reason,
        headers: value
            .headers
            .into_iter()
            .map(|header| (header.name, header.value))
            .collect(),
        set_cookies: value.set_cookies,
        cookie_mutations: value
            .cookie_mutations
            .into_iter()
            .map(|mutation| CookieMutation {
                source_url: mutation.source_url,
                header: mutation.header,
            })
            .collect(),
        body,
        body_base64,
        binary,
        final_url: value.final_url,
        http_version: value.http_version,
        received_bytes: value.received_bytes,
        stored_bytes: value.stored_bytes,
        full_body_sha256: value.full_body_sha256,
        timings: ResponseTimings {
            dns_ms: value.timings.dns_ms,
            connect_ms: value.timings.connect_ms,
            tls_ms: value.timings.tls_ms,
            first_byte_ms: value.timings.first_byte_ms,
            download_ms: value.timings.download_ms,
        },
        cookies: value
            .cookies
            .into_iter()
            .map(|cookie| ResponseCookie {
                name: cookie.name,
                domain: cookie.domain,
                path: cookie.path,
                secure: cookie.secure,
                http_only: cookie.http_only,
                same_site: cookie.same_site,
                expires_at: cookie.expires_at,
            })
            .collect(),
        duration_ms: value.duration_ms,
        truncated: value.truncated,
        redirects: value
            .redirects
            .into_iter()
            .map(|redirect| Redirect {
                status: redirect.status,
                from: redirect.from,
                to: redirect.to,
                method: redirect.method,
                cross_origin: redirect.cross_origin,
            })
            .collect(),
        test_results: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        HttpMethod, PreparedMultipartPart, PreparedMultipartValue, RequestId, RequestSettings,
    };

    fn minimal_v2_response() -> Value {
        json!({
            "success": true,
            "version": 2,
            "requestId": "response-1",
            "operationId": "operation-1",
            "status": 204,
            "reason": "No Content",
            "headers": [],
            "setCookies": [],
            "body": "",
            "bodyBase64": "",
            "finalUrl": "https://example.test",
            "httpVersion": "HTTP/2",
            "receivedBytes": 0,
            "storedBytes": 0,
            "full_body_sha256": null,
            "timings": {
                "dnsMs": null, "connectMs": null, "tlsMs": null,
                "firstByteMs": null, "downloadMs": null
            },
            "cookies": [],
            "durationMs": 1,
            "truncated": false,
            "redirects": []
        })
    }

    #[test]
    fn response_requires_service_success_and_status() {
        assert!(
            decode_response(json!({"success": false, "error": "refused"}), None, None).is_err()
        );
        assert!(decode_response(json!({"success": true}), None, None).is_err());
    }

    #[test]
    fn v2_binary_response_remains_byte_faithful() {
        let response = decode_response(
            json!({
                "success": true,
                "version": 2,
                "requestId": "response-1",
                "operationId": "operation-1",
                "status": 200,
                "reason": "OK",
                "headers": [],
                "setCookies": [],
                "body": null,
                "bodyBase64": base64::engine::general_purpose::STANDARD.encode([0xff, 0, 1]),
                "finalUrl": "https://example.test/final",
                "httpVersion": "HTTP/2",
                "receivedBytes": 40,
                "storedBytes": 3,
                "full_body_sha256": null,
                "durationMs": 19,
                "timings": {
                    "dnsMs": 2, "connectMs": null, "tlsMs": null,
                    "firstByteMs": 11, "downloadMs": 6
                },
                "cookies": [{
                    "name": "session", "domain": "example.test", "path": "/",
                    "secure": true, "httpOnly": true, "sameSite": "Lax",
                    "expiresAt": null
                }],
                "redirects": [{
                    "status": 303, "from": "https://example.test/start",
                    "to": "https://example.test/final", "method": "GET",
                    "crossOrigin": false
                }],
                "truncated": true,
            }),
            Some("response-1"),
            Some("operation-1"),
        )
        .unwrap();
        assert!(response.binary);
        assert!(response.truncated);
        assert_eq!(response.http_version, "HTTP/2");
        assert_eq!((response.received_bytes, response.stored_bytes), (40, 3));
        assert_eq!(response.timings.first_byte_ms, Some(11));
        assert_eq!(response.cookies[0].name, "session");
        assert_eq!(response.redirects[0].method, "GET");
        let snapshot = response_snapshot(&response);
        assert_eq!(snapshot.final_url, "https://example.test/final");
        assert_eq!(snapshot.received_bytes, 40);
        assert_eq!(snapshot.cookies[0].name, "session");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(response.body_base64)
                .unwrap(),
            vec![0xff, 0, 1]
        );
    }

    #[test]
    fn v2_response_preserves_source_qualified_cookie_mutations() {
        let mut value = minimal_v2_response();
        value["cookieMutations"] = json!([{
            "sourceUrl": "https://identity.example.test/login",
            "header": "session=ready; Secure; Path=/"
        }]);
        let response = decode_response(value, Some("response-1"), Some("operation-1")).unwrap();
        assert_eq!(response.cookie_mutations.len(), 1);
        assert_eq!(
            response.cookie_mutations[0].source_url,
            "https://identity.example.test/login"
        );

        let mut invalid = minimal_v2_response();
        invalid["cookieMutations"] = json!([{
            "sourceUrl": "file:///tmp/not-an-http-origin",
            "header": "session=wrong; Path=/"
        }]);
        assert!(
            decode_response(invalid, None, None)
                .unwrap_err()
                .contains("invalid cookie mutation source")
        );
    }

    #[test]
    fn v2_response_rejects_missing_metadata_and_malformed_rows() {
        assert!(
            decode_response(
                json!({
                    "success": true,
                    "version": 2,
                    "status": 200
                }),
                None,
                None
            )
            .unwrap_err()
            .contains("malformed v2 response")
        );

        assert!(
            decode_response(
                json!({
                    "success": true,
                    "version": 2,
                    "requestId": "response-1",
                    "operationId": "operation-1",
                    "status": 200,
                    "reason": "OK",
                    "headers": [{"name": "missing-value"}],
                    "setCookies": [],
                    "body": "",
                    "bodyBase64": "",
                    "finalUrl": "https://example.test",
                    "httpVersion": "HTTP/1.1",
                    "receivedBytes": 0,
                    "storedBytes": 0,
                    "full_body_sha256": null,
                    "timings": {
                        "dnsMs": null, "connectMs": null, "tlsMs": null,
                        "firstByteMs": null, "downloadMs": null
                    },
                    "cookies": [],
                    "durationMs": 1,
                    "truncated": false,
                    "redirects": []
                }),
                None,
                None
            )
            .unwrap_err()
            .contains("malformed v2 response")
        );
    }

    #[test]
    fn v2_success_and_error_require_exact_correlation_and_shape() {
        assert!(
            decode_response(
                minimal_v2_response(),
                Some("response-1"),
                Some("another-operation")
            )
            .unwrap_err()
            .contains("another operation")
        );

        let error = json!({
            "success": false,
            "version": 2,
            "requestId": "response-1",
            "operationId": "operation-1",
            "error": "terminal failure"
        });
        assert!(
            decode_response(error.clone(), Some("another-request"), Some("operation-1"))
                .unwrap_err()
                .contains("another request")
        );
        assert_eq!(
            decode_response(error, Some("response-1"), Some("operation-1")).unwrap_err(),
            "terminal failure"
        );

        let mut success_with_extra = minimal_v2_response();
        success_with_extra["unexpected"] = Value::Bool(true);
        assert!(
            decode_response(success_with_extra, None, None)
                .unwrap_err()
                .contains("unknown field")
        );

        let mut error_with_extra = json!({
            "success": false,
            "version": 2,
            "requestId": "response-1",
            "operationId": "operation-1",
            "error": "terminal failure"
        });
        error_with_extra["unexpected"] = Value::Bool(true);
        assert!(
            decode_response(error_with_extra, None, None)
                .unwrap_err()
                .contains("unknown field")
        );
    }

    #[test]
    fn v1_response_remains_compatible() {
        let response = decode_response(
            json!({
                "success": true,
                "status": 204,
                "body": "",
            }),
            None,
            None,
        )
        .unwrap();
        assert_eq!(response.status, 204);
        assert!(!response.binary);
    }

    #[test]
    fn typed_multipart_serialization_keeps_text_and_file_bytes() {
        let path = std::env::temp_dir().join(format!(
            "agentops-workbench-upload-{}.bin",
            std::process::id()
        ));
        std::fs::write(&path, [0, 1, 2, 0xff]).unwrap();
        let request = PreparedRequest {
            request_id: RequestId::new(),
            method: HttpMethod::new("POST").unwrap(),
            url: "https://example.test".into(),
            headers: Vec::new(),
            body: PreparedBody::Multipart(vec![
                PreparedMultipartPart {
                    name: "title".into(),
                    value: PreparedMultipartValue::Text("hello".into()),
                },
                PreparedMultipartPart {
                    name: "asset".into(),
                    value: PreparedMultipartValue::File(path.to_string_lossy().into_owned()),
                },
            ]),
            settings: RequestSettings::default(),
            aws_sigv4: None,
            redactions: Vec::new(),
        };
        let capabilities = FileCapabilities::default();
        capabilities
            .issue_picker_selection(path.clone(), &request.request_id, 0)
            .unwrap();
        let PreparedBody::Multipart(parts) = &request.body else {
            panic!("expected multipart body")
        };
        assert!(matches!(parts[1].value, PreparedMultipartValue::File(_)));
        let (body, content_type) =
            prepared_body(&request.body, request.request_id.as_str(), &capabilities).unwrap();
        let body = body.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("name=\"title\""));
        assert!(body.windows(4).any(|bytes| bytes == [0, 1, 2, 0xff]));
        assert!(
            content_type
                .unwrap()
                .starts_with("multipart/form-data; boundary=")
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn imported_file_paths_cannot_name_picker_grants_and_tokens_are_one_use() {
        let path = std::env::temp_dir().join(format!(
            "agentops-workbench-authorization-{}.txt",
            std::process::id()
        ));
        std::fs::write(&path, b"private fixture").unwrap();
        let body = PreparedBody::File(path.to_string_lossy().into_owned());
        let capabilities = FileCapabilities::default();
        let first_request = RequestId::new();
        let second_request = RequestId::new();

        let refused = prepared_body(&body, first_request.as_str(), &capabilities).unwrap_err();
        assert!(refused.contains("not picker-authorized"));

        let grant = capabilities
            .issue_picker_selection(path.clone(), &first_request, 0)
            .unwrap();
        assert_eq!(grant.filename, path.file_name().unwrap().to_string_lossy());
        assert!(
            prepared_body(&body, second_request.as_str(), &capabilities).is_err(),
            "a picker grant must be bound to the request that selected it"
        );
        let authorized = prepared_body(&body, first_request.as_str(), &capabilities)
            .unwrap()
            .0
            .unwrap();
        assert_eq!(authorized, b"private fixture");
        assert!(
            prepared_body(&body, first_request.as_str(), &capabilities)
                .unwrap_err()
                .contains("not picker-authorized")
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn picker_grants_are_slot_scoped_and_expire() {
        let path = std::env::temp_dir().join(format!(
            "agentops-workbench-expired-{}.txt",
            std::process::id()
        ));
        std::fs::write(&path, b"private fixture").unwrap();
        let request = RequestId::new();
        let capabilities = FileCapabilities::default();
        capabilities
            .issue_picker_selection_with_ttl(path.clone(), &request, 1, Duration::from_millis(0))
            .unwrap();
        assert!(capabilities.consume(request.as_str(), 0).is_err());
        assert!(
            capabilities
                .consume(request.as_str(), 1)
                .unwrap_err()
                .contains("expired")
        );
        let _ = std::fs::remove_file(path);
    }
}

#[test]
fn script_results_preserve_mutations_and_named_assertions() {
    let result = decode_script_result(json!({
        "success": true,
        "version": 1,
        "result": {
            "variables": {"token": "value"},
            "request": {
                "method": "POST",
                "url": "https://example.test/items",
                "headers": [["X-Test", "yes"]],
                "body": "{}"
            },
            "tests": [{"name": "created", "passed": true}]
        }
    }))
    .unwrap();
    assert_eq!(result.variables["token"], "value");
    assert_eq!(result.request.method, "POST");
    assert_eq!(result.tests[0].name, "created");
    assert!(result.tests[0].passed);
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "target", rename_all = "camelCase")]
pub enum ScriptNextRequest {
    Stop,
    Request(String),
}

#[cfg(test)]
mod parity_tests {
    use super::*;
    #[test]
    fn skipped_results_and_branch_commands_survive_wire_and_persistence() {
        let test: ScriptTestResult =
            serde_json::from_str(r#"{"name":"later","passed":false,"skipped":true}"#).unwrap();
        let (tests, _) = script_diagnostics(&[test], &[], "Post-response", &[]);
        let persisted = serde_json::to_string(&tests).unwrap();
        let loaded: Vec<crate::TestResult> = serde_json::from_str(&persisted).unwrap();
        assert!(loaded[0].skipped);
        let old: crate::TestResult =
            serde_json::from_str(r#"{"name":"old","passed":true,"error":null}"#).unwrap();
        assert!(!old.skipped);
        let command = ScriptNextRequest::Request("Login".into());
        assert_eq!(
            serde_json::to_value(command).unwrap(),
            serde_json::json!({"kind":"request","target":"Login"})
        );
        let mut scopes = ScriptScopes::default();
        scopes.globals.insert("key".into(), "global".into());
        scopes.collection.insert("key".into(), "collection".into());
        assert_eq!(scopes.resolved()["key"], "collection");
        scopes
            .environment
            .insert("key".into(), "environment".into());
        scopes.iteration_data.insert("key".into(), "data".into());
        scopes.local.insert("key".into(), "local".into());
        assert_eq!(scopes.resolved()["key"], "local");
    }
}
