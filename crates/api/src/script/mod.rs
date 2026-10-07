//! Sandboxed JavaScript runtime for API Workbench scripts.
//!
//! Boa exposes ECMAScript only. We deliberately do not link `boa_runtime`, so
//! scripts have no filesystem, process, module, or unrestricted network APIs.
//! Timers are bounded inside the worker's existing wall-clock deadline.

use base64::Engine as _;
use boa_engine::{Context, JsNativeError, JsResult, JsString, JsValue, NativeFunction, Source};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt as _;

const MAX_SCRIPT_BYTES: usize = 64 * 1024;
/// A script may inspect a complete API response after it arrives. This is kept
/// separate from request-body mutation, which remains intentionally smaller.
const MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_BODY_BYTES: usize = 16 * 1024 * 1024;
const MAX_VARIABLES: usize = 512;
const MAX_VARIABLE_BYTES: usize = 256 * 1024;
const MAX_RESULT_BYTES: usize = 512 * 1024;
const MAX_CONSOLE_ENTRIES: usize = 256;
const MAX_SUBREQUESTS: usize = 4;
const LOOP_BUDGET: u64 = 100_000;
const SCRIPT_WALL_TIMEOUT: Duration = Duration::from_millis(1_500);
/// JSON framing and escaping add overhead to the largest accepted response.
const MAX_WORKER_REQUEST_BYTES: u64 = 32 * 1024 * 1024;
const WORKER_RESULT_PREFIX: &str = "SWITCHYARD_API_SCRIPT_RESULT:";
/// Hidden argument that turns the app binary into the script worker.
pub const WORKER_ARG: &str = "--switchyard-api-script-worker";

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptRequestView {
    #[serde(default)]
    pub method: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub body: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptExecutionRequest {
    #[serde(default)]
    pub assertions_only: bool,
    #[serde(default)]
    pub globals: BTreeMap<String, String>,
    #[serde(default)]
    pub environment_name: Option<String>,
    #[serde(default)]
    pub next_request: Option<ScriptNextRequest>,
    #[serde(default = "wire_version")]
    pub version: u32,
    pub script: String,
    #[serde(default)]
    pub variables: BTreeMap<String, String>,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default)]
    pub collection_variables: BTreeMap<String, String>,
    #[serde(default)]
    pub local_variables: BTreeMap<String, String>,
    #[serde(default)]
    pub iteration_data: BTreeMap<String, String>,
    /// Named values referenced by script templates, supplied transiently by
    /// the native caller and never included in the returned variable scopes.
    #[serde(default)]
    pub vault: BTreeMap<String, String>,
    /// Transient cookie values selected by the native jar for this URL.
    #[serde(default)]
    pub cookies: BTreeMap<String, String>,
    #[serde(default)]
    pub request: ScriptRequestView,
    #[serde(default)]
    pub response: Option<ScriptResponseView>,
    /// Optional id used only by the service's existing cancellation route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    #[serde(default)]
    pub starts_operation: bool,
    #[serde(default = "default_final_phase")]
    pub final_phase: bool,
    #[serde(default)]
    pub allow_private_network: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) subrequest_responses: Vec<ScriptSubrequestResponse>,
}

const fn default_final_phase() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "target", rename_all = "camelCase")]
pub enum ScriptNextRequest {
    Stop,
    Request(String),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptTestResult {
    #[serde(default)]
    pub skipped: bool,
    pub name: String,
    pub passed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptExecutionResult {
    #[serde(default)]
    pub globals: BTreeMap<String, String>,
    #[serde(default)]
    pub environment_name: Option<String>,
    #[serde(default)]
    pub next_request: Option<ScriptNextRequest>,
    /// Compatibility alias for localVariables.
    pub variables: BTreeMap<String, String>,
    pub environment: BTreeMap<String, String>,
    pub collection_variables: BTreeMap<String, String>,
    pub local_variables: BTreeMap<String, String>,
    pub iteration_data: BTreeMap<String, String>,
    pub cookies: BTreeMap<String, String>,
    pub request: ScriptRequestView,
    pub tests: Vec<ScriptTestResult>,
    pub console: Vec<ScriptConsoleEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptConsoleEntry {
    pub level: String,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ScriptSubrequestResponse {
    #[serde(default)]
    error: Option<String>,
    code: u16,
    status: String,
    headers: Vec<(String, String)>,
    body: String,
    response_time_ms: u64,
}

const fn wire_version() -> u32 {
    1
}

/// Run a script behind a process boundary. This future owns the wall-clock deadline;
/// dropping it (cancellation) kills the child through `kill_on_drop`. Boa's loop and
/// recursion budgets stop runaway scripts inside the worker. AgentOps also capped the
/// child's CPU and memory with `setrlimit` / a Windows job object; both need `unsafe`,
/// which this workspace denies, so they are not ported (DECISIONS 2026-10-07).
pub async fn execute_isolated(
    mut request: ScriptExecutionRequest,
) -> Result<ScriptExecutionResult, String> {
    validate(&request)?;
    request.subrequest_responses.clear();
    for _ in 0..=MAX_SUBREQUESTS {
        match execute_worker_once(&request).await {
            Err(error) if error.contains(SUBREQUEST_MARKER) => {
                if request.assertions_only {
                    return Err(
                        "Network requests are disabled while rerunning response tests".into(),
                    );
                }
                if request.subrequest_responses.len() == MAX_SUBREQUESTS {
                    return Err(format!(
                        "pm.sendRequest exceeds the {MAX_SUBREQUESTS}-subrequest limit"
                    ));
                }
                let subrequest = decode_subrequest(&error)?;
                request.subrequest_responses.push(
                    match execute_subrequest(subrequest, request.allow_private_network).await {
                        Ok(response) => response,
                        Err(error) => ScriptSubrequestResponse {
                            error: Some(error),
                            code: 0,
                            status: String::new(),
                            headers: Vec::new(),
                            body: String::new(),
                            response_time_ms: 0,
                        },
                    },
                );
            }
            result => return result,
        }
    }
    Err("pm.sendRequest exceeded its subrequest limit".into())
}

const SUBREQUEST_MARKER: &str = "SWITCHYARD_API_SUBREQUEST:";

async fn execute_worker_once(
    request: &ScriptExecutionRequest,
) -> Result<ScriptExecutionResult, String> {
    let request = serde_json::to_vec(&request)
        .map_err(|error| format!("encode Workbench script request: {error}"))?;
    let mut command = worker_command()?;
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| format!("start Workbench script sandbox: {error}"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Workbench script sandbox has no stdin".to_string())?;
    stdin
        .write_all(&request)
        .await
        .map_err(|error| format!("send Workbench script to sandbox: {error}"))?;
    drop(stdin);

    let output = tokio::time::timeout(SCRIPT_WALL_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| {
            format!(
                "Workbench script exceeded the {} ms wall-clock limit",
                SCRIPT_WALL_TIMEOUT.as_millis()
            )
        })?
        .map_err(|error| format!("wait for Workbench script sandbox: {error}"))?;
    if !output.status.success() {
        return Err("the script sandbox stopped unexpectedly".into());
    }
    decode_worker_reply(&output.stdout)
}

#[derive(Deserialize)]
struct PendingSubrequest {
    #[serde(default = "default_subrequest_method")]
    method: String,
    url: String,
    #[serde(default)]
    headers: Vec<(String, String)>,
    #[serde(default)]
    body: Option<String>,
}

fn default_subrequest_method() -> String {
    "GET".into()
}

fn decode_subrequest(error: &str) -> Result<PendingSubrequest, String> {
    let payload = error
        .split_once(SUBREQUEST_MARKER)
        .map(|(_, payload)| payload.trim())
        .ok_or_else(|| "invalid pm.sendRequest handoff".to_string())?;
    let mut decoder = serde_json::Deserializer::from_str(payload);
    PendingSubrequest::deserialize(&mut decoder)
        .map_err(|error| format!("invalid pm.sendRequest payload: {error}"))
}

async fn execute_subrequest(
    request: PendingSubrequest,
    allow_private_network: bool,
) -> Result<ScriptSubrequestResponse, String> {
    let response = crate::http::send_api_workbench_request(crate::http::ApiWorkbenchRequest {
        method: request.method,
        url: request.url,
        headers: request.headers,
        body: request.body.map(String::into_bytes),
        content_type: None,
        allow_private_network,
        timeout_ms: 3_000,
        max_redirects: 2,
        response_limit_bytes: 64 * 1024,
        aws_sigv4: None,
    })
    .await
    .map_err(|error| format!("pm.sendRequest failed: {error}"))?;
    if response.truncated {
        return Err("pm.sendRequest response exceeds the 65536-byte limit".into());
    }
    Ok(ScriptSubrequestResponse {
        error: None,
        code: response.status,
        status: response.reason,
        headers: response.headers,
        body: String::from_utf8(response.body)
            .map_err(|_| "pm.sendRequest response must be UTF-8 text".to_string())?,
        response_time_ms: response.duration_ms,
    })
}

fn worker_command() -> Result<tokio::process::Command, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("locate Workbench script sandbox: {error}"))?;
    let mut command = tokio::process::Command::new(executable);
    #[cfg(not(test))]
    command.arg(WORKER_ARG);
    #[cfg(test)]
    command
        .args([
            "--exact",
            "script::tests::isolated_worker_process",
            "--ignored",
            "--nocapture",
        ])
        .env("SWITCHYARD_API_TEST_WORKER", "1");
    Ok(command)
}

#[derive(Serialize, Deserialize)]
struct WorkerReply {
    result: Option<ScriptExecutionResult>,
    error: Option<String>,
}

fn decode_worker_reply(stdout: &[u8]) -> Result<ScriptExecutionResult, String> {
    let stdout = String::from_utf8_lossy(stdout);
    let payload = stdout
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(WORKER_RESULT_PREFIX))
        .ok_or_else(|| "Workbench script sandbox returned no result".to_string())?;
    let reply: WorkerReply = serde_json::from_str(payload)
        .map_err(|error| format!("decode Workbench script sandbox result: {error}"))?;
    match (reply.result, reply.error) {
        (Some(result), None) => Ok(result),
        (None, Some(error)) => Err(error),
        _ => Err("Workbench script sandbox returned an invalid result".into()),
    }
}

/// Hidden subprocess entry point. This is called before normal service
/// dispatch and deliberately accepts exactly one bounded JSON request.
pub fn run_worker(mut input: impl Read, mut output: impl Write) -> u8 {
    let mut bytes = Vec::new();
    let read = input
        .by_ref()
        .take(MAX_WORKER_REQUEST_BYTES.saturating_add(1))
        .read_to_end(&mut bytes);
    let result = match read {
        Ok(_) if bytes.len() as u64 <= MAX_WORKER_REQUEST_BYTES => {
            serde_json::from_slice::<ScriptExecutionRequest>(&bytes)
                .map_err(|error| format!("decode Workbench script request: {error}"))
                .and_then(execute)
                .map_err(normalize_worker_error)
        }
        Ok(_) => Err("Workbench script request exceeds the worker input limit".into()),
        Err(error) => Err(format!("read Workbench script request: {error}")),
    };
    let reply = match result {
        Ok(result) => WorkerReply {
            result: Some(result),
            error: None,
        },
        Err(error) => WorkerReply {
            result: None,
            error: Some(error),
        },
    };
    let encoded = match serde_json::to_string(&reply) {
        Ok(encoded) => encoded,
        Err(_) => return 1,
    };
    if writeln!(output, "{WORKER_RESULT_PREFIX}{encoded}").is_err() {
        return 1;
    }
    0
}

fn normalize_worker_error(error: String) -> String {
    let lower = error.to_ascii_lowercase();
    if lower.contains("invalid layout")
        || lower.contains("out of memory")
        || lower.contains("memory allocation")
    {
        "the script ran out of memory".to_string()
    } else {
        error
    }
}

fn encoding_input(args: &[JsValue]) -> JsResult<String> {
    let value = args.first().and_then(JsValue::as_string).ok_or_else(|| {
        JsNativeError::typ().with_message("Base64 encoding helpers require a string")
    })?;
    if value.len() > MAX_REQUEST_BODY_BYTES {
        return Err(JsNativeError::range()
            .with_message("Base64 input exceeds the 1 MiB limit")
            .into());
    }
    let text = value.to_std_string().map_err(|_| {
        JsNativeError::typ().with_message("Base64 input contains an unpaired Unicode surrogate")
    })?;
    if text.len() > MAX_REQUEST_BODY_BYTES {
        return Err(JsNativeError::range()
            .with_message("Base64 input exceeds the 1 MiB limit")
            .into());
    }
    Ok(text)
}

fn base64_encode(_: &JsValue, args: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    let input = encoding_input(args)?;
    Ok(JsString::from(base64::engine::general_purpose::STANDARD.encode(input.as_bytes())).into())
}

fn base64_decode(_: &JsValue, args: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    let input = encoding_input(args)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(input)
        .map_err(|_| JsNativeError::typ().with_message("Invalid standard Base64 text"))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| JsNativeError::typ().with_message("Decoded Base64 is not valid UTF-8 text"))?;
    Ok(JsString::from(text).into())
}

fn runtime_variable(_: &JsValue, args: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    let name = args
        .first()
        .and_then(JsValue::as_string)
        .ok_or_else(|| JsNativeError::typ().with_message("Runtime variable name must be a string"))?
        .to_std_string()
        .map_err(|_| JsNativeError::typ().with_message("Invalid runtime variable name"))?;
    let timestamp = args
        .get(1)
        .and_then(JsValue::as_number)
        .filter(|value| value.is_finite())
        .ok_or_else(|| JsNativeError::typ().with_message("Invalid runtime timestamp"))?;
    match crate::runtime_variable(&name, timestamp as i64) {
        Ok(value) => Ok(JsString::from(value).into()),
        Err(crate::CompileError::UnresolvedVariable(_)) => Ok(JsValue::undefined()),
        Err(error) => Err(JsNativeError::typ().with_message(error.to_string()).into()),
    }
}

fn script_random_uint32(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    let mut bytes = [0_u8; 4];
    use ring::rand::SecureRandom as _;
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| JsNativeError::error().with_message("random generator unavailable"))?;
    Ok(JsValue::from(u32::from_ne_bytes(bytes)))
}

fn script_module(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let name = args
        .first()
        .unwrap_or(&JsValue::undefined())
        .to_string(context)?
        .to_std_string_escaped();
    let source = match name.as_str() {
        "moment" => include_str!("../../vendor/workbench-js/moment.min.js"),
        "crypto-js" => include_str!("../../vendor/workbench-js/crypto-js.min.js"),
        "cheerio" => include_str!("../../vendor/workbench-js/cheerio.min.js"),
        "xml2js" => include_str!("../../vendor/workbench-js/xml2js.min.js"),
        "chai" => include_str!("../../vendor/workbench-js/chai.min.js"),
        "ajv" => include_str!("../../vendor/workbench-js/ajv.min.js"),
        _ => {
            return Err(JsNativeError::typ()
                .with_message(format!(
                    "Module '{name}' is unavailable in the Workbench sandbox"
                ))
                .into());
        }
    };
    // Each Browserify bundle has a private resolver containing only its pinned
    // dependencies. The user's require cannot access the host filesystem.
    context.eval(Source::from_bytes(&format!(
        "(function(){{var require;{source};return require({});}})()",
        serde_json::Value::from(name.as_str())
    )))
}

fn script_sleep(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let delay = args
        .first()
        .unwrap_or(&JsValue::undefined())
        .to_number(context)?;
    if !delay.is_finite() || !(0.0..=1000.0).contains(&delay) {
        return Err(JsNativeError::range()
            .with_message("Workbench timer delay exceeds 1000 ms")
            .into());
    }
    std::thread::sleep(Duration::from_millis(delay as u64));
    Ok(JsValue::undefined())
}

fn execute(request: ScriptExecutionRequest) -> Result<ScriptExecutionResult, String> {
    // Boa parsing of bundled libraries needs more native stack than Windows'
    // default test/main thread. This stack remains inside the worker's unchanged
    // OS memory and wall-clock limits.
    std::thread::Builder::new()
        .name("workbench-script".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || execute_inner(request))
        .map_err(|error| format!("start script interpreter: {error}"))?
        .join()
        .map_err(|_| "Workbench script interpreter failed".to_string())?
}

fn execute_inner(request: ScriptExecutionRequest) -> Result<ScriptExecutionResult, String> {
    validate(&request)?;
    let initial = serde_json::json!({
        "variables": if request.local_variables.is_empty() { request.variables.clone() } else { request.local_variables.clone() },
        "environment": request.environment,
        "globals": request.globals,
        "assertionsOnly": request.assertions_only,
        "environmentName": request.environment_name,
        "nextRequest": request.next_request,
        "collectionVariables": request.collection_variables,
        "localVariables": if request.local_variables.is_empty() { request.variables } else { request.local_variables },
        "iterationData": request.iteration_data,
        "vault": request.vault,
        "cookies": request.cookies,
        "request": request.request,
        "response": request.response,
        "tests": [],
        "console": [],
        "subrequestResponses": request.subrequest_responses,
    });
    let initial = serde_json::to_string(&initial).map_err(|error| error.to_string())?;
    let mut context = Context::default();
    let limits = context.runtime_limits_mut();
    limits.set_loop_iteration_limit(LOOP_BUDGET);
    limits.set_recursion_limit(128);
    limits.set_stack_size_limit(1024);
    for (name, function) in [
        (
            "__aoBase64Encode",
            base64_encode as boa_engine::native_function::NativeFunctionPointer,
        ),
        ("__aoBase64Decode", base64_decode),
        ("__aoSleep", script_sleep),
        ("__aoLoadModule", script_module),
        ("__aoRandomUint32", script_random_uint32),
        ("__aoRuntimeVariable", runtime_variable),
    ] {
        context
            .register_global_builtin_callable(
                JsString::from(name),
                1,
                NativeFunction::from_fn_ptr(function),
            )
            .map_err(|error| format!("initialize Workbench encoding helpers: {error}"))?;
    }

    let program = format!(
        "globalThis.__ao={initial};\n{}\n{}\n{}",
        include_str!("workbench_assertions.js"),
        PM_BOOTSTRAP,
        request.script
    );
    context
        .eval(Source::from_bytes(&program))
        .map_err(|error| format!("Workbench script failed: {error}"))?;
    for _ in 0..=128 {
        context
            .run_jobs()
            .map_err(|error| format!("Workbench async script failed: {error}"))?;
        let drained = context
            .eval(Source::from_bytes("__aoDrainTimer()"))
            .map_err(|error| format!("Workbench timer failed: {error}"))?;
        if !drained.to_boolean() {
            break;
        }
    }
    context
        .eval(Source::from_bytes("__aoFinish()"))
        .map_err(|error| format!("Workbench async script failed: {error}"))?;
    let output = context
        .eval(Source::from_bytes(
            "JSON.stringify({globals:__ao.globals,environmentName:__ao.environmentName,nextRequest:__ao.nextRequest,variables:__ao.localVariables,environment:__ao.environment,collectionVariables:__ao.collectionVariables,localVariables:__ao.localVariables,iterationData:__ao.iterationData,cookies:__ao.cookies,request:Object.assign({},__ao.request,{headers:__ao.requestHeaders,body:String(__ao.request.body)}),tests:__ao.tests,console:__ao.console})",
        ))
        .map_err(|error| format!("serialize Workbench script result: {error}"))?;
    let output = output
        .as_string()
        .ok_or_else(|| "Workbench script produced a non-string result".to_string())?
        .to_std_string_escaped();
    if output.len() > MAX_RESULT_BYTES {
        return Err(format!(
            "Workbench script result exceeds the {MAX_RESULT_BYTES}-byte limit"
        ));
    }
    let result: ScriptExecutionResult = serde_json::from_str(&output)
        .map_err(|error| format!("decode Workbench script result: {error}"))?;
    validate_result(&result)?;
    Ok(result)
}

fn validate(request: &ScriptExecutionRequest) -> Result<(), String> {
    if request.version != 1 {
        return Err(format!(
            "unsupported Workbench script version {}",
            request.version
        ));
    }
    if request.script.len() > MAX_SCRIPT_BYTES {
        return Err(format!(
            "Workbench script exceeds the {MAX_SCRIPT_BYTES}-byte limit"
        ));
    }
    if request.request.body.len() > MAX_REQUEST_BODY_BYTES {
        return Err(format!(
            "Workbench script request body exceeds the {MAX_REQUEST_BODY_BYTES}-byte limit"
        ));
    }
    if request
        .response
        .as_ref()
        .is_some_and(|response| response.body.len() > MAX_RESPONSE_BODY_BYTES)
    {
        return Err(format!(
            "Workbench response body exceeds the {MAX_RESPONSE_BODY_BYTES}-byte script limit"
        ));
    }
    validate_variables(&request.variables)?;
    validate_variables(&request.environment)?;
    validate_variables(&request.globals)?;
    validate_variables(&request.collection_variables)?;
    validate_variables(&request.local_variables)?;
    validate_variables(&request.iteration_data)?;
    validate_variables(&request.vault)?;
    validate_variables(&request.cookies)
}

fn validate_result(result: &ScriptExecutionResult) -> Result<(), String> {
    validate_variables(&result.variables)?;
    if result.request.body.len() > MAX_REQUEST_BODY_BYTES {
        return Err(format!(
            "Workbench script request body exceeds the {MAX_REQUEST_BODY_BYTES}-byte limit"
        ));
    }
    if result.tests.len() > 512 {
        return Err("Workbench script produced more than 512 tests".into());
    }
    if result.console.len() > MAX_CONSOLE_ENTRIES {
        return Err(format!(
            "Workbench script produced more than {MAX_CONSOLE_ENTRIES} console entries"
        ));
    }
    validate_variables(&result.environment)?;
    validate_variables(&result.globals)?;
    validate_variables(&result.collection_variables)?;
    validate_variables(&result.local_variables)?;
    validate_variables(&result.iteration_data)?;
    validate_variables(&result.cookies)?;
    Ok(())
}

fn validate_variables(variables: &BTreeMap<String, String>) -> Result<(), String> {
    if variables.len() > MAX_VARIABLES {
        return Err(format!(
            "Workbench script has more than {MAX_VARIABLES} variables"
        ));
    }
    let bytes = variables
        .iter()
        .map(|(key, value)| key.len().saturating_add(value.len()))
        .sum::<usize>();
    if bytes > MAX_VARIABLE_BYTES {
        return Err(format!(
            "Workbench script variables exceed the {MAX_VARIABLE_BYTES}-byte limit"
        ));
    }
    Ok(())
}

const PM_BOOTSTRAP: &str = include_str!("workbench_sandbox.js");

#[cfg(test)]
mod tests {
    use super::*;

    fn request(script: &str) -> ScriptExecutionRequest {
        ScriptExecutionRequest {
            assertions_only: false,
            globals: BTreeMap::new(),
            environment_name: None,
            next_request: None,
            version: 1,
            script: script.into(),
            variables: BTreeMap::from([("base".into(), "https://example.test".into())]),
            environment: BTreeMap::new(),
            collection_variables: BTreeMap::new(),
            local_variables: BTreeMap::new(),
            iteration_data: BTreeMap::new(),
            vault: BTreeMap::new(),
            cookies: BTreeMap::new(),
            request: ScriptRequestView {
                method: "GET".into(),
                url: "{{base}}/items".into(),
                ..Default::default()
            },
            response: Some(ScriptResponseView {
                code: 200,
                body: r#"{"ok":true}"#.into(),
                ..Default::default()
            }),
            request_id: None,
            operation_id: None,
            starts_operation: false,
            final_phase: true,
            allow_private_network: false,
            subrequest_responses: Vec::new(),
        }
    }

    #[test]
    fn bundled_libraries_execute_the_guides_date_crypto_html_and_xml_recipes() {
        let input = request(
            r#"
            pm.test('Chai',()=>{require('chai').expect({user:{name:'Jane'}}).to.have.nested.property('user.name','Jane');pm.expect([1,2,3]).to.include.members([2,3])});
            pm.test('date',()=>pm.expect(require('moment')('2021-08-15').add(1,'days').format('DD.MM.YYYY')).to.equal('16.08.2021'));
            pm.test('sha256',()=>pm.expect(CryptoJS.SHA256('abc').toString()).to.equal('ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad'));
            pm.test('hmac',()=>pm.expect(CryptoJS.HmacSHA1('Message','Key').toString()).to.equal('c0334f18a5fbca1030ae7d7863ceca0206ce1712'));
            pm.test('AES',()=>{const encrypted=CryptoJS.AES.encrypt('message','secret').toString();pm.expect(CryptoJS.AES.decrypt(encrypted,'secret').toString(CryptoJS.enc.Utf8)).to.equal('message')});
            pm.test('HTML',()=>{const $=cheerio.load('<meta name="csrf" Content="the code">');pm.expect($("meta[name='csrf']").attr('content')).to.equal('the code')});
            pm.test('XML',()=>pm.expect(xml2Json('<root><name>Jane</name></root>').root.name).to.equal('Jane'));
            pm.test('xml2js',()=>require('xml2js').parseString('<root><name>Jane</name></root>',(error,result)=>{pm.expect(error).to.equal(null);pm.expect(result.root.name[0]).to.equal('Jane')}));
        "#,
        );
        let result = execute(input).unwrap();
        assert!(
            result.tests.iter().all(|test| test.passed),
            "{:?}",
            result.tests
        );
        assert!(
            execute(request("require('fs')"))
                .unwrap_err()
                .contains("unavailable")
        );
    }

    #[tokio::test]
    async fn bundled_libraries_work_inside_the_existing_process_budget() {
        for source in [
            "pm.test('moment',()=>pm.expect(require('moment')('2021-08-15').add(1,'days').format('DD.MM.YYYY')).to.equal('16.08.2021'))",
            "pm.test('crypto',()=>{const c=CryptoJS;pm.expect(c.AES.decrypt(c.AES.encrypt('message','secret'),'secret').toString(c.enc.Utf8)).to.equal('message')})",
            "pm.test('html',()=>pm.expect(cheerio.load('<p>text</p>')('p').text()).to.equal('text'))",
            "pm.test('xml',()=>pm.expect(xml2Json('<root>text</root>').root).to.equal('text'))",
            "pm.test('schema',()=>pm.response.to.have.jsonSchema({type:'object',properties:{ok:{type:'boolean'}}}))",
        ] {
            let result = execute_isolated(request(source))
                .await
                .unwrap_or_else(|error| panic!("{source}: {error}"));
            assert!(
                result.tests.iter().all(|test| test.passed),
                "{:?}",
                result.tests
            );
        }
    }

    #[test]
    fn schema_local_references_and_formats_validate_real_values() {
        let result = execute(request(r##"
            const schema={definitions:{email:{type:'string',format:'email'}},type:'object',properties:{email:{$ref:'#/definitions/email'}},required:['email']};
            pm.test('valid',()=>workbenchValidateSchema({email:'jane@example.com'},schema));
            pm.test('invalid format',()=>workbenchValidateSchema({email:'broken'},schema));
            pm.test('remote ref',()=>workbenchValidateSchema({},{$ref:'https://example.test/schema'}));
            pm.test('unknown keyword',()=>workbenchValidateSchema({},{typo:true}));
            pm.test('unknown format',()=>workbenchValidateSchema('a',{format:'made-up'}));
        "##)).unwrap();
        assert_eq!(
            result
                .tests
                .iter()
                .map(|test| test.passed)
                .collect::<Vec<_>>(),
            [true, false, false, false, false]
        );
    }

    #[tokio::test]
    async fn assertion_reruns_block_network_even_for_forged_handoffs() {
        for script in [
            "const send=pm.sendRequest;send('https://example.test')",
            "throw new Error('SWITCHYARD_API_SUBREQUEST:'+JSON.stringify({method:'GET',url:'https://example.test',headers:[],body:null}))",
        ] {
            let mut input = request(script);
            input.assertions_only = true;
            let error = execute_isolated(input).await.unwrap_err();
            assert!(error.contains("Network requests are disabled"), "{error}");
        }
    }

    #[test]
    fn body_objects_and_legacy_assignment_change_the_wire_body() {
        for script in [
            "pm.request.body.raw = '{\"changed\":true}';",
            "pm.request.body.update({mode:'raw',raw:'{\"changed\":true}'});",
            "pm.request.body.update('{\"changed\":true}');",
            "pm.request.body = '{\"changed\":true}';",
        ] {
            let result = execute(request(script)).unwrap();
            assert_eq!(result.request.body, r#"{"changed":true}"#);
        }
        assert!(
            execute(request("pm.request.body.update({mode:'file'});"))
                .unwrap_err()
                .contains("raw body")
        );
    }

    #[test]
    fn globals_environment_metadata_and_flow_round_trip() {
        let mut input = request(
            "pm.test('scope',()=>{pm.expect(pm.variables.get('token')).to.equal('environment');pm.expect(pm.globals.get('token')).to.equal('global');pm.expect(pm.environment.name).to.equal('Development')});pm.globals.set('saved',42);postman.setNextRequest('Login');pm.execution.setNextRequest(null);",
        );
        input.environment_name = Some("Development".into());
        input.globals.insert("token".into(), "global".into());
        input
            .environment
            .insert("token".into(), "environment".into());
        let result = execute(input).unwrap();
        assert!(result.tests[0].passed);
        assert_eq!(result.globals["saved"], "42");
        assert_eq!(result.next_request, Some(ScriptNextRequest::Stop));
        assert_eq!(result.environment_name.as_deref(), Some("Development"));
        let mut input = request("");
        input.next_request = Some(ScriptNextRequest::Request("Next".into()));
        assert_eq!(
            execute(input).unwrap().next_request,
            Some(ScriptNextRequest::Request("Next".into()))
        );
    }

    #[test]
    fn skipped_async_and_timer_tests_have_accurate_results() {
        let result = execute(request(r#"
            pm.test.skip('skip',()=>{throw new Error('must never execute')});
            pm.test('promise',async()=>{await Promise.resolve();pm.expect(2).to.equal(2)});
            pm.test('timer',done=>setTimeout(()=>{pm.variables.set('timer','done');done()},1));
            pm.test('timer failure',done=>setTimeout(()=>{pm.expect(1).to.equal(2);done()},0));
            pm.test('missing done',done=>{});
            const cancelled=setTimeout(()=>{throw new Error('cancelled timer ran')},0);clearTimeout(cancelled);
        "#)).unwrap();
        assert!(result.tests[0].skipped);
        assert!(!result.tests[0].passed);
        assert!(result.tests[1].passed);
        assert!(result.tests[2].passed);
        assert!(!result.tests[3].passed);
        assert!(!result.tests[4].passed);
        assert!(
            result.tests[4]
                .error
                .as_ref()
                .unwrap()
                .contains("did not complete")
        );
        assert_eq!(result.variables["timer"], "done");
        assert!(
            execute(request("setTimeout(()=>{},1001)"))
                .unwrap_err()
                .contains("1000")
        );
    }

    #[test]
    fn asynchronous_subrequests_still_reach_the_host() {
        for source in [
            "pm.test('async',async()=>{await Promise.resolve();pm.sendRequest('https://example.test')})",
            "pm.test('timer',done=>setTimeout(()=>{pm.sendRequest('https://example.test');done()},0))",
        ] {
            let error = execute(request(source)).unwrap_err();
            assert_eq!(
                decode_subrequest(&error).unwrap().url,
                "https://example.test"
            );
        }
    }

    #[test]
    fn subrequest_forms_preserve_fields_and_reject_files() {
        let error = execute(request("pm.sendRequest({url:'https://example.test',body:{mode:'urlencoded',urlencoded:[{key:'a b',value:'x&y'},{key:'no',value:'secret',disabled:true}]}})")).unwrap_err();
        let pending = decode_subrequest(&error).unwrap();
        assert_eq!(pending.body.as_deref(), Some("a+b=x%26y"));
        assert!(
            pending
                .headers
                .iter()
                .any(|(k, v)| k == "Content-Type" && v == "application/x-www-form-urlencoded")
        );
        let error = execute(request("pm.sendRequest({url:'https://example.test',body:{mode:'formdata',formdata:[{key:'name',value:'Jane'}]}})")).unwrap_err();
        let pending = decode_subrequest(&error).unwrap();
        assert!(
            pending
                .body
                .unwrap()
                .contains("name=\"name\"\r\n\r\nJane\r\n")
        );
        assert!(execute(request("pm.sendRequest({url:'https://example.test',body:{mode:'formdata',formdata:[{key:'file',type:'file',src:'private'}]}})")).unwrap_err().contains("file bodies are unavailable"));
    }

    #[test]
    fn subrequest_callbacks_receive_transport_errors_and_response_stream_text() {
        let mut input = request(
            "pm.sendRequest('https://example.test',(err,res)=>{pm.variables.set('error',err.message);pm.expect(res).to.equal(null)})",
        );
        input.subrequest_responses.push(ScriptSubrequestResponse {
            error: Some("connection refused".into()),
            code: 0,
            status: String::new(),
            headers: Vec::new(),
            body: String::new(),
            response_time_ms: 0,
        });
        assert_eq!(
            execute(input).unwrap().variables["error"],
            "connection refused"
        );
        let mut input = request(
            "pm.sendRequest('https://example.test',(err,res)=>pm.variables.set('stream',res.stream.toString()))",
        );
        input.subrequest_responses.push(ScriptSubrequestResponse {
            error: None,
            code: 200,
            status: "OK".into(),
            headers: Vec::new(),
            body: "text".into(),
            response_time_ms: 0,
        });
        assert_eq!(execute(input).unwrap().variables["stream"], "text");
    }

    #[test]
    fn postman_assertions_reject_false_positives_and_support_common_chains() {
        let cases = [
            ("pm.expect({role:'user'}).to.include({role:'admin'})", false),
            ("pm.expect({code:200}).to.have.property('code',404)", false),
            ("pm.expect(undefined).to.exist", false),
            ("pm.expect(null).to.exist", false),
            ("pm.expect(false).to.exist", true),
            ("pm.expect(1).to.unsupportedMatcher", false),
            ("pm.expect({a:1,b:2}).to.eql({b:2,a:1})", true),
            ("pm.expect({a:1}).to.equal({a:1})", false),
            ("pm.expect({a:1}).to.deep.equal({a:1})", true),
            ("pm.expect([1,2]).to.eql([2,1])", false),
            ("pm.expect({a:undefined}).to.eql({})", false),
            (
                "pm.expect({name:'Jane',id:1}).to.include({name:'Jane'})",
                true,
            ),
            (
                "pm.expect({nested:{a:1}}).to.deep.include({nested:{a:1}})",
                true,
            ),
            (
                "pm.expect({code:200}).to.have.property('code',200).that.is.a('number')",
                true,
            ),
            ("pm.expect({}).to.not.have.property('code')", true),
            ("pm.expect(200).to.be.oneOf([200,201])", true),
            ("pm.expect(400).to.be.oneOf([200,201])", false),
            ("pm.expect([1]).to.be.an('array').that.is.not.empty", true),
            ("pm.expect([]).to.be.an('array').that.is.not.empty", false),
            ("pm.expect('hello').to.match(/^hell/)", true),
            ("pm.expect('hello').to.not.include('goodbye')", true),
            ("pm.expect([1,2]).to.not.include(2)", false),
            ("pm.expect.fail('deliberate failure')", false),
            ("pm.response.to.have.status(200)", true),
            ("pm.response.to.have.status(404)", false),
            ("pm.response.to.have.body('{\"ok\":true}')", true),
            ("pm.response.to.have.header('missing')", false),
            ("pm.response.to.have.unknown", false),
        ];
        for (assertion, passed) in cases {
            let result = execute(request(&format!("pm.test('case',()=>{{{assertion};}});")))
                .unwrap_or_else(|error| panic!("{assertion}: {error}"));
            assert_eq!(result.tests.len(), 1, "{assertion}");
            assert_eq!(
                result.tests[0].passed, passed,
                "{assertion}: {:?}",
                result.tests
            );
            if !passed {
                assert!(result.tests[0].error.is_some(), "{assertion}");
            }
        }
        let mut input = request(
            "pm.test('header',()=>pm.response.to.have.header('x-test','value')); pm.test('wrong header',()=>pm.response.to.have.header('x-test','wrong'));",
        );
        input.response.as_mut().unwrap().headers = vec![("X-Test".into(), "value".into())];
        let tests = execute(input).unwrap().tests;
        assert!(tests[0].passed);
        assert!(!tests[1].passed);
    }

    #[test]
    fn json_schema_checks_values_and_rejects_invalid_schemas() {
        let cases = [
            (
                r#"{type:'object',required:['ok'],properties:{ok:{type:'boolean'}},additionalProperties:false}"#,
                true,
                "",
            ),
            (
                r#"{type:'object',required:['missing']}"#,
                false,
                "required property",
            ),
            (
                r#"{properties:{ok:{type:'string'}}}"#,
                false,
                "should be string",
            ),
            (
                r#"{additionalProperties:false}"#,
                false,
                "additional properties",
            ),
            (r#"{enum:[{ok:true}]}"#, true, ""),
            (r#"{const:{ok:false}}"#, false, "equal to constant"),
            (r#"{oneOf:[{type:'object'},{type:'string'}]}"#, true, ""),
            (r#"{oneOf:[true,true]}"#, false, "exactly one"),
            (
                r#"{anyOf:[{type:'string'},{type:'number'}]}"#,
                false,
                "match some schema",
            ),
            (r#"{not:{type:'object'}}"#, false, "NOT be valid"),
            (r#"{properties:{absent:{format:'email'}}}"#, true, ""),
            (
                r#"{anyOf:[true,{$ref:'#/definitions/user'}]}"#,
                false,
                "can't resolve reference",
            ),
            (r#"{type:'typo'}"#, false, "schema is invalid"),
            (r#"{required:'ok'}"#, false, "schema is invalid"),
            (r#"{items:[]}"#, false, "schema is invalid"),
            (r#"{minimum:'1'}"#, false, "schema is invalid"),
        ];
        for (schema, passed, error) in cases {
            let result = execute(request(&format!(
                "pm.test('schema',()=>pm.response.to.have.jsonSchema({schema}));"
            )))
            .unwrap();
            assert_eq!(
                result.tests[0].passed, passed,
                "{schema}: {:?}",
                result.tests
            );
            if !passed {
                assert!(
                    result.tests[0].error.as_ref().unwrap().contains(error),
                    "{schema}: {:?}",
                    result.tests
                );
            }
        }
        let mut input = request(
            r#"
            pm.test('array',()=>pm.response.to.have.jsonSchema({type:'array',minItems:2,maxItems:3,uniqueItems:true,items:{type:'object',required:['name','age'],properties:{name:{type:'string',minLength:1,maxLength:8,pattern:'^[A-Z]'},age:{type:'integer',minimum:0,exclusiveMaximum:100}}}}));
            pm.test('unicode length',()=>workbenchValidateSchema('😀',{type:'string',maxLength:1}));
            pm.test('unique objects',()=>workbenchValidateSchema([{a:1,b:2},{b:2,a:1}],{uniqueItems:true}));
            pm.test('unknown options',()=>pm.response.to.have.jsonSchema({}, {coerceTypes:true}));
        "#,
        );
        input.response.as_mut().unwrap().body =
            r#"[{"name":"Jane","age":30},{"name":"John","age":31}]"#.into();
        let result = execute(input).unwrap();
        assert_eq!(
            result
                .tests
                .iter()
                .map(|test| test.passed)
                .collect::<Vec<_>>(),
            [true, true, false, false]
        );
    }

    #[test]
    fn subrequests_parse_string_headers_and_reject_structured_body_corruption() {
        let error = execute(request(r#"pm.sendRequest({url:'https://example.test',method:'POST',header:'X-Foo: foo\r\nX-Url: https://example.test',body:{mode:'raw',raw:'{"ok":true}'}});"#)).unwrap_err();
        let pending = decode_subrequest(&error).unwrap();
        assert_eq!(
            pending.headers,
            [
                ("X-Foo".into(), "foo".into()),
                ("X-Url".into(), "https://example.test".into())
            ]
        );
        assert_eq!(pending.body.as_deref(), Some(r#"{"ok":true}"#));
        for body in ["{mode:'file',src:'private.txt'}", "{anything:'else'}"] {
            let error = execute(request(&format!(
                "pm.sendRequest({{url:'https://example.test',body:{body}}});"
            )))
            .unwrap_err();
            assert!(error.contains("supports only text"), "{error}");
            assert!(
                !error.contains(SUBREQUEST_MARKER),
                "must reject before network handoff"
            );
        }
        let error = execute(request(
            "pm.sendRequest({url:'https://example.test',header:'invalid'});",
        ))
        .unwrap_err();
        assert!(error.contains("header must be Name: value"));
    }

    #[test]
    fn subrequests_inside_tests_reach_the_host_and_async_failures_are_recorded() {
        let error = execute(request(
            "pm.test('subrequest',()=>pm.sendRequest('https://example.test'));",
        ))
        .unwrap_err();
        assert_eq!(
            decode_subrequest(&error).unwrap().url,
            "https://example.test"
        );
        let result = execute(request(
            "pm.test('async',async()=>{throw new Error('later failure')});",
        ))
        .unwrap();
        assert!(!result.tests[0].passed);
        assert!(
            result.tests[0]
                .error
                .as_ref()
                .unwrap()
                .contains("later failure")
        );
    }

    #[test]
    fn runtime_variables_are_available_in_both_script_phases() {
        for post_response in [false, true] {
            let mut input = request(
                r#"
                const id = pm.variables.get('$uuid');
                pm.test('UUID v4', () => pm.expect(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(id)).to.equal(true));
                pm.test('cached UUID', () => pm.expect(pm.variables.replaceIn('{{$uuid}}')).to.equal(id));
                pm.test('timestamp snapshot', () => pm.expect(Number(pm.variables.get('$timestamp'))).to.equal(Math.floor(Number(pm.variables.get('$timestampMs')) / 1000)));
                pm.test('date alias', () => pm.expect(pm.variables.get('DateFrom')).to.equal(pm.variables.get('$date')));
                pm.test('nested alias', () => pm.expect(pm.variables.replaceIn('{{Nested}}')).to.equal(pm.variables.get('$date')));
                pm.test('shadow', () => pm.expect(pm.variables.get('$time')).to.equal('override'));
                pm.test('missing', () => pm.expect(pm.variables.get('missing')).to.equal(undefined));
                pm.variables.set('capturedId', id);
                pm.variables.set('random', pm.variables.get('$randomInt'));
                pm.variables.set('iso', pm.variables.get('$isoTimestamp'));
            "#,
            );
            if !post_response {
                input.response = None;
            }
            input
                .variables
                .insert("DateFrom".into(), "{{$datetime:%Y-%m-%d}}".into());
            input
                .variables
                .insert("Nested".into(), "{{DateFrom}}".into());
            input.variables.insert("$time".into(), "override".into());
            let result = execute(input).unwrap();
            assert!(
                result.tests.iter().all(|test| test.passed),
                "{:?}",
                result.tests
            );
            assert_eq!(result.variables["capturedId"].len(), 36);
            assert!(result.variables["random"].parse::<u32>().unwrap() <= 1000);
            assert!(result.variables["iso"].ends_with('Z'));
            assert!(!result.variables.contains_key("$uuid"));
            assert_eq!(result.variables["DateFrom"], "{{$datetime:%Y-%m-%d}}");
        }
    }

    #[test]
    fn runtime_script_templates_reject_invalid_formats_and_cycles() {
        for expression in ["{{$datetime:}}", "{{$datetime:%Q}}"] {
            let script = format!(
                "pm.variables.replaceIn({})",
                serde_json::to_string(expression).unwrap()
            );
            assert!(execute(request(&script)).unwrap_err().contains("format"));
        }
        let mut input = request("pm.variables.get('cycle')");
        input.variables.insert("cycle".into(), "{{cycle}}".into());
        assert!(execute(input).unwrap_err().contains("Variable cycle"));
    }

    #[test]
    fn unsupported_dynamic_templates_fail_instead_of_silently_erasing_values() {
        for source in [
            "pm.variables.set('name', pm.variables.replaceIn('name={{$notImplementedFirstName}}'));",
            "pm.environment.replaceIn('{{$notImplementedEmail}}');",
            "pm.collectionVariables.replaceIn('{{$notImplementedUUID}}');",
        ] {
            let error = execute(request(source)).unwrap_err();
            assert!(
                error.contains("Unsupported Workbench dynamic variable"),
                "{error}"
            );
        }
        let result = execute(request(
            r#"
            pm.variables.set('$notImplementedFirstName', 'provided');
            pm.variables.set('name', pm.variables.replaceIn('{{$notImplementedFirstName}}'));
            pm.variables.set('optional', pm.variables.replaceIn('prefix{{ordinaryMissing}}'));
            pm.variables.set('id', pm.variables.replaceIn('{{$guid}}'));
        "#,
        ))
        .unwrap();
        assert_eq!(result.variables["name"], "provided");
        assert_eq!(result.variables["optional"], "prefix");
        assert_eq!(result.variables["id"].len(), 36);
    }

    #[test]
    fn postman_pdf_schema_recipes_accept_valid_and_reject_invalid_responses() {
        // Printed pp. 25–26: object, optional property, required property,
        // nested required properties. Exercise the native Boa API end to end.
        type Recipe = (&'static str, &'static str, &'static [(&'static str, bool)]);
        let recipes: &[Recipe] = &[
            (
                "object",
                r#"{"type":"object"}"#,
                &[("{}", true), ("[]", false)],
            ),
            (
                "optional property",
                r#"{"type":"object","properties":{"code":{"type":"string"}}}"#,
                &[
                    ("{}", true),
                    (r#"{"code":"FX002"}"#, true),
                    (r#"{"code":2}"#, false),
                ],
            ),
            (
                "required property",
                r#"{"type":"object","properties":{"code":{"type":"string"}},"required":["code"]}"#,
                &[(r#"{"code":"FX002"}"#, true), ("{}", false)],
            ),
            (
                "nested required property",
                r#"{"type":"object","properties":{"code":{"type":"string"},"error":{"type":"object","properties":{"message":{"type":"string"}},"required":["message"]}},"required":["code","error"]}"#,
                &[
                    (r#"{"code":"2","error":{"message":"Not permitted."}}"#, true),
                    (r#"{"code":"2","error":{}}"#, false),
                    (r#"{"code":"2","error":[]}"#, false),
                ],
            ),
        ];
        for (name, schema, responses) in recipes {
            for (body, passed) in *responses {
                let mut input = request(&format!(
                    "pm.test('PDF schema recipe',()=>pm.response.to.have.jsonSchema({schema}));"
                ));
                input.response.as_mut().unwrap().body = body.to_string();
                let result = execute(input).unwrap();
                assert_eq!(result.tests.len(), 1);
                assert_eq!(
                    result.tests[0].passed, *passed,
                    "{name}, {body}: {:?}",
                    result.tests
                );
            }
        }
    }

    #[test]
    fn scripts_encode_credentials_and_replace_payload_variables() {
        let mut input = request(
            r#"
            const credentials = pm.variables.get('username') + ':' + pm.variables.get('password');
            pm.variables.set('credentials', pm.encoding.base64Encode(credentials));
            pm.request.body = pm.variables.replaceIn(pm.request.body);
            "#,
        );
        input.variables.insert("username".into(), "username".into());
        input.variables.insert("password".into(), "password".into());
        input.request.body = r#"{"credentials":"{{credentials}}"}"#.into();
        let result = execute(input).unwrap();
        assert_eq!(result.variables["credentials"], "dXNlcm5hbWU6cGFzc3dvcmQ=");
        assert_eq!(
            result.request.body,
            r#"{"credentials":"dXNlcm5hbWU6cGFzc3dvcmQ="}"#
        );
    }

    #[test]
    fn scripts_base64_round_trip_utf8_and_padding() {
        let result = execute(request(
            r#"
            for (const [text, encoded] of [['',''], ['f','Zg=='], ['fo','Zm8='], ['foo','Zm9v'], ['ü:密🔑','w7w65a+G8J+UkQ==']]) {
                pm.test('encode ' + text, () => pm.expect(pm.encoding.base64Encode(text)).to.equal(encoded));
                pm.test('decode ' + text, () => pm.expect(pm.encoding.base64Decode(encoded)).to.equal(text));
            }
            "#,
        )).unwrap();
        assert_eq!(result.tests.len(), 10);
        assert!(
            result.tests.iter().all(|test| test.passed),
            "{:?}",
            result.tests
        );
    }

    #[test]
    fn scripts_base64_reject_invalid_inputs() {
        for script in [
            "pm.encoding.base64Encode()",
            "pm.encoding.base64Encode(42)",
            "pm.encoding.base64Encode('\\uD800')",
            "pm.encoding.base64Decode(null)",
            "pm.encoding.base64Decode('!@#$')",
            "pm.encoding.base64Decode('Zg')",
            "pm.encoding.base64Decode('Zh==')",
            "pm.encoding.base64Decode('/w==')",
        ] {
            let error = execute(request(script)).unwrap_err();
            assert!(error.contains("TypeError"), "{script}: {error}");
        }
        let error = execute(request("pm.encoding.base64Encode('a'.repeat(1048577))")).unwrap_err();
        assert!(error.contains("limit"), "{error}");
        let error = execute(request("pm.encoding.base64Encode('密'.repeat(400000))")).unwrap_err();
        assert!(error.contains("limit"), "{error}");
    }

    #[test]
    fn scripts_mutate_variables_requests_and_record_tests() {
        let result = execute(request(
            r#"
            pm.variables.set('token','secret');
            pm.request.url=pm.variables.replaceIn(pm.request.url);
            pm.request.headers.upsert({key:'X-Test',value:'yes'});
            pm.test('status',()=>pm.expect(pm.response).to.have.status(200));
            pm.test('json',()=>pm.expect(pm.response.json().ok).to.be.true);
        "#,
        ))
        .unwrap();
        assert_eq!(result.variables["token"], "secret");
        assert_eq!(result.request.url, "https://example.test/items");
        assert_eq!(
            result.request.headers,
            vec![("X-Test".into(), "yes".into())]
        );
        assert!(result.tests.iter().all(|test| test.passed));
    }

    #[test]
    fn scripts_read_vault_templates_and_resolve_variable_precedence() {
        let mut input = request(
            r#"
            pm.test('environment secret',()=>pm.expect(pm.variables.get('environmentOnly')).to.equal('environment-secret'));
            pm.test('collection secret',()=>pm.expect(pm.variables.get('collectionOnly')).to.equal('collection-secret'));
            pm.test('local precedence',()=>pm.expect(pm.variables.get('same')).to.equal('local'));
            pm.variables.unset('same');
            pm.test('data precedence',()=>pm.expect(pm.variables.get('same')).to.equal('data'));
            pm.test('vault template',()=>pm.expect(pm.variables.replaceIn('{{vault.shared}}')).to.equal("quote'\\n{{vault.literal}}-value"));
            pm.request.headers.upsert({key:'Authorization',value:'Basic '+pm.encoding.base64Encode(pm.variables.get('environmentOnly')+':'+pm.variables.replaceIn('{{vault.shared}}'))});
            pm.test('scope output',()=>pm.expect(pm.variables.toObject().same).to.equal('data'));
            pm.test('no prototype values',()=>pm.expect(pm.variables.get('toString')).to.equal(undefined));
        "#,
        );
        input.variables.clear();
        input.environment = BTreeMap::from([
            ("environmentOnly".into(), "environment-secret".into()),
            ("same".into(), "environment".into()),
        ]);
        input.collection_variables = BTreeMap::from([
            ("collectionOnly".into(), "collection-secret".into()),
            ("same".into(), "collection".into()),
        ]);
        input.iteration_data.insert("same".into(), "data".into());
        input.local_variables.insert("same".into(), "local".into());
        input
            .vault
            .insert("shared".into(), "quote'\\n{{vault.literal}}-value".into());
        let result = execute(input).unwrap();
        assert!(
            result.tests.iter().all(|test| test.passed),
            "{:?}",
            result.tests
        );
        assert!(result.local_variables.is_empty());
        assert!(result.variables.is_empty());
        assert_eq!(
            result.request.headers,
            vec![(
                "Authorization".into(),
                format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD
                        .encode("environment-secret:quote'\\n{{vault.literal}}-value")
                )
            )]
        );
        assert!(
            !serde_json::to_value(&result)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("vault")
        );

        let error = execute(request("pm.variables.replaceIn('{{vault.missing}}')")).unwrap_err();
        assert!(
            error.contains("Vault secret 'missing' is missing or unavailable"),
            "{error}"
        );
    }

    #[test]
    fn scripts_keep_scopes_cookies_and_console_distinct() {
        let mut input = request(
            r#"
            pm.environment.set('same','environment-next');
            pm.collectionVariables.set('same','collection-next');
            pm.variables.set('same','local-next');
            pm.cookies.set('session','new-cookie');
            console.warn('scope', pm.environment.get('same'));
            pm.test('iteration data is read-only',()=>pm.expect(typeof pm.iterationData.set).to.equal('undefined'));
        "#,
        );
        input
            .environment
            .insert("same".into(), "environment".into());
        input
            .collection_variables
            .insert("same".into(), "collection".into());
        input.local_variables.insert("same".into(), "local".into());
        input
            .iteration_data
            .insert("row".into(), "iteration-only".into());
        input.cookies.insert("session".into(), "old-cookie".into());

        let result = execute(input).unwrap();
        assert_eq!(result.environment["same"], "environment-next");
        assert_eq!(result.collection_variables["same"], "collection-next");
        assert_eq!(result.local_variables["same"], "local-next");
        assert_eq!(result.iteration_data["row"], "iteration-only");
        assert!(!result.local_variables.contains_key("row"));
        assert_eq!(result.cookies["session"], "new-cookie");
        assert_eq!(result.console[0].level, "warn");
        assert_eq!(result.console[0].message, "scope environment-next");
    }

    #[test]
    fn scripts_have_no_host_io_and_infinite_loops_are_stopped() {
        let error = execute(request("fetch('https://example.test')")).unwrap_err();
        assert!(error.contains("fetch is unavailable"), "{error}");
        let error = execute(request("while(true){}")).unwrap_err();
        assert!(error.to_ascii_lowercase().contains("runtime"), "{error}");
    }

    #[test]
    #[ignore = "subprocess entry point"]
    fn isolated_worker_process() {
        if std::env::var_os("SWITCHYARD_API_TEST_WORKER").is_some() {
            std::process::exit(run_worker(std::io::stdin().lock(), std::io::stdout()).into());
        }
    }

    #[tokio::test]
    async fn isolated_scripts_enforce_wall_clock_and_memory_ceilings() {
        let wall = execute_isolated(request("RegExp('(a+)+$').test('a'.repeat(60000)+'!')"))
            .await
            .unwrap_err();
        assert!(
            wall.contains("wall-clock") || wall.contains("CPU"),
            "{wall}"
        );

        let memory = execute_isolated(request(
            "globalThis.huge = new Uint8Array(300 * 1024 * 1024); huge.fill(7)",
        ))
        .await
        .unwrap_err();
        assert!(
            memory.contains("memory ceiling") || memory.contains("wall-clock"),
            "{memory}"
        );
    }

    #[tokio::test]
    async fn isolated_send_request_uses_the_bounded_outbound_policy() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}")
                .await
                .unwrap();
        });
        let mut input = request(&format!(
            "pm.test('subrequest', () => pm.sendRequest('http://{address}/sub', (err, res) => {{ pm.expect(err).to.equal(null); res.to.have.status(200); pm.variables.set('sub', res.json().ok); }}));"
        ));
        input.allow_private_network = true;
        let result = execute_isolated(input).await.unwrap();
        assert_eq!(result.variables["sub"], "true");
        assert_eq!(result.tests.len(), 1);
        assert!(result.tests[0].passed, "{:?}", result.tests);

        let callback = execute_isolated(request(
            "pm.sendRequest('http://127.0.0.1:1/private', error => pm.variables.set('denied',error.message));",
        )).await.unwrap();
        assert!(callback.variables["denied"].contains("private, loopback, or link-local"));

        let blocked = execute_isolated(request("pm.sendRequest('http://127.0.0.1:1/private');"))
            .await
            .unwrap_err();
        assert!(
            blocked.contains("private, loopback, or link-local"),
            "{blocked}"
        );
    }
}
