//! One request, sent the way the desktop composer sends it: session secrets
//! persisted, managed OAuth renewed, pre-request script run, sign-in sessions
//! resolved, the request compiled and its cookies applied, then the send,
//! the 401 re-sign-in, cookie ingest and the post-response script.
//!
//! [`prepare_standalone_send`] and [`execute_standalone_send`] are split so a
//! caller can show the compiled request before the network phase; a headless
//! caller simply runs one after the other.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    AuthConfig, Body, Collection, CompileContext, CookieJar, Environment, Exchange, ExchangeId,
    Folder, FolderId, HttpMethod, KeyValueRow, PreparedRequest, RedactedRequestSnapshot, RowId,
    SavedRequest, SecretRef, SecretResolver, SecretStore, SecretValue, TestResult, Variable,
    VariableValue, WorkbenchStore, WorkspaceId, compile_request_with_folder_chain,
};

use super::login::{LoginChain, LoginOwner, LoginSession, replace_bearer};
use super::oauth::{
    BrowserAuthorization, OAuthVariables, PKCE_NEEDS_BROWSER, describe_missing_auth_secret,
    finish_browser_authorization, pkce_needs_browser, refresh_environment_oauth,
    refresh_expired_oauth_token_in_context, send_auth_owner,
};
use super::secrets::{DraftSecrets, parse_session_variables};
use super::transport::{
    FileCapabilities, OperationPhase, Response, ScriptRequestView, ScriptResponseView,
    ScriptResult, ScriptScopes, WorkbenchTransport, response_snapshot,
};

/// Everything one send needs, assembled by the caller from its workspace.
pub struct StandaloneSendInput {
    pub operation_id: String,
    pub definition: SavedRequest,
    pub collection: Option<Collection>,
    pub folders: Vec<Folder>,
    /// The saved active environment — where a renewed sign-in cache lands.
    pub environment: Option<Environment>,
    pub environment_source: String,
    pub environment_scope: String,
    pub environment_base_url: String,
    pub environment_auth: AuthConfig,
    /// A PKCE grant the send finishes before compiling — planned by the
    /// desktop app, where the browser could be opened. Headless callers
    /// leave this `None`; a send that needs the browser then fails closed.
    pub browser_authorization: Option<BrowserAuthorization>,
    pub secrets: DraftSecrets,
    pub workspace: WorkspaceId,
    pub store: Arc<WorkbenchStore>,
    pub secret_store: Arc<dyn SecretStore>,
    pub cookie_jar: CookieJar,
    pub manual_cookies: Vec<(String, String)>,
    pub file_capabilities: FileCapabilities,
    pub transport: Arc<dyn WorkbenchTransport>,
}

/// The outcome of one send, before it is turned into an [`Exchange`].
pub struct StandaloneSendResult {
    pub console: Vec<crate::ConsoleEntry>,
    pub collection: Option<Collection>,
    pub definition: SavedRequest,
    /// The active environment with a renewed sign-in cache, when one was.
    pub environment: Option<Environment>,
    pub prepared: PreparedRequest,
    pub snapshot: RedactedRequestSnapshot,
    pub response: Result<Response, String>,
    pub test_results: Vec<TestResult>,
    pub cookie_jar: CookieJar,
}

/// A compiled send awaiting its network phase.
pub struct PreparedStandaloneSend {
    pub collection: Option<Collection>,
    pub initial_script_state: ScriptScopes,
    pub environment_scope: String,
    pub operation_id: String,
    pub definition: SavedRequest,
    /// The sign-in the request authenticates through, for a 401 retry.
    pub login: Option<LoginSession>,
    pub environment: Option<Environment>,
    pub workspace: WorkspaceId,
    pub prepared: PreparedRequest,
    pub snapshot: RedactedRequestSnapshot,
    pub post_script: String,
    pub pre_script_ran: bool,
    pub pre_tests: Vec<TestResult>,
    pub pre_console: Vec<crate::ConsoleEntry>,
    pub script_state: ScriptScopes,
    pub store: Arc<WorkbenchStore>,
    pub secret_store: Arc<dyn SecretStore>,
    pub cookie_jar: CookieJar,
    pub file_capabilities: FileCapabilities,
    pub transport: Arc<dyn WorkbenchTransport>,
}

/// Closes a successful non-final phase if caller-side work fails before the
/// next service phase can terminalize the operation itself.
pub struct OperationAbortGuard {
    operation_id: String,
    transport: Arc<dyn WorkbenchTransport>,
    armed: bool,
}

impl OperationAbortGuard {
    pub fn new(operation_id: String, transport: Arc<dyn WorkbenchTransport>) -> Self {
        Self {
            operation_id,
            transport,
            armed: false,
        }
    }

    pub fn arm(&mut self) {
        self.armed = true;
    }

    pub fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for OperationAbortGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.transport.cancel(&self.operation_id);
        }
    }
}

pub fn ordered_script(
    collection: Option<&Collection>,
    folders: &[&Folder],
    request: &SavedRequest,
    tests: bool,
) -> String {
    collection
        .into_iter()
        .map(|collection| {
            if tests {
                collection.scripts.tests.as_str()
            } else {
                collection.scripts.pre_request.as_str()
            }
        })
        .chain(folders.iter().map(|folder| {
            if tests {
                folder.scripts.tests.as_str()
            } else {
                folder.scripts.pre_request.as_str()
            }
        }))
        .chain(std::iter::once(if tests {
            request.scripts.tests.as_str()
        } else {
            request.scripts.pre_request.as_str()
        }))
        .filter(|script| !script.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn script_variables(scopes: &[&[Variable]]) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    for scope in scopes {
        for variable in *scope {
            if !variable.enabled {
                continue;
            }
            if let VariableValue::Plain(value) = &variable.value {
                values.insert(variable.key.clone(), value.clone());
            }
        }
    }
    values
}

// Resolve available secrets for script reads, while missing vault entries remain
// absent so a login script can replace an expired/missing credential.
pub fn resolved_script_variables(
    variables: &[Variable],
    secrets: &DraftSecrets,
) -> BTreeMap<String, String> {
    variables
        .iter()
        .filter(|variable| variable.enabled)
        .filter_map(|variable| {
            let value = match &variable.value {
                VariableValue::Plain(value) => crate::vault::resolve_template_with(value, |name| {
                    secrets.resolve(&crate::vault::vault_secret_reference(name)?)
                })
                .ok(),
                VariableValue::Secret(reference) => secrets.resolve(reference).ok(),
                VariableValue::MissingSecret(_) => None,
            }?;
            Some((variable.key.clone(), value))
        })
        .collect()
}

/// Send only the named values referenced by script templates to the sandbox;
/// interpolate them at runtime so secret text is never executable JavaScript.
pub(super) fn script_vault_values<'a>(
    templates: impl IntoIterator<Item = &'a str>,
    secrets: &DraftSecrets,
) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    for template in templates {
        let _ = crate::vault::resolve_template_with(template, |name| {
            if let Ok(value) = secrets.resolve(&crate::vault::vault_secret_reference(name)?) {
                values.insert(name.to_string(), value);
            }
            Ok(String::new())
        });
    }
    values
}

pub fn script_secret_values(variables: &[Variable], secrets: &DraftSecrets) -> Vec<String> {
    let mut values = Vec::new();
    for variable in variables.iter().filter(|variable| variable.enabled) {
        match &variable.value {
            VariableValue::Plain(value) => {
                values.extend(script_vault_values([value.as_str()], secrets).into_values());
            }
            VariableValue::Secret(reference) => {
                if let Ok(value) = secrets.resolve(reference) {
                    values.push(value);
                }
            }
            VariableValue::MissingSecret(_) => {}
        }
    }
    values
}

pub fn request_script_vault(
    request: &SavedRequest,
    scripts: [&str; 2],
    secrets: &DraftSecrets,
) -> BTreeMap<String, String> {
    let view = script_request(request);
    script_vault_values(
        scripts
            .into_iter()
            .chain([view.url.as_str(), view.body.as_str()])
            .chain(
                view.headers
                    .iter()
                    .flat_map(|(name, value)| [name.as_str(), value.as_str()]),
            ),
        secrets,
    )
}

/// Apply only the script's changed keys to the latest saved rows. Unrelated
/// edits, disabled rows and opaque secret references keep their metadata.
fn merge_script_variables(
    variables: &mut Vec<Variable>,
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) {
    variables.retain(|variable| {
        !(variable.enabled
            && before.contains_key(&variable.key)
            && !after.contains_key(&variable.key))
    });
    for (key, value) in after
        .iter()
        .filter(|(key, value)| before.get(*key) != Some(*value))
    {
        if let Some(variable) = variables
            .iter_mut()
            .rev()
            .find(|variable| variable.enabled && variable.key == *key)
        {
            variable.value = VariableValue::Plain(value.clone());
        } else {
            variables.push(Variable {
                id: RowId::new(),
                key: key.clone(),
                value: VariableValue::Plain(value.clone()),
                enabled: true,
                description: "script variable".into(),
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn vault_script_changes(
    variables: &mut [Variable],
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
    scope: &str,
    original: &[Variable],
    workspace: &WorkspaceId,
    secrets: &dyn SecretStore,
    redactions: &mut Vec<String>,
) -> Result<(), String> {
    for variable in variables.iter_mut().filter(|variable| variable.enabled) {
        let Some(value) = after
            .get(&variable.key)
            .filter(|value| before.get(&variable.key) != Some(*value))
        else {
            continue;
        };
        let existing_reference = original
            .iter()
            .find(|row| row.enabled && row.key == variable.key)
            .and_then(|row| match &row.value {
                VariableValue::Secret(reference) | VariableValue::MissingSecret(reference) => {
                    Some(reference.clone())
                }
                _ => None,
            });
        if existing_reference.is_some()
            || crate::persistence_safety::sensitive_name(&variable.key)
            || redactions
                .iter()
                .any(|secret| !secret.is_empty() && value.contains(secret.as_str()))
        {
            // Use the editor's deterministic secret namespace so rehydrated
            // masked rows resolve the same vault entry on the next send.
            let reference = SecretRef::new(format!("workbench.{scope}.variable.{}", variable.key))?;
            redactions.push(value.clone());
            secrets
                .set_secret(workspace, &reference, SecretValue::new(value))
                .map_err(|error| {
                    let recovery = match error {
                        crate::SecretStoreError::TooLarge => "",
                        _ => {
                            ". Unlock or reconnect your operating system credential vault and retry"
                        }
                    };
                    format!(
                        "Could not save script credential in the secret store: {error}{recovery}"
                    )
                })?;
            variable.value = VariableValue::Secret(reference);
        }
    }
    Ok(())
}

fn persist_script_scopes(input: &mut PreparedStandaloneSend) -> Vec<TestResult> {
    let mut failures = Vec::new();
    let mut collection_saved = false;
    if let Err(error) = persist_globals(
        &input.store,
        input.secret_store.as_ref(),
        &input.workspace,
        &input.initial_script_state.globals,
        &input.script_state.globals,
        &mut input.prepared.redactions,
    ) {
        failures.push(TestResult {
            name: "global variable persistence".into(),
            passed: false,
            skipped: false,
            error: Some(error),
        });
    }
    for values in [
        &input.script_state.environment,
        &input.script_state.collection,
        &input.script_state.local,
    ] {
        input.prepared.redactions.extend(
            values
                .iter()
                .filter(|(key, _)| crate::persistence_safety::sensitive_name(key))
                .map(|(_, value)| value.clone()),
        );
    }
    if input.initial_script_state.environment != input.script_state.environment {
        let saved = (|| {
            let original = input
                .environment
                .as_ref()
                .ok_or("Select an environment to save pm.environment variables")?;
            let mut environment = input
                .store
                .list_environments(&input.workspace)
                .map_err(|e| e.to_string())?
                .into_iter()
                .find(|environment| environment.id == original.id)
                .ok_or("The script environment no longer exists")?;
            let original_variables = environment.variables.clone();
            merge_script_variables(
                &mut environment.variables,
                &input.initial_script_state.environment,
                &input.script_state.environment,
            );
            vault_script_changes(
                &mut environment.variables,
                &input.initial_script_state.environment,
                &input.script_state.environment,
                &input.environment_scope,
                &original_variables,
                &input.workspace,
                input.secret_store.as_ref(),
                &mut input.prepared.redactions,
            )?;
            input
                .store
                .upsert_environment(&environment)
                .map_err(|e| e.to_string())?;
            input.environment = Some(environment);
            Ok::<_, String>(())
        })();
        if let Err(error) = saved {
            failures.push(TestResult {
                name: "environment variable persistence".into(),
                passed: false,
                skipped: false,
                error: Some(error),
            });
        }
    }
    if input.initial_script_state.collection != input.script_state.collection {
        let saved = (|| {
            let original = input
                .collection
                .as_ref()
                .ok_or("Save the request in a collection to save pm.collectionVariables")?;
            let mut collection = input
                .store
                .collection(&input.workspace, &original.id)
                .map_err(|e| e.to_string())?
                .ok_or("The script collection no longer exists")?;
            let original_variables = collection.variables.clone();
            merge_script_variables(
                &mut collection.variables,
                &input.initial_script_state.collection,
                &input.script_state.collection,
            );
            vault_script_changes(
                &mut collection.variables,
                &input.initial_script_state.collection,
                &input.script_state.collection,
                &format!("collection.{}", collection.id.as_str()),
                &original_variables,
                &input.workspace,
                input.secret_store.as_ref(),
                &mut input.prepared.redactions,
            )?;
            input
                .store
                .upsert_collection(&collection)
                .map_err(|e| e.to_string())?;
            input.collection = Some(collection);
            collection_saved = true;
            Ok::<_, String>(())
        })();
        if let Err(error) = saved {
            failures.push(TestResult {
                name: "collection variable persistence".into(),
                passed: false,
                skipped: false,
                error: Some(error),
            });
        }
    }
    if !collection_saved {
        input.collection = None;
    }
    failures
}

pub fn carry_shared_script_scopes(
    runtime: &mut ScriptScopes,
    completed_request: &ScriptScopes,
    initial_local: &BTreeMap<String, String>,
) {
    runtime.globals = completed_request.globals.clone();
    runtime.environment = completed_request.environment.clone();
    runtime.collection = completed_request.collection.clone();
    // Cookies are domain/path scoped in CookieJar, not a global runner scope.
    // The next request is seeded from the jar for its own URL.
    runtime.cookies.clear();
    // Saved request defaults shadow run locals for this request only. Carry
    // script changes, including removals, while restoring unchanged run values.
    for key in initial_local.keys().chain(completed_request.local.keys()) {
        if initial_local.get(key) != completed_request.local.get(key) {
            match completed_request.local.get(key) {
                Some(value) => {
                    runtime.local.insert(key.clone(), value.clone());
                }
                None => {
                    runtime.local.remove(key);
                }
            }
        }
    }
}

pub fn request_script_scopes(runtime: &ScriptScopes, request: &SavedRequest) -> ScriptScopes {
    let mut scopes = runtime.clone();
    scopes.local.extend(script_variables(&[&request.variables]));
    scopes.next_request = None;
    scopes.allow_private_network = request.settings.allow_private_network;
    scopes
}

pub fn script_request(request: &SavedRequest) -> ScriptRequestView {
    ScriptRequestView {
        method: request.method.as_str().to_string(),
        url: request.url.clone(),
        headers: request
            .headers
            .iter()
            .filter(|row| row.enabled)
            .map(|row| (row.key.clone(), row.value.clone()))
            .collect(),
        body: match &request.body {
            Body::Raw { text, .. } => text.clone(),
            Body::GraphQl { query, .. } => query.clone(),
            _ => String::new(),
        },
    }
}

pub fn apply_pre_request_result(
    request: &mut SavedRequest,
    result: ScriptResult,
) -> Result<(), String> {
    request.method = HttpMethod::new(result.request.method)
        .map_err(|error| format!("pre-request script produced an invalid method: {error}"))?;
    request.url = result.request.url;
    request.headers = result
        .request
        .headers
        .into_iter()
        .map(|(key, value)| KeyValueRow {
            id: RowId::new(),
            key,
            value,
            enabled: true,
            description: "pre-request script".into(),
        })
        .collect();
    match &mut request.body {
        Body::Raw { text, .. } => *text = result.request.body,
        Body::GraphQl { query, .. } => *query = result.request.body,
        _ => {}
    }
    Ok(())
}

pub fn apply_script_cookie_mutations(
    jar: &mut CookieJar,
    secret_store: &dyn SecretStore,
    request_url: &str,
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> Result<(), String> {
    let now = now_seconds();
    for name in before.keys().filter(|name| !after.contains_key(*name)) {
        jar.set_cookie(
            secret_store,
            request_url,
            &format!("{name}=; Max-Age=0; Path=/"),
            now,
        )
        .map_err(|error| error.to_string())?;
    }
    for (name, value) in after
        .iter()
        .filter(|(name, value)| before.get(*name) != Some(*value))
    {
        jar.set_cookie(
            secret_store,
            request_url,
            &format!("{name}={value}; Path=/"),
            now,
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

pub fn cookie_scope_for_url(
    jar: &CookieJar,
    secret_store: &dyn SecretStore,
    request_url: &str,
) -> Result<BTreeMap<String, String>, String> {
    if !url::Url::parse(request_url)
        .ok()
        .is_some_and(|url| matches!(url.scheme(), "http" | "https"))
    {
        return Ok(BTreeMap::new());
    }
    let Some(header) = jar
        .header_for_url(secret_store, request_url, now_seconds())
        .map_err(|error| error.to_string())?
    else {
        return Ok(BTreeMap::new());
    };
    Ok(header
        .expose_secret()
        .split(';')
        .filter_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            Some((name.trim().to_string(), value.trim().to_string()))
        })
        .collect())
}

pub fn resolved_cookie_scope_url(
    request: &SavedRequest,
    collection: Option<&Collection>,
    folders: &[&Folder],
    context: &CompileContext<'_>,
) -> String {
    // Resolve only the URL through the canonical compiler. Auth, body, header,
    // and parameter inputs may intentionally be completed by the pre-request
    // script and must not prevent already-resolvable cookies from entering its
    // scope.
    let mut probe = request.clone();
    probe.params.clear();
    probe.headers.clear();
    probe.auth = AuthConfig::None;
    probe.body = Body::None;
    compile_request_with_folder_chain(&probe, collection, folders, context)
        .map(|(prepared, _)| prepared.url)
        .unwrap_or_else(|_| request.url.clone())
}

pub fn variables_from_map(values: &BTreeMap<String, String>) -> Vec<Variable> {
    values
        .iter()
        .map(|(key, value)| Variable {
            id: RowId::new(),
            key: key.clone(),
            value: VariableValue::Plain(value.clone()),
            enabled: true,
            description: "pre-request script".into(),
        })
        .collect()
}

pub fn prepare_standalone_send(
    mut input: StandaloneSendInput,
) -> Result<PreparedStandaloneSend, String> {
    let mut operation_guard =
        OperationAbortGuard::new(input.operation_id.clone(), input.transport.clone());
    let script_request_id = input.definition.id.as_str().to_string();
    input
        .secrets
        .persist(input.secret_store.as_ref(), &input.workspace)?;
    let (environment, environment_secrets) =
        parse_session_variables(&input.environment_source, &input.environment_scope)?;
    environment_secrets.persist(input.secret_store.as_ref(), &input.workspace)?;
    input.secrets.merge(environment_secrets);
    let globals = input
        .store
        .global_variables(&input.workspace)
        .map_err(|e| e.to_string())?;
    finish_browser_authorization(&mut input)?;
    // Only the browser can get a PKCE client its first token or replace an
    // expired one it cannot refresh. The desktop app plans that grant before
    // preparing; anyone else stops here with the reason.
    if pkce_needs_browser(send_auth_owner(&input).0, now_seconds()) {
        return Err(PKCE_NEEDS_BROWSER.into());
    }
    let folder_refs = input.folders.iter().collect::<Vec<_>>();
    refresh_expired_oauth_token_in_context(
        &mut input.definition,
        &mut input.secrets,
        &input.workspace,
        input.store.as_ref(),
        input.secret_store.as_ref(),
        input.transport.as_ref(),
        OAuthVariables {
            globals: &globals,
            collection: input.collection.as_ref(),
            folders: &folder_refs,
            environment: &environment,
            base_url: Some(&input.environment_base_url),
        },
    )?;
    refresh_environment_oauth(&mut input)?;
    let folder_refs = input.folders.iter().collect::<Vec<_>>();
    let pre_script = ordered_script(
        input.collection.as_ref(),
        &folder_refs,
        &input.definition,
        false,
    );
    let pre_script_runs = !pre_script.trim().is_empty();
    let pre_phase = OperationPhase {
        operation_id: input.operation_id.clone(),
        starts_operation: true,
        final_phase: false,
    };
    let seed_context = CompileContext {
        global: &globals,
        environment: &environment,
        data: &[],
        local: &[],
        secrets: &input.secrets,
        environment_base_url: Some(&input.environment_base_url),
        environment_auth: Some(&input.environment_auth),
    };
    let cookie_seed_url = resolved_cookie_scope_url(
        &input.definition,
        input.collection.as_ref(),
        &folder_refs,
        &seed_context,
    );
    let mut initial_cookies = cookie_scope_for_url(
        &input.cookie_jar,
        input.secret_store.as_ref(),
        &cookie_seed_url,
    )?;
    initial_cookies.extend(input.manual_cookies.iter().cloned());
    let post_script = ordered_script(
        input.collection.as_ref(),
        &folder_refs,
        &input.definition,
        true,
    );
    let initial_script_state = ScriptScopes {
        globals: resolved_script_variables(&globals, &input.secrets),
        environment_name: input.environment.as_ref().map(|e| e.name.clone()),
        next_request: None,
        environment: resolved_script_variables(&environment, &input.secrets),
        collection: input
            .collection
            .as_ref()
            .map(|collection| resolved_script_variables(&collection.variables, &input.secrets))
            .unwrap_or_default(),
        local: resolved_script_variables(&input.definition.variables, &input.secrets),
        iteration_data: BTreeMap::new(),
        vault: request_script_vault(
            &input.definition,
            [pre_script.as_str(), post_script.as_str()],
            &input.secrets,
        ),
        cookies: initial_cookies.clone(),
        allow_private_network: input.definition.settings.allow_private_network,
        assertions_only: false,
    };
    let mut pre_redactions = super::transport::script_scope_redactions(&initial_script_state);
    pre_redactions.extend(super::transport::script_request_redactions(
        &script_request(&input.definition),
    ));
    if let Ok((prepared, _)) = compile_request_with_folder_chain(
        &input.definition,
        input.collection.as_ref(),
        &folder_refs,
        &seed_context,
    ) {
        pre_redactions.extend(prepared.redactions);
    }
    for variables in [
        &globals[..],
        &environment[..],
        input
            .collection
            .as_ref()
            .map(|c| c.variables.as_slice())
            .unwrap_or_default(),
        &input.definition.variables[..],
    ] {
        pre_redactions.extend(script_secret_values(variables, &input.secrets));
    }
    let pre_result = input
        .transport
        .run_script(
            &pre_script,
            initial_script_state.clone(),
            script_request(&input.definition),
            None,
            Some(&script_request_id),
            Some(&pre_phase),
        )
        .map_err(|error| {
            format!(
                "Pre-request script failed: {}",
                super::transport::diagnostic_text(&error, &pre_redactions)
            )
        })?;
    if pre_script_runs {
        operation_guard.arm();
    }
    let mut script_state = pre_result.scopes();
    pre_redactions.extend(super::transport::script_scope_redactions(&script_state));
    let pre_tests = pre_result.tests.clone();
    let pre_console = pre_result.console.clone();
    script_state.vault = initial_script_state.vault.clone();
    script_state.allow_private_network = initial_script_state.allow_private_network;
    let mut executed_definition = input.definition.clone();
    apply_pre_request_result(&mut executed_definition, pre_result)?;
    let mut scripted_environment = environment.clone();
    merge_script_variables(
        &mut scripted_environment,
        &initial_script_state.environment,
        &script_state.environment,
    );
    let mut scripted_local = input.definition.variables.clone();
    merge_script_variables(
        &mut scripted_local,
        &initial_script_state.local,
        &script_state.local,
    );
    let mut scripted_collection = input.collection.clone();
    if let Some(collection) = scripted_collection.as_mut() {
        merge_script_variables(
            &mut collection.variables,
            &initial_script_state.collection,
            &script_state.collection,
        );
    }
    executed_definition.variables = scripted_local.clone();
    // Auth URLs use the same variable values as the request, including changes
    // made by its pre-request script. Keep the saved templates on the definition.
    let scripted_globals = variables_from_map(&script_state.globals);
    let mut login = LoginSession::resolve(LoginChain {
        globals: &scripted_globals,
        request: &executed_definition,
        folders: &folder_refs,
        collection: scripted_collection.as_ref(),
        environment_auth: &input.environment_auth,
        environment_scope: &input.environment_scope,
        environment: &scripted_environment,
        base_url: &input.environment_base_url,
        secrets: &input.secrets,
    })?;
    if let Some(session) = login
        .as_mut()
        .filter(|session| session.needs_sign_in(now_seconds()))
    {
        session.sign_in(
            &input.workspace,
            input.secret_store.as_ref(),
            input.transport.as_ref(),
            &input.file_capabilities,
            now_seconds(),
        )?;
        session.apply_to(&mut input.definition, &mut input.environment_auth);
        session.apply_to(&mut executed_definition, &mut input.environment_auth);
        input.secrets = session.secrets.clone();
        if let Some(environment) = session.updated_environment(input.environment.as_ref()) {
            input
                .store
                .upsert_environment(&environment)
                .map_err(|error| error.to_string())?;
            input.environment = Some(environment);
        }
    }
    let context = CompileContext {
        global: &scripted_globals,
        environment: &scripted_environment,
        data: &[],
        local: &scripted_local,
        secrets: &input.secrets,
        environment_base_url: Some(&input.environment_base_url),
        environment_auth: Some(&input.environment_auth),
    };
    let (mut prepared, mut snapshot) = compile_request_with_folder_chain(
        &executed_definition,
        scripted_collection.as_ref(),
        &folder_refs,
        &context,
    )
    .map_err(|error| describe_missing_auth_secret(error.to_string()))?;
    prepared
        .redactions
        .extend(initial_script_state.vault.values().cloned());
    prepared.redactions.extend(pre_redactions);
    for values in [
        &initial_script_state.environment,
        &initial_script_state.collection,
        &script_state.environment,
        &script_state.collection,
        &script_state.local,
    ] {
        prepared.redactions.extend(
            values
                .iter()
                .filter(|(key, _)| crate::persistence_safety::sensitive_name(key))
                .map(|(_, value)| value.clone()),
        );
    }
    for variables in [
        &environment[..],
        input
            .collection
            .as_ref()
            .map(|collection| collection.variables.as_slice())
            .unwrap_or_default(),
        &input.definition.variables[..],
    ] {
        prepared
            .redactions
            .extend(script_secret_values(variables, &input.secrets));
    }
    // Composer cookies are an explicit mutation of the durable jar even when
    // the pre-request script leaves its cookie scope unchanged. Apply them to
    // the compiled URL so templated request URLs receive the correct host
    // scope, then reconcile any script additions/deletions below.
    for (name, value) in &input.manual_cookies {
        input
            .cookie_jar
            .set_cookie(
                input.secret_store.as_ref(),
                &prepared.url,
                &format!("{name}={value}; Path=/"),
                now_seconds(),
            )
            .map_err(|error| error.to_string())?;
    }
    apply_script_cookie_mutations(
        &mut input.cookie_jar,
        input.secret_store.as_ref(),
        &prepared.url,
        &initial_cookies,
        &script_state.cookies,
    )?;
    input
        .store
        .save_cookie_jar(&input.cookie_jar)
        .map_err(|error| error.to_string())?;
    if let Some(cookie) = input
        .cookie_jar
        .header_for_url(input.secret_store.as_ref(), &prepared.url, now_seconds())
        .map_err(|error| error.to_string())?
    {
        let value = cookie.expose_secret().to_string();
        prepared
            .headers
            .retain(|(name, _)| !name.eq_ignore_ascii_case("cookie"));
        prepared.headers.push(("Cookie".into(), value.clone()));
        prepared.redactions.push(value);
        snapshot
            .headers
            .retain(|(name, _)| !name.eq_ignore_ascii_case("cookie"));
        snapshot
            .headers
            .push(("Cookie".into(), "<redacted>".into()));
    }
    input
        .store
        .upsert_request(&input.definition)
        .map_err(|error| error.to_string())?;
    let (pre_tests, pre_console) = super::transport::script_diagnostics(
        &pre_tests,
        &pre_console,
        "Pre-request",
        &prepared.redactions,
    );
    let prepared = PreparedStandaloneSend {
        collection: input.collection,
        initial_script_state,
        environment_scope: input.environment_scope,
        operation_id: input.operation_id,
        definition: input.definition,
        login,
        environment: input.environment,
        workspace: input.workspace,
        prepared,
        snapshot,
        post_script,
        pre_script_ran: pre_script_runs,
        pre_tests,
        pre_console,
        script_state,
        store: input.store,
        secret_store: input.secret_store,
        cookie_jar: input.cookie_jar,
        file_capabilities: input.file_capabilities,
        transport: input.transport,
    };
    operation_guard.disarm();
    Ok(prepared)
}

pub fn execute_standalone_send(
    mut input: PreparedStandaloneSend,
) -> Result<StandaloneSendResult, String> {
    let post_script_runs = !input.post_script.trim().is_empty();
    let script_request_id = input.definition.id.as_str().to_string();
    let send_phase = OperationPhase {
        operation_id: input.operation_id.clone(),
        starts_operation: !input.pre_script_ran,
        final_phase: !post_script_runs,
    };
    let mut test_results = std::mem::take(&mut input.pre_tests);
    let mut console = std::mem::take(&mut input.pre_console);
    let mut operation_guard =
        OperationAbortGuard::new(input.operation_id.clone(), input.transport.clone());
    if input.pre_script_ran {
        operation_guard.arm();
    }
    let mut sent = input
        .transport
        .send(&input.prepared, &input.file_capabilities, &send_phase);
    let mut renewed_environment = None;
    // A 401 against a sign-in session means the server no longer honours
    // the cached value: sign in once more and resend, whatever the expiry
    // said. Only the resend reaches history.
    if let (Ok(response), Some(session)) = (&sent, input.login.as_mut())
        && response.status == 401
    {
        sent = session
            .sign_in(
                &input.workspace,
                input.secret_store.as_ref(),
                input.transport.as_ref(),
                &input.file_capabilities,
                now_seconds(),
            )
            .map_err(|error| format!("Signed in again after HTTP 401, but: {error}"))
            .and_then(|value| {
                replace_bearer(&mut input.prepared, &value);
                let mut environment_auth = AuthConfig::None;
                session.apply_to(&mut input.definition, &mut environment_auth);
                if session.owner == LoginOwner::Request {
                    input
                        .store
                        .upsert_request(&input.definition)
                        .map_err(|error| error.to_string())?;
                }
                if let Some(environment) = session.updated_environment(input.environment.as_ref()) {
                    input
                        .store
                        .upsert_environment(&environment)
                        .map_err(|error| error.to_string())?;
                    renewed_environment = Some(environment);
                }
                input.transport.send(
                    &input.prepared,
                    &input.file_capabilities,
                    &OperationPhase {
                        starts_operation: false,
                        ..send_phase.clone()
                    },
                )
            });
    }
    let mut response =
        match sent {
            Ok(mut response) => {
                if post_script_runs {
                    operation_guard.arm();
                } else {
                    operation_guard.disarm();
                }
                let response_url = if response.final_url.is_empty() {
                    input.prepared.url.as_str()
                } else {
                    response.final_url.as_str()
                };
                for mutation in &response.cookie_mutations {
                    if let Err(error) = input.cookie_jar.set_cookie(
                        input.secret_store.as_ref(),
                        &mutation.source_url,
                        &mutation.header,
                        now_seconds(),
                    ) {
                        test_results.push(TestResult {
                            name: "cookie persistence".into(),
                            passed: false,
                            skipped: false,
                            error: Some(error.to_string()),
                        });
                    }
                }
                for set_cookie in &response.set_cookies {
                    if let Err(error) = input.cookie_jar.set_cookie(
                        input.secret_store.as_ref(),
                        response_url,
                        set_cookie,
                        now_seconds(),
                    ) {
                        test_results.push(TestResult {
                            name: "cookie persistence".into(),
                            passed: false,
                            skipped: false,
                            error: Some(error.to_string()),
                        });
                    }
                }
                let post_cookie_before = match cookie_scope_for_url(
                    &input.cookie_jar,
                    input.secret_store.as_ref(),
                    response_url,
                ) {
                    Ok(cookies) => cookies,
                    Err(error) => {
                        test_results.push(TestResult {
                            name: "cookie persistence".into(),
                            passed: false,
                            skipped: false,
                            error: Some(error),
                        });
                        BTreeMap::new()
                    }
                };
                input.script_state.cookies = post_cookie_before.clone();
                input
                    .prepared
                    .redactions
                    .extend(super::transport::script_scope_redactions(
                        &input.script_state,
                    ));
                let post_phase = OperationPhase {
                    operation_id: input.operation_id.clone(),
                    starts_operation: false,
                    final_phase: true,
                };
                let post = input.transport.run_script(
                    &input.post_script,
                    input.script_state.clone(),
                    ScriptRequestView {
                        method: input.prepared.method.as_str().into(),
                        url: input.prepared.url.clone(),
                        headers: input.prepared.headers.clone(),
                        body: String::new(),
                    },
                    Some(ScriptResponseView {
                        code: response.status,
                        status: response.reason.clone(),
                        headers: response.headers.clone(),
                        body: response.body.clone(),
                        response_time_ms: response.duration_ms,
                    }),
                    Some(&script_request_id),
                    Some(&post_phase),
                );
                // The post phase is final even when its script fails; service
                // errors terminalize their registration before returning.
                operation_guard.disarm();
                match post {
                    Ok(post) => {
                        input.script_state = post.scopes();
                        input.prepared.redactions.extend(
                            super::transport::script_scope_redactions(&input.script_state),
                        );
                        let (post_tests, post_console) = super::transport::script_diagnostics(
                            &post.tests,
                            &post.console,
                            "Post-response",
                            &input.prepared.redactions,
                        );
                        console.extend(post_console);
                        if let Err(error) = apply_script_cookie_mutations(
                            &mut input.cookie_jar,
                            input.secret_store.as_ref(),
                            response_url,
                            &post_cookie_before,
                            &post.cookies,
                        ) {
                            test_results.push(TestResult {
                                name: "post-response cookie mutation".into(),
                                passed: false,
                                skipped: false,
                                error: Some(error),
                            });
                        }
                        test_results.extend(post_tests);
                    }
                    Err(error) => {
                        console.push(super::transport::script_error_console(
                            "Post-response",
                            &error,
                            &input.prepared.redactions,
                        ));
                        test_results.push(TestResult {
                            name: "[Post-response] script runtime".into(),
                            passed: false,
                            skipped: false,
                            error: Some(error),
                        });
                    }
                }
                if let Err(error) = input.store.save_cookie_jar(&input.cookie_jar) {
                    test_results.push(TestResult {
                        name: "cookie persistence".into(),
                        passed: false,
                        skipped: false,
                        error: Some(error.to_string()),
                    });
                }
                response.console = console.clone();
                Ok(response)
            }
            Err(error) => Err(super::transport::diagnostic_text(
                &error,
                &input.prepared.redactions,
            )),
        };
    if let Some(environment) = renewed_environment {
        input.environment = Some(environment);
    }
    test_results.extend(persist_script_scopes(&mut input));
    let redact = super::transport::diagnostic_redactor(&input.prepared.redactions);
    for entry in &mut console {
        entry.message = redact(&entry.message);
    }
    for test in &mut test_results {
        test.name = redact(&test.name);
        test.error = test.error.as_ref().map(|error| redact(error));
    }
    if let Ok(response) = &mut response {
        response.console = console.clone();
    }
    Ok(StandaloneSendResult {
        console,
        collection: input.collection,
        definition: input.definition,
        environment: input.environment,
        prepared: input.prepared,
        snapshot: input.snapshot,
        response,
        test_results,
        cookie_jar: input.cookie_jar,
    })
}

/// The history row a send becomes, the way the desktop composer records it:
/// the redacted request snapshot plus either the response (with the
/// post-script test results attached) or the error. `redactions` are the
/// values `store.record_exchange` must mask.
pub fn exchange_from_send(
    result: StandaloneSendResult,
    workspace_id: &WorkspaceId,
    started_at: i64,
) -> (Exchange, Vec<String>, Result<Response, String>) {
    let redactions = result.prepared.redactions.clone();
    let mut exchange = Exchange {
        console: result.console,
        test_results: result.test_results.clone(),
        id: ExchangeId::new(),
        workspace_id: workspace_id.clone(),
        request_id: Some(result.definition.id.clone()),
        request: result.snapshot,
        response: None,
        error: None,
        started_at,
        completed_at: now_millis(),
    };
    let response = match result.response {
        Ok(mut response) => {
            response.test_results = result.test_results;
            exchange.response = Some(response_snapshot(&response));
            Ok(response)
        }
        Err(error) => {
            exchange.error = Some(error.clone());
            Err(error)
        }
    };
    (exchange, redactions, response)
}

pub fn folder_chain<'a>(folders: &'a [Folder], request: &SavedRequest) -> Vec<&'a Folder> {
    let mut chain = Vec::new();
    let mut id = request.folder_id.as_ref();
    while let Some(folder_id) = id {
        let Some(folder) = folders.iter().find(|folder| &folder.id == folder_id) else {
            break;
        };
        chain.push(folder);
        id = folder.parent_id.as_ref();
    }
    chain.reverse();
    chain
}

pub fn folder_is_within(folder_id: &FolderId, root: &FolderId, folders: &[Folder]) -> bool {
    let mut current = Some(folder_id);
    let mut visited = HashSet::new();
    while let Some(id) = current {
        if id == root {
            return true;
        }
        if !visited.insert(id.clone()) {
            return false;
        }
        current = folders
            .iter()
            .find(|folder| &folder.id == id)
            .and_then(|folder| folder.parent_id.as_ref());
    }
    false
}

pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64
}

/// Persist only changed global values, retaining concurrent edits and vault references.
pub(super) fn persist_globals(
    store: &WorkbenchStore,
    secrets: &dyn SecretStore,
    workspace: &WorkspaceId,
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
    redactions: &mut Vec<String>,
) -> Result<(), String> {
    if before == after {
        return Ok(());
    }
    let mut variables = store
        .global_variables(workspace)
        .map_err(|e| e.to_string())?;
    let original = variables.clone();
    merge_script_variables(&mut variables, before, after);
    vault_script_changes(
        &mut variables,
        before,
        after,
        "globals",
        &original,
        workspace,
        secrets,
        redactions,
    )?;
    store
        .save_global_variables(workspace, &variables)
        .map_err(|e| e.to_string())
}

/// Re-evaluate assertions on the current response without sending the main request.
/// Network subrequests are disabled and scope mutations are transient.
pub fn rerun_response_tests(
    transport: &dyn WorkbenchTransport,
    script: &str,
    scopes: ScriptScopes,
    request: ScriptRequestView,
    response: &mut Response,
) -> Result<(), String> {
    rerun_response_tests_with_redactions(transport, script, scopes, request, response, &[])
}

/// Like [`rerun_response_tests`], retaining secret metadata from the editor scopes.
pub fn rerun_response_tests_with_redactions(
    transport: &dyn WorkbenchTransport,
    script: &str,
    scopes: ScriptScopes,
    request: ScriptRequestView,
    response: &mut Response,
    secret_values: &[String],
) -> Result<(), String> {
    let mut scopes = scopes;
    scopes.assertions_only = true;
    let mut redactions = super::transport::script_scope_redactions(&scopes);
    redactions.extend(secret_values.iter().cloned());
    redactions.extend(super::transport::script_request_redactions(&request));
    let view = ScriptResponseView {
        code: response.status,
        status: response.reason.clone(),
        headers: response.headers.clone(),
        body: response.body.clone(),
        response_time_ms: response.duration_ms,
    };
    let result = transport
        .run_script(script, scopes, request, Some(view), None, None)
        .map_err(|error| super::transport::diagnostic_text(&error, &redactions))?;
    redactions.extend(super::transport::script_scope_redactions(&result.scopes()));
    let (tests, console) = super::transport::script_diagnostics(
        &result.tests,
        &result.console,
        "Post-response",
        &redactions,
    );
    response.test_results = tests;
    response.console = console;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::transport::ScriptTestResult;
    use crate::runtime::workspace::WorkspaceData;
    use crate::{CollectionId, MemorySecretStore, RequestId, RequestSettings, Scripts};
    use std::sync::Mutex;

    /// Minimal service stand-in: `200 {"ok":true}` for every send, a passing
    /// assertion for every non-empty post script, cancels recorded.
    #[derive(Default)]
    struct FakeTransport {
        cancels: Mutex<Vec<String>>,
        requests: Mutex<Vec<PreparedRequest>>,
        script_inputs: Mutex<Vec<(bool, ScriptScopes)>>,
        login_response: Option<Response>,
    }

    impl WorkbenchTransport for FakeTransport {
        fn send(
            &self,
            request: &PreparedRequest,
            _files: &FileCapabilities,
            _phase: &OperationPhase,
        ) -> Result<Response, String> {
            self.requests.lock().unwrap().push(request.clone());
            if request.url.contains("/unreachable/") {
                return Err(format!("connection failed: {}", request.url));
            }
            if request.url == "https://auth.example.test/session"
                && let Some(response) = &self.login_response
            {
                return Ok(response.clone());
            }
            Ok(Response {
                console: Vec::new(),
                status: 200,
                reason: "OK".into(),
                headers: vec![("Content-Type".into(), "application/json".into())],
                set_cookies: request
                    .url
                    .contains("/new-cookie")
                    .then(|| "sid=private-cookie-value; Path=/".to_string())
                    .into_iter()
                    .collect(),
                cookie_mutations: Vec::new(),
                body: r#"{"ok":true}"#.into(),
                body_base64: "eyJvayI6dHJ1ZX0=".into(),
                binary: false,
                final_url: request.url.clone(),
                http_version: "HTTP/2".into(),
                received_bytes: 11,
                stored_bytes: 11,
                full_body_sha256: Some("a".repeat(64)),
                timings: Default::default(),
                cookies: Vec::new(),
                duration_ms: 7,
                truncated: false,
                redirects: Vec::new(),
                test_results: Vec::new(),
            })
        }

        fn run_script(
            &self,
            script: &str,
            mut scopes: ScriptScopes,
            request: ScriptRequestView,
            response: Option<ScriptResponseView>,
            _request_id: Option<&str>,
            _phase: Option<&OperationPhase>,
        ) -> Result<ScriptResult, String> {
            self.script_inputs
                .lock()
                .unwrap()
                .push((response.is_some(), scopes.clone()));
            let asserts = response.is_some() && !script.trim().is_empty();
            let diagnostic_message = (script == "diagnostics").then(|| {
                scopes
                    .resolved()
                    .into_values()
                    .collect::<Vec<_>>()
                    .join(" | ")
            });
            let diagnostic_pre = diagnostic_message.is_some() && response.is_none();
            if script == "capture token" {
                scopes
                    .environment
                    .insert("accessToken".into(), "response-token-value".into());
                scopes.collection.insert("sharedId".into(), "42".into());
                scopes
                    .local
                    .insert("temporary".into(), "request-only".into());
                scopes.environment.remove("obsolete");
            }
            if script == "global token" {
                scopes
                    .globals
                    .insert("accessToken".into(), "global-secret-value".into());
            }
            if script == "prepare variable" {
                scopes
                    .environment
                    .insert("preValue".into(), "prepared".into());
            }
            if script == "fail post" {
                return Err("script failed".into());
            }
            if script == "diagnostics error" {
                return Err(scopes
                    .resolved()
                    .into_values()
                    .chain(scopes.cookies.values().cloned())
                    .chain(request.headers.iter().map(|(_, value)| value.clone()))
                    .collect::<Vec<_>>()
                    .join(" | "));
            }

            Ok(ScriptResult {
                globals: scopes.globals.clone(),
                environment_name: scopes.environment_name.clone(),
                next_request: scopes.next_request.clone(),
                variables: scopes.local.clone(),
                environment: scopes.environment,
                collection_variables: scopes.collection,
                local_variables: scopes.local,
                cookies: scopes.cookies,
                request,
                tests: (asserts || diagnostic_pre)
                    .then(|| ScriptTestResult {
                        name: diagnostic_message
                            .clone()
                            .unwrap_or_else(|| "typed service assertion".into()),
                        passed: !diagnostic_pre,
                        skipped: false,
                        error: diagnostic_pre.then(|| diagnostic_message.clone().unwrap()),
                    })
                    .into_iter()
                    .collect(),
                console: diagnostic_message
                    .into_iter()
                    .map(|message| super::super::transport::ScriptConsoleEntry {
                        level: "log".into(),
                        message,
                    })
                    .collect(),
            })
        }

        fn cancel(&self, operation_id: &str) -> Result<bool, String> {
            self.cancels.lock().unwrap().push(operation_id.into());
            Ok(true)
        }
    }

    fn open_workspace(name: &str) -> (WorkspaceData, CollectionId) {
        let workspace = WorkspaceId::new(format!("/test/runtime-send/{name}")).unwrap();
        let path = std::env::temp_dir().join(format!(
            "agentops-core-runtime-send-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        let mut data = WorkspaceData::open(&path, workspace).unwrap();
        let collection_id = data.ensure_collection().unwrap();
        (data, collection_id)
    }

    fn definition(collection_id: &CollectionId, auth: AuthConfig, tests: &str) -> SavedRequest {
        SavedRequest {
            id: RequestId::new(),
            collection_id: collection_id.clone(),
            folder_id: None,
            name: "Headless".into(),
            method: HttpMethod::get(),
            url: "https://example.test/items".into(),
            params: Vec::new(),
            headers: Vec::new(),
            auth,
            body: Body::None,
            variables: Vec::new(),
            scripts: Scripts {
                pre_request: String::new(),
                tests: tests.into(),
            },
            settings: RequestSettings::default(),
            extensions: Default::default(),
            sort_key: 0,
        }
    }

    fn send_input(
        data: &WorkspaceData,
        definition: SavedRequest,
        transport: Arc<dyn WorkbenchTransport>,
    ) -> StandaloneSendInput {
        let secret_store: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::new());
        StandaloneSendInput {
            operation_id: RequestId::new().as_str().to_string(),
            definition,
            collection: None,
            folders: Vec::new(),
            environment: None,
            environment_source: String::new(),
            environment_scope: "environment".into(),
            environment_base_url: String::new(),
            environment_auth: AuthConfig::None,
            browser_authorization: None,
            secrets: DraftSecrets::with_store(secret_store.clone(), data.workspace.clone()),
            workspace: data.workspace.clone(),
            store: data.store.clone(),
            secret_store,
            cookie_jar: CookieJar::new(data.workspace.clone()),
            manual_cookies: Vec::new(),
            file_capabilities: FileCapabilities::default(),
            transport,
        }
    }

    fn row(key: &str, value: &str) -> Variable {
        Variable {
            id: RowId::new(),
            key: key.into(),
            value: VariableValue::Plain(value.into()),
            enabled: true,
            description: String::new(),
        }
    }

    fn environment(data: &WorkspaceData) -> Environment {
        Environment {
            label: Default::default(),
            id: crate::EnvironmentId::new(),
            workspace_id: data.workspace.clone(),
            name: "Test".into(),
            base_url: String::new(),
            auth: AuthConfig::None,
            variables: vec![row("obsolete", "old")],
            active: true,
            extensions: Default::default(),
        }
    }

    #[test]
    fn request_defaults_do_not_leak_but_script_local_changes_and_removals_carry() {
        let mut runtime = ScriptScopes::default();
        runtime.local.insert("shared".into(), "run-value".into());
        runtime.local.insert("removed".into(), "old".into());
        let initial = BTreeMap::from([
            ("shared".into(), "request-default".into()),
            ("onlyHere".into(), "default".into()),
            ("removed".into(), "old".into()),
        ]);
        let mut completed = runtime.clone();
        completed.local = initial.clone();
        completed.local.remove("removed");
        completed.local.insert("created".into(), "new".into());
        carry_shared_script_scopes(&mut runtime, &completed, &initial);
        assert_eq!(runtime.local["shared"], "run-value");
        assert_eq!(runtime.local["created"], "new");
        assert!(!runtime.local.contains_key("onlyHere"));
        assert!(!runtime.local.contains_key("removed"));
        completed
            .local
            .insert("shared".into(), "script-value".into());
        carry_shared_script_scopes(&mut runtime, &completed, &initial);
        assert_eq!(runtime.local["shared"], "script-value");
    }

    #[test]
    fn login_url_and_credentials_resolve_workspace_globals() {
        let (data, collection_id) = open_workspace("login-global-scope");
        let (globals, _) = parse_session_variables(
            "auth_url=https://auth.example.test\nlogin_user=global-user",
            "globals",
        )
        .unwrap();
        data.store
            .save_global_variables(&data.workspace, &globals)
            .unwrap();
        let transport = Arc::new(login_transport(200));
        let mut input = send_input(
            &data,
            definition(&collection_id, basic_login_auth(), ""),
            transport.clone(),
        );
        input.secrets.insert(
            &SecretRef::new("basic-login-password").unwrap(),
            "global-login-secret",
        );
        let prepared = prepare_standalone_send(input).unwrap();
        assert!(prepared.prepared.headers.iter().any(|(name, value)| name == "Authorization" && value == "Bearer basic-login-token"));
        assert_eq!(
            transport.requests.lock().unwrap()[0].url,
            "https://auth.example.test/session"
        );
    }

    #[test]
    fn global_credentials_round_trip_through_vault_and_next_send() {
        let (data, collection_id) = open_workspace("global-credential");
        let transport = Arc::new(FakeTransport::default());
        let input = send_input(
            &data,
            definition(&collection_id, AuthConfig::None, "global token"),
            transport.clone(),
        );
        let secrets = input.secret_store.clone();
        execute_standalone_send(prepare_standalone_send(input).unwrap()).unwrap();
        let variables = data.store.global_variables(&data.workspace).unwrap();
        assert!(
            !serde_json::to_string(&variables)
                .unwrap()
                .contains("global-secret-value")
        );
        let mut next = send_input(
            &data,
            definition(&collection_id, AuthConfig::None, ""),
            transport,
        );
        next.secret_store = secrets.clone();
        next.secrets = DraftSecrets::with_store(secrets, data.workspace.clone());
        next.definition.url = "https://example.test/{{accessToken}}".into();
        let prepared = prepare_standalone_send(next).unwrap();
        assert_eq!(
            prepared.prepared.url,
            "https://example.test/global-secret-value"
        );
        assert!(
            prepared
                .prepared
                .redactions
                .contains(&"global-secret-value".into())
        );
    }

    #[test]
    fn rerunning_assertions_does_not_send_the_request_again() {
        let (data, collection_id) = open_workspace("rerun-assertions");
        let transport = Arc::new(FakeTransport::default());
        let definition = definition(&collection_id, AuthConfig::None, "");
        let request = script_request(&definition);
        let input = send_input(&data, definition, transport.clone());
        let result = execute_standalone_send(prepare_standalone_send(input).unwrap()).unwrap();
        let (_, _, response) = exchange_from_send(result, &data.workspace, now_millis());
        let mut response = response.unwrap();
        rerun_response_tests(
            transport.as_ref(),
            "assertions",
            ScriptScopes::default(),
            request.clone(),
            &mut response,
        )
        .unwrap();
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
        assert_eq!(response.test_results.len(), 1);
        let mut scopes = ScriptScopes::default();
        scopes
            .local
            .insert("custom".into(), "private-rerun-value".into());
        rerun_response_tests_with_redactions(
            transport.as_ref(),
            "diagnostics",
            scopes,
            request,
            &mut response,
            &["private-rerun-value".into()],
        )
        .unwrap();
        assert!(
            !serde_json::to_string(&response_snapshot(&response))
                .unwrap()
                .contains("private-rerun-value")
        );
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn scripts_resolve_vault_variables_and_templates_in_both_phases_without_persisting_values() {
        let (data, collection_id) = open_workspace("script-vault");
        let mut request = definition(&collection_id, AuthConfig::None, "{{vault.shared}}");
        request.scripts.pre_request = "{{vault.shared}}".into();
        let (variables, mut secrets) = parse_session_variables(
            "secret:opaque=opaque-value\nnamed=prefix {{vault.shared}}\nmissing={{vault.missing}}\n# disabled={{vault.shared}}",
            "request",
        ).unwrap();
        request.variables = variables;
        secrets.insert(
            &crate::vault::vault_secret_reference("shared").unwrap(),
            "quote'\\n{{vault.literal}}-value",
        );
        let transport = Arc::new(FakeTransport::default());
        let mut input = send_input(&data, request.clone(), transport.clone());
        input.secrets.merge(secrets);
        let prepared = prepare_standalone_send(input).unwrap();
        assert_eq!(prepared.script_state.local["opaque"], "opaque-value");
        assert_eq!(
            prepared.script_state.local["named"],
            "prefix quote'\\n{{vault.literal}}-value"
        );
        assert!(!prepared.script_state.local.contains_key("missing"));
        assert!(!prepared.script_state.local.contains_key("disabled"));
        assert_eq!(prepared.script_state.vault.len(), 1);
        assert!(
            prepared
                .prepared
                .redactions
                .contains(&"quote'\\n{{vault.literal}}-value".into())
        );
        let sent = execute_standalone_send(prepared).unwrap();
        assert!(sent.response.is_ok());
        let inputs = transport.script_inputs.lock().unwrap();
        assert_eq!(inputs.len(), 2);
        assert!(!inputs[0].0);
        assert!(inputs[1].0);
        for (_, scopes) in inputs.iter() {
            assert_eq!(scopes.vault["shared"], "quote'\\n{{vault.literal}}-value");
            assert_eq!(scopes.local["opaque"], "opaque-value");
        }
        assert_eq!(sent.definition.variables, request.variables);
        let saved = data.store.list_requests(&collection_id).unwrap();
        let encoded = serde_json::to_string(&saved).unwrap();
        assert!(!encoded.contains("opaque-value"));
        assert!(!encoded.contains("quote'"));
    }

    #[test]
    fn copying_a_vault_value_to_an_ordinary_script_variable_keeps_it_secret() {
        let workspace = WorkspaceId::new("/test/script-secret-copy").unwrap();
        let store = MemorySecretStore::new();
        let mut variables = vec![row("copy", "prefix named-secret")];
        let after = BTreeMap::from([("copy".into(), "prefix named-secret".into())]);
        let mut redactions = vec!["named-secret".into()];
        vault_script_changes(
            &mut variables,
            &BTreeMap::new(),
            &after,
            "environment",
            &[],
            &workspace,
            &store,
            &mut redactions,
        )
        .unwrap();
        let VariableValue::Secret(reference) = &variables[0].value else {
            panic!("copied vault credentials must remain secret");
        };
        assert_eq!(
            store
                .get_secret(&workspace, reference)
                .unwrap()
                .expose_secret(),
            "prefix named-secret"
        );
        assert!(
            !serde_json::to_string(&variables)
                .unwrap()
                .contains("named-secret")
        );
    }

    fn basic_login_auth() -> AuthConfig {
        AuthConfig::Login {
            url: "{{auth_url}}/session".into(),
            headers: Vec::new(),
            basic: Some(crate::BasicLoginCredentials {
                username: "{{login_user}}".into(),
                password: SecretRef::new("basic-login-password").unwrap(),
            }),
            method: "POST".into(),
            body: String::new(),
            token_path: "data.access_token".into(),
            ttl_secs: None,
            access_token: None,
            expires_at: None,
        }
    }

    fn login_transport(status: u16) -> FakeTransport {
        use base64::Engine as _;
        FakeTransport {
            login_response: Some(super::super::response_from_snapshot(
                &crate::ResponseSnapshot {
                    status,
                    body_base64: base64::engine::general_purpose::STANDARD.encode(
                        r#"{"data":{"access_token":"basic-login-token"},"expires_in":600}"#,
                    ),
                    ..Default::default()
                },
            )),
            ..Default::default()
        }
    }

    #[test]
    fn basic_auth_url_matches_direct_request_credentials_and_resolves_login_headers() {
        use base64::Engine as _;
        for from_environment in [false, true] {
            let (data, collection_id) = open_workspace("basic-login-direct-parity");
            let transport = Arc::new(login_transport(200));
            let mut auth = basic_login_auth();
            let login_method = if from_environment { "GET" } else { "POST" };
            let password_ref = crate::vault::parse_vault_expression("{{vault.login_password}}")
                .unwrap()
                .unwrap();
            let headers = vec![
                KeyValueRow::enabled("Accept", "application/json"),
                KeyValueRow::enabled("X-Tenant", "{{tenant}}"),
                KeyValueRow::enabled("X-Api-Key", "{{vault.login_key}}"),
            ];
            let AuthConfig::Login {
                headers: login_headers,
                basic: Some(basic),
                method,
                ..
            } = &mut auth
            else {
                unreachable!()
            };
            *login_headers = headers.clone();
            *method = login_method.into();
            basic.password = password_ref.clone();
            let mut request = definition(
                &collection_id,
                if from_environment {
                    AuthConfig::Inherit
                } else {
                    auth.clone()
                },
                "",
            );
            request
                .variables
                .push(row("login_user", "{{vault.login_user}}"));
            let mut input = send_input(&data, request.clone(), transport.clone());
            input.collection = data
                .store
                .collection(&data.workspace, &collection_id)
                .unwrap();
            input
                .collection
                .as_mut()
                .unwrap()
                .variables
                .push(row("tenant", "test-tenant"));
            input.environment_source =
                "auth_url=https://auth.example.test\nlogin_user=shadowed-user".into();
            if from_environment {
                input.environment_auth = auth.clone();
            }
            input.secrets.insert(&password_ref, "p:a+ss/word!=");
            for (name, value) in [
                ("login_user", "tést-user"),
                ("login_key", "vault-header-value"),
            ] {
                let reference =
                    crate::vault::parse_vault_expression(&format!("{{{{vault.{name}}}}}"))
                        .unwrap()
                        .unwrap();
                input.secrets.insert(&reference, value);
            }
            let mut direct = request;
            direct.url = "{{auth_url}}/session".into();
            direct.method = HttpMethod::new(login_method).unwrap();
            direct.headers = headers;
            direct.auth = AuthConfig::Basic {
                username: "{{login_user}}".into(),
                password: password_ref,
            };
            let (environment, _) =
                parse_session_variables(&input.environment_source, "test").unwrap();
            let context = CompileContext {
                global: &[],
                environment: &environment,
                data: &[],
                local: &[],
                secrets: &input.secrets,
                environment_base_url: None,
                environment_auth: None,
            };
            let (direct, _) =
                crate::compile_request(&direct, input.collection.as_ref(), &context).unwrap();
            let prepared = prepare_standalone_send(input).unwrap();
            let sent = transport.requests.lock().unwrap();
            assert_eq!(sent.len(), 1);
            assert_eq!(sent[0].url, direct.url);
            assert_eq!(sent[0].method, direct.method);
            assert_eq!(sent[0].body, direct.body);
            assert_eq!(sent[0].headers, direct.headers);
            let encoded =
                base64::engine::general_purpose::STANDARD.encode("tést-user:p:a+ss/word!=");
            assert!(
                sent[0]
                    .headers
                    .contains(&("Authorization".into(), format!("Basic {encoded}")))
            );
            assert!(
                sent[0]
                    .headers
                    .contains(&("X-Tenant".into(), "test-tenant".into()))
            );
            assert!(
                sent[0]
                    .headers
                    .contains(&("X-Api-Key".into(), "vault-header-value".into()))
            );
            let snapshot = serde_json::to_string(&sent[0].redacted_snapshot()).unwrap();
            assert!(!snapshot.contains(&encoded));
            assert!(!snapshot.contains("vault-header-value"));
            assert!(
                !prepared
                    .prepared
                    .headers
                    .iter()
                    .any(|(name, _)| name == "X-Api-Key")
            );
        }
    }

    #[test]
    fn basic_auth_url_uses_pre_request_script_credentials() {
        let (data, collection_id) = open_workspace("basic-login-pre-script");
        let transport = Arc::new(login_transport(200));
        let mut auth = basic_login_auth();
        let AuthConfig::Login {
            basic: Some(basic),
            headers,
            ..
        } = &mut auth
        else {
            unreachable!()
        };
        basic.username = "{{preValue}}".into();
        headers.push(KeyValueRow::enabled("X-Sign-In", "{{preValue}}"));
        let mut request = definition(&collection_id, auth, "");
        request.scripts.pre_request = "prepare variable".into();
        let mut input = send_input(&data, request, transport.clone());
        input.environment_source = "auth_url=https://auth.example.test".into();
        input
            .secrets
            .insert(&SecretRef::new("basic-login-password").unwrap(), "password");
        let prepared = prepare_standalone_send(input).unwrap();
        let sent = transport.requests.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].headers.contains(&(
            "Authorization".into(),
            "Basic cHJlcGFyZWQ6cGFzc3dvcmQ=".into()
        )));
        assert!(
            sent[0]
                .headers
                .contains(&("X-Sign-In".into(), "prepared".into()))
        );
        let AuthConfig::Login {
            basic: Some(basic), ..
        } = &prepared.definition.auth
        else {
            unreachable!()
        };
        assert_eq!(basic.username, "{{preValue}}");
    }

    #[test]
    fn basic_login_automatically_authenticates_and_reuses_request_or_environment_cache() {
        use base64::Engine as _;
        for from_environment in [false, true] {
            let (data, collection_id) = open_workspace(if from_environment {
                "basic-login-environment"
            } else {
                "basic-login-request"
            });
            let transport = Arc::new(login_transport(200));
            let auth = basic_login_auth();
            let request = definition(
                &collection_id,
                if from_environment {
                    AuthConfig::Inherit
                } else {
                    auth.clone()
                },
                "",
            );
            let mut input = send_input(&data, request, transport.clone());
            if from_environment {
                let mut saved = environment(&data);
                saved.auth = auth.clone();
                data.store.upsert_environment(&saved).unwrap();
                input.environment_scope = format!("environment.{}", saved.id.as_str());
                input.environment = Some(saved);
                input.environment_auth = auth;
            }
            input.environment_base_url = "https://api.example.test".into();
            input.environment_source =
                "auth_url=https://auth.example.test\nlogin_user=operator".into();
            let password_ref = SecretRef::new("basic-login-password").unwrap();
            input.secrets.insert(&password_ref, "login-password!");
            let secret_store = input.secret_store.clone();

            let prepared = prepare_standalone_send(input).unwrap();
            assert_eq!(transport.requests.lock().unwrap().len(), 1);
            assert_eq!(prepared.prepared.url, "https://example.test/items");
            assert_eq!(
                prepared.prepared.headers,
                vec![("Authorization".into(), "Bearer basic-login-token".into())]
            );
            let result = execute_standalone_send(prepared).unwrap();
            assert_eq!(result.response.as_ref().unwrap().status, 200);
            let durable = serde_json::to_string(&result.snapshot).unwrap();
            assert!(!durable.contains("basic-login-token"));
            assert!(!durable.contains("login-password!"));

            let mut second = send_input(&data, result.definition, transport.clone());
            second.secret_store = secret_store.clone();
            second.secrets = DraftSecrets::with_store(secret_store, data.workspace.clone());
            second.environment_source =
                "auth_url=https://auth.example.test\nlogin_user=operator".into();
            if let Some(saved) = result.environment {
                second.environment_scope = format!("environment.{}", saved.id.as_str());
                second.environment_auth = saved.auth.clone();
                second.environment = Some(saved);
            }
            let result = execute_standalone_send(prepare_standalone_send(second).unwrap()).unwrap();
            assert_eq!(result.response.as_ref().unwrap().status, 200);

            let sent = transport.requests.lock().unwrap();
            assert_eq!(sent.len(), 3, "one login followed by two API requests");
            assert_eq!(sent[0].url, "https://auth.example.test/session");
            assert_eq!(sent[0].method.as_str(), "POST");
            assert_eq!(sent[0].body, crate::PreparedBody::None);
            let credential =
                base64::engine::general_purpose::STANDARD.encode("operator:login-password!");
            assert_eq!(
                sent[0].headers,
                vec![("Authorization".into(), format!("Basic {credential}"))]
            );
            assert!(sent[0].redactions.contains(&credential));
            assert!(sent[0].redactions.contains(&"login-password!".to_string()));
            for request in &sent[1..] {
                assert_eq!(request.url, "https://example.test/items");
                assert_eq!(
                    request.headers,
                    vec![("Authorization".into(), "Bearer basic-login-token".into())]
                );
            }
        }
    }

    #[test]
    fn basic_login_failure_stops_the_api_send() {
        let (data, collection_id) = open_workspace("basic-login-rejected");
        let transport = Arc::new(login_transport(401));
        let mut input = send_input(
            &data,
            definition(&collection_id, basic_login_auth(), ""),
            transport.clone(),
        );
        input.environment_source = "auth_url=https://auth.example.test\nlogin_user=operator".into();
        input.secrets.insert(
            &SecretRef::new("basic-login-password").unwrap(),
            "wrong-password",
        );
        let error = prepare_standalone_send(input).err().unwrap();
        assert!(error.contains("Sign-in request failed with HTTP 401"));
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn captured_token_survives_next_send_without_overwriting_other_edits() {
        let (data, collection_id) = open_workspace("capture-token");
        let mut environment = environment(&data);
        let mut disabled = row("disabled", "keep");
        disabled.enabled = false;
        environment.variables.push(disabled.clone());
        data.store.upsert_environment(&environment).unwrap();
        let mut input = send_input(
            &data,
            definition(&collection_id, AuthConfig::None, "capture token"),
            Arc::new(FakeTransport::default()),
        );
        input.environment = Some(environment.clone());
        input.environment_source = "obsolete=old".into();
        input.environment_scope = format!("environment.{}", environment.id.as_str());
        input.collection = data
            .store
            .collection(&data.workspace, &collection_id)
            .unwrap();
        let secrets = input.secret_store.clone();
        let prepared = prepare_standalone_send(input).unwrap();
        // An edit made while the HTTP request is running must survive.
        environment.variables.push(row("editedDuringSend", "keep"));
        data.store.upsert_environment(&environment).unwrap();
        let result = execute_standalone_send(prepared).unwrap();
        assert!(result.test_results.iter().all(|test| test.passed));
        let stored = data
            .store
            .list_environments(&data.workspace)
            .unwrap()
            .remove(0);
        assert!(
            !stored
                .variables
                .iter()
                .any(|row| row.key == "obsolete" || row.key == "temporary")
        );
        assert!(stored.variables.contains(&disabled));
        assert!(
            stored
                .variables
                .iter()
                .any(|row| row.key == "editedDuringSend")
        );
        let reference = match &stored
            .variables
            .iter()
            .find(|row| row.key == "accessToken")
            .unwrap()
            .value
        {
            VariableValue::Secret(reference) => reference,
            value => panic!("expected vault reference, got {value:?}"),
        };
        assert_eq!(
            secrets
                .get_secret(&data.workspace, reference)
                .unwrap()
                .expose_secret(),
            "response-token-value"
        );
        assert!(
            !serde_json::to_string(&stored)
                .unwrap()
                .contains("response-token-value")
        );
        assert!(
            result
                .prepared
                .redactions
                .contains(&"response-token-value".into())
        );
        assert!(
            result
                .collection
                .unwrap()
                .variables
                .iter()
                .any(|row| row.key == "sharedId")
        );
        let mut next = send_input(
            &data,
            definition(&collection_id, AuthConfig::None, ""),
            Arc::new(FakeTransport::default()),
        );
        next.definition.headers.push(KeyValueRow {
            id: RowId::new(),
            key: "Authorization".into(),
            value: "Bearer {{accessToken}}".into(),
            enabled: true,
            description: String::new(),
        });
        next.environment_scope = format!("environment.{}", stored.id.as_str());
        next.environment_source = "secret:accessToken=".into();
        next.environment = Some(stored);
        next.secrets = DraftSecrets::with_store(secrets.clone(), data.workspace.clone());
        next.secret_store = secrets;
        let next = prepare_standalone_send(next).unwrap();
        assert_eq!(
            next.script_state
                .environment
                .get("accessToken")
                .map(String::as_str),
            Some("response-token-value")
        );
        assert!(
            next.prepared
                .headers
                .contains(&("Authorization".into(), "Bearer response-token-value".into()))
        );
    }

    #[test]
    fn pre_request_shared_changes_survive_a_post_script_failure() {
        let (data, collection_id) = open_workspace("pre-persist");
        let environment = environment(&data);
        data.store.upsert_environment(&environment).unwrap();
        let mut request = definition(&collection_id, AuthConfig::None, "fail post");
        request.scripts.pre_request = "prepare variable".into();
        let mut input = send_input(&data, request, Arc::new(FakeTransport::default()));
        input.environment = Some(environment);
        let result = execute_standalone_send(prepare_standalone_send(input).unwrap()).unwrap();
        assert!(result.test_results.iter().any(|test| !test.passed));
        assert!(
            result
                .environment
                .unwrap()
                .variables
                .iter()
                .any(|row| row.key == "preValue")
        );
    }

    #[test]
    fn capture_without_a_selected_environment_reports_failure() {
        let (data, collection_id) = open_workspace("missing-environment");
        let input = send_input(
            &data,
            definition(&collection_id, AuthConfig::None, "capture token"),
            Arc::new(FakeTransport::default()),
        );
        let result = execute_standalone_send(prepare_standalone_send(input).unwrap()).unwrap();
        assert!(
            result
                .test_results
                .iter()
                .any(|test| test.name == "environment variable persistence" && !test.passed)
        );
    }

    #[test]
    fn vault_failure_is_reported_without_persisting_plaintext() {
        use crate::SecretStoreError;
        struct UnavailableVault;
        impl SecretStore for UnavailableVault {
            fn set_secret(
                &self,
                _: &WorkspaceId,
                _: &SecretRef,
                _: SecretValue,
            ) -> Result<(), SecretStoreError> {
                Err(SecretStoreError::BackendUnavailable)
            }
            fn get_secret(
                &self,
                _: &WorkspaceId,
                _: &SecretRef,
            ) -> Result<SecretValue, SecretStoreError> {
                Err(SecretStoreError::BackendUnavailable)
            }
            fn delete_secret(
                &self,
                _: &WorkspaceId,
                _: &SecretRef,
            ) -> Result<(), SecretStoreError> {
                Err(SecretStoreError::BackendUnavailable)
            }
        }
        let (data, collection_id) = open_workspace("vault-failure");
        let environment = environment(&data);
        data.store.upsert_environment(&environment).unwrap();
        let mut input = send_input(
            &data,
            definition(&collection_id, AuthConfig::None, "capture token"),
            Arc::new(FakeTransport::default()),
        );
        input.environment = Some(environment.clone());
        input.secret_store = Arc::new(UnavailableVault);
        let result = execute_standalone_send(prepare_standalone_send(input).unwrap()).unwrap();
        assert!(result.test_results.iter().any(|test| test.name
            == "environment variable persistence"
            && !test.passed
            && test.error.as_deref().is_some_and(|error| {
                error.contains("secure secret storage is unavailable")
                    && error.contains("credential vault and retry")
                    && !error.contains("response-token-value")
            })));
        assert!(
            result
                .prepared
                .redactions
                .contains(&"response-token-value".into())
        );
        assert_eq!(
            data.store.list_environments(&data.workspace).unwrap()[0].variables,
            environment.variables
        );
    }

    #[test]
    fn non_credential_named_secret_stays_secret_in_compilation_and_history() {
        let (data, collection_id) = open_workspace("custom-secret");
        let mut input = send_input(
            &data,
            definition(&collection_id, AuthConfig::None, ""),
            Arc::new(FakeTransport::default()),
        );
        let reference = SecretRef::new("custom-ref").unwrap();
        input
            .secret_store
            .set_secret(
                &data.workspace,
                &reference,
                SecretValue::new("private-value"),
            )
            .unwrap();
        let mut variable = row("custom", "");
        variable.value = VariableValue::Secret(reference);
        input.definition.variables.push(variable);
        input.definition.url = "https://example.test/{{custom}}".into();
        let prepared = prepare_standalone_send(input).unwrap();
        assert_eq!(prepared.prepared.url, "https://example.test/private-value");
        assert_eq!(
            prepared
                .script_state
                .local
                .get("custom")
                .map(String::as_str),
            Some("private-value")
        );
        assert!(
            prepared
                .prepared
                .redactions
                .contains(&"private-value".into())
        );
        assert!(
            !serde_json::to_string(&prepared.snapshot)
                .unwrap()
                .contains("private-value")
        );
    }

    #[test]
    fn pre_request_failures_and_console_are_retained_and_redacted() {
        let (data, collection_id) = open_workspace("phase-diagnostics");
        let mut request = definition(&collection_id, AuthConfig::None, "diagnostics");
        request.scripts.pre_request = "diagnostics".into();
        let (variables, secrets) = parse_session_variables(
            "secret:custom=private-script-value\naccessToken=literal-script-value\npublic=visible",
            "request",
        )
        .unwrap();
        request.variables = variables;
        let transport = Arc::new(FakeTransport::default());
        let mut input = send_input(&data, request, transport.clone());
        input.secrets.merge(secrets);
        let prepared = prepare_standalone_send(input).unwrap();
        assert_eq!(prepared.pre_tests.len(), 1);
        assert!(!prepared.pre_tests[0].passed);
        let result = execute_standalone_send(prepared).unwrap();
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
        let (exchange, redactions, response) =
            exchange_from_send(result, &data.workspace, now_millis());
        let response = response.unwrap();
        assert_eq!(response.test_results.len(), 2);
        assert!(response.test_results[0].name.starts_with("[Pre-request]"));
        assert!(!response.test_results[0].passed);
        assert!(response.test_results[1].name.starts_with("[Post-response]"));
        assert_eq!(response.console.len(), 2);
        assert_eq!(response.console[0].phase, "Pre-request");
        assert_eq!(response.console[1].phase, "Post-response");
        assert!(response.console[0].message.contains("visible"));
        let live = serde_json::to_string(&response_snapshot(&response)).unwrap();
        assert!(!live.contains("private-script-value"));
        assert!(!live.contains("literal-script-value"));
        data.store.record_exchange(&exchange, &redactions).unwrap();
        let saved = data.store.history(&data.workspace, 10).unwrap();
        assert_eq!(saved[0].console, response.console);
        let saved = serde_json::to_string(&saved).unwrap();
        assert!(!saved.contains("private-script-value"));
        assert!(!saved.contains("literal-script-value"));
    }

    #[test]
    fn script_errors_redact_secret_values_before_returning_to_the_caller() {
        for pre in [true, false] {
            let (data, collection_id) = open_workspace(if pre {
                "pre-diagnostic-error"
            } else {
                "post-diagnostic-error"
            });
            let mut request = definition(
                &collection_id,
                AuthConfig::None,
                if pre { "" } else { "diagnostics error" },
            );
            if pre {
                request.scripts.pre_request = "diagnostics error".into();
            }
            request.url = "https://example.test/new-cookie".into();
            if pre {
                request.url = "https://example.test/{{missing}}".into();
                request.headers.push(KeyValueRow::enabled(
                    "Authorization",
                    "Bearer literal-header-value",
                ));
            }
            let (variables, secrets) =
                parse_session_variables("secret:custom=private-error-value", "request").unwrap();
            request.variables = variables;
            let mut input = send_input(&data, request, Arc::new(FakeTransport::default()));
            input.secrets.merge(secrets);
            if pre {
                let error = prepare_standalone_send(input)
                    .err()
                    .expect("pre script must fail");
                assert!(!error.contains("private-error-value"));
                assert!(!error.contains("literal-header-value"));
                assert!(error.contains("redacted"));
            } else {
                let result =
                    execute_standalone_send(prepare_standalone_send(input).unwrap()).unwrap();
                assert!(!result.test_results[0].passed);
                assert!(
                    !serde_json::to_string(&result.test_results)
                        .unwrap()
                        .contains("private-error-value")
                );
                assert_eq!(result.console[0].level, "error");
                assert!(!result.console[0].message.contains("private-error-value"));
                assert!(!result.console[0].message.contains("private-cookie-value"));
            }
        }
    }

    #[test]
    fn failed_send_keeps_redacted_pre_request_console_and_tests() {
        let (data, collection_id) = open_workspace("failed-send-diagnostics");
        let mut request = definition(&collection_id, AuthConfig::None, "");
        request.scripts.pre_request = "diagnostics".into();
        request.url = "https://example.test/unreachable/{{custom}}".into();
        let (variables, secrets) =
            parse_session_variables("secret:custom=private-script-value", "request").unwrap();
        request.variables = variables;
        let mut input = send_input(&data, request, Arc::new(FakeTransport::default()));
        input.secrets.merge(secrets);
        let result = execute_standalone_send(prepare_standalone_send(input).unwrap()).unwrap();
        let (exchange, redactions, response) =
            exchange_from_send(result, &data.workspace, now_millis());
        assert!(response.is_err());
        assert!(exchange.response.is_none());
        assert_eq!(exchange.console.len(), 1);
        assert_eq!(exchange.test_results.len(), 1);
        assert!(!exchange.test_results[0].passed);
        assert!(
            !serde_json::to_string(&exchange)
                .unwrap()
                .contains("private-script-value")
        );
        data.store.record_exchange(&exchange, &redactions).unwrap();
        assert_eq!(
            data.store.history(&data.workspace, 10).unwrap()[0].console,
            exchange.console
        );
    }

    #[test]
    fn a_headless_send_round_trips_into_a_recorded_exchange() {
        let (data, collection_id) = open_workspace("round-trip");
        let definition = definition(
            &collection_id,
            AuthConfig::None,
            "pm.test('typed service assertion', () => {});",
        );
        data.store.upsert_request(&definition).unwrap();
        let transport = Arc::new(FakeTransport::default());
        let started_at = now_millis();

        let prepared =
            prepare_standalone_send(send_input(&data, definition.clone(), transport.clone()))
                .unwrap();
        assert_eq!(prepared.prepared.url, "https://example.test/items");
        assert!(!prepared.pre_script_ran);
        let result = execute_standalone_send(prepared).unwrap();
        let (exchange, redactions, response) =
            exchange_from_send(result, &data.workspace, started_at);

        assert!(redactions.is_empty());
        let response = response.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.test_results.len(), 1);
        assert!(response.test_results[0].passed);
        assert_eq!(exchange.request_id.as_ref(), Some(&definition.id));
        assert_eq!(exchange.error, None);
        let snapshot = exchange.response.as_ref().unwrap();
        assert_eq!(snapshot.status, 200);
        assert_eq!(snapshot.test_results, response.test_results);
        assert!(exchange.completed_at >= exchange.started_at);
        data.store.record_exchange(&exchange, &redactions).unwrap();
        let history = data.store.history(&data.workspace, 10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, exchange.id);
        // Nothing was aborted on the way through.
        assert!(transport.cancels.lock().unwrap().is_empty());
    }

    #[test]
    fn a_pkce_client_without_a_token_fails_closed_without_a_browser() {
        let (data, collection_id) = open_workspace("pkce-fail-closed");
        let definition = definition(
            &collection_id,
            AuthConfig::OAuth2AuthorizationCodePkce {
                headers: Vec::new(),
                authorization_endpoint: "https://identity.example.test/authorize".into(),
                token_endpoint: "https://identity.example.test/token".into(),
                client_id: "desktop-client".into(),
                scopes: Vec::new(),
                redirect_uri: "http://127.0.0.1/callback".into(),
                access_token: None,
                refresh_token: None,
                expires_at: None,
            },
            "",
        );
        data.store.upsert_request(&definition).unwrap();
        let transport = Arc::new(FakeTransport::default());

        let error = prepare_standalone_send(send_input(&data, definition, transport.clone()))
            .err()
            .unwrap();

        assert_eq!(error, PKCE_NEEDS_BROWSER);
        assert!(error.contains("open the request in the app"));
        // The prepare guard had nothing armed, so no operation was cancelled.
        assert!(transport.cancels.lock().unwrap().is_empty());
    }
}
