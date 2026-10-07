//! Collection runs: every selected request, once per data row, with the
//! script scopes, cookie jar, OAuth caches and result rows the desktop
//! runner keeps — but driven by a plain loop a sidecar can call.
//!
//! [`prepare_collection_run`] does the work the desktop does before it shows
//! "Running…"; [`run_collection`] is the loop itself, reporting each item
//! through [`RunControl::on_item`] and stopping when [`RunControl::cancel`]
//! is raised. [`RunSession`] is that loop one item at a time, for a caller
//! that waits between items on its own clock.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::{
    AuthConfig, Collection, CollectionId, CollectionRun, CompileContext, CookieJar, Environment,
    EnvironmentId, Folder, FolderId, PreparedRequest, RequestId, RunId, RunItemResult, RunStatus,
    SavedRequest, SecretStore, TestResult, Variable, WorkbenchStore, WorkspaceId,
    compile_request_with_folder_chain, effective_auth,
};

use super::oauth::{
    OAuthVariables, refresh_expired_oauth_auth_with, refresh_expired_oauth_token_in_context,
    same_managed_auth_source,
};
use super::secrets::{DraftSecrets, parse_data_rows, parse_session_variables};
use super::send::{
    OperationAbortGuard, apply_pre_request_result, apply_script_cookie_mutations,
    carry_shared_script_scopes, cookie_scope_for_url, folder_chain, folder_is_within, now_millis,
    now_seconds, ordered_script, request_script_scopes, request_script_vault,
    resolved_cookie_scope_url, resolved_script_variables, script_request, script_secret_values,
    script_variables, variables_from_map,
};
use super::transport::{
    FileCapabilities, OperationPhase, ScriptRequestView, ScriptResponseView, ScriptScopes,
    WorkbenchTransport, response_snapshot,
};

/// The desktop runner's bounds on its iteration and delay fields.
pub const MAX_ITERATIONS: usize = 1_000;
pub const MAX_DELAY_MS: u64 = 60_000;
/// Upper bound on executed requests, including script-directed loops.
pub const MAX_RUN_STEPS: usize = 10_000;

/// A run ready to execute: its `Running` row, the requests in order, the
/// parsed environment and data rows, and the stores it writes to.
pub struct RunPreparation {
    pub run: CollectionRun,
    pub collection: Option<Collection>,
    pub folders: Vec<Folder>,
    pub requests: Vec<SavedRequest>,
    pub environment: Vec<Variable>,
    pub globals: Vec<Variable>,
    pub environment_name: Option<String>,
    pub environment_base_url: String,
    pub environment_auth: AuthConfig,
    pub rows: Vec<Vec<Variable>>,
    pub secrets: DraftSecrets,
    pub delay_ms: u64,
    pub keep_variables: bool,
    pub store: Arc<WorkbenchStore>,
    pub secret_store: Arc<dyn SecretStore>,
    pub cookie_jar: CookieJar,
    pub file_capabilities: FileCapabilities,
}

/// What a caller supplies to [`prepare_collection_run`]. `requests` are the
/// candidates in collection order; [`select_run_requests`] applies the
/// folder/selection filter the desktop runner applies.
pub struct RunPreparationInput {
    pub workspace_id: WorkspaceId,
    pub collection_id: CollectionId,
    pub environment_id: Option<EnvironmentId>,
    /// The saved environment the run resolves through, when one is active:
    /// a renewed environment-level OAuth cache is written back onto it.
    pub environment: Option<Environment>,
    pub stop_on_error: bool,
    pub selected_folder_id: Option<FolderId>,
    pub selected_request_ids: Vec<RequestId>,
    pub collection: Option<Collection>,
    pub folders: Vec<Folder>,
    pub requests: Vec<SavedRequest>,
    pub environment_source: String,
    pub environment_scope: String,
    pub environment_base_url: String,
    pub environment_auth: AuthConfig,
    pub data_source: String,
    pub secrets: DraftSecrets,
    pub iteration_limit: Option<usize>,
    pub delay_ms: u64,
    pub keep_variables: bool,
    pub store: Arc<WorkbenchStore>,
    pub secret_store: Arc<dyn SecretStore>,
    /// The durable jar the run reads from and writes back to.
    pub cookie_jar: CookieJar,
    /// Picker grants for file bodies; headless callers pass the default and
    /// file bodies fail with a message to choose the file in the app.
    pub file_capabilities: FileCapabilities,
}

impl RunPreparationInput {
    /// The desktop runner's field validation.
    pub fn validate(&self) -> Result<(), String> {
        if self
            .iteration_limit
            .is_some_and(|limit| !(1..=MAX_ITERATIONS).contains(&limit))
        {
            return Err("Iterations must be between 1 and 1000.".into());
        }
        if self.delay_ms > MAX_DELAY_MS {
            return Err("Runner delay must be between 0 and 60000 ms.".into());
        }
        if self.requests.is_empty() {
            return Err("Select at least one request for this run.".into());
        }
        Ok(())
    }
}

/// The requests a run covers, filtered the way the desktop runner filters:
/// the collection's requests, within `selected_folder_id` when set, and
/// among `selected_request_ids` when that selection is active.
pub fn select_run_requests(
    requests: &[SavedRequest],
    folders: &[Folder],
    collection_id: &CollectionId,
    selected_folder_id: Option<&FolderId>,
    selected_request_ids: Option<&[RequestId]>,
) -> Vec<SavedRequest> {
    requests
        .iter()
        .filter(|request| {
            request.collection_id == *collection_id
                && selected_folder_id.is_none_or(|folder| {
                    request.folder_id.as_ref().is_some_and(|request_folder| {
                        folder_is_within(request_folder, folder, folders)
                    })
                })
                && selected_request_ids.is_none_or(|ids| ids.contains(&request.id))
        })
        .cloned()
        .collect()
}

/// How a caller steers [`run_collection`].
#[derive(Default)]
pub struct RunControl {
    /// Raised to stop before the next item; the run ends `Canceled`.
    pub cancel: Arc<AtomicBool>,
    /// Called after each item completes, with the row just appended.
    #[allow(clippy::type_complexity)]
    pub on_item: Option<Box<dyn Fn(&RunItemResult) + Send + Sync>>,
}

impl RunControl {
    fn canceled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
}

/// The end of a run: the persisted `CollectionRun`, the redactions its
/// storage was masked with, the cookie jar the run left behind, and the
/// storage error if the final write failed.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub run: CollectionRun,
    pub redactions: Vec<String>,
    /// The jar as the run left it (persisted unless `error` is set).
    pub cookie_jar: CookieJar,
    pub error: Option<String>,
}

/// Parse the data rows and environment, persist the session secrets and
/// renew every request's managed OAuth cache, as the desktop runner does
/// before its first item.
pub fn prepare_collection_run(
    mut input: RunPreparationInput,
    transport: &dyn WorkbenchTransport,
) -> Result<RunPreparation, String> {
    input.validate()?;
    let mut rows = parse_data_rows(&input.data_source)?;
    if rows.is_empty() {
        return Err("Runner data contains no iteration rows.".into());
    }
    if let Some(limit) = input.iteration_limit {
        let source = rows.clone();
        rows = (0..limit)
            .map(|index| source[index % source.len()].clone())
            .collect();
    }
    let (environment, environment_secrets) =
        parse_session_variables(&input.environment_source, &input.environment_scope)?;
    input.secrets.merge(environment_secrets);
    input
        .secrets
        .persist(input.secret_store.as_ref(), &input.workspace_id)?;
    let globals = input
        .store
        .global_variables(&input.workspace_id)
        .map_err(|e| e.to_string())?;
    for request in &mut input.requests {
        let folders = folder_chain(&input.folders, request);
        refresh_expired_oauth_token_in_context(
            request,
            &mut input.secrets,
            &input.workspace_id,
            input.store.as_ref(),
            input.secret_store.as_ref(),
            transport,
            OAuthVariables {
                globals: &globals,
                collection: input.collection.as_ref(),
                folders: &folders,
                environment: &environment,
                base_url: Some(&input.environment_base_url),
            },
        )?;
    }
    refresh_run_environment_oauth(&mut input, transport)?;
    let environment_name = input.environment.as_ref().map(|e| e.name.clone());
    Ok(RunPreparation {
        globals,
        environment_name,
        run: CollectionRun {
            id: RunId::new(),
            workspace_id: input.workspace_id,
            collection_id: input.collection_id,
            environment_id: input.environment_id,
            iteration_count: rows.len() as u32,
            stop_on_error: input.stop_on_error,
            selected_folder_id: input.selected_folder_id,
            selected_request_ids: input.selected_request_ids,
            delay_ms: input.delay_ms,
            keep_variable_values: input.keep_variables,
            item_results: Vec::new(),
            status: RunStatus::Running,
            started_at: now_millis(),
            completed_at: None,
        },
        collection: input.collection,
        folders: input.folders,
        requests: input.requests,
        environment,
        environment_base_url: input.environment_base_url,
        environment_auth: input.environment_auth,
        rows,
        secrets: input.secrets,
        delay_ms: input.delay_ms,
        keep_variables: input.keep_variables,
        store: input.store,
        secret_store: input.secret_store,
        cookie_jar: input.cookie_jar,
        file_capabilities: input.file_capabilities,
    })
}

/// Renew the environment's managed OAuth cache when any selected request's
/// auth chain ends at the environment — what a standalone send does through
/// `refresh_environment_oauth`, which the runner otherwise never reaches.
/// The renewed cache lands on the saved environment when it still describes
/// the same client, so the app's next send reuses it.
fn refresh_run_environment_oauth(
    input: &mut RunPreparationInput,
    transport: &dyn WorkbenchTransport,
) -> Result<(), String> {
    let environment_owned = input.requests.iter().any(|request| {
        let effective = effective_auth(
            request,
            &folder_chain(&input.folders, request),
            input.collection.as_ref(),
            Some(&input.environment_auth),
        );
        std::ptr::eq(effective, &input.environment_auth)
    });
    if !environment_owned {
        return Ok(());
    }
    let allow_private_network = input
        .requests
        .iter()
        .any(|request| request.settings.allow_private_network);
    if super::oauth::oauth_refresh_needed(
        &input.environment_auth,
        allow_private_network,
        now_seconds(),
    )?
    .is_none()
    {
        return Ok(());
    }
    let (environment, _) =
        parse_session_variables(&input.environment_source, &input.environment_scope)?;
    let request = input
        .requests
        .iter()
        .find(|request| {
            std::ptr::eq(
                effective_auth(
                    request,
                    &folder_chain(&input.folders, request),
                    input.collection.as_ref(),
                    Some(&input.environment_auth),
                ),
                &input.environment_auth,
            )
        })
        .ok_or_else(|| "no request uses the environment's auth".to_string())?;
    let folders = folder_chain(&input.folders, request);
    let globals = input
        .store
        .global_variables(&input.workspace_id)
        .map_err(|e| e.to_string())?;
    let variables = OAuthVariables {
        globals: &globals,
        collection: input.collection.as_ref(),
        folders: &folders,
        environment: &environment,
        base_url: Some(&input.environment_base_url),
    };
    let headers = variables.headers(&input.environment_auth, &request.variables, &input.secrets)?;
    let request_variables = request.variables.clone();
    let request_secrets = input.secrets.clone();
    let refreshed = refresh_expired_oauth_auth_with(
        &mut input.environment_auth,
        &input.environment_scope,
        allow_private_network,
        &mut input.secrets,
        &input.workspace_id,
        input.secret_store.as_ref(),
        now_seconds(),
        |mut request| {
            variables.resolve_token_request(&mut request, &request_variables, &request_secrets)?;
            request.headers = headers;
            transport.exchange_oauth_token(request)
        },
    )?;
    if !refreshed {
        return Ok(());
    }
    if let Some(saved) = input
        .environment
        .as_ref()
        .filter(|saved| same_managed_auth_source(&saved.auth, &input.environment_auth))
    {
        let environment = Environment {
            auth: input.environment_auth.clone(),
            ..saved.clone()
        };
        input
            .store
            .upsert_environment(&environment)
            .map_err(|error| error.to_string())?;
        input.environment = Some(environment);
    }
    Ok(())
}

/// One runner item up to its compiled request: pre-request script, compile,
/// cookie mutations; returns `(request, tests, scopes, operation_id,
/// pre_script_ran, cookie_jar)`.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub fn prepare_run_request(
    original: SavedRequest,
    collection: Option<Collection>,
    all_folders: Vec<Folder>,
    environment: (String, AuthConfig),
    row: Vec<Variable>,
    mut runtime_scopes: ScriptScopes,
    secrets: DraftSecrets,
    mut cookie_jar: CookieJar,
    store: Arc<WorkbenchStore>,
    secret_store: Arc<dyn SecretStore>,
    operation_id: String,
    runner_transport: Arc<dyn WorkbenchTransport>,
    diagnostic_redactions: Vec<String>,
) -> Result<
    (
        PreparedRequest,
        String,
        ScriptScopes,
        String,
        bool,
        CookieJar,
        Vec<TestResult>,
        Vec<crate::ConsoleEntry>,
    ),
    String,
> {
    let mut operation_guard =
        OperationAbortGuard::new(operation_id.clone(), runner_transport.clone());
    let script_request_id = original.id.as_str().to_string();
    let folders = folder_chain(&all_folders, &original);
    let script = ordered_script(collection.as_ref(), &folders, &original, false);
    let pre_script_ran = !script.trim().is_empty();
    let pre_phase = OperationPhase {
        operation_id: operation_id.clone(),
        starts_operation: true,
        final_phase: false,
    };
    runtime_scopes
        .local
        .extend(resolved_script_variables(&original.variables, &secrets));
    runtime_scopes.iteration_data = resolved_script_variables(&row, &secrets);
    let tests = ordered_script(collection.as_ref(), &folders, &original, true);
    runtime_scopes.vault =
        request_script_vault(&original, [script.as_str(), tests.as_str()], &secrets);
    runtime_scopes.allow_private_network = original.settings.allow_private_network;
    let seed_environment = variables_from_map(&runtime_scopes.environment);
    let seed_data = variables_from_map(&runtime_scopes.iteration_data);
    let seed_local = variables_from_map(&runtime_scopes.local);
    let mut seed_collection = collection.clone();
    if let Some(collection) = seed_collection.as_mut() {
        collection.variables = variables_from_map(&runtime_scopes.collection);
    }
    let (environment_base_url, environment_auth) = environment;
    let seed_globals = variables_from_map(&runtime_scopes.globals);
    let seed_context = CompileContext {
        global: &seed_globals,
        environment: &seed_environment,
        data: &seed_data,
        local: &seed_local,
        secrets: &secrets,
        environment_base_url: Some(&environment_base_url),
        environment_auth: Some(&environment_auth),
    };
    let cookie_seed_url =
        resolved_cookie_scope_url(&original, seed_collection.as_ref(), &folders, &seed_context);
    runtime_scopes.cookies =
        cookie_scope_for_url(&cookie_jar, secret_store.as_ref(), &cookie_seed_url)?;
    let initial_script_cookies = runtime_scopes.cookies.clone();
    let mut pre_redactions = super::transport::script_scope_redactions(&runtime_scopes);
    pre_redactions.extend(super::transport::script_request_redactions(
        &script_request(&original),
    ));
    pre_redactions.extend(diagnostic_redactions);
    if let Ok((prepared, _)) = compile_request_with_folder_chain(
        &original,
        seed_collection.as_ref(),
        &folders,
        &seed_context,
    ) {
        pre_redactions.extend(prepared.redactions);
    }
    for variables in [&original.variables[..], &row[..]] {
        pre_redactions.extend(script_secret_values(variables, &secrets));
    }
    let result = runner_transport
        .run_script(
            &script,
            runtime_scopes.clone(),
            script_request(&original),
            None,
            Some(&script_request_id),
            Some(&pre_phase),
        )
        .map_err(|error| {
            format!(
                "Runner pre-request script failed: {}",
                super::transport::diagnostic_text(&error, &pre_redactions)
            )
        })?;
    if pre_script_ran {
        operation_guard.arm();
    }
    let mut next_scopes = result.scopes();
    pre_redactions.extend(super::transport::script_scope_redactions(&next_scopes));
    let pre_tests = result.tests.clone();
    let pre_console = result.console.clone();
    next_scopes.vault = runtime_scopes.vault;
    next_scopes.iteration_data = runtime_scopes.iteration_data;
    next_scopes.allow_private_network = runtime_scopes.allow_private_network;
    let mut scripted_request = original;
    apply_pre_request_result(&mut scripted_request, result)?;
    let scripted_environment = variables_from_map(&next_scopes.environment);
    let scripted_local = variables_from_map(&next_scopes.local);
    let scripted_data = variables_from_map(&next_scopes.iteration_data);
    let mut scripted_collection = collection.clone();
    if let Some(collection) = scripted_collection.as_mut() {
        collection.variables = variables_from_map(&next_scopes.collection);
    }
    let scripted_globals = variables_from_map(&next_scopes.globals);
    let context = CompileContext {
        global: &scripted_globals,
        environment: &scripted_environment,
        data: &scripted_data,
        local: &scripted_local,
        secrets: &secrets,
        environment_base_url: Some(&environment_base_url),
        environment_auth: Some(&environment_auth),
    };
    let tests = ordered_script(
        scripted_collection.as_ref(),
        &folders,
        &scripted_request,
        true,
    );
    let (mut request, _) = compile_request_with_folder_chain(
        &scripted_request,
        scripted_collection.as_ref(),
        &folders,
        &context,
    )
    .map_err(|error| format!("Runner compile failed: {error}"))?;
    request
        .redactions
        .extend(next_scopes.vault.values().cloned());
    request.redactions.extend(pre_redactions);
    request
        .redactions
        .extend(script_secret_values(&scripted_request.variables, &secrets));
    request
        .redactions
        .extend(script_secret_values(&row, &secrets));
    let cookie_mutation_url = url::Url::parse(&scripted_request.url)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https"))
        .map(|url| url.to_string())
        .unwrap_or_else(|| request.url.clone());
    apply_script_cookie_mutations(
        &mut cookie_jar,
        secret_store.as_ref(),
        &cookie_mutation_url,
        &initial_script_cookies,
        &next_scopes.cookies,
    )?;
    store
        .save_cookie_jar(&cookie_jar)
        .map_err(|error| error.to_string())?;
    if let Some(cookie) = cookie_jar
        .header_for_url(secret_store.as_ref(), &request.url, now_seconds())
        .map_err(|error| error.to_string())?
    {
        let value = cookie.expose_secret().to_string();
        request
            .headers
            .retain(|(name, _)| !name.eq_ignore_ascii_case("cookie"));
        request.headers.push(("Cookie".into(), value.clone()));
        request.redactions.push(value);
    }
    let (pre_tests, pre_console) = super::transport::script_diagnostics(
        &pre_tests,
        &pre_console,
        "Pre-request",
        &request.redactions,
    );
    let prepared = (
        request,
        tests,
        next_scopes,
        operation_id,
        pre_script_ran,
        cookie_jar,
        pre_tests,
        pre_console,
    );
    operation_guard.disarm();
    Ok(prepared)
}

/// The next item a [`RunSession`] will execute: its operation identity
/// (what a cancel targets) and the delay that applies before it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NextRunItem {
    pub operation_id: String,
    /// The run's `delay_ms`, or 0 before the first item.
    pub delay_ms: u64,
}

/// A prepared run executed one item at a time, so a caller can wait between
/// items on its own clock and stop between them: [`run_collection`] drives
/// it with a blocking sleep, the desktop runner with its UI timer.
pub struct RunSession {
    prepared: RunPreparation,
    cookie_jar: CookieJar,
    base_scopes: ScriptScopes,
    runtime_scopes: ScriptScopes,
    results: Vec<RunItemResult>,
    redactions: Vec<String>,
    /// `(iteration, request index)` of the next item.
    cursor: (usize, usize),
    stopped: bool,
}

impl RunSession {
    /// Persist the `Running` row and start stepping; the run comes back as
    /// an outcome carrying the storage error when that first write fails.
    #[allow(clippy::result_large_err)]
    pub fn start(prepared: RunPreparation) -> Result<Self, RunOutcome> {
        if let Err(error) = prepared.store.upsert_run(&prepared.run, &[]) {
            return Err(RunOutcome {
                cookie_jar: prepared.cookie_jar,
                run: prepared.run,
                redactions: Vec::new(),
                error: Some(error.to_string()),
            });
        }
        let base_scopes = ScriptScopes {
            globals: resolved_script_variables(&prepared.globals, &prepared.secrets),
            environment_name: prepared.environment_name.clone(),
            environment: resolved_script_variables(&prepared.environment, &prepared.secrets),
            collection: prepared
                .collection
                .as_ref()
                .map(|collection| {
                    resolved_script_variables(&collection.variables, &prepared.secrets)
                })
                .unwrap_or_default(),
            ..Default::default()
        };
        let mut redactions = script_secret_values(&prepared.environment, &prepared.secrets);
        redactions.extend(script_secret_values(&prepared.globals, &prepared.secrets));
        if let Some(collection) = &prepared.collection {
            redactions.extend(script_secret_values(
                &collection.variables,
                &prepared.secrets,
            ));
        }
        Ok(Self {
            cookie_jar: prepared.cookie_jar.clone(),
            runtime_scopes: base_scopes.clone(),
            base_scopes,
            results: Vec::with_capacity(
                prepared.rows.len().saturating_mul(prepared.requests.len()),
            ),
            redactions,
            cursor: (0, 0),
            stopped: false,
            prepared,
        })
    }

    /// The `Running` row as persisted at start.
    pub fn run(&self) -> &CollectionRun {
        &self.prepared.run
    }

    /// The redactions the run's storage is masked with so far, for a caller
    /// that persists the run itself when it stops early.
    pub fn redactions(&self) -> &[String] {
        &self.redactions
    }

    /// How many items the run covers.
    pub fn item_count(&self) -> usize {
        self.prepared
            .rows
            .len()
            .saturating_mul(self.prepared.requests.len())
    }

    /// The item [`run_item`](Self::run_item) executes next, or `None` once
    /// the run is exhausted or stopped on an error.
    pub fn next_item(&self) -> Option<NextRunItem> {
        if self.stopped
            || self.prepared.requests.is_empty()
            || self.cursor.0 >= self.prepared.rows.len()
        {
            return None;
        }
        Some(NextRunItem {
            operation_id: RequestId::new().as_str().to_string(),
            delay_ms: if self.results.is_empty() {
                0
            } else {
                self.prepared.delay_ms
            },
        })
    }

    fn row(&self, iteration: usize) -> &Vec<Variable> {
        &self.prepared.rows[iteration]
    }

    fn push_result(&mut self, result: RunItemResult) -> &RunItemResult {
        let mut result = result;
        result.error = result
            .error
            .as_ref()
            .map(|error| super::transport::diagnostic_text(error, &self.redactions));
        if self.prepared.run.stop_on_error && result.error.is_some() {
            self.stopped = true;
        }
        let index = self.results.len();
        self.results.push(result);
        &self.results[index]
    }

    /// Execute the next item under `operation_id` (from
    /// [`next_item`](Self::next_item)) and return its result row.
    pub fn run_item(
        &mut self,
        operation_id: String,
        transport: &Arc<dyn WorkbenchTransport>,
    ) -> &RunItemResult {
        let (iteration, index) = self.cursor;
        if self
            .results
            .last()
            .is_none_or(|item| item.iteration as usize != iteration)
        {
            if iteration > 0 && !self.prepared.keep_variables {
                let local = self.runtime_scopes.local.clone();
                self.runtime_scopes = self.base_scopes.clone();
                self.runtime_scopes.local = local;
            }
            self.runtime_scopes.iteration_data = script_variables(&[self.row(iteration)]);
        }
        self.cursor = if index + 1 < self.prepared.requests.len() {
            (iteration, index + 1)
        } else {
            (iteration + 1, 0)
        };
        let store = self.prepared.store.clone();
        let runner_secret_store = self.prepared.secret_store.clone();
        let file_capabilities = self.prepared.file_capabilities.clone();
        let runner_transport = transport.clone();
        let original = self.prepared.requests[index].clone();
        self.redactions.extend(script_secret_values(
            &original.variables,
            &self.prepared.secrets,
        ));
        let globals_before = self.runtime_scopes.globals.clone();
        let inherited_local = self.runtime_scopes.local.clone();
        let mut request_scopes = request_script_scopes(&self.runtime_scopes, &original);
        request_scopes.local.extend(resolved_script_variables(
            &original.variables,
            &self.prepared.secrets,
        ));
        let initial_local = request_scopes.local.clone();
        self.redactions
            .extend(super::transport::script_scope_redactions(&request_scopes));
        let original_id = original.id.clone();
        let iteration_row = self.row(iteration).clone();
        self.redactions
            .extend(script_secret_values(&iteration_row, &self.prepared.secrets));
        let folders = folder_chain(&self.prepared.folders, &original);
        let scripts = [false, true].map(|tests| {
            ordered_script(
                self.prepared.collection.as_ref(),
                &folders,
                &original,
                tests,
            )
        });
        self.redactions.extend(
            request_script_vault(
                &original,
                [scripts[0].as_str(), scripts[1].as_str()],
                &self.prepared.secrets,
            )
            .into_values(),
        );
        let prepared_item = prepare_run_request(
            original,
            self.prepared.collection.clone(),
            self.prepared.folders.clone(),
            (
                self.prepared.environment_base_url.clone(),
                self.prepared.environment_auth.clone(),
            ),
            iteration_row,
            request_scopes,
            self.prepared.secrets.clone(),
            self.cookie_jar.clone(),
            store.clone(),
            runner_secret_store.clone(),
            operation_id,
            runner_transport.clone(),
            self.redactions.clone(),
        );
        let (
            request,
            tests,
            request_scopes,
            operation_id,
            pre_script_ran,
            next_cookie_jar,
            pre_tests,
            mut console,
        ) = match prepared_item {
            Ok(item) => item,
            Err(error) => {
                let console = vec![super::transport::script_error_console(
                    "Pre-request",
                    &error,
                    &self.redactions,
                )];
                return self.push_result(RunItemResult {
                    request_id: original_id,
                    iteration: iteration as u32,
                    status: None,
                    duration_ms: 0,
                    error: Some(error),
                    response: None,
                    console,
                    test_results: Vec::new(),
                });
            }
        };
        self.cookie_jar = next_cookie_jar;
        carry_shared_script_scopes(&mut self.runtime_scopes, &request_scopes, &initial_local);
        self.redactions.extend(request.redactions.iter().cloned());
        let request_id = request.request_id.clone();
        let started = Instant::now();
        let script_request = ScriptRequestView {
            method: request.method.as_str().to_string(),
            url: request.url.clone(),
            headers: request.headers.clone(),
            body: String::new(),
        };
        let sent_url = request.url.clone();
        let send_phase = OperationPhase {
            operation_id: operation_id.clone(),
            starts_operation: !pre_script_ran,
            final_phase: tests.trim().is_empty(),
        };
        let mut operation_guard =
            OperationAbortGuard::new(operation_id.clone(), runner_transport.clone());
        if pre_script_ran {
            operation_guard.arm();
        }
        let result = runner_transport.send(&request, &file_capabilities, &send_phase);
        let (status, mut response, mut test_results, mut error, next_variables) = match result {
            Ok(mut response) => {
                if tests.trim().is_empty() {
                    operation_guard.disarm();
                } else {
                    operation_guard.arm();
                }
                let response_url = if response.final_url.is_empty() {
                    sent_url
                } else {
                    response.final_url.clone()
                };
                for mutation in &response.cookie_mutations {
                    if let Err(error) = self.cookie_jar.set_cookie(
                        runner_secret_store.as_ref(),
                        &mutation.source_url,
                        &mutation.header,
                        now_seconds(),
                    ) {
                        self.redactions.push(error.to_string());
                    }
                }
                for set_cookie in &response.set_cookies {
                    if let Err(error) = self.cookie_jar.set_cookie(
                        runner_secret_store.as_ref(),
                        &response_url,
                        set_cookie,
                        now_seconds(),
                    ) {
                        self.redactions.push(error.to_string());
                    }
                }
                let script_response = ScriptResponseView {
                    code: response.status,
                    status: response.reason.clone(),
                    headers: response.headers.clone(),
                    body: response.body.clone(),
                    response_time_ms: response.duration_ms,
                };
                let (post_cookie_before, post_cookie_error) = match cookie_scope_for_url(
                    &self.cookie_jar,
                    runner_secret_store.as_ref(),
                    &response_url,
                ) {
                    Ok(cookies) => (cookies, None),
                    Err(error) => (BTreeMap::new(), Some(error)),
                };
                let mut post_scopes_input = request_scopes.clone();
                post_scopes_input.cookies = post_cookie_before.clone();
                self.redactions
                    .extend(super::transport::script_scope_redactions(
                        &post_scopes_input,
                    ));
                let mut post_scopes_fallback = request_scopes.clone();
                post_scopes_fallback.cookies = post_cookie_before.clone();
                let script_request_id = request_id.as_str().to_string();
                let post_phase = OperationPhase {
                    operation_id,
                    starts_operation: false,
                    final_phase: true,
                };
                let tests = runner_transport.run_script(
                    &tests,
                    post_scopes_input,
                    script_request,
                    Some(script_response),
                    Some(&script_request_id),
                    Some(&post_phase),
                );
                operation_guard.disarm();
                let (mut test_results, mut test_error, post_scopes) = match tests {
                    Ok(result) => {
                        let mut post_scopes = result.scopes();
                        self.redactions
                            .extend(super::transport::script_scope_redactions(&post_scopes));
                        let (results, post_console) = super::transport::script_diagnostics(
                            &result.tests,
                            &result.console,
                            "Post-response",
                            &self.redactions,
                        );
                        console.extend(post_console);
                        post_scopes.iteration_data = post_scopes_fallback.iteration_data.clone();
                        let error = results
                            .iter()
                            .find(|result| !result.passed && !result.skipped)
                            .map(|result| {
                                result
                                    .error
                                    .clone()
                                    .unwrap_or_else(|| format!("test {:?} failed", result.name))
                            });
                        (results, error, post_scopes)
                    }
                    Err(error) => {
                        console.push(super::transport::script_error_console(
                            "Post-response",
                            &error,
                            &self.redactions,
                        ));
                        (
                            vec![TestResult {
                                name: "[Post-response] script runtime".into(),
                                passed: false,
                                skipped: false,
                                error: Some(error.clone()),
                            }],
                            Some(error),
                            post_scopes_fallback,
                        )
                    }
                };
                if let Some(error) = post_cookie_error {
                    test_results.push(TestResult {
                        name: "cookie persistence".into(),
                        passed: false,
                        skipped: false,
                        error: Some(error.clone()),
                    });
                    test_error.get_or_insert(error);
                }
                if let Err(error) = apply_script_cookie_mutations(
                    &mut self.cookie_jar,
                    runner_secret_store.as_ref(),
                    &response_url,
                    &post_cookie_before,
                    &post_scopes.cookies,
                ) {
                    test_results.push(TestResult {
                        name: "post-response cookie mutation".into(),
                        passed: false,
                        skipped: false,
                        error: Some(error.clone()),
                    });
                    test_error.get_or_insert(error);
                }
                if let Err(error) = store.save_cookie_jar(&self.cookie_jar) {
                    let error = error.to_string();
                    test_results.push(TestResult {
                        name: "cookie persistence".into(),
                        passed: false,
                        skipped: false,
                        error: Some(error.clone()),
                    });
                    test_error.get_or_insert(error);
                }
                response.console = console.clone();
                (
                    Some(response.status),
                    Some(response_snapshot(&response)),
                    test_results,
                    test_error,
                    post_scopes,
                )
            }
            Err(error) => {
                operation_guard.disarm();
                (None, None, Vec::new(), Some(error), request_scopes.clone())
            }
        };
        test_results.splice(0..0, pre_tests);
        if error.is_none() {
            error = test_results
                .iter()
                .find(|test| !test.passed && !test.skipped)
                .map(|test| {
                    test.error
                        .clone()
                        .unwrap_or_else(|| format!("test {:?} failed", test.name))
                });
        }
        let redact = super::transport::diagnostic_redactor(&self.redactions);
        for test in &mut test_results {
            test.name = redact(&test.name);
            test.error = test.error.as_ref().map(|error| redact(error));
        }
        error = error.as_ref().map(|error| redact(error));
        for entry in &mut console {
            entry.message = redact(&entry.message);
        }
        if let Some(response) = response.as_mut() {
            response.test_results = test_results.clone();
            response.console = console.clone();
        }
        if let Err(persistence_error) = super::send::persist_globals(
            &self.prepared.store,
            self.prepared.secret_store.as_ref(),
            &self.prepared.run.workspace_id,
            &globals_before,
            &next_variables.globals,
            &mut self.redactions,
        ) {
            error.get_or_insert(persistence_error);
        }
        self.runtime_scopes.local = inherited_local;
        carry_shared_script_scopes(&mut self.runtime_scopes, &next_variables, &initial_local);
        match &next_variables.next_request {
            Some(super::transport::ScriptNextRequest::Stop) => self.stopped = true,
            Some(super::transport::ScriptNextRequest::Request(target)) => {
                let by_id = self
                    .prepared
                    .requests
                    .iter()
                    .position(|request| request.id.as_str() == target);
                let by_name: Vec<_> = self
                    .prepared
                    .requests
                    .iter()
                    .enumerate()
                    .filter(|(_, request)| request.name == *target)
                    .map(|(index, _)| index)
                    .collect();
                if let Some(index) = by_id.or_else(|| (by_name.len() == 1).then(|| by_name[0])) {
                    self.cursor = (iteration, index);
                } else {
                    self.stopped = true;
                    error.get_or_insert_with(|| format!("Next request {target:?} is missing, ambiguous, or outside this run's selection"));
                }
            }
            None => {}
        }
        if self.results.len() + 1 >= MAX_RUN_STEPS
            && !self.stopped
            && self.cursor.0 < self.prepared.rows.len()
        {
            self.stopped = true;
            error.get_or_insert_with(|| {
                format!("Run stopped after {MAX_RUN_STEPS} requests; check setNextRequest loops")
            });
        }
        self.push_result(RunItemResult {
            console,
            request_id,
            iteration: iteration as u32,
            status,
            duration_ms: started.elapsed().as_millis() as u64,
            error,
            response,
            test_results,
        })
    }

    /// Persist the run (`Canceled` when `canceled`, else `Completed` or
    /// `Failed`) and the cookie jar, the way the desktop runner does.
    pub fn finish(mut self, canceled: bool) -> RunOutcome {
        let failures = self
            .results
            .iter()
            .filter(|result| result.error.is_some())
            .count();
        self.prepared.run.item_results = self.results;
        self.prepared.run.status = if canceled {
            RunStatus::Canceled
        } else if failures == 0 {
            RunStatus::Completed
        } else {
            RunStatus::Failed
        };
        self.prepared.run.completed_at = Some(now_millis());
        let error = self
            .prepared
            .store
            .upsert_run(&self.prepared.run, &self.redactions)
            .and_then(|()| self.prepared.store.save_cookie_jar(&self.cookie_jar))
            .map_err(|error| error.to_string())
            .err();
        RunOutcome {
            run: self.prepared.run,
            redactions: self.redactions,
            cookie_jar: self.cookie_jar,
            error,
        }
    }
}

/// Execute a prepared run to completion, `Failed`, or `Canceled`, and persist
/// it (with the cookie jar) the way the desktop runner does.
pub fn run_collection(
    prepared: RunPreparation,
    transport: Arc<dyn WorkbenchTransport>,
    control: &RunControl,
) -> RunOutcome {
    let mut session = match RunSession::start(prepared) {
        Ok(session) => session,
        Err(outcome) => return outcome,
    };
    let mut canceled = false;
    while let Some(item) = session.next_item() {
        if control.canceled() {
            canceled = true;
            break;
        }
        if item.delay_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(item.delay_ms));
            if control.canceled() {
                canceled = true;
                break;
            }
        }
        let result = session.run_item(item.operation_id, &transport);
        if let Some(on_item) = &control.on_item {
            on_item(result);
        }
    }
    session.finish(canceled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::transport::{
        OAuthTokenRequest, OAuthTokenResponse, Response, ScriptRequestView, ScriptResponseView,
        ScriptResult, ScriptTestResult,
    };
    use crate::runtime::workspace::WorkspaceData;
    use crate::{
        Body, HttpMethod, MemorySecretStore, RequestSettings, Scripts, SecretRef, SecretValue,
        redact_collection_run,
    };
    use base64::Engine as _;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    /// Headless stand-in for the service: records every operation phase,
    /// answers `200 {"ok":true}` (echoing the Authorization header so the
    /// redaction tests have a secret to find), and returns one passing typed
    /// assertion from every non-empty post-response script.
    #[derive(Default)]
    struct FakeTransport {
        phases: Mutex<Vec<(String, String, bool, bool)>>,
        script_inputs: Mutex<Vec<ScriptScopes>>,
        sent_urls: Mutex<Vec<String>>,
        failing_url_fragment: Option<String>,
        post_calls: AtomicUsize,
        token_exchanges: AtomicUsize,
    }

    impl FakeTransport {
        fn failing(fragment: &str) -> Self {
            Self {
                failing_url_fragment: Some(fragment.into()),
                ..Default::default()
            }
        }

        fn phases(&self) -> Vec<(String, String, bool, bool)> {
            self.phases.lock().unwrap().clone()
        }
    }

    impl WorkbenchTransport for FakeTransport {
        fn send(
            &self,
            request: &PreparedRequest,
            _files: &FileCapabilities,
            phase: &OperationPhase,
        ) -> Result<Response, String> {
            self.phases.lock().unwrap().push((
                "send".into(),
                phase.operation_id.clone(),
                phase.starts_operation,
                phase.final_phase,
            ));
            self.sent_urls.lock().unwrap().push(request.url.clone());
            if let Some(fragment) = &self.failing_url_fragment
                && request.url.contains(fragment.as_str())
            {
                return Err(format!("connection refused for {}", request.url));
            }
            let authorization = request
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                .map(|(_, value)| value.clone())
                .unwrap_or_default();
            let body =
                serde_json::json!({ "ok": true, "authorization": authorization }).to_string();
            let body_base64 = base64::engine::general_purpose::STANDARD.encode(body.as_bytes());
            Ok(Response {
                console: Vec::new(),
                status: 200,
                reason: "OK".into(),
                headers: vec![("Content-Type".into(), "application/json".into())],
                set_cookies: Vec::new(),
                cookie_mutations: Vec::new(),
                received_bytes: body.len() as u64,
                stored_bytes: body.len() as u64,
                body,
                body_base64,
                binary: false,
                final_url: request.url.clone(),
                http_version: "HTTP/2".into(),
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
            scopes: ScriptScopes,
            request: ScriptRequestView,
            response: Option<ScriptResponseView>,
            _request_id: Option<&str>,
            phase: Option<&OperationPhase>,
        ) -> Result<ScriptResult, String> {
            let mut scopes = scopes;
            if let Some(target) = script.strip_prefix("next:") {
                scopes.next_request = Some(if target == "stop" {
                    super::super::transport::ScriptNextRequest::Stop
                } else {
                    super::super::transport::ScriptNextRequest::Request(target.into())
                });
                scopes.local.insert("runValue".into(), "retained".into());
                scopes
                    .globals
                    .insert("shared".into(), "global-value".into());
            }
            let phase = phase.expect("runner scripts carry an operation phase");
            self.script_inputs.lock().unwrap().push(scopes.clone());
            let asserts = response.is_some() && !script.trim().is_empty();
            let diagnostic_message = (script == "diagnostics").then(|| {
                scopes
                    .resolved()
                    .into_values()
                    .collect::<Vec<_>>()
                    .join(" | ")
            });
            let diagnostic_pre = diagnostic_message.is_some() && response.is_none();
            self.phases.lock().unwrap().push((
                if response.is_some() { "post" } else { "pre" }.into(),
                phase.operation_id.clone(),
                phase.starts_operation,
                phase.final_phase,
            ));
            if response.is_some() {
                self.post_calls.fetch_add(1, Ordering::SeqCst);
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
            self.phases
                .lock()
                .unwrap()
                .push(("cancel".into(), operation_id.into(), false, true));
            Ok(true)
        }

        fn exchange_oauth_token(
            &self,
            request: OAuthTokenRequest,
        ) -> Result<OAuthTokenResponse, String> {
            self.token_exchanges.fetch_add(1, Ordering::SeqCst);
            assert_eq!(request.flow, "client_credentials");
            assert_eq!(request.client_secret.as_deref(), Some("env-client-secret"));
            Ok(OAuthTokenResponse {
                access_token: "fresh-env-token".into(),
                expires_in: Some(600),
                refresh_token: None,
            })
        }
    }

    struct Fixture {
        workspace: WorkspaceId,
        collection_id: CollectionId,
        store: Arc<WorkbenchStore>,
        secret_store: Arc<dyn SecretStore>,
    }

    fn fixture(name: &str) -> Fixture {
        let workspace = WorkspaceId::new(format!("/test/runtime-collection/{name}")).unwrap();
        let path = std::env::temp_dir().join(format!(
            "agentops-core-runtime-collection-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        let mut data = WorkspaceData::open(&path, workspace.clone()).unwrap();
        let collection_id = data.ensure_collection().unwrap();
        Fixture {
            workspace,
            collection_id,
            store: data.store.clone(),
            secret_store: Arc::new(MemorySecretStore::new()),
        }
    }

    fn saved_request(fixture: &Fixture, name: &str, url: &str, tests: &str) -> SavedRequest {
        let request = SavedRequest {
            id: RequestId::new(),
            collection_id: fixture.collection_id.clone(),
            folder_id: None,
            name: name.into(),
            method: HttpMethod::get(),
            url: url.into(),
            params: Vec::new(),
            headers: Vec::new(),
            auth: AuthConfig::None,
            body: Body::None,
            variables: Vec::new(),
            scripts: Scripts {
                pre_request: String::new(),
                tests: tests.into(),
            },
            settings: RequestSettings::default(),
            extensions: Default::default(),
            sort_key: 0,
        };
        fixture.store.upsert_request(&request).unwrap();
        request
    }

    fn run_input(
        fixture: &Fixture,
        requests: Vec<SavedRequest>,
        data_source: &str,
        stop_on_error: bool,
    ) -> RunPreparationInput {
        RunPreparationInput {
            workspace_id: fixture.workspace.clone(),
            collection_id: fixture.collection_id.clone(),
            environment_id: None,
            environment: None,
            stop_on_error,
            selected_folder_id: None,
            selected_request_ids: requests.iter().map(|request| request.id.clone()).collect(),
            collection: None,
            folders: Vec::new(),
            requests,
            environment_source: String::new(),
            environment_scope: "environment".into(),
            environment_base_url: String::new(),
            environment_auth: AuthConfig::None,
            data_source: data_source.into(),
            secrets: DraftSecrets::with_store(
                fixture.secret_store.clone(),
                fixture.workspace.clone(),
            ),
            iteration_limit: None,
            delay_ms: 0,
            keep_variables: false,
            store: fixture.store.clone(),
            secret_store: fixture.secret_store.clone(),
            cookie_jar: CookieJar::new(fixture.workspace.clone()),
            file_capabilities: FileCapabilities::default(),
        }
    }

    const TWO_ROWS: &str = r#"[{"user":"alice"},{"user":"bob"}]"#;

    #[test]
    fn script_branch_stop_and_globals_persist_without_sending_skipped_request() {
        let fixture = fixture("branching");
        let first = saved_request(
            &fixture,
            "First",
            "https://example.test/first",
            "next:Third",
        );
        let second = saved_request(&fixture, "Second", "https://example.test/second", "");
        let third = saved_request(
            &fixture,
            "Third",
            "https://example.test/{{shared}}",
            "next:stop",
        );
        let transport = Arc::new(FakeTransport::default());
        let prepared = prepare_collection_run(
            run_input(
                &fixture,
                vec![first.clone(), second, third.clone()],
                TWO_ROWS,
                false,
            ),
            transport.as_ref(),
        )
        .unwrap();
        let outcome = run_collection(prepared, transport.clone(), &RunControl::default());
        assert_eq!(outcome.run.status, RunStatus::Completed);
        assert_eq!(outcome.run.item_results.len(), 2);
        assert_eq!(outcome.run.item_results[1].request_id, third.id);
        assert_eq!(
            transport.sent_urls.lock().unwrap()[1],
            "https://example.test/global-value"
        );
        assert!(
            transport
                .script_inputs
                .lock()
                .unwrap()
                .iter()
                .any(|scopes| scopes
                    .local
                    .get("runValue")
                    .is_some_and(|v| v == "retained"))
        );
        assert_eq!(
            fixture.store.global_variables(&fixture.workspace).unwrap()[0].key,
            "shared"
        );
    }

    #[test]
    fn script_loop_is_bounded_and_bad_targets_fail() {
        for target in ["First", "Missing"] {
            let fixture = fixture("loop-limit");
            let first = saved_request(
                &fixture,
                "First",
                "https://example.test/first",
                &format!("next:{target}"),
            );
            let transport: Arc<dyn WorkbenchTransport> = Arc::new(FakeTransport::default());
            let prepared = prepare_collection_run(
                run_input(&fixture, vec![first], TWO_ROWS, false),
                transport.as_ref(),
            )
            .unwrap();
            let mut session = RunSession::start(prepared).unwrap();
            // Exercise the production bound without making ten thousand fake exchanges.
            if target == "First" {
                let operation = session.next_item().unwrap();
                let row = session.run_item(operation.operation_id, &transport).clone();
                session.results.resize(MAX_RUN_STEPS - 1, row);
            }
            let operation = session.next_item().unwrap();
            let row = session.run_item(operation.operation_id, &transport);
            assert!(row.error.is_some());
            assert!(session.next_item().is_none());
            assert_eq!(session.finish(false).run.status, RunStatus::Failed);
        }
    }

    #[test]
    fn collection_scripts_resolve_secret_scopes_and_redact_their_values() {
        let fixture = fixture("script-secrets");
        let mut request = saved_request(
            &fixture,
            "Secrets",
            "https://example.test/items",
            "{{vault.shared}}",
        );
        request.scripts.pre_request = "{{vault.shared}}".into();
        let (variables, local_secrets) =
            parse_session_variables("secret:localValue=local-secret", "request").unwrap();
        request.variables = variables;
        request.headers.push(crate::KeyValueRow::enabled(
            "Authorization",
            "{{localValue}}",
        ));
        let mut input = run_input(
            &fixture,
            vec![request],
            r#"[{"rowValue":"{{vault.shared}}"}]"#,
            false,
        );
        input.environment_source =
            "secret:envValue=environment-secret\nnamed={{vault.shared}}".into();
        let mut collection = fixture
            .store
            .list_collections(&fixture.workspace)
            .unwrap()
            .remove(0);
        let (variables, collection_secrets) =
            parse_session_variables("secret:collectionValue=collection-secret", "collection")
                .unwrap();
        collection.variables = variables;
        input.collection = Some(collection);
        input.secrets.merge(local_secrets);
        input.secrets.merge(collection_secrets);
        input.secrets.insert(
            &crate::vault::vault_secret_reference("shared").unwrap(),
            "named-secret",
        );
        let transport = Arc::new(FakeTransport::default());
        let prepared = prepare_collection_run(input, transport.as_ref()).unwrap();
        let outcome = run_collection(prepared, transport.clone(), &RunControl::default());
        assert_eq!(outcome.run.status, RunStatus::Completed);
        let scripts = transport.script_inputs.lock().unwrap();
        assert_eq!(scripts.len(), 2);
        for scopes in scripts.iter() {
            assert_eq!(scopes.environment["envValue"], "environment-secret");
            assert_eq!(scopes.environment["named"], "named-secret");
            assert_eq!(scopes.collection["collectionValue"], "collection-secret");
            assert_eq!(scopes.local["localValue"], "local-secret");
            assert_eq!(scopes.iteration_data["rowValue"], "named-secret");
            assert_eq!(scopes.vault["shared"], "named-secret");
        }
        for secret in [
            "environment-secret",
            "collection-secret",
            "local-secret",
            "named-secret",
        ] {
            assert!(outcome.redactions.contains(&secret.to_string()), "{secret}");
        }
        let redacted = redact_collection_run(&outcome.run, &outcome.redactions);
        let encoded = serde_json::to_string(&redacted).unwrap();
        assert!(!encoded.contains("local-secret"));
    }

    #[test]
    fn two_requests_over_two_rows_run_in_order_with_iteration_numbers() {
        let fixture = fixture("ordering");
        let first = saved_request(&fixture, "First", "https://example.test/{{user}}/first", "");
        let second = saved_request(
            &fixture,
            "Second",
            "https://example.test/{{user}}/second",
            "",
        );
        let transport = Arc::new(FakeTransport::default());
        let prepared = prepare_collection_run(
            run_input(
                &fixture,
                vec![first.clone(), second.clone()],
                TWO_ROWS,
                false,
            ),
            transport.as_ref(),
        )
        .unwrap();
        assert_eq!(prepared.run.iteration_count, 2);

        let outcome = run_collection(prepared, transport.clone(), &RunControl::default());

        assert_eq!(outcome.error, None);
        assert_eq!(outcome.run.status, RunStatus::Completed);
        assert!(outcome.run.completed_at.is_some());
        let observed = outcome
            .run
            .item_results
            .iter()
            .map(|item| (item.request_id.clone(), item.iteration, item.status))
            .collect::<Vec<_>>();
        assert_eq!(
            observed,
            vec![
                (first.id.clone(), 0, Some(200)),
                (second.id.clone(), 0, Some(200)),
                (first.id.clone(), 1, Some(200)),
                (second.id.clone(), 1, Some(200)),
            ]
        );
        assert!(
            outcome
                .run
                .item_results
                .iter()
                .all(|item| item.error.is_none())
        );
        assert_eq!(
            transport.sent_urls.lock().unwrap().clone(),
            vec![
                "https://example.test/alice/first",
                "https://example.test/alice/second",
                "https://example.test/bob/first",
                "https://example.test/bob/second",
            ]
        );
        // Like the desktop runner, every item runs pre script, send and post
        // script under one operation id; with no scripts the send phase both
        // opens and closes the operation.
        let phases = transport.phases();
        assert_eq!(phases.len(), 12);
        for item in phases.chunks(3) {
            let operation_id = &item[0].1;
            assert_eq!(item[0].0, "pre");
            assert_eq!(
                item[1],
                ("send".to_string(), operation_id.clone(), true, true)
            );
            assert_eq!(
                item[2],
                ("post".to_string(), operation_id.clone(), false, true)
            );
        }
    }

    #[test]
    fn stop_on_error_halts_after_the_first_failing_item() {
        let fixture = fixture("stop-on-error");
        let healthy = saved_request(&fixture, "Healthy", "https://example.test/ok", "");
        let broken = saved_request(&fixture, "Broken", "https://example.test/fail", "");
        let after = saved_request(&fixture, "After", "https://example.test/after", "");
        let transport = Arc::new(FakeTransport::failing("/fail"));
        let prepared = prepare_collection_run(
            run_input(
                &fixture,
                vec![healthy.clone(), broken.clone(), after],
                TWO_ROWS,
                true,
            ),
            transport.as_ref(),
        )
        .unwrap();

        let outcome = run_collection(prepared, transport.clone(), &RunControl::default());

        assert_eq!(outcome.error, None);
        assert_eq!(outcome.run.status, RunStatus::Failed);
        assert_eq!(outcome.run.item_results.len(), 2);
        assert_eq!(outcome.run.item_results[0].request_id, healthy.id);
        assert_eq!(outcome.run.item_results[0].error, None);
        let failed = &outcome.run.item_results[1];
        assert_eq!(failed.request_id, broken.id);
        assert_eq!(failed.iteration, 0);
        assert_eq!(failed.status, None);
        assert!(failed.response.is_none());
        assert_eq!(
            failed.error.as_deref(),
            Some("connection refused for https://example.test/fail")
        );
        assert_eq!(transport.sent_urls.lock().unwrap().len(), 2);
    }

    #[test]
    fn without_stop_on_error_a_failure_is_recorded_and_the_run_continues() {
        let fixture = fixture("continue-on-error");
        let broken = saved_request(&fixture, "Broken", "https://example.test/fail", "");
        let after = saved_request(&fixture, "After", "https://example.test/after", "");
        let transport = Arc::new(FakeTransport::failing("/fail"));
        let prepared = prepare_collection_run(
            run_input(&fixture, vec![broken, after], "", false),
            transport.as_ref(),
        )
        .unwrap();

        let outcome = run_collection(prepared, transport, &RunControl::default());

        assert_eq!(outcome.run.status, RunStatus::Failed);
        assert_eq!(outcome.run.item_results.len(), 2);
        assert!(outcome.run.item_results[0].error.is_some());
        assert_eq!(outcome.run.item_results[1].status, Some(200));
    }

    #[test]
    fn a_cancel_raised_from_the_item_callback_ends_the_run_as_canceled() {
        let fixture = fixture("cancel");
        let first = saved_request(&fixture, "First", "https://example.test/first", "");
        let second = saved_request(&fixture, "Second", "https://example.test/second", "");
        let transport = Arc::new(FakeTransport::default());
        let prepared = prepare_collection_run(
            run_input(&fixture, vec![first.clone(), second], TWO_ROWS, false),
            transport.as_ref(),
        )
        .unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(AtomicUsize::new(0));
        let control = RunControl {
            cancel: cancel.clone(),
            on_item: Some(Box::new({
                let cancel = cancel.clone();
                let seen = seen.clone();
                move |item: &RunItemResult| {
                    seen.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(item.status, Some(200));
                    cancel.store(true, Ordering::SeqCst);
                }
            })),
        };

        let outcome = run_collection(prepared, transport.clone(), &control);

        assert_eq!(outcome.error, None);
        assert_eq!(outcome.run.status, RunStatus::Canceled);
        assert!(outcome.run.completed_at.is_some());
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        assert_eq!(outcome.run.item_results.len(), 1);
        assert_eq!(outcome.run.item_results[0].request_id, first.id);
        assert_eq!(transport.sent_urls.lock().unwrap().len(), 1);
        let persisted = fixture.store.runs(&fixture.workspace, 10).unwrap();
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].status, RunStatus::Canceled);
    }

    #[test]
    fn the_finished_run_is_persisted_and_reads_back_from_the_store() {
        let fixture = fixture("persisted");
        let first = saved_request(&fixture, "First", "https://example.test/first", "");
        let second = saved_request(&fixture, "Second", "https://example.test/second", "");
        let transport = Arc::new(FakeTransport::default());
        let mut input = run_input(&fixture, vec![first, second], TWO_ROWS, false);
        input.iteration_limit = Some(3);
        input.keep_variables = true;
        let prepared = prepare_collection_run(input, transport.as_ref()).unwrap();
        // Three iterations cycle the two data rows.
        assert_eq!(prepared.rows.len(), 3);
        let run_id = prepared.run.id.clone();

        let outcome = run_collection(prepared, transport, &RunControl::default());

        assert_eq!(outcome.error, None);
        assert_eq!(outcome.run.status, RunStatus::Completed);
        let persisted = fixture.store.runs(&fixture.workspace, 10).unwrap();
        assert_eq!(persisted.len(), 1);
        let persisted = &persisted[0];
        assert_eq!(persisted.id, run_id);
        assert_eq!(persisted.collection_id, fixture.collection_id);
        assert_eq!(persisted.status, RunStatus::Completed);
        assert_eq!(persisted.iteration_count, 3);
        assert!(persisted.keep_variable_values);
        assert_eq!(persisted.selected_request_ids.len(), 2);
        assert_eq!(persisted.item_results.len(), 6);
        assert_eq!(
            persisted
                .item_results
                .iter()
                .map(|item| item.iteration)
                .collect::<Vec<_>>(),
            vec![0, 0, 1, 1, 2, 2]
        );
        assert!(
            persisted
                .item_results
                .iter()
                .all(|item| item.status == Some(200) && item.response.is_some())
        );
        assert_eq!(persisted.completed_at, outcome.run.completed_at);
    }

    #[test]
    fn a_vault_bearer_value_never_reaches_the_persisted_or_returned_run() {
        let fixture = fixture("bearer-redaction");
        let secret_value = "vault-only-value-9f3c1a";
        let mut request = saved_request(&fixture, "Private", "https://example.test/private", "");
        let reference = SecretRef::new(format!(
            "workbench.request.{}.auth.token",
            request.id.as_str()
        ))
        .unwrap();
        fixture
            .secret_store
            .set_secret(
                &fixture.workspace,
                &reference,
                SecretValue::new(secret_value),
            )
            .unwrap();
        request.auth = AuthConfig::Bearer {
            token: reference.clone(),
        };
        fixture.store.upsert_request(&request).unwrap();
        let transport = Arc::new(FakeTransport::default());
        let prepared = prepare_collection_run(
            run_input(&fixture, vec![request], "", false),
            transport.as_ref(),
        )
        .unwrap();

        let outcome = run_collection(prepared, transport, &RunControl::default());

        assert_eq!(outcome.error, None);
        assert_eq!(outcome.run.status, RunStatus::Completed);
        assert!(
            outcome
                .redactions
                .iter()
                .any(|redaction| redaction == secret_value)
        );
        // The raw outcome still carries the echoed header body; the caller
        // must scrub it with the run redactions before showing or storing it.
        let raw_body = base64::engine::general_purpose::STANDARD
            .decode(
                &outcome.run.item_results[0]
                    .response
                    .as_ref()
                    .unwrap()
                    .body_base64,
            )
            .unwrap();
        assert!(String::from_utf8(raw_body).unwrap().contains(secret_value));

        let redacted = redact_collection_run(&outcome.run, &outcome.redactions);
        let redacted_json = serde_json::to_string(&redacted).unwrap();
        assert!(!redacted_json.contains(secret_value));
        let redacted_body = base64::engine::general_purpose::STANDARD
            .decode(
                &redacted.item_results[0]
                    .response
                    .as_ref()
                    .unwrap()
                    .body_base64,
            )
            .unwrap();
        assert!(!String::from_utf8_lossy(&redacted_body).contains(secret_value));

        let persisted = fixture.store.runs(&fixture.workspace, 10).unwrap();
        assert_eq!(persisted.len(), 1);
        let persisted_json = serde_json::to_string(&persisted[0]).unwrap();
        assert!(!persisted_json.contains(secret_value));
        let persisted_body = base64::engine::general_purpose::STANDARD
            .decode(
                &persisted[0].item_results[0]
                    .response
                    .as_ref()
                    .unwrap()
                    .body_base64,
            )
            .unwrap();
        assert!(!String::from_utf8_lossy(&persisted_body).contains(secret_value));
    }

    #[test]
    fn post_script_assertions_become_typed_test_results() {
        let fixture = fixture("test-results");
        let scripted = saved_request(
            &fixture,
            "Scripted",
            "https://example.test/scripted",
            "pm.test('typed service assertion', () => {});",
        );
        let plain = saved_request(&fixture, "Plain", "https://example.test/plain", "");
        let transport = Arc::new(FakeTransport::default());
        let prepared = prepare_collection_run(
            run_input(&fixture, vec![scripted.clone(), plain.clone()], "", false),
            transport.as_ref(),
        )
        .unwrap();

        let outcome = run_collection(prepared, transport.clone(), &RunControl::default());

        assert_eq!(outcome.error, None);
        assert_eq!(outcome.run.status, RunStatus::Completed);
        assert_eq!(transport.post_calls.load(Ordering::SeqCst), 2);
        let [with_tests, without_tests] = outcome.run.item_results.as_slice() else {
            panic!("expected two item results");
        };
        assert_eq!(with_tests.request_id, scripted.id);
        assert_eq!(
            with_tests.test_results,
            vec![TestResult {
                name: "[Post-response] typed service assertion".into(),
                passed: true,
                skipped: false,
                error: None,
            }]
        );
        assert_eq!(with_tests.error, None);
        assert_eq!(without_tests.request_id, plain.id);
        assert!(without_tests.test_results.is_empty());
        // The scripted item's send opens its operation and leaves it open for
        // the post script; the plain item's send is self-contained.
        let phases = transport.phases();
        assert_eq!(phases.len(), 6);
        let scripted_operation = &phases[0].1;
        assert_eq!(phases[0].0, "pre");
        assert_eq!(
            phases[1],
            ("send".to_string(), scripted_operation.clone(), true, false)
        );
        assert_eq!(
            phases[2],
            ("post".to_string(), scripted_operation.clone(), false, true)
        );
        let plain_operation = &phases[3].1;
        assert_ne!(plain_operation, scripted_operation);
        assert_eq!(
            phases[4],
            ("send".to_string(), plain_operation.clone(), true, true)
        );
        let persisted = fixture.store.runs(&fixture.workspace, 10).unwrap();
        assert_eq!(
            persisted[0].item_results[0].test_results,
            with_tests.test_results
        );
    }

    #[test]
    fn failed_pre_request_tests_stop_runs_and_console_is_safe_before_persistence() {
        let fixture = fixture("phase-diagnostics");
        let mut request = saved_request(
            &fixture,
            "Scripted",
            "https://example.test/items",
            "diagnostics",
        );
        request.scripts.pre_request = "diagnostics".into();
        let (variables, secrets) = parse_session_variables(
            "secret:custom=private-script-value\naccessToken=literal-script-value\npublic=visible",
            "request",
        )
        .unwrap();
        request.variables = variables;
        let plain = saved_request(&fixture, "Not reached", "https://example.test/next", "");
        let mut input = run_input(&fixture, vec![request, plain], "", true);
        input.secrets.merge(secrets);
        let transport = Arc::new(FakeTransport::default());
        let prepared = prepare_collection_run(input, transport.as_ref()).unwrap();
        let outcome = run_collection(prepared, transport, &RunControl::default());
        assert_eq!(outcome.run.status, RunStatus::Failed);
        assert_eq!(outcome.run.item_results.len(), 1);
        let item = &outcome.run.item_results[0];
        assert_eq!(item.status, Some(200));
        assert_eq!(item.test_results.len(), 2);
        assert!(item.test_results[0].name.starts_with("[Pre-request]"));
        assert!(!item.test_results[0].passed);
        assert!(item.error.is_some());
        assert_eq!(item.console.len(), 2);
        assert!(item.console[0].message.contains("visible"));
        let live = serde_json::to_string(item).unwrap();
        assert!(!live.contains("private-script-value"));
        assert!(!live.contains("literal-script-value"));
        let stored = fixture.store.runs(&fixture.workspace, 10).unwrap();
        assert_eq!(stored[0].item_results[0].console, item.console);
    }

    #[test]
    fn run_preparation_validates_bounds_and_selection() {
        let fixture = fixture("validation");
        let request = saved_request(&fixture, "Only", "https://example.test/only", "");
        let transport = FakeTransport::default();

        let mut input = run_input(&fixture, vec![request.clone()], "", false);
        input.iteration_limit = Some(0);
        assert_eq!(
            prepare_collection_run(input, &transport).err().as_deref(),
            Some("Iterations must be between 1 and 1000.")
        );
        let mut input = run_input(&fixture, vec![request.clone()], "", false);
        input.iteration_limit = Some(MAX_ITERATIONS + 1);
        assert!(prepare_collection_run(input, &transport).is_err());
        let mut input = run_input(&fixture, vec![request.clone()], "", false);
        input.delay_ms = MAX_DELAY_MS + 1;
        assert_eq!(
            prepare_collection_run(input, &transport).err().as_deref(),
            Some("Runner delay must be between 0 and 60000 ms.")
        );
        assert_eq!(
            prepare_collection_run(run_input(&fixture, Vec::new(), "", false), &transport)
                .err()
                .as_deref(),
            Some("Select at least one request for this run.")
        );
        assert_eq!(
            prepare_collection_run(run_input(&fixture, vec![request], "[]", false), &transport)
                .err()
                .as_deref(),
            Some("Runner data contains no iteration rows.")
        );
    }

    #[test]
    fn a_run_renews_a_stale_environment_oauth_cache_before_its_first_item() {
        let fixture = fixture("environment-oauth");
        let mut request = saved_request(&fixture, "Inherits", "https://api.example.test/x", "");
        request.auth = AuthConfig::Inherit;
        fixture.store.upsert_request(&request).unwrap();
        let environment_id = EnvironmentId::new();
        let scope = format!("environment.{}", environment_id.as_str());
        let client_secret =
            SecretRef::new(format!("workbench.{scope}.auth.client_secret")).unwrap();
        let stale_access = SecretRef::new(format!("workbench.{scope}.auth.token")).unwrap();
        let environment = Environment {
            id: environment_id.clone(),
            workspace_id: fixture.workspace.clone(),
            name: "QA".into(),
            base_url: String::new(),
            auth: AuthConfig::OAuth2ClientCredentials {
                headers: Vec::new(),
                token_endpoint: "https://identity.example.test/token".into(),
                client_id: "automation".into(),
                client_secret: client_secret.clone(),
                scopes: Vec::new(),
                access_token: Some(stale_access.clone()),
                // Already inside the refresh safety window.
                expires_at: Some(now_seconds().saturating_add(5)),
            },
            variables: Vec::new(),
            active: true,
            extensions: Default::default(),
        };
        fixture.store.upsert_environment(&environment).unwrap();
        fixture
            .secret_store
            .set_secret(
                &fixture.workspace,
                &client_secret,
                SecretValue::new("env-client-secret"),
            )
            .unwrap();
        fixture
            .secret_store
            .set_secret(
                &fixture.workspace,
                &stale_access,
                SecretValue::new("stale-env-token"),
            )
            .unwrap();

        let mut input = run_input(&fixture, vec![request], "", false);
        input.environment_id = Some(environment_id.clone());
        input.environment = Some(environment);
        input.environment_scope = scope;
        input.environment_auth = input.environment.as_ref().unwrap().auth.clone();
        let transport = Arc::new(FakeTransport::default());
        let prepared = prepare_collection_run(input, transport.as_ref()).unwrap();
        assert_eq!(transport.token_exchanges.load(Ordering::SeqCst), 1);
        let outcome = run_collection(prepared, transport.clone(), &RunControl::default());
        assert_eq!(outcome.run.status, RunStatus::Completed);

        // The request went out with the renewed token…
        let sent = outcome.run.item_results[0]
            .response
            .as_ref()
            .and_then(|response| {
                base64::engine::general_purpose::STANDARD
                    .decode(&response.body_base64)
                    .ok()
            })
            .map(|body| String::from_utf8_lossy(&body).into_owned())
            .unwrap_or_default();
        assert!(sent.contains("Bearer fresh-env-token"), "{sent}");
        // …and the saved environment carries the renewed cache for the app.
        let saved = fixture
            .store
            .list_environments(&fixture.workspace)
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == environment_id)
            .unwrap();
        let AuthConfig::OAuth2ClientCredentials { expires_at, .. } = &saved.auth else {
            panic!("environment auth kind changed: {:?}", saved.auth);
        };
        assert!(expires_at.is_some_and(|at| at >= now_seconds().saturating_add(500)));
        assert_eq!(
            fixture
                .secret_store
                .get_secret(&fixture.workspace, &stale_access)
                .unwrap()
                .expose_secret(),
            "fresh-env-token"
        );
    }
}
