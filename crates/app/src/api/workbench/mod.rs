//! API Workbench — durable API composition, execution, inspection and review.
//!
//! The workbench is a Space surface, not a Projects tab. Definitions, examples,
//! environments, history and runs are project-scoped in `WorkbenchStore`;
//! resolved secret values remain transient and only redacted snapshots persist.

mod assist;
mod body_editor;
mod collection_urls;
mod coordinator;
mod draft;
mod empty_state;
mod entries;
mod environment_label;
mod globals;
mod keys;
mod layout;
mod login;
mod move_request;
mod persistence;
mod pretty;
mod rail_rename;
mod rename_workspace;
mod request_creation;
mod response_controls;
mod response_copy;
mod response_editor;
mod response_inspector;
mod response_preview;
mod response_query;
mod scope_editor;
mod script_variables;
mod tab_menu;
mod transport;
mod ux;
mod view;
mod workspaces;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use switchyard_api::runtime::oauth::{
    BrowserAuthorization, OAuthVariables, pkce_needs_browser, preserve_managed_oauth_state,
    same_managed_auth_source, send_auth_owner, store_oauth_token,
};
use switchyard_api::runtime::{
    RunPreparationInput, RunSession, StandaloneSendInput, execute_standalone_send, folder_chain,
    folder_is_within, now_millis, now_seconds, prepare_collection_run, prepare_standalone_send,
    select_run_requests, workspace_id_for,
};
use switchyard_api::{
    AuthConfig, Body, Collection, CollectionId, CollectionRun, Environment, EnvironmentId, Example,
    ExampleId, Exchange, ExchangeId, Folder, FolderId, ImportResult, ImportSelection,
    RedactedRequestSnapshot, RequestId, RunId, RunStatus, SavedRequest, Scripts, SecretRef,
    SecretResolver, SnippetLanguage, Variable, VariableValue, WorkspaceId,
    compile_redacted_request_with_folder_chain, diff_exchanges,
};

use gpui_kit::component::input::TextareaState;
use gpui_kit::component::{
    ActiveTheme,
    dock::PanelEvent,
    input::{InputEvent, InputState},
};
use gpui_kit::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, PathPromptOptions, SharedString,
    Window, prelude::*,
};

use crate::api::compat::field;
use crate::api::compat::theme::tokens::{radius, space};
use crate::api::compat::theme::{palette, text};
use crate::api::compat::{AnyInput, TextValue};

pub use keys::{FocusUrl, ShowCompose, ShowDiff, ShowEnvs, ShowHistory, ShowImport, ShowRunner};

/// Key scope for request-draft commands such as Save.
pub const KEY_CONTEXT: &str = "Workbench";
/// Key scope of the collection rail; F2 renames its selected row there.
pub const RAIL_KEY_CONTEXT: &str = "WorkbenchRail";
gpui_kit::actions!(
    workbench,
    [
        SendRequest,
        CancelRequest,
        NewRequest,
        CloseRequest,
        NextRequest,
        PreviousRequest,
        RenameRailItem
    ]
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Compose,
    Import,
    Data,
    Environments,
    History,
    Diff,
}

impl Tab {
    const ALL: [Self; 6] = [
        Self::Compose,
        Self::Import,
        Self::Data,
        Self::Environments,
        Self::History,
        Self::Diff,
    ];

    fn id(self) -> &'static str {
        match self {
            Self::Compose => "compose",
            Self::Import => "import",
            Self::Data => "data",
            Self::Environments => "envs",
            Self::History => "history",
            Self::Diff => "diff",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Compose => "Compose",
            Self::Import => "Import",
            Self::Data => "Runner",
            Self::Environments => "Envs",
            Self::History => "History",
            Self::Diff => "Diff",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposerTab {
    Params,
    Headers,
    Body,
    Auth,
    Vars,
    Tests,
    Snippet,
    Settings,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PreparedExport {
    contents: String,
    suggested_name: String,
    workspace: WorkspaceId,
    generation: u64,
}

impl ComposerTab {
    const ALL: [Self; 8] = [
        Self::Params,
        Self::Headers,
        Self::Body,
        Self::Auth,
        Self::Vars,
        Self::Tests,
        Self::Snippet,
        Self::Settings,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Params => "Params",
            Self::Headers => "Headers",
            Self::Body => "Body",
            Self::Auth => "Auth",
            Self::Vars => "Vars",
            Self::Tests => "Scripts",
            Self::Snippet => "Snippet",
            Self::Settings => "Settings",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseTab {
    Pretty,
    Raw,
    Headers,
    Trace,
    Tests,
    Console,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum SendState {
    #[default]
    Idle,
    PreparingRun,
    Sending,
    Cancelling,
}

const SNIPPET_LANGUAGES: [(SnippetLanguage, &str); 8] = [
    (SnippetLanguage::Curl, "cURL"),
    (SnippetLanguage::RawHttp, "HTTP"),
    (SnippetLanguage::RustReqwest, "Rust"),
    (SnippetLanguage::JavaScriptFetch, "JavaScript"),
    (SnippetLanguage::TypeScriptFetch, "TypeScript"),
    (SnippetLanguage::PythonRequests, "Python"),
    (SnippetLanguage::PowerShell, "PowerShell"),
    (SnippetLanguage::CSharpHttpClient, "C#"),
];

// `InputState::set_value` emits one `Change` event for every hydrated editor.
const REQUEST_EDITOR_INPUTS: usize = 11;

impl ResponseTab {
    const ALL: [Self; 6] = [
        Self::Pretty,
        Self::Raw,
        Self::Headers,
        Self::Trace,
        Self::Tests,
        Self::Console,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Pretty => "Pretty",
            Self::Raw => "Raw",
            Self::Headers => "Headers",
            Self::Trace => "Trace",
            Self::Tests => "Tests",
            Self::Console => "Console",
        }
    }
}

#[derive(Clone)]
struct HistoryEntry {
    exchange: Exchange,
    method: String,
    target: String,
    response: Arc<transport::Response>,
    elapsed_ms: u128,
    redactions: Vec<String>,
}

#[derive(Clone)]
struct PendingExchange {
    exchange: Exchange,
    redactions: Vec<String>,
}

/// One request draft in the Compose tab strip. GPUI owns one set of editor
/// entities, so inactive tabs retain their values here and are rehydrated on
/// activation instead of sharing the active editor's mutable state.
#[derive(Clone)]
struct RequestTabState {
    id: u64,
    current_definition: Option<SavedRequest>,
    current_request_id: Option<RequestId>,
    current_collection_id: Option<CollectionId>,
    selected_folder_id: Option<FolderId>,
    method: transport::Method,
    body_mode: draft::BodyMode,
    auth_mode: draft::AuthMode,
    allow_private_network: bool,
    dirty: bool,
    input_values: Vec<String>,
    error: Option<String>,
    response: Option<Arc<transport::Response>>,
    response_body: Option<pretty::PreparedBody>,
    response_request: Option<RedactedRequestSnapshot>,
    elapsed_ms: Option<u128>,
}

impl RequestTabState {
    fn blank(id: u64, current_collection_id: Option<CollectionId>) -> Self {
        Self {
            id,
            current_definition: None,
            current_request_id: None,
            current_collection_id,
            selected_folder_id: None,
            method: transport::Method::Get,
            body_mode: draft::BodyMode::None,
            auth_mode: draft::AuthMode::None,
            allow_private_network: true,
            dirty: false,
            input_values: vec![String::new(); REQUEST_EDITOR_INPUTS],
            error: None,
            response: None,
            response_body: None,
            response_request: None,
            elapsed_ms: None,
        }
    }

    fn name(&self) -> &str {
        self.input_values
            .get(1)
            .map(String::as_str)
            .filter(|name| !name.trim().is_empty())
            .unwrap_or("Untitled request")
    }
}

/// What the active environment contributes to a send or a sign-in.
struct EnvironmentSendContext {
    environment: Option<Environment>,
    scope: String,
    source: String,
    base_url: String,
    auth: AuthConfig,
    secrets: draft::DraftSecrets,
}

impl HistoryEntry {
    fn from_exchange(exchange: Exchange) -> Self {
        let response = Arc::new(
            exchange
                .response
                .as_ref()
                .map(transport::response_from_snapshot)
                .unwrap_or_else(|| transport::Response {
                    console: exchange.console.clone(),
                    status: 0,
                    reason: exchange.error.clone().unwrap_or_default(),
                    headers: Vec::new(),
                    set_cookies: Vec::new(),
                    cookie_mutations: Vec::new(),
                    body: String::new(),
                    body_base64: String::new(),
                    binary: false,
                    final_url: exchange.request.url.clone(),
                    http_version: String::new(),
                    received_bytes: 0,
                    stored_bytes: 0,
                    full_body_sha256: None,
                    timings: Default::default(),
                    cookies: Vec::new(),
                    duration_ms: 0,
                    truncated: false,
                    redirects: Vec::new(),
                    test_results: exchange.test_results.clone(),
                }),
        );
        Self {
            method: exchange.request.method.clone(),
            target: exchange.request.url.clone(),
            elapsed_ms: response.duration_ms.into(),
            response,
            exchange,
            redactions: Vec::new(),
        }
    }
}

pub struct WorkbenchPanel {
    focus_handle: FocusHandle,
    ux: ux::WorkbenchUx,
    tab: Tab,
    composer_tab: ComposerTab,
    response_tab: ResponseTab,
    method: transport::Method,
    custom_method: Entity<InputState>,
    container_name: Entity<InputState>,
    request_name: Entity<InputState>,
    url: Entity<InputState>,
    params: Entity<TextareaState>,
    headers: Entity<TextareaState>,
    body: Entity<TextareaState>,
    cookies: Entity<TextareaState>,
    pre_request_script: Entity<TextareaState>,
    authorization: Entity<TextareaState>,
    variables: Entity<TextareaState>,
    assertions: Entity<TextareaState>,
    import_source: Entity<TextareaState>,
    data_source: Entity<TextareaState>,
    /// Render-only derived data, discarded by the source input's Change event.
    /// No duplicate raw source is retained alongside the parsed rows.
    data_preview: std::cell::RefCell<Option<std::rc::Rc<view::DataPreview>>>,
    runner_iterations: Entity<InputState>,
    runner_delay_ms: Entity<InputState>,
    history_filter: Entity<InputState>,
    environment_variables: Entity<TextareaState>,
    /// Rail filter — narrows the tree by name or URL; not a request editor.
    rail_filter: Entity<InputState>,
    /// The Data tab's scenario prose, carried into the generate-data prompt.
    data_prompt: Entity<TextareaState>,
    /// The Data tab's seed readout, carried into the generate-data prompt.
    runner_seed: Entity<InputState>,
    /// The rail's inline rename field, drawn in place of the row it renames.
    rail_rename: Option<rail_rename::RailRename>,
    /// The rail row last clicked: what F2 renames.
    rail_selection: Option<RenameTarget>,
    /// Key focus of the collection rail, so F2 reaches it.
    rail_focus: FocusHandle,
    url_replacement: Option<Entity<collection_urls::UrlReplacement>>,
    /// History shows only the last 24 hours.
    history_last_24h: bool,
    /// Indices into [`view::DATA_PRESETS`] that are toggled on.
    data_presets: HashSet<usize>,
    data_schema_valid: bool,
    data_unique_keys: bool,
    /// The row of the parsed data table that "Use as body" copies.
    data_selected_row: Option<usize>,
    /// Environments that arrived through Import, for the SOURCE column.
    imported_environment_ids: HashSet<EnvironmentId>,
    /// The name a not-yet-saved environment will be stored under.
    /// The Envs header's inline name field; blank for a new draft.
    environment_name: Entity<InputState>,
    /// Prefixed onto relative request URLs when this environment is active.
    environment_base_url: Entity<InputState>,
    /// What requests inheriting all the way up authenticate with: the type
    /// chip and the `key=value` text the typed form serializes into.
    environment_auth_mode: draft::AuthMode,
    environment_auth: Entity<TextareaState>,
    /// The Envs editor's Production / Staging / Development / Local picker.
    environment_label: switchyard_api::EnvironmentLabel,
    /// Outcome of the last "Sign in now" for the Envs status line.
    environment_login_status: Option<String>,
    _login_work: Option<gpui_kit::Task<()>>,
    import_status: Option<String>,
    staged_import: Option<ImportResult>,
    import_selection: Option<ImportSelection>,
    import_generation: u64,
    body_mode: draft::BodyMode,
    auth_mode: draft::AuthMode,
    /// The Postman-style grids and typed forms over the text editors above.
    headers_grid: Entity<entries::KvGrid>,
    params_grid: Entity<entries::KvGrid>,
    variables_grid: Entity<entries::KvGrid>,
    environment_grid: Entity<entries::KvGrid>,
    auth_form: Entity<entries::AuthForm>,
    environment_auth_form: Entity<entries::AuthForm>,
    /// The rail's selected folder — where `+ New request` files the next
    /// request and what Rename/Delete folder act on. Clicking a folder never
    /// moves the open request; that is the explicit "Move request here".
    selected_folder_id: Option<FolderId>,
    bound_workspace: WorkspaceId,
    pending_workspace: Option<WorkspaceId>,
    workspace_data: Option<persistence::WorkspaceData>,
    /// The named workspaces; `None` until listed. Empty shows only the
    /// "Add project" page.
    workspaces: Option<Vec<switchyard_api::WorkspaceEntry>>,
    _workspace_work: Option<gpui_kit::Task<()>>,
    request_tabs: Vec<RequestTabState>,
    active_request_tab: usize,
    next_request_tab_id: u64,
    right_clicked_request_tab: Option<u64>,
    current_definition: Option<SavedRequest>,
    session_secrets: draft::DraftSecrets,
    current_request_id: Option<RequestId>,
    current_collection_id: Option<CollectionId>,
    /// The rail renders only the selected collection's tree. Selection stays
    /// intact when that tree is collapsed so the open request remains usable.
    collection_tree_expanded: bool,
    /// Folder expansion belongs to the rail, independent of open request tabs.
    collapsed_folder_ids: HashSet<FolderId>,
    pending_request_to_load: Option<RequestId>,
    active_environment_id: Option<EnvironmentId>,
    dirty: bool,
    pending_editor_hydration_changes: usize,
    storage_loading: bool,
    storage_generation: u64,
    storage_error: Option<String>,
    navigation_notice: Option<String>,
    run_status: Option<String>,
    allow_private_network: bool,
    send_state: SendState,
    error: Option<String>,
    response: Option<Arc<transport::Response>>,
    /// Cached body presentation prepared on the background executor. `None`
    /// while a response is still being prepared.
    response_body: Option<pretty::PreparedBody>,
    response_pretty_editor: Entity<response_editor::ResponseEditor>,
    response_raw_editor: Entity<response_editor::ResponseEditor>,
    response_body_generation: u64,
    /// The redacted request that produced `response` — the send's exchange, a
    /// replayed history entry, or a saved example. `None` when the example
    /// kept no request snapshot; the Chat handoff then compiles the draft.
    response_request: Option<RedactedRequestSnapshot>,
    elapsed_ms: Option<u128>,
    history: Vec<HistoryEntry>,
    selected_history: Vec<usize>,
    history_retention: usize,
    import_origin: Option<String>,
    import_origin_source: Option<String>,
    diff_generation: u64,
    diff_result: Option<PreparedDiff>,
    send_generation: u64,
    active_request_id: Option<String>,
    pending_exchange: Option<PendingExchange>,
    active_run: Option<CollectionRun>,
    active_run_redactions: Vec<String>,
    secret_store: Arc<dyn switchyard_api::SecretStore>,
    cookie_jar: switchyard_api::CookieJar,
    file_capabilities: transport::FileCapabilities,
    transport: Arc<dyn transport::WorkbenchTransport>,
    #[cfg(test)]
    preparation_gate_delay: Option<std::time::Duration>,
    snippet_language: SnippetLanguage,
    snippet_output: Option<String>,
    snippet_generation: u64,
    export_output: Option<String>,
    prepared_export: Option<PreparedExport>,
    export_generation: u64,
    stop_on_error: bool,
    keep_runner_variables: bool,
    runner_folder_id: Option<FolderId>,
    runner_request_ids: HashSet<RequestId>,
    runner_request_selection_active: bool,
    selected_run_result: Option<(RunId, usize)>,
    _request_work: Option<gpui_kit::Task<()>>,
    _response_body_work: Option<gpui_kit::Task<()>>,
    _import_work: Option<gpui_kit::Task<()>>,
    _storage_work: Option<gpui_kit::Task<()>>,
    _diff_work: Option<gpui_kit::Task<()>>,
    _snippet_work: Option<gpui_kit::Task<()>>,
    _export_work: Option<gpui_kit::Task<()>>,
    assist_generation: u64,
    _assist_work: Option<gpui_kit::Task<()>>,
}

impl WorkbenchPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut panel = Self::new_with_workspace_result(
            window,
            cx,
            current_workspace_id(),
            Err("Workbench storage is loading.".into()),
            crate::api::compat::secrets(cx),
        );
        panel.storage_error = None;
        panel.load_workspaces(window, cx);
        panel
    }

    fn set_response(&mut self, response: Option<transport::Response>, cx: &mut Context<Self>) {
        self.set_shared_response(response.map(Arc::new), None, cx);
    }

    fn set_shared_response(
        &mut self,
        response: Option<Arc<transport::Response>>,
        prepared: Option<pretty::PreparedBody>,
        cx: &mut Context<Self>,
    ) {
        self.ux.console_cleared = 0;
        self.ux.console_cleared_error = None;
        self.response_body_generation = self.response_body_generation.wrapping_add(1);
        let generation = self.response_body_generation;
        self._response_body_work = None;
        self.response = response.clone();
        self.ux.response_view.reset();
        self.response_body = prepared;
        self.sync_response_editors(cx);
        self.refresh_response_view(cx);
        let Some(response) = response.filter(|_| self.response_body.is_none()) else {
            return;
        };
        self._response_body_work = Some(cx.spawn(async move |this, cx| {
            let prepared = crate::api::compat::blocking(move || {
                let content_type = response
                    .headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("Content-Type"))
                    .map(|(_, value)| value.as_str());
                pretty::prepare(content_type, &response.body, response.binary)
            })
            .await;
            let _ = this.update(cx, |panel, cx| {
                if panel.response_body_generation != generation {
                    return;
                }
                panel.response_body = Some(prepared);
                panel.sync_response_editors(cx);
                panel._response_body_work = None;
                cx.notify();
            });
        }));
    }

    fn run_storage_command(
        &mut self,
        command: coordinator::StorageCommand,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.storage_loading {
            self.storage_error = Some("A Workbench storage operation is already running.".into());
            cx.notify();
            return false;
        }
        let Some(data) = self.workspace_data.as_ref() else {
            self.storage_error = Some("Workbench storage is unavailable".into());
            cx.notify();
            return false;
        };
        let store = data.store.clone();
        let workspace = data.workspace.clone();
        self.storage_generation = self.storage_generation.wrapping_add(1);
        let generation = self.storage_generation;
        self.storage_loading = true;
        self.storage_error = None;
        self._storage_work = Some(cx.spawn(async move |this, cx| {
            let command_workspace = workspace.clone();
            let result = crate::api::compat::blocking(move || {
                coordinator::execute(store, command_workspace, command)
            })
            .await;
            let _ = this.update(cx, |panel, cx| {
                if panel.storage_generation != generation || panel.bound_workspace != workspace {
                    return;
                }
                panel.storage_loading = false;
                panel._storage_work = None;
                match result {
                    Ok(data) => {
                        panel.workspace_data = Some(data);
                        panel.storage_error = None;
                    }
                    Err(error) => panel.storage_error = Some(error),
                }
                cx.notify();
            });
        }));
        cx.notify();
        true
    }

    fn new_with_workspace_result(
        window: &mut Window,
        cx: &mut Context<Self>,
        workspace: WorkspaceId,
        workspace_data: Result<persistence::WorkspaceData, String>,
        secret_store: Arc<dyn switchyard_api::SecretStore>,
    ) -> Self {
        let request_name = cx.new(|cx| InputState::new(window, cx).placeholder("Untitled request"));
        let custom_method = cx.new(|cx| InputState::new(window, cx).placeholder("CUSTOM"));
        let container_name = cx.new(|cx| InputState::new(window, cx).placeholder("Name"));
        let url =
            cx.new(|cx| InputState::new(window, cx).placeholder("https://api.example.com/v1"));
        let headers = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Accept: application/json\nX-Request-ID: example")
                .auto_grow(2, 5)
        });
        let params = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("limit=25\n# disabled=value")
                .auto_grow(2, 6)
        });
        let body = cx.new(|cx| TextareaState::new(window, cx).placeholder("Request body"));
        let cookies = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("session=value; theme=dark")
                .auto_grow(2, 5)
        });
        let pre_request_script = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("pm.environment.set('nonce', 'value');")
                .auto_grow(2, 6)
        });
        // Serialized by the typed auth form, never painted directly; the
        // form's own cells carry the masking.
        let authorization = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("token=...")
                .auto_grow(1, 12)
        });
        let variables = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("resource_id=42\napi_token=...")
                .auto_grow(2, 6)
        });
        let assertions = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(
                    "pm.test('status is 200', () => pm.expect(pm.response.code).to.eql(200));",
                )
                .auto_grow(2, 6)
        });
        let import_source = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Paste OpenAPI JSON/YAML, Postman, Insomnia, HAR, AgentOps, or cURL")
                .auto_grow(8, 18)
        });
        let data_source = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Paste CSV or a JSON array of iteration data")
                .auto_grow(8, 18)
        });
        let runner_iterations =
            cx.new(|cx| InputState::new(window, cx).placeholder("all data rows"));
        let runner_delay_ms = cx.new(|cx| InputState::new(window, cx).placeholder("0"));
        let history_filter =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter method, URL, status"));
        let environment_name =
            cx.new(|cx| InputState::new(window, cx).placeholder("Environment name"));
        let environment_base_url =
            cx.new(|cx| InputState::new(window, cx).placeholder("https://api.example.com"));
        let environment_auth = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Fields the selected type needs")
                .auto_grow(3, 12)
        });
        let environment_variables = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("base_url=https://api.example.com\napi_token=...")
                .auto_grow(6, 16)
        });
        let rail_filter =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter collections"));
        let data_prompt = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Describe the scenario — e.g. 200 users across three tiers, a few with expired tokens")
                .auto_grow(4, 8)
        });
        let runner_seed = cx.new(|cx| InputState::new(window, cx).placeholder("auto"));
        let headers_grid = cx.new(|_| {
            entries::KvGrid::new(
                headers.clone(),
                ':',
                "workbench-headers",
                "Repeated names are preserved · disabled rows stay in the draft",
                ("Header", "value"),
                false,
            )
        });
        let params_grid = cx.new(|_| {
            entries::KvGrid::new(
                params.clone(),
                '=',
                "workbench-params",
                "Query parameters · repeat a key to send it twice",
                ("key", "value"),
                false,
            )
        });
        let variables_grid = cx.new(|_| {
            entries::KvGrid::new(
                variables.clone(),
                '=',
                "workbench-vars",
                "Request-scoped variables shadow the environment for this request only · secret:name keeps the value in the vault",
                ("name", "value"),
                true,
            )
        });
        let environment_grid = cx.new(|_| {
            entries::KvGrid::new(
                environment_variables.clone(),
                '=',
                "workbench-env-vars",
                "Variables · {{name}} in any request · secret:name keeps the value in the vault",
                ("name", "value"),
                true,
            )
        });
        let auth_form = cx.new(|_| entries::AuthForm::new(authorization.clone(), "workbench-auth"));
        let environment_auth_form =
            cx.new(|_| entries::AuthForm::new(environment_auth.clone(), "workbench-env-auth"));
        fn on_editor_change(
            this: &mut WorkbenchPanel,
            event: &InputEvent,
            cx: &mut Context<WorkbenchPanel>,
        ) {
            if matches!(event, InputEvent::Change) {
                if this.pending_editor_hydration_changes > 0 {
                    this.pending_editor_hydration_changes -= 1;
                } else {
                    this.dirty = true;
                }
                cx.notify();
            }
        }
        for input in [&request_name, &custom_method, &url] {
            cx.subscribe(input, |this, _, event: &InputEvent, cx| {
                on_editor_change(this, event, cx)
            })
            .detach();
        }
        for input in [
            &params,
            &headers,
            &body,
            &cookies,
            &pre_request_script,
            &authorization,
            &variables,
            &assertions,
        ] {
            cx.subscribe(input, |this, _, event: &InputEvent, cx| {
                on_editor_change(this, event, cx)
            })
            .detach();
        }
        for input in [&history_filter, &rail_filter] {
            cx.subscribe(input, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            })
            .detach();
        }
        Self::subscribe_rail_rename(&container_name, window, cx);
        cx.subscribe(&data_source, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.data_preview.borrow_mut().take();
                cx.notify();
            }
        })
        .detach();
        let (workspace_data, storage_error) = match workspace_data {
            Ok(data) => (Some(data), None),
            Err(error) => (None, Some(error)),
        };
        let history = workspace_data
            .as_ref()
            .map(|data| {
                data.history
                    .iter()
                    .cloned()
                    .map(HistoryEntry::from_exchange)
                    .collect()
            })
            .unwrap_or_default();
        let active_environment_id = workspace_data.as_ref().and_then(|data| {
            data.active_environment()
                .map(|environment| environment.id.clone())
        });
        let current_collection_id = workspace_data
            .as_ref()
            .and_then(|data| data.collections.first().map(|value| value.id.clone()));
        let cookie_jar = workspace_data
            .as_ref()
            .map(|data| data.cookies.clone())
            .unwrap_or_else(|| switchyard_api::CookieJar::new(workspace.clone()));
        let mut panel = Self {
            ux: ux::WorkbenchUx::new(body.clone(), window, cx),
            focus_handle: cx.focus_handle(),
            tab: Tab::Compose,
            composer_tab: ComposerTab::Params,
            response_tab: ResponseTab::Pretty,
            method: transport::Method::Get,
            custom_method,
            container_name,
            request_name,
            url,
            params,
            headers,
            body,
            cookies,
            pre_request_script,
            authorization,
            variables,
            assertions,
            import_source,
            data_source,
            data_preview: Default::default(),
            runner_iterations,
            runner_delay_ms,
            history_filter,
            environment_name,
            environment_base_url,
            environment_auth_mode: draft::AuthMode::None,
            environment_auth,
            environment_label: Default::default(),
            environment_login_status: None,
            _login_work: None,
            environment_variables,
            rail_filter,
            data_prompt,
            runner_seed,
            rail_rename: None,
            rail_selection: None,
            rail_focus: cx.focus_handle(),
            url_replacement: None,
            history_last_24h: false,
            data_presets: HashSet::new(),
            data_schema_valid: true,
            data_unique_keys: true,
            data_selected_row: None,
            imported_environment_ids: HashSet::new(),
            import_status: None,
            staged_import: None,
            import_selection: None,
            import_generation: 0,
            body_mode: draft::BodyMode::None,
            auth_mode: draft::AuthMode::None,
            headers_grid,
            params_grid,
            variables_grid,
            environment_grid,
            auth_form,
            environment_auth_form,
            selected_folder_id: None,
            bound_workspace: workspace.clone(),
            pending_workspace: None,
            workspace_data,
            workspaces: None,
            _workspace_work: None,
            // A tab opens only when a request is opened or created.
            request_tabs: Vec::new(),
            active_request_tab: 0,
            next_request_tab_id: 1,
            right_clicked_request_tab: None,
            current_definition: None,
            session_secrets: draft::DraftSecrets::with_store(
                secret_store.clone(),
                workspace.clone(),
            ),
            current_request_id: None,
            current_collection_id,
            collection_tree_expanded: true,
            collapsed_folder_ids: HashSet::new(),
            pending_request_to_load: None,
            active_environment_id,
            dirty: false,
            pending_editor_hydration_changes: 0,
            storage_loading: false,
            storage_generation: 0,
            storage_error,
            navigation_notice: None,
            run_status: None,
            allow_private_network: true,
            send_state: SendState::Idle,
            error: None,
            response: None,
            response_body: None,
            response_pretty_editor: cx.new(|_| response_editor::ResponseEditor::new(true)),
            response_raw_editor: cx.new(|_| response_editor::ResponseEditor::new(false)),
            response_body_generation: 0,
            response_request: None,
            elapsed_ms: None,
            history,
            selected_history: Vec::new(),
            history_retention: 1_000,
            import_origin: None,
            import_origin_source: None,
            diff_generation: 0,
            diff_result: None,
            send_generation: 0,
            active_request_id: None,
            pending_exchange: None,
            active_run: None,
            active_run_redactions: Vec::new(),
            secret_store,
            cookie_jar,
            file_capabilities: transport::FileCapabilities::default(),
            transport: Arc::new(transport::NativeWorkbenchTransport::from_env()),
            #[cfg(test)]
            preparation_gate_delay: None,
            snippet_language: SnippetLanguage::Curl,
            snippet_output: None,
            snippet_generation: 0,
            export_output: None,
            prepared_export: None,
            export_generation: 0,
            stop_on_error: false,
            keep_runner_variables: true,
            runner_folder_id: None,
            runner_request_ids: HashSet::new(),
            runner_request_selection_active: false,
            selected_run_result: None,
            _request_work: None,
            _response_body_work: None,
            _import_work: None,
            _storage_work: None,
            _diff_work: None,
            _snippet_work: None,
            _export_work: None,
            assist_generation: 0,
            _assist_work: None,
        };
        if let Some(request) = panel
            .workspace_data
            .as_ref()
            .and_then(|data| data.requests.first())
            .cloned()
        {
            panel.load_saved_request(&request, window, cx);
        }
        let environment = panel
            .workspace_data
            .as_ref()
            .and_then(persistence::WorkspaceData::active_environment)
            .cloned();
        panel.load_environment_editor(environment.as_ref(), window, cx);
        panel.dirty = false;
        panel
    }

    fn rehydrate_workspace(
        &mut self,
        workspace: WorkspaceId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_workspace_hydration(workspace, window, cx);
    }

    fn start_workspace_hydration(
        &mut self,
        workspace: WorkspaceId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.ux.creation.busy() {
            self.navigation_notice = Some(
                "Requests are still being created. Try Sync again when creation finishes.".into(),
            );
            cx.notify();
            return;
        }
        if self.bound_workspace != workspace {
            self.invalidate_export();
        }
        self.bound_workspace = workspace.clone();
        self.pending_workspace = None;
        if self.send_state != SendState::Idle {
            self.cancel(cx);
        }
        self.send_generation = self.send_generation.wrapping_add(1);
        self.storage_generation = self.storage_generation.wrapping_add(1);
        let generation = self.storage_generation;
        self.storage_loading = true;
        self.storage_error = None;
        let data_dir = workbench_data_dir();
        let opened_workspace = workspace.clone();
        self._storage_work = Some(cx.spawn_in(window, async move |this, cx| {
            let opened = crate::api::compat::blocking(move || {
                let data = data_dir
                    .ok_or_else(|| "Cannot resolve AgentOps user data directory.".to_string())
                    .and_then(|path| persistence::WorkspaceData::open(&path, opened_workspace))?;
                let history = data
                    .history
                    .iter()
                    .cloned()
                    .map(HistoryEntry::from_exchange)
                    .collect();
                Ok::<_, String>((data, history))
            })
            .await;
            let _ = this.update_in(cx, |panel, window, cx| {
                if panel.storage_generation != generation || panel.bound_workspace != workspace {
                    return;
                }
                panel.apply_workspace_hydration(opened, workspace, window, cx);
            });
        }));
        cx.notify();
    }

    fn invalidate_export(&mut self) {
        self.export_generation = self.export_generation.wrapping_add(1);
        self.export_output = None;
        self.prepared_export = None;
        self._export_work = None;
    }

    fn owns_export(
        &self,
        workspace: &WorkspaceId,
        generation: u64,
        current_workspace: &WorkspaceId,
    ) -> bool {
        self.bound_workspace == *workspace
            && self.export_generation == generation
            && workspace == current_workspace
    }

    fn apply_export_result(
        &mut self,
        workspace: &WorkspaceId,
        generation: u64,
        current_workspace: &WorkspaceId,
        suggested_name: String,
        result: Result<String, String>,
    ) -> bool {
        if !self.owns_export(workspace, generation, current_workspace) {
            return false;
        }
        match result {
            Ok(contents) => {
                self.export_output = Some(contents.clone());
                self.prepared_export = Some(PreparedExport {
                    contents,
                    suggested_name,
                    workspace: workspace.clone(),
                    generation,
                });
            }
            Err(error) => {
                self.export_output = Some(format!("Export failed: {error}"));
                self.prepared_export = None;
            }
        }
        self._export_work = None;
        true
    }

    fn apply_export_picker_error(
        &mut self,
        workspace: &WorkspaceId,
        generation: u64,
        current_workspace: &WorkspaceId,
        error: &str,
    ) -> bool {
        if !self.owns_export(workspace, generation, current_workspace) {
            return false;
        }
        self.tab = Tab::Compose;
        self.composer_tab = ComposerTab::Snippet;
        self.export_output = Some(format!("Could not open file picker: {error}"));
        true
    }

    fn apply_workspace_hydration(
        &mut self,
        opened: Result<(persistence::WorkspaceData, Vec<HistoryEntry>), String>,
        workspace: WorkspaceId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.storage_loading = false;
        self._storage_work = None;
        match opened {
            Ok((data, history)) => {
                self.history = history;
                self.active_environment_id = data
                    .active_environment()
                    .map(|environment| environment.id.clone());
                self.current_collection_id = data
                    .collections
                    .first()
                    .map(|collection| collection.id.clone());
                self.collection_tree_expanded = true;
                self.collapsed_folder_ids.clear();
                let request = data.requests.first().cloned();
                let environment = data.active_environment().cloned();
                self.cookie_jar = data.cookies.clone();
                self.workspace_data = Some(data);
                self.ux.request_settings.clear();
                self.ux.creation = Default::default();
                // No blank tab: `load_saved_request` opens one for the
                // first request, and an empty project shows the empty state.
                self.request_tabs.clear();
                self.active_request_tab = 0;
                self.current_request_id = None;
                self.current_definition = None;
                self.session_secrets =
                    draft::DraftSecrets::with_store(self.secret_store.clone(), workspace.clone());
                self.selected_history.clear();
                self.diff_generation = self.diff_generation.wrapping_add(1);
                self.diff_result = None;
                self._diff_work = None;
                self.set_response(None, cx);
                self.response_request = None;
                self.active_run = None;
                self.error = None;
                self.storage_error = None;
                if let Some(request) = request {
                    self.load_saved_request(&request, window, cx);
                } else {
                    self.clear_request_editor(window, cx);
                }
                self.load_environment_editor(environment.as_ref(), window, cx);
            }
            Err(error) => {
                self.workspace_data = None;
                self.storage_error = Some(error);
            }
        }
        self.dirty = false;
        cx.notify();
    }

    fn clear_request_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.active_request_tab_id() {
            self.ux.request_settings.remove(&id);
        }
        self.current_request_id = None;
        self.current_definition = None;
        self.method = transport::Method::Get;
        self.body_mode = draft::BodyMode::None;
        self.auth_mode = draft::AuthMode::None;
        self.allow_private_network = true;
        self.pending_editor_hydration_changes += REQUEST_EDITOR_INPUTS;
        for input in self.request_editor_inputs() {
            set_input(input.as_text(), "", window, cx);
        }
        self.dirty = false;
    }

    fn capture_active_request_tab(&mut self, cx: &App) {
        let input_values = self
            .request_editor_inputs()
            .iter()
            .map(|input| input.as_text().text(cx).to_string())
            .collect();
        let Some(tab) = self.request_tabs.get_mut(self.active_request_tab) else {
            return;
        };
        tab.current_definition = self.current_definition.clone();
        tab.current_request_id = self.current_request_id.clone();
        tab.current_collection_id = self.current_collection_id.clone();
        tab.selected_folder_id = self.selected_folder_id.clone();
        tab.method = self.method;
        tab.body_mode = self.body_mode;
        tab.auth_mode = self.auth_mode;
        tab.allow_private_network = self.allow_private_network;
        tab.dirty = self.dirty;
        tab.input_values = input_values;
        tab.error = self.error.clone();
        tab.response = self.response.clone();
        tab.response_body = self.response_body.clone();
        tab.response_request = self.response_request.clone();
        tab.elapsed_ms = self.elapsed_ms;
    }

    fn restore_active_request_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // With no tab open, a blank template resets the hidden editor.
        let tab = self
            .request_tabs
            .get(self.active_request_tab)
            .cloned()
            .unwrap_or_else(|| {
                RequestTabState::blank(self.next_request_tab_id, self.current_collection_id.clone())
            });
        self.current_definition = tab.current_definition;
        self.current_request_id = tab.current_request_id;
        self.current_collection_id = tab.current_collection_id;
        self.selected_folder_id = tab.selected_folder_id;
        self.method = tab.method;
        self.body_mode = tab.body_mode;
        self.auth_mode = tab.auth_mode;
        self.allow_private_network = tab.allow_private_network;
        self.dirty = tab.dirty;
        self.error = tab.error;
        self.set_shared_response(tab.response, tab.response_body, cx);
        self.response_request = tab.response_request;
        self.elapsed_ms = tab.elapsed_ms;
        self.pending_editor_hydration_changes += REQUEST_EDITOR_INPUTS;
        for (input, value) in self
            .request_editor_inputs()
            .into_iter()
            .zip(tab.input_values)
        {
            set_input(input.as_text(), &value, window, cx);
        }
    }

    fn request_tab_switch_is_blocked(&mut self, cx: &mut Context<Self>) -> bool {
        if self.send_state == SendState::Idle && (!self.storage_loading || self.ux.creation.busy())
        {
            return false;
        }
        self.navigation_notice =
            Some("Wait for the active request operation to finish before switching tabs.".into());
        cx.notify();
        true
    }

    fn activate_request_tab(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.request_tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        if index == self.active_request_tab || self.request_tab_switch_is_blocked(cx) {
            return;
        }
        self.capture_active_request_tab(cx);
        self.active_request_tab = index;
        self.restore_active_request_tab(window, cx);
        self.navigation_notice = None;
        cx.notify();
    }

    fn open_request_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.request_tab_switch_is_blocked(cx) {
            return;
        }
        self.capture_active_request_tab(cx);
        let id = self.next_request_tab_id;
        self.next_request_tab_id = self.next_request_tab_id.wrapping_add(1);
        self.request_tabs.push(RequestTabState::blank(
            id,
            self.current_collection_id.clone(),
        ));
        self.active_request_tab = self.request_tabs.len() - 1;
        self.restore_active_request_tab(window, cx);
        self.dirty = true;
        self.capture_active_request_tab(cx);
        cx.notify();
    }

    fn close_request_tab(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.close_request_tabs(&[id], None, window, cx);
    }

    /// Close the named drafts as one state transition. IDs, rather than indices,
    /// keep a context-menu action tied to the tab it was opened over.
    fn close_request_tabs(
        &mut self,
        ids: &[u64],
        preferred_active: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(first_closed_index) = self
            .request_tabs
            .iter()
            .position(|tab| ids.contains(&tab.id))
        else {
            return;
        };
        if self.request_tab_switch_is_blocked(cx) {
            return;
        }
        self.capture_active_request_tab(cx);
        let active_id = self.active_request_tab_id();
        self.request_tabs.retain(|tab| !ids.contains(&tab.id));
        self.ux.request_settings.retain(|id, _| !ids.contains(id));
        // Closing the last tab leaves none; the Compose area then shows the
        // empty state instead of a fresh blank tab.
        let remaining = self
            .request_tabs
            .iter()
            .map(|tab| tab.id)
            .collect::<Vec<_>>();
        self.active_request_tab = empty_state::active_after_close(
            &remaining,
            preferred_active.or(active_id),
            first_closed_index,
        )
        .unwrap_or(0);
        self.restore_active_request_tab(window, cx);
        self.navigation_notice = None;
        cx.notify();
    }

    fn request_tab_menu_subject(&self) -> Option<(u64, tab_menu::Subject)> {
        let id = self.right_clicked_request_tab?;
        let index = self.request_tabs.iter().position(|tab| tab.id == id)?;
        Some((
            id,
            tab_menu::Subject {
                index,
                total: self.request_tabs.len(),
            },
        ))
    }

    fn run_request_tab_action(
        &mut self,
        action: tab_menu::Action,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.request_tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        self.right_clicked_request_tab = None;
        match action {
            tab_menu::Action::Close => self.close_request_tabs(&[id], None, window, cx),
            tab_menu::Action::CloseOthers => {
                let ids = self
                    .request_tabs
                    .iter()
                    .filter(|tab| tab.id != id)
                    .map(|tab| tab.id)
                    .collect::<Vec<_>>();
                self.close_request_tabs(&ids, Some(id), window, cx);
            }
            tab_menu::Action::CloseToRight => {
                let ids = self.request_tabs[index + 1..]
                    .iter()
                    .map(|tab| tab.id)
                    .collect::<Vec<_>>();
                self.close_request_tabs(&ids, Some(id), window, cx);
            }
            tab_menu::Action::CloseAll => {
                let ids = self
                    .request_tabs
                    .iter()
                    .map(|tab| tab.id)
                    .collect::<Vec<_>>();
                self.close_request_tabs(&ids, None, window, cx);
            }
            tab_menu::Action::Duplicate => {
                if self.request_tab_switch_is_blocked(cx) {
                    return;
                }
                self.capture_active_request_tab(cx);
                let mut duplicate = self.request_tabs[index].clone();
                duplicate.id = self.next_request_tab_id;
                let settings = self
                    .ux
                    .request_settings
                    .get(&self.request_tabs[index].id)
                    .cloned()
                    .or_else(|| {
                        duplicate
                            .current_definition
                            .as_ref()
                            .map(|request| request.settings.clone())
                    });
                if let Some(settings) = settings {
                    self.ux.request_settings.insert(duplicate.id, settings);
                }
                self.next_request_tab_id = self.next_request_tab_id.wrapping_add(1);
                duplicate.current_definition = None;
                duplicate.current_request_id = None;
                duplicate.dirty = true;
                duplicate.error = None;
                duplicate.response = None;
                duplicate.response_request = None;
                duplicate.elapsed_ms = None;
                self.request_tabs.insert(index + 1, duplicate);
                self.active_request_tab = index + 1;
                self.restore_active_request_tab(window, cx);
                self.navigation_notice = None;
                cx.notify();
            }
        }
    }

    /// Every text input the request editor is made of — the ones
    /// [`Self::clear_request_editor`] blanks, `REQUEST_EDITOR_INPUTS` in number.
    fn request_editor_inputs(&self) -> [AnyInput<'_>; REQUEST_EDITOR_INPUTS] {
        [
            AnyInput::Line(&self.custom_method),
            AnyInput::Line(&self.request_name),
            AnyInput::Line(&self.url),
            AnyInput::Area(&self.params),
            AnyInput::Area(&self.headers),
            AnyInput::Area(&self.body),
            AnyInput::Area(&self.cookies),
            AnyInput::Area(&self.pre_request_script),
            AnyInput::Area(&self.authorization),
            AnyInput::Area(&self.variables),
            AnyInput::Area(&self.assertions),
        ]
    }

    /// A new request nobody has typed into yet. It is marked dirty so the
    /// tab shows it is unsaved, but there is nothing in it to lose.
    fn untouched_new_draft(&self, cx: &App) -> bool {
        self.current_definition.is_none()
            && self
                .request_editor_inputs()
                .iter()
                .all(|input| input.as_text().text(cx).trim().is_empty())
    }

    fn new_request(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.create_request_in_selection(window, cx);
    }

    fn new_collection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.refuse_dirty_switch(cx) {
            return;
        }
        let Some(data) = self.workspace_data.as_ref() else {
            return;
        };
        let collection = Collection {
            id: CollectionId::new(),
            workspace_id: data.workspace.clone(),
            name: format!("Collection {}", data.collections.len() + 1),
            description: String::new(),
            auth: AuthConfig::None,
            variables: Vec::new(),
            scripts: Scripts::default(),
            extensions: Default::default(),
        };
        if self.run_storage_command(
            coordinator::StorageCommand::UpsertCollection(collection.clone()),
            cx,
        ) {
            self.current_collection_id = Some(collection.id);
            self.clear_request_editor(window, cx);
            set_input(&self.container_name, &collection.name, window, cx);
            // With no request tab open there is no draft to mark unsaved.
            self.dirty = self.has_request_tab();
        }
        cx.notify();
    }

    fn delete_current_collection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.refuse_dirty_switch(cx) {
            return;
        }
        let Some(id) = self.current_collection_id.clone() else {
            return;
        };
        if self.run_storage_command(coordinator::StorageCommand::DeleteCollection(id), cx) {
            self.current_collection_id = None;
            self.clear_request_editor(window, cx);
        }
        cx.notify();
    }

    /// A rail row's "Delete request". Only deleting the open request is a
    /// switch the unsaved-changes guard has a say in.
    fn delete_request(
        &mut self,
        request_id: &RequestId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(collection_id) = self.workspace_data.as_ref().and_then(|data| {
            data.requests
                .iter()
                .find(|request| &request.id == request_id)
                .map(|request| request.collection_id.clone())
        }) else {
            return;
        };
        let open = self.current_request_id.as_ref() == Some(request_id);
        if open && self.refuse_dirty_switch(cx) {
            return;
        }
        if self.run_storage_command(
            coordinator::StorageCommand::DeleteRequest {
                collection_id,
                request_id: request_id.clone(),
            },
            cx,
        ) && open
        {
            self.clear_request_editor(window, cx);
        }
        cx.notify();
    }

    /// Make `id` the current collection, going through the same
    /// unsaved-changes guard a rail click does. `false` when the guard kept
    /// the old one, so a row menu's action stops instead of acting on it.
    fn focus_collection(
        &mut self,
        id: CollectionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.current_collection_id.as_ref() != Some(&id) {
            self.select_collection(id.clone(), window, cx);
        }
        self.current_collection_id.as_ref() == Some(&id)
    }

    /// Create a durable request in the chosen location and open its own tab.
    fn new_request_in(
        &mut self,
        collection: CollectionId,
        folder: Option<FolderId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.create_request_at(collection, folder, None, window, cx);
    }

    /// A collection or folder row's "New folder": created under `parent`
    /// and selected, so the next "New request here" lands in it. The open
    /// request is left where it is.
    fn new_folder_in(
        &mut self,
        collection: CollectionId,
        parent: Option<FolderId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.focus_collection(collection.clone(), window, cx) {
            return;
        }
        self.expand_folder_ancestors(parent.as_ref());
        if let Some(folder) = self.create_folder(None, collection, parent, window, cx) {
            self.selected_folder_id = Some(folder);
        }
        cx.notify();
    }

    /// The `+` menu's "New folder": a sibling of the open request's folder
    /// (in an implicit "My API" collection when there is none yet), which the
    /// open request moves into.
    fn new_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(data) = self.workspace_data.as_ref() else {
            return;
        };
        let implicit_collection = self.current_collection_id.is_none().then(|| Collection {
            id: CollectionId::new(),
            workspace_id: data.workspace.clone(),
            name: "My API".into(),
            description: String::new(),
            auth: AuthConfig::None,
            variables: Vec::new(),
            scripts: Scripts::default(),
            extensions: Default::default(),
        });
        let collection_id = match (
            self.current_collection_id.clone(),
            implicit_collection.as_ref(),
        ) {
            (Some(value), _) => value,
            (None, Some(collection)) => collection.id.clone(),
            (None, None) => return,
        };
        let parent = self
            .current_definition
            .as_ref()
            .and_then(|request| request.folder_id.clone());
        if let Some(folder) =
            self.create_folder(implicit_collection, collection_id, parent, window, cx)
            && let Some(request) = self.current_definition.as_mut()
        {
            request.folder_id = Some(folder);
            self.dirty = true;
        }
        cx.notify();
    }

    /// Persist a new "Folder N" and make its collection current; the id
    /// comes back once the store accepted it.
    fn create_folder(
        &mut self,
        implicit_collection: Option<Collection>,
        collection_id: CollectionId,
        parent_id: Option<FolderId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<FolderId> {
        let data = self.workspace_data.as_ref()?;
        let folder = Folder {
            id: FolderId::new(),
            collection_id,
            parent_id,
            name: format!("Folder {}", data.folders.len() + 1),
            auth: AuthConfig::Inherit,
            variables: Vec::new(),
            scripts: Scripts::default(),
            sort_key: data.folders.len() as i64,
            extensions: Default::default(),
        };
        self.run_storage_command(
            coordinator::StorageCommand::UpsertFolder {
                collection: implicit_collection,
                folder: folder.clone(),
            },
            cx,
        )
        .then(|| {
            self.current_collection_id = Some(folder.collection_id.clone());
            set_input(&self.container_name, &folder.name, window, cx);
            folder.id
        })
    }

    fn select_collection(&mut self, id: CollectionId, window: &mut Window, cx: &mut Context<Self>) {
        if self.request_tab_switch_is_blocked(cx) {
            return;
        }
        let Some(data) = self.workspace_data.as_ref() else {
            return;
        };
        let name = data
            .collections
            .iter()
            .find(|collection| collection.id == id)
            .map(|collection| collection.name.clone())
            .unwrap_or_default();
        let request = data
            .requests
            .iter()
            .find(|request| request.collection_id == id)
            .cloned();
        if self.current_collection_id.as_ref() != Some(&id) {
            // A folder or request scope names the previous collection's
            // items; carried over it would scope the next run to nothing.
            self.runner_folder_id = None;
            self.runner_request_ids.clear();
            self.runner_request_selection_active = false;
        }
        if let Some(request) = request {
            self.load_saved_request(&request, window, cx);
        } else {
            if self.current_collection_id.as_ref() != Some(&id)
                && (self.current_request_id.is_some() || !self.untouched_new_draft(cx))
            {
                self.open_request_tab(window, cx);
            }
            self.current_collection_id = Some(id);
            self.clear_request_editor(window, cx);
        }
        self.collection_tree_expanded = true;
        self.selected_folder_id = None;
        set_input(&self.container_name, &name, window, cx);
        self.capture_active_request_tab(cx);
        cx.notify();
    }

    /// Toggle the selected collection's visible rail tree. Choosing another
    /// collection still loads its first request, while closing this tree keeps
    /// that request selected in the composer.
    fn toggle_collection_tree(
        &mut self,
        id: CollectionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.current_collection_id.as_ref() == Some(&id) {
            self.collection_tree_expanded = !self.collection_tree_expanded;
            cx.notify();
        } else {
            self.select_collection(id, window, cx);
        }
    }

    fn toggle_folder_tree(&mut self, id: FolderId, cx: &mut Context<Self>) {
        if !self.collapsed_folder_ids.remove(&id) {
            self.collapsed_folder_ids.insert(id);
        }
        cx.notify();
    }

    fn set_collection_folders_expanded(
        &mut self,
        collection: &CollectionId,
        expanded: bool,
        cx: &mut Context<Self>,
    ) {
        if expanded && self.current_collection_id.as_ref() == Some(collection) {
            self.collection_tree_expanded = true;
        }
        if let Some(data) = &self.workspace_data {
            for folder in data
                .folders
                .iter()
                .filter(|folder| &folder.collection_id == collection)
            {
                if expanded {
                    self.collapsed_folder_ids.remove(&folder.id);
                } else {
                    self.collapsed_folder_ids.insert(folder.id.clone());
                }
            }
        }
        cx.notify();
    }

    fn expand_folder_ancestors(&mut self, folder: Option<&FolderId>) {
        let mut current = folder.cloned();
        let mut visited = HashSet::new();
        while let Some(id) = current {
            if !visited.insert(id.clone()) {
                break;
            }
            self.collapsed_folder_ids.remove(&id);
            current = self
                .workspace_data
                .as_ref()
                .and_then(|data| data.folders.iter().find(|folder| folder.id == id))
                .and_then(|folder| folder.parent_id.clone());
        }
    }

    fn select_folder(&mut self, id: FolderId, window: &mut Window, cx: &mut Context<Self>) {
        let name = self
            .workspace_data
            .as_ref()
            .and_then(|data| data.folders.iter().find(|folder| folder.id == id))
            .map(|folder| folder.name.clone())
            .unwrap_or_default();
        set_input(&self.container_name, &name, window, cx);
        self.selected_folder_id = Some(id);
        cx.notify();
    }

    /// The base URL relative requests are prefixed with right now: the Envs
    /// editor's text while it is being edited, else the active environment's
    /// saved one. Empty when neither has one.
    fn environment_base_url_value(&self, cx: &App) -> String {
        let live = self
            .environment_base_url
            .read(cx)
            .value()
            .trim()
            .to_string();
        if !live.is_empty() {
            return live;
        }
        self.active_environment()
            .map(|environment| environment.base_url.trim().to_string())
            .unwrap_or_default()
    }

    /// The active environment's contribution to a send: its saved record,
    /// the live editor text its variables come from, and the base URL and
    /// auth the Envs tab currently shows. The auth is parsed live like the
    /// variables are, then given the saved sign-in cache when it still
    /// describes the same sign-in.
    fn environment_send_context(&self, cx: &App) -> Result<EnvironmentSendContext, String> {
        let environment = self.active_environment().cloned();
        let scope = self
            .active_environment_id
            .as_ref()
            .map(|id| format!("environment.{}", id.as_str()))
            .unwrap_or_else(|| "environment.unsaved".into());
        let (mut auth, entered_secrets) = draft::parse_auth(
            self.environment_auth_mode,
            &self.environment_auth.read(cx).value(),
            &scope,
        )
        .map_err(|error| format!("Environment auth: {error}"))?;
        // Reloaded fields contain only vault references. Keep saved values
        // available to sign-in and OAuth while letting editor values override them.
        let mut secrets = draft::DraftSecrets::with_store(
            self.secret_store.clone(),
            self.bound_workspace.clone(),
        );
        secrets.merge(entered_secrets);
        if let Some(saved) = environment.as_ref() {
            preserve_saved_auth(
                &saved.auth,
                &mut auth,
                &secrets,
                &draft::DraftSecrets::with_store(
                    self.secret_store.clone(),
                    self.bound_workspace.clone(),
                ),
            );
        }
        Ok(EnvironmentSendContext {
            environment,
            scope,
            source: self.environment_variables.read(cx).value().to_string(),
            base_url: self.environment_base_url_value(cx),
            auth,
            secrets,
        })
    }

    /// The Envs tab's "Sign in now": run the environment's sign-in request
    /// and cache its value, reporting on the status line.
    fn sign_in_environment(&mut self, cx: &mut Context<Self>) {
        let context = match self.environment_send_context(cx) {
            Ok(context) => context,
            Err(error) => {
                self.environment_login_status = Some(error);
                cx.notify();
                return;
            }
        };
        let (variables, variable_secrets) =
            match draft::parse_session_variables(&context.source, &context.scope) {
                Ok(parsed) => parsed,
                Err(error) => {
                    self.environment_login_status = Some(error);
                    cx.notify();
                    return;
                }
            };
        let mut secrets = context.secrets;
        secrets.merge(variable_secrets);
        let Some(mut session) = login::LoginSession::for_environment(
            &context.auth,
            &context.scope,
            &variables,
            &context.base_url,
            &secrets,
        ) else {
            self.environment_login_status = Some(
                "Set an Auth URL in Basic or choose Sign-in request and fill in its URL.".into(),
            );
            cx.notify();
            return;
        };
        let Some(data) = self.workspace_data.as_ref() else {
            self.environment_login_status = Some("Workbench storage is unavailable".into());
            cx.notify();
            return;
        };
        let workspace = data.workspace.clone();
        let store = data.store.clone();
        let secret_store = self.secret_store.clone();
        let transport = self.transport.clone();
        let files = self.file_capabilities.clone();
        let saved = context.environment;
        self.environment_login_status = Some("Signing in…".into());
        self._login_work = Some(cx.spawn(async move |this, cx| {
            let result = crate::api::compat::blocking(move || {
                secrets.persist(secret_store.as_ref(), &workspace)?;
                let mut globals = store
                    .global_variables(&workspace)
                    .map_err(|e| e.to_string())?;
                globals.extend(session.environment.clone());
                session.environment = globals;
                session.sign_in(
                    &workspace,
                    secret_store.as_ref(),
                    transport.as_ref(),
                    &files,
                    now_seconds(),
                )?;
                let environment = session.updated_environment(saved.as_ref());
                if let Some(environment) = environment.as_ref() {
                    store
                        .upsert_environment(environment)
                        .map_err(|error| error.to_string())?;
                }
                Ok::<_, String>((session.auth, environment))
            })
            .await;
            let _ = this.update(cx, |panel, cx| {
                panel._login_work = None;
                match result {
                    Ok((auth, environment)) => {
                        panel.environment_login_status =
                            login::session_status(&auth, now_seconds());
                        if let Some(environment) = environment {
                            if let Some(existing) = panel.workspace_data.as_mut().and_then(|data| {
                                data.environments
                                    .iter_mut()
                                    .find(|saved| saved.id == environment.id)
                            }) {
                                *existing = environment;
                            }
                        } else {
                            panel.environment_login_status =
                                Some("Signed in · save the environment to keep the session".into());
                        }
                    }
                    Err(error) => panel.environment_login_status = Some(error),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Where an inheriting request's credentials come from — the Auth tab's
    /// "Inherit" hint, walking the same chain `effective_auth` does: folders
    /// deepest first, the collection, then the active environment.
    fn inherited_auth_source(&self) -> String {
        let Some(data) = self.workspace_data.as_ref() else {
            return "no credentials are sent".into();
        };
        let carries = |auth: &AuthConfig| !matches!(auth, AuthConfig::Inherit | AuthConfig::None);
        let mut folder_id = self.selected_folder();
        while let Some(id) = folder_id {
            let Some(folder) = data.folders.iter().find(|folder| folder.id == id) else {
                break;
            };
            if carries(&folder.auth) {
                return format!(
                    "folder {} · {}",
                    folder.name,
                    auth_mode(&folder.auth).label()
                );
            }
            folder_id = folder.parent_id.clone();
        }
        if let Some(collection) = self.current_collection_id.as_ref().and_then(|id| {
            data.collections
                .iter()
                .find(|collection| &collection.id == id)
        }) && carries(&collection.auth)
        {
            return format!(
                "collection {} · {}",
                collection.name,
                auth_mode(&collection.auth).label()
            );
        }
        if let Some(environment) = self.active_environment()
            && carries(&environment.auth)
        {
            return format!(
                "environment {} · {}",
                environment.name,
                auth_mode(&environment.auth).label()
            );
        }
        "no credentials are sent".into()
    }

    /// The folder Rename/Delete act on and `+ New request` files into: the
    /// rail's selection, else the open request's own folder.
    fn selected_folder(&self) -> Option<FolderId> {
        self.selected_folder_id.clone().or_else(|| {
            self.current_definition
                .as_ref()
                .and_then(|request| request.folder_id.clone())
        })
    }

    /// The `+` menu uses the same move operation as the rail and its menus.
    fn move_request_to_selected_folder(&mut self, cx: &mut Context<Self>) {
        let Some(folder_id) = self.selected_folder_id.clone() else {
            self.storage_error = Some("Select a folder in the rail first.".into());
            cx.notify();
            return;
        };
        if let Some(request) = self.current_request_id.clone() {
            let collection = self.workspace_data.as_ref().and_then(|data| {
                data.folders
                    .iter()
                    .find(|folder| folder.id == folder_id)
                    .map(|folder| folder.collection_id.clone())
            });
            if let Some(collection) = collection {
                self.move_saved_request(&request, &collection, Some(&folder_id), cx);
                return;
            }
        }
        match self.current_definition.as_mut() {
            Some(request) if request.folder_id.as_ref() != Some(&folder_id) => {
                request.folder_id = Some(folder_id);
                self.dirty = true;
            }
            Some(_) => {}
            None => {
                self.storage_error =
                    Some("Create or select a request before assigning a folder.".into());
            }
        }
        cx.notify();
    }

    fn delete_current_folder(&mut self, cx: &mut Context<Self>) {
        let Some(folder_id) = self.selected_folder() else {
            return;
        };
        let Some(collection_id) = self.workspace_data.as_ref().and_then(|data| {
            data.folders
                .iter()
                .find(|folder| folder.id == folder_id)
                .map(|folder| folder.collection_id.clone())
        }) else {
            return;
        };
        if self.run_storage_command(
            coordinator::StorageCommand::DeleteFolder {
                collection_id,
                folder_id: folder_id.clone(),
            },
            cx,
        ) {
            // The store detaches the folder's requests itself; mirror that
            // on the open definition so it still matches what is saved.
            if let Some(request) = self.current_definition.as_mut()
                && request.folder_id.as_ref() == Some(&folder_id)
            {
                request.folder_id = None;
            }
            self.selected_folder_id = None;
        }
        cx.notify();
    }

    fn refuse_dirty_switch(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.dirty || self.untouched_new_draft(cx) {
            return false;
        }
        let name = self.request_name.read(cx).value().trim().to_string();
        self.navigation_notice = Some(if name.is_empty() {
            "Unsaved changes in the open request — save or discard them first.".into()
        } else {
            format!("Unsaved changes in “{name}” — save or discard them first.")
        });
        cx.notify();
        true
    }

    fn load_pending_request(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(request_id) = self.pending_request_to_load.clone() else {
            return;
        };
        let request = self.workspace_data.as_ref().and_then(|data| {
            data.requests
                .iter()
                .find(|request| request.id == request_id)
                .cloned()
        });
        let Some(request) = request else {
            self.pending_request_to_load = None;
            self.storage_error = Some("Imported request is unavailable after commit.".into());
            cx.notify();
            return;
        };
        if self.current_request_id.as_ref() != Some(&request.id) && self.dirty {
            if self.navigation_notice.is_none() {
                self.refuse_dirty_switch(cx);
            }
            return;
        }
        self.pending_request_to_load = None;
        self.load_saved_request(&request, window, cx);
    }

    fn discard_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.active_request_tab_id() {
            self.ux.request_settings.remove(&id);
        }
        if let Some(request) = self.current_definition.clone() {
            self.load_saved_request(&request, window, cx);
        } else {
            self.clear_request_editor(window, cx);
        }
        self.dirty = false;
        self.navigation_notice = None;
        self.load_pending_request(window, cx);
        cx.notify();
    }

    fn set_method(
        &mut self,
        method: transport::Method,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.method == method {
            return;
        }
        self.method = method;
        self.dirty = true;
        // The selected method controls transmission, not ownership of the draft.
        let _ = window;
        self.error = None;
        cx.notify();
    }

    fn current_method(&self, cx: &App) -> String {
        if self.method == transport::Method::Custom {
            self.custom_method.read(cx).value().trim().to_string()
        } else {
            self.method.label().into()
        }
    }

    /// The collection the draft belongs to, creating the implicit "My API"
    /// collection when the workspace still has none.
    ///
    /// A first-run workspace hydrates with an empty collection list, so
    /// `current_collection_id` is `None` and every caller of
    /// [`Self::draft_request`] - send, save, "Acquire token", snippets and
    /// collection runs - refused before it looked at the request at all. The
    /// `+` menu's "New folder" already materialises this collection on
    /// demand; doing the same here is what makes a fresh install able to send
    /// anything. One `INSERT` against the already-open store, and a no-op as
    /// soon as a collection exists.
    fn ensure_current_collection(&mut self) -> Result<CollectionId, String> {
        if let Some(id) = self.current_collection_id.clone() {
            return Ok(id);
        }
        let id = self
            .workspace_data
            .as_mut()
            .ok_or_else(|| "Workbench storage is unavailable".to_string())?
            .ensure_collection()?;
        self.current_collection_id = Some(id.clone());
        Ok(id)
    }

    fn draft_request(&mut self, cx: &App) -> Result<(SavedRequest, draft::DraftSecrets), String> {
        let request_id = self.current_request_id.clone().unwrap_or_default();
        self.current_request_id = Some(request_id.clone());
        let collection_id = self.ensure_current_collection()?;
        let (mut request, secrets) = draft::build(draft::DraftInput {
            request_id: Some(request_id),
            collection_id,
            name: &self.request_name.read(cx).value(),
            method: &self.current_method(cx),
            url: &self.url.read(cx).value(),
            params: &self.params.read(cx).value(),
            headers: &self.headers.read(cx).value(),
            cookies: &self.cookies.read(cx).value(),
            body_mode: if self.method.forbids_body() {
                draft::BodyMode::None
            } else {
                self.body_mode
            },
            body: &self.body.read(cx).value(),
            auth_mode: self.auth_mode,
            auth: &self.authorization.read(cx).value(),
            variables: &self.variables.read(cx).value(),
            pre_request_script: &self.pre_request_script.read(cx).value(),
            test_script: &self.assertions.read(cx).value(),
            allow_private_network: self.allow_private_network,
        })?;
        if self.current_definition.is_none() {
            request.folder_id = self.selected_folder_id.clone();
        }
        if let Some(original) = self
            .current_definition
            .as_ref()
            .filter(|original| original.id == request.id)
        {
            request.folder_id = original.folder_id.clone();
            request.extensions = original.extensions.clone();
            request.sort_key = original.sort_key;
            let allow_private_network = request.settings.allow_private_network;
            request.settings = original.settings.clone();
            request.settings.allow_private_network = allow_private_network;
            // An imported auth the editor cannot express is kept as-is
            // rather than silently downgraded to "none".
            if matches!(original.auth, AuthConfig::Unsupported { .. })
                && self.auth_mode == draft::AuthMode::None
            {
                request.auth = original.auth.clone();
            } else {
                preserve_saved_auth(
                    &original.auth,
                    &mut request.auth,
                    &secrets,
                    &draft::DraftSecrets::with_store(
                        self.secret_store.clone(),
                        self.bound_workspace.clone(),
                    ),
                );
            }
        }
        if let Some(settings) = self
            .active_request_tab_id()
            .and_then(|id| self.ux.request_settings.get(&id))
        {
            request.settings = settings.clone();
            request.settings.allow_private_network = self.allow_private_network;
        }
        self.session_secrets.merge(secrets.clone());
        Ok((request, self.session_secrets.clone()))
    }

    fn save_clicked(&mut self, cx: &mut Context<Self>) {
        if !self.has_request_tab() {
            return;
        }
        if self.storage_loading {
            self.storage_error = Some("A Workbench storage operation is already running.".into());
            cx.notify();
            return;
        }
        let (request, secrets) = match self.draft_request(cx) {
            Ok(value) => value,
            Err(error) => {
                self.storage_error = Some(error);
                cx.notify();
                return;
            }
        };
        let Some(data) = self.workspace_data.as_ref() else {
            self.storage_error = Some("Workbench storage is unavailable".into());
            cx.notify();
            return;
        };
        let store = data.store.clone();
        let workspace = data.workspace.clone();
        let secret_store = self.secret_store.clone();
        self.storage_generation = self.storage_generation.wrapping_add(1);
        let generation = self.storage_generation;
        self.storage_loading = true;
        self.storage_error = None;
        self.navigation_notice = None;
        let saved_request = request.clone();
        let pending_collection = self.ux.creation.pending_collection(&request.collection_id);
        let completion_workspace = workspace.clone();
        self._storage_work = Some(cx.spawn(async move |this, cx| {
            let saved = crate::api::compat::blocking(move || {
                if let Some(collection) = pending_collection {
                    store
                        .upsert_collection(&collection)
                        .map_err(|error| error.to_string())?;
                }
                coordinator::execute(
                    store,
                    workspace,
                    coordinator::StorageCommand::UpsertRequest {
                        request,
                        secrets,
                        secret_store,
                    },
                )
            })
            .await;
            let _ = this.update(cx, |panel, cx| {
                if panel.storage_generation != generation
                    || panel.bound_workspace != completion_workspace
                {
                    return;
                }
                panel.storage_loading = false;
                panel._storage_work = None;
                match saved {
                    Ok(data) => {
                        panel
                            .ux
                            .creation
                            .collection_persisted(&saved_request.collection_id);
                        panel.workspace_data = Some(data);
                        panel.current_request_id = Some(saved_request.id.clone());
                        panel.current_collection_id = Some(saved_request.collection_id.clone());
                        panel.current_definition = Some(saved_request);
                        panel.dirty = false;
                        panel.storage_error = None;
                    }
                    Err(error) => panel.storage_error = Some(error),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn save_example(&mut self, cx: &mut Context<Self>) {
        let Some(request_id) = self.current_request_id.clone() else {
            self.storage_error = Some("Save the request before saving an example.".into());
            cx.notify();
            return;
        };
        let Some(entry) = self
            .history
            .iter()
            .find(|entry| entry.exchange.request_id.as_ref() == Some(&request_id))
        else {
            self.storage_error = Some("Send the request before saving an example.".into());
            cx.notify();
            return;
        };
        let exchange = &entry.exchange;
        let Some(response) = exchange.response.clone() else {
            self.storage_error = Some("The latest exchange has no response to save.".into());
            cx.notify();
            return;
        };
        let name = self.request_name.read(cx).value().to_string();
        let example = Example {
            id: ExampleId::new(),
            request_id,
            name: format!("{name} example"),
            request: Some(exchange.request.clone()),
            response,
            extensions: Default::default(),
            sort_key: 0,
        };
        self.run_storage_command(
            coordinator::StorageCommand::UpsertExample {
                example,
                redactions: entry.redactions.clone(),
            },
            cx,
        );
        cx.notify();
    }

    fn replay_example(&mut self, id: ExampleId, window: &mut Window, cx: &mut Context<Self>) {
        let example = self.workspace_data.as_ref().and_then(|data| {
            data.examples
                .iter()
                .find(|example| example.id == id)
                .cloned()
        });
        let Some(example) = example else {
            return;
        };
        if let Some(request) = self.workspace_data.as_ref().and_then(|data| {
            data.requests
                .iter()
                .find(|request| request.id == example.request_id)
                .cloned()
        }) {
            self.load_saved_request(&request, window, cx);
        }
        self.set_response(
            Some(transport::response_from_snapshot(&example.response)),
            cx,
        );
        self.response_request = example.request.clone();
        self.elapsed_ms = Some(example.response.duration_ms.into());
        self.tab = Tab::Compose;
        self.storage_error = None;
        cx.notify();
    }

    fn delete_example(&mut self, id: ExampleId, cx: &mut Context<Self>) {
        let request_id = self.workspace_data.as_ref().and_then(|data| {
            data.examples
                .iter()
                .find(|example| example.id == id)
                .map(|example| example.request_id.clone())
        });
        let Some(request_id) = request_id else {
            return;
        };
        self.run_storage_command(
            coordinator::StorageCommand::DeleteExample {
                request_id,
                example_id: id,
            },
            cx,
        );
        cx.notify();
    }

    /// Resolve a rail click against the current snapshot. Cloning the payload
    /// belongs to opening an editor, not constructing every row on each frame.
    fn open_saved_request(
        &mut self,
        request_id: &RequestId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let request = self.workspace_data.as_ref().and_then(|data| {
            data.requests
                .iter()
                .find(|request| &request.id == request_id)
                .cloned()
        });
        if let Some(request) = request {
            self.expand_folder_ancestors(request.folder_id.as_ref());
            self.load_saved_request(&request, window, cx);
        }
    }

    fn load_saved_request(
        &mut self,
        request: &SavedRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.current_request_id.as_ref() != Some(&request.id) {
            if let Some(tab) = self
                .request_tabs
                .iter()
                .find(|tab| tab.current_request_id.as_ref() == Some(&request.id))
            {
                self.activate_request_tab(tab.id, window, cx);
                return;
            }
            if self.request_tab_switch_is_blocked(cx) {
                return;
            }
            let initial_empty_tab =
                self.request_tabs.len() == 1 && !self.dirty && self.untouched_new_draft(cx);
            if !initial_empty_tab {
                self.capture_active_request_tab(cx);
                let id = self.next_request_tab_id;
                self.next_request_tab_id = self.next_request_tab_id.wrapping_add(1);
                self.request_tabs.push(RequestTabState::blank(
                    id,
                    Some(request.collection_id.clone()),
                ));
                self.active_request_tab = self.request_tabs.len() - 1;
            }
        }
        if let Some(id) = self.active_request_tab_id() {
            self.ux.request_settings.remove(&id);
        }
        self.current_request_id = Some(request.id.clone());
        self.current_collection_id = Some(request.collection_id.clone());
        self.current_definition = Some(request.clone());
        self.selected_folder_id = None;
        self.method = transport::Method::from_label(request.method.as_str())
            .unwrap_or(transport::Method::Custom);
        self.pending_editor_hydration_changes += REQUEST_EDITOR_INPUTS;
        set_input(&self.custom_method, request.method.as_str(), window, cx);
        set_input(&self.request_name, &request.name, window, cx);
        // The tab shows the start of the name, not the tail `set_value`
        // scrolls to.
        set_input(&self.url, &request.url, window, cx);
        set_input(&self.params, &format_rows(&request.params, '='), window, cx);
        let mut cookie = String::new();
        let headers = request
            .headers
            .iter()
            .filter_map(|row| {
                if row.key.eq_ignore_ascii_case("cookie") {
                    cookie = row.value.clone();
                    None
                } else {
                    Some(row.clone())
                }
            })
            .collect::<Vec<_>>();
        set_input(&self.headers, &format_rows(&headers, ':'), window, cx);
        set_input(&self.cookies, &cookie, window, cx);
        let (mode, body) = format_body(&request.body);
        self.body_mode = mode;
        set_input(&self.body, &body, window, cx);
        self.auth_mode = auth_mode(&request.auth);
        set_input(
            &self.authorization,
            &draft::format_auth(&request.auth),
            window,
            cx,
        );
        set_input(
            &self.variables,
            &format_variables(&request.variables),
            window,
            cx,
        );
        set_input(
            &self.pre_request_script,
            &request.scripts.pre_request,
            window,
            cx,
        );
        set_input(&self.assertions, &request.scripts.tests, window, cx);
        self.allow_private_network = request.settings.allow_private_network;
        self.dirty = false;
        self.error = None;
        self.navigation_notice = None;
        self.capture_active_request_tab(cx);
        cx.notify();
    }

    /// The Body tab's **Format**: re-indent a JSON payload in place. Only
    /// offered while the text parses and is not already formatted, so this
    /// never rewrites a body the user is mid-way through typing.
    fn format_body(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let source = self.body.read(cx).value().to_string();
        if let Some(formatted) = pretty::format_json(&source).filter(|f| *f != source) {
            set_input(&self.body, &formatted, window, cx);
            self.dirty = true;
            cx.notify();
        }
    }

    /// Send without the Production confirmation; see `send`.
    fn send_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // No open request tab: nothing to send, whatever triggered it.
        if !self.has_request_tab() {
            return;
        }
        self.ux.console_cleared_error = None;
        if self.bound_workspace != current_workspace_id() {
            self.error =
                Some("Save or discard changes before finishing the project switch.".into());
            cx.notify();
            return;
        }
        if self.send_state != SendState::Idle {
            return;
        }
        let (definition, secrets) = match self.draft_request(cx) {
            Ok(request) => request,
            Err(error) => {
                self.error = Some(error);
                self.set_response(None, cx);
                self.response_request = None;
                cx.notify();
                return;
            }
        };
        let Some(data) = self.workspace_data.as_ref() else {
            self.storage_error = Some("Workbench storage is unavailable".into());
            cx.notify();
            return;
        };
        let collection = data.collection(&definition.collection_id).cloned();
        let folders = folder_chain(&data.folders, &definition)
            .into_iter()
            .cloned()
            .collect();
        let workspace_id = data.workspace.clone();
        let terminal_store = data.store.clone();
        let operation_id = RequestId::new().as_str().to_string();
        let environment = match self.environment_send_context(cx) {
            Ok(environment) => environment,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        let mut secrets = secrets;
        secrets.merge(environment.secrets);
        let mut input = StandaloneSendInput {
            operation_id: operation_id.clone(),
            definition: definition.clone(),
            collection,
            folders,
            environment: environment.environment,
            environment_source: environment.source,
            environment_scope: environment.scope,
            environment_base_url: environment.base_url,
            environment_auth: environment.auth,
            browser_authorization: None,
            secrets,
            workspace: workspace_id.clone(),
            store: data.store.clone(),
            secret_store: self.secret_store.clone(),
            cookie_jar: self.cookie_jar.clone(),
            manual_cookies: match draft::parse_cookie_pairs(&self.cookies.read(cx).value()) {
                Ok(cookies) => cookies,
                Err(error) => {
                    self.error = Some(error);
                    cx.notify();
                    return;
                }
            },
            file_capabilities: self.file_capabilities.clone(),
            transport: self.transport.clone(),
        };
        let initial_environment_source = input.environment_source.clone();
        let initial_environment_variables = input
            .environment
            .as_ref()
            .map(|environment| environment.variables.clone())
            .unwrap_or_default();
        // A PKCE grant without a usable token is authorized as part of the
        // send: the browser opens now and the send waits for the callback.
        let browser_authorization = match plan_browser_authorization(&input, now_seconds()) {
            Ok(authorization) => authorization,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        if let Some(url) = browser_authorization
            .as_ref()
            .and_then(OAuthAcquisition::authorization_url)
        {
            cx.open_url(url);
        }
        input.browser_authorization =
            browser_authorization.map(OAuthAcquisition::into_browser_authorization);
        let exchange_id = ExchangeId::new();
        let started_at = now_millis();
        let started = Instant::now();
        self.send_generation = self.send_generation.wrapping_add(1);
        let generation = self.send_generation;
        self.send_state = SendState::Sending;
        self.error = input
            .browser_authorization
            .is_some()
            .then(|| "Authorize in the browser to finish the send…".to_string());
        self.set_response(None, cx);
        self.response_request = None;
        self.elapsed_ms = None;
        self.active_request_id = Some(operation_id);
        // History becomes cancelable only once the canonical compiler has
        // produced the exact redacted request. A cancel during preparation
        // therefore cannot persist a fabricated `<preparing>` exchange.
        self.pending_exchange = None;
        #[cfg(test)]
        let preparation_gate_delay = self.preparation_gate_delay;
        self._request_work = Some(cx.spawn_in(window, async move |this, cx| {
            #[cfg(test)]
            if let Some(delay) = preparation_gate_delay {
                cx.background_executor().timer(delay).await;
                let still_active = this
                    .update(cx, |panel, _| {
                        panel.send_generation == generation
                            && panel.active_request_id.as_deref()
                                == Some(input.operation_id.as_str())
                            && panel.send_state == SendState::Sending
                    })
                    .unwrap_or(false);
                if !still_active {
                    return;
                }
            }
            let prepared =
                crate::api::compat::blocking(move || prepare_standalone_send(input)).await;
            let prepared = match prepared {
                Ok(prepared) => prepared,
                Err(error) => {
                    let _ = this.update(cx, |panel, cx| {
                        if panel.send_generation == generation {
                            panel.send_state = SendState::Idle;
                            panel.active_request_id = None;
                            panel.pending_exchange = None;
                            panel.error = Some(error);
                            panel._request_work = None;
                            cx.notify();
                        }
                    });
                    return;
                }
            };
            let published = this
                .update(cx, |panel, cx| {
                    if panel.send_generation != generation
                        || panel.workspace_data.as_ref().map(|data| &data.workspace)
                            != Some(&workspace_id)
                    {
                        return false;
                    }
                    panel.pending_exchange = Some(PendingExchange {
                        exchange: Exchange {
                            console: Vec::new(),
                            test_results: Vec::new(),
                            id: exchange_id.clone(),
                            workspace_id: workspace_id.clone(),
                            request_id: Some(definition.id.clone()),
                            request: prepared.snapshot.clone(),
                            response: None,
                            error: None,
                            started_at,
                            completed_at: started_at,
                        },
                        redactions: prepared.prepared.redactions.clone(),
                    });
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !published {
                return;
            }
            let result =
                crate::api::compat::blocking(move || execute_standalone_send(prepared)).await;
            let elapsed_ms = started.elapsed().as_millis();
            let result = match result {
                Ok(result) => result,
                Err(error) => {
                    let _ = this.update(cx, |panel, cx| {
                        if panel.send_generation == generation {
                            panel.send_state = SendState::Idle;
                            panel.active_request_id = None;
                            panel.pending_exchange = None;
                            panel.error = Some(error);
                            panel._request_work = None;
                            cx.notify();
                        }
                    });
                    return;
                }
            };
            // Snapshotting, redaction, body presentation and persistence all
            // scale with response size. Keep that entire completion pipeline
            // off GPUI's foreground executor, then publish one compact state
            // transition below.
            let exchange_workspace = workspace_id.clone();
            let (
                response,
                prepared_body,
                response_error,
                safe_exchange,
                persisted,
                history_entry,
                result_definition,
                result_environment,
                result_collection,
                result_cookie_jar,
            ) = crate::api::compat::blocking(move || {
                let redactions = result.prepared.redactions;
                let mut exchange = Exchange {
                    console: result.console.clone(),
                    test_results: result
                        .test_results
                        .iter()
                        .map(|test| switchyard_api::TestResult {
                            name: test.name.clone(),
                            passed: test.passed,
                            skipped: test.skipped,
                            error: test.error.clone(),
                        })
                        .collect(),
                    id: exchange_id,
                    workspace_id: exchange_workspace,
                    request_id: Some(definition.id),
                    request: result.snapshot,
                    response: None,
                    error: None,
                    started_at,
                    completed_at: now_millis(),
                };
                let (response, response_error) = match result.response {
                    Ok(mut response) => {
                        response.test_results = result.test_results;
                        exchange.response = Some(transport::response_snapshot(&response));
                        (Some(response), None)
                    }
                    Err(error) => {
                        exchange.error = Some(error.clone());
                        (None, Some(error))
                    }
                };
                let prepared_body = response.as_ref().map(|response| {
                    let content_type = response
                        .headers
                        .iter()
                        .find(|(name, _)| name.eq_ignore_ascii_case("Content-Type"))
                        .map(|(_, value)| value.as_str());
                    pretty::prepare(content_type, &response.body, response.binary)
                });
                let safe_exchange = switchyard_api::redact_exchange(&exchange, &redactions);
                let mut history_entry = HistoryEntry::from_exchange(safe_exchange.clone());
                let response = response.or_else(|| Some((*history_entry.response).clone()));
                history_entry.elapsed_ms = elapsed_ms;
                history_entry.redactions = redactions.clone();
                let persisted = coordinator::persist_terminal(
                    terminal_store,
                    vec![coordinator::TerminalCommand::RecordExchange {
                        exchange,
                        redactions: redactions.clone(),
                    }],
                );
                (
                    response.map(Arc::new),
                    prepared_body,
                    response_error,
                    safe_exchange,
                    persisted,
                    history_entry,
                    result.definition,
                    result.environment,
                    result.collection,
                    result.cookie_jar,
                )
            })
            .await;
            let _ = this.update_in(cx, |panel, window, cx| {
                if panel.send_generation != generation
                    || panel.workspace_data.as_ref().map(|data| &data.workspace)
                        != Some(&workspace_id)
                {
                    return;
                }
                panel.send_state = SendState::Idle;
                panel.active_request_id = None;
                panel.pending_exchange = None;
                panel.set_shared_response(response, prepared_body, cx);
                panel.response_request = Some(safe_exchange.request.clone());
                panel.error = response_error;
                panel.elapsed_ms = panel.response.as_ref().map(|_| elapsed_ms);
                panel.cookie_jar = result_cookie_jar;
                panel.current_request_id = Some(result_definition.id.clone());
                panel.current_collection_id = Some(result_definition.collection_id.clone());
                panel.current_definition = Some(result_definition.clone());
                panel.dirty = false;
                if let Some(environment) = result_environment.as_ref().filter(|environment| {
                    panel.active_environment_id.as_ref() == Some(&environment.id)
                }) {
                    let current = panel.environment_variables.read(cx).value().to_string();
                    let merged = script_variables::merge_editor(
                        &initial_environment_source,
                        &current,
                        &initial_environment_variables,
                        &environment.variables,
                    );
                    if merged != current {
                        set_input(&panel.environment_variables, &merged, window, cx);
                    }
                }
                if let Some(data) = panel.workspace_data.as_mut() {
                    if let Some(collection) = result_collection
                        && let Some(existing) = data
                            .collections
                            .iter_mut()
                            .find(|saved| saved.id == collection.id)
                    {
                        *existing = collection;
                    }
                    if let Some(existing) = data
                        .requests
                        .iter_mut()
                        .find(|request| request.id == result_definition.id)
                    {
                        *existing = result_definition.clone();
                    } else {
                        data.requests.push(result_definition.clone());
                    }
                    if let Some(environment) = result_environment
                        && let Some(existing) = data
                            .environments
                            .iter_mut()
                            .find(|saved| saved.id == environment.id)
                    {
                        *existing = environment;
                    }
                }
                panel.history.insert(0, history_entry);
                if let Some(data) = panel.workspace_data.as_mut() {
                    data.history.insert(0, safe_exchange);
                    data.history.truncate(1_000);
                }
                panel.selected_history.clear();
                panel.diff_generation = panel.diff_generation.wrapping_add(1);
                panel.diff_result = None;
                panel._diff_work = None;
                if let Err(error) = persisted {
                    panel.storage_error =
                        Some(format!("Terminal exchange could not be persisted: {error}"));
                }
                panel._request_work = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// The Auth tab's "Acquire token": fetch the open request's OAuth 2
    /// token now (a PKCE grant opens the browser) and cache it on the
    /// saved request.
    fn acquire_oauth_token(&mut self, cx: &mut Context<Self>) {
        if self.storage_loading {
            self.navigation_notice =
                Some("Wait for the current storage operation before signing in.".into());
            cx.notify();
            return;
        }
        let (mut definition, mut secrets) = match self.draft_request(cx) {
            Ok(value) => value,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        let workspace = self.bound_workspace.clone();
        let store = match self.workspace_data.as_ref() {
            Some(data) => data.store.clone(),
            None => {
                self.storage_error = Some("Workbench storage is unavailable".into());
                cx.notify();
                return;
            }
        };
        let acquisition = match (|| {
            let context = self.environment_send_context(cx)?;
            let (environment, variable_secrets) =
                draft::parse_session_variables(&context.source, &context.scope)?;
            secrets.merge(context.secrets);
            secrets.merge(variable_secrets);
            let collection = self
                .workspace_data
                .as_ref()
                .and_then(|data| data.collection(&definition.collection_id));
            let folders = self
                .workspace_data
                .as_ref()
                .map(|data| folder_chain(&data.folders, &definition))
                .unwrap_or_default();
            let globals = store
                .global_variables(&workspace)
                .map_err(|e| e.to_string())?;
            let variables = OAuthVariables {
                globals: &globals,
                collection,
                folders: &folders,
                environment: &environment,
                base_url: Some(&context.base_url),
            };
            let headers = variables.headers(&definition.auth, &definition.variables, &secrets)?;
            let resolved_auth =
                variables.resolve_auth(&definition.auth, &definition.variables, &secrets)?;
            plan_oauth_acquisition(
                &resolved_auth,
                definition.settings.allow_private_network,
                headers,
            )
        })() {
            Ok(acquisition) => acquisition,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        if let Some(url) = acquisition.authorization_url() {
            cx.open_url(url);
        }
        let scope = format!("request.{}", definition.id.as_str());
        let secret_store = self.secret_store.clone();
        self.storage_generation = self.storage_generation.wrapping_add(1);
        let generation = self.storage_generation;
        self.storage_loading = true;
        self.error = Some("Completing OAuth authorization…".into());
        let completion_workspace = workspace.clone();
        self._storage_work = Some(cx.spawn(async move |this, cx| {
            let acquired = crate::api::compat::blocking(move || {
                complete_oauth_acquisition(
                    acquisition,
                    &mut definition.auth,
                    &scope,
                    &secrets,
                    &workspace,
                    secret_store.as_ref(),
                )?;
                store
                    .upsert_request(&definition)
                    .map_err(|error| error.to_string())?;
                let data = persistence::WorkspaceData::hydrate(store, workspace)?;
                Ok::<_, String>((definition, data))
            })
            .await;
            let _ = this.update(cx, |panel, cx| {
                if panel.storage_generation != generation
                    || panel.bound_workspace != completion_workspace
                {
                    return;
                }
                panel.storage_loading = false;
                panel._storage_work = None;
                match acquired {
                    Ok((definition, data)) => {
                        panel.workspace_data = Some(data);
                        panel.current_definition = Some(definition);
                        panel.error = Some("OAuth access token stored in the native vault.".into());
                        panel.dirty = false;
                    }
                    Err(error) => {
                        panel.error = Some(format!("OAuth authorization failed: {error}"))
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// The Envs tab's "Acquire token": fetch the environment's OAuth 2 token
    /// now and cache it on the saved environment, reporting on the status
    /// line. Client credentials are also fetched on demand by the first
    /// send; a PKCE grant needs this button and the browser.
    fn acquire_environment_oauth_token(&mut self, cx: &mut Context<Self>) {
        let context = match self.environment_send_context(cx) {
            Ok(context) => context,
            Err(error) => {
                self.environment_login_status = Some(error);
                cx.notify();
                return;
            }
        };
        let (environment, variable_secrets) =
            match draft::parse_session_variables(&context.source, &context.scope) {
                Ok(parsed) => parsed,
                Err(error) => {
                    self.environment_login_status = Some(error);
                    cx.notify();
                    return;
                }
            };
        let mut secrets = context.secrets;
        secrets.merge(variable_secrets);
        let acquisition = match (|| {
            let globals = self
                .workspace_data
                .as_ref()
                .ok_or("Workbench storage is unavailable")?
                .store
                .global_variables(&self.bound_workspace)
                .map_err(|e| e.to_string())?;
            let variables = OAuthVariables {
                globals: &globals,
                environment: &environment,
                base_url: Some(&context.base_url),
                ..Default::default()
            };
            let headers = variables.headers(&context.auth, &[], &secrets)?;
            let resolved_auth = variables.resolve_auth(&context.auth, &[], &secrets)?;
            plan_oauth_acquisition(&resolved_auth, false, headers)
        })() {
            Ok(acquisition) => acquisition,
            Err(error) => {
                self.environment_login_status = Some(error);
                cx.notify();
                return;
            }
        };
        let Some(data) = self.workspace_data.as_ref() else {
            self.environment_login_status = Some("Workbench storage is unavailable".into());
            cx.notify();
            return;
        };
        if let Some(url) = acquisition.authorization_url() {
            cx.open_url(url);
        }
        let workspace = data.workspace.clone();
        let store = data.store.clone();
        let secret_store = self.secret_store.clone();
        let saved = context.environment;
        let scope = context.scope;
        let mut auth = context.auth;
        self.environment_login_status = Some("Acquiring token…".into());
        self._login_work = Some(cx.spawn(async move |this, cx| {
            let result = crate::api::compat::blocking(move || {
                secrets.persist(secret_store.as_ref(), &workspace)?;
                complete_oauth_acquisition(
                    acquisition,
                    &mut auth,
                    &scope,
                    &secrets,
                    &workspace,
                    secret_store.as_ref(),
                )?;
                let environment = saved
                    .filter(|saved| same_managed_auth_source(&saved.auth, &auth))
                    .map(|saved| Environment {
                        auth: auth.clone(),
                        ..saved
                    });
                if let Some(environment) = environment.as_ref() {
                    store
                        .upsert_environment(environment)
                        .map_err(|error| error.to_string())?;
                }
                Ok::<_, String>((auth, environment))
            })
            .await;
            let _ = this.update(cx, |panel, cx| {
                panel._login_work = None;
                match result {
                    Ok((auth, environment)) => {
                        panel.environment_login_status =
                            login::session_status(&auth, now_seconds());
                        if let Some(environment) = environment {
                            if let Some(existing) = panel.workspace_data.as_mut().and_then(|data| {
                                data.environments
                                    .iter_mut()
                                    .find(|saved| saved.id == environment.id)
                            }) {
                                *existing = environment;
                            }
                        } else {
                            panel.environment_login_status =
                                Some("Token stored · save the environment to keep it".into());
                        }
                    }
                    Err(error) => {
                        panel.environment_login_status =
                            Some(format!("OAuth authorization failed: {error}"))
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn choose_upload_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let body_mode = self.body_mode;
        let body_source = self.body.read(cx).value().to_string();
        let request_id = self
            .current_request_id
            .get_or_insert_with(RequestId::new)
            .clone();
        let slot_count = match draft::upload_slot_count(body_mode, &body_source) {
            Ok(0) => {
                self.error = Some("This body does not contain any file upload fields.".into());
                cx.notify();
                return;
            }
            Ok(count) => count,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: slot_count > 1,
            prompt: Some(
                format!(
                    "Choose {slot_count} upload file{}",
                    if slot_count == 1 { "" } else { "s" }
                )
                .into(),
            ),
        });
        let capabilities = self.file_capabilities.clone();
        self.error = Some("Waiting for native file selection…".into());
        cx.spawn_in(window, async move |this, cx| {
            let paths = match chosen.await {
                Ok(Ok(Some(paths))) => paths,
                Ok(Ok(None)) | Err(_) => return,
                Ok(Err(error)) => {
                    let _ = this.update(cx, |panel, cx| {
                        panel.error = Some(format!("Could not open the file picker: {error}"));
                        cx.notify();
                    });
                    return;
                }
            };
            let result = crate::api::compat::blocking(move || {
                if paths.len() != slot_count {
                    return Err(format!(
                        "choose exactly {slot_count} file{} for this body",
                        if slot_count == 1 { "" } else { "s" }
                    ));
                }
                paths
                    .into_iter()
                    .enumerate()
                    .map(|(slot, path)| {
                        capabilities.issue_picker_selection(path, &request_id, slot)
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .await;
            let _ = this.update_in(cx, |panel, _window, cx| {
                match result {
                    Ok(grants) => {
                        panel.error = Some(format!(
                            "Selected {} for one send.",
                            grants
                                .iter()
                                .map(|grant| grant.filename.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                    Err(error) => panel.error = Some(format!("File selection failed: {error}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Compile the draft into its redacted snapshot off the frame thread.
    ///
    /// Resolve the same environment, folder chain and secrets as a send, but
    /// compile only a redacted preview. This keeps snippets and Chat usable
    /// when a managed credential needs refreshing while preserving all static
    /// request validation. `Err` is the draft being incomplete (no URL, no
    /// name); the task's `Err` is a compile failure.
    #[allow(clippy::type_complexity)]
    fn compile_redacted_snapshot(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Result<
        std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<RedactedRequestSnapshot, String>>>,
        >,
        String,
    > {
        let (request, secrets) = self.draft_request(cx)?;
        let environment_scope = self
            .active_environment_id
            .as_ref()
            .map(|id| format!("environment.{}", id.as_str()))
            .unwrap_or_else(|| "environment.unsaved".into());
        let environment_source = self.environment_variables.read(cx).value().to_string();
        let environment_base_url = self.environment_base_url_value(cx);
        let environment_auth = self
            .active_environment()
            .map(|environment| environment.auth.clone())
            .unwrap_or_default();
        let collection = self
            .workspace_data
            .as_ref()
            .and_then(|data| data.collection(&request.collection_id))
            .cloned();
        let folders = self
            .workspace_data
            .as_ref()
            .map(|data| {
                folder_chain(&data.folders, &request)
                    .into_iter()
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let globals_store = self
            .workspace_data
            .as_ref()
            .map(|data| (data.store.clone(), data.workspace.clone()));
        Ok(Box::pin(crate::api::compat::blocking(move || {
            let globals = match globals_store {
                Some((store, workspace)) => store
                    .global_variables(&workspace)
                    .map_err(|error| error.to_string())?,
                None => Vec::new(),
            };
            let (environment, environment_secrets) =
                draft::parse_session_variables(&environment_source, &environment_scope)?;
            let mut secrets = secrets;
            secrets.merge(environment_secrets);
            let folder_refs = folders.iter().collect::<Vec<_>>();
            let context = switchyard_api::CompileContext {
                global: &globals,
                environment: &environment,
                data: &[],
                local: &[],
                secrets: &secrets,
                environment_base_url: Some(&environment_base_url),
                environment_auth: Some(&environment_auth),
            };
            compile_redacted_request_with_folder_chain(
                &request,
                collection.as_ref(),
                &folder_refs,
                &context,
            )
            .map_err(|error| error.to_string())
        })))
    }

    fn generate_snippet(&mut self, cx: &mut Context<Self>) {
        let compile = match self.compile_redacted_snapshot(cx) {
            Ok(task) => task,
            Err(error) => {
                self.snippet_output = Some(format!(
                    "Complete the request to generate a snippet:\n{error}"
                ));
                cx.notify();
                return;
            }
        };
        let language = self.snippet_language;
        self.snippet_generation = self.snippet_generation.wrapping_add(1);
        let generation = self.snippet_generation;
        self.snippet_output = Some("Generating redacted snippet…".into());
        self._snippet_work = Some(cx.spawn(async move |this, cx| {
            let result = compile
                .await
                .map(|snapshot| switchyard_api::generate_snippet(language, &snapshot));
            let _ = this.update(cx, |panel, cx| {
                if panel.snippet_generation != generation {
                    return;
                }
                panel.snippet_output = Some(result.unwrap_or_else(|error| {
                    format!("Complete the request to generate a snippet:\n{error}")
                }));
                panel._snippet_work = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Hand the request — and its response, when the intent wants one — to
    /// the Chat composer. See [`assist`] for what may cross that boundary.
    ///
    /// Response intents pair the shown response with the redacted request that
    /// produced it (`response_request`), so no re-compilation is needed and the
    /// prompt is exactly what History shows. The review intent — and an example
    /// that kept no request snapshot — compiles the draft afresh, off the frame
    /// thread.
    fn ask_agent(&mut self, intent: assist::AssistIntent, cx: &mut Context<Self>) {
        let response = intent
            .needs_response()
            .then(|| self.response.clone())
            .flatten();
        if intent.needs_response() && response.is_none() {
            self.navigation_notice =
                Some("Send the request before asking about its response.".into());
            cx.notify();
            return;
        }
        let error = self.error.clone();
        let sections = self.assist_sections(intent, cx);
        if !intent.needs_request() {
            let prompt = assist::compose(
                intent,
                &assist::AssistContext {
                    request: None,
                    response: None,
                    error: None,
                    sections,
                },
            );
            cx.emit(AskAgentRequested { prompt });
            return;
        }
        if let Some(request) = self
            .response_request
            .as_ref()
            .filter(|_| response.is_some())
        {
            let prompt = assist::compose(
                intent,
                &assist::AssistContext {
                    request: Some(request),
                    response: response.as_deref(),
                    error: error.as_deref(),
                    sections,
                },
            );
            cx.emit(AskAgentRequested { prompt });
            return;
        }
        let compile = match self.compile_redacted_snapshot(cx) {
            Ok(task) => task,
            Err(error) => {
                self.navigation_notice =
                    Some(format!("Complete the request before asking AI: {error}"));
                cx.notify();
                return;
            }
        };
        self.assist_generation = self.assist_generation.wrapping_add(1);
        let generation = self.assist_generation;
        self._assist_work = Some(cx.spawn(async move |this, cx| {
            let result = compile.await;
            let _ = this.update(cx, |panel, cx| {
                if panel.assist_generation != generation {
                    return;
                }
                panel._assist_work = None;
                match result {
                    Ok(snapshot) => {
                        let prompt = assist::compose(
                            intent,
                            &assist::AssistContext {
                                request: Some(&snapshot),
                                response: response.as_deref(),
                                error: error.as_deref(),
                                sections,
                            },
                        );
                        cx.emit(AskAgentRequested { prompt });
                    }
                    Err(error) => {
                        panel.navigation_notice =
                            Some(format!("Complete the request before asking AI: {error}"));
                        cx.notify();
                    }
                }
            });
        }));
    }

    /// The extra prompt sections an intent carries. Only workspace metadata
    /// — names, keys, counts, the scenario prose — never a resolved value.
    fn assist_sections(
        &self,
        intent: assist::AssistIntent,
        cx: &App,
    ) -> Vec<(&'static str, String)> {
        let mut sections = Vec::new();
        match intent {
            assist::AssistIntent::GenerateBody => {
                sections.push((
                    "Body mode",
                    format!(
                        "{} · validated against: none (no schema attached)",
                        self.body_mode.label()
                    ),
                ));
            }
            assist::AssistIntent::GenerateData => {
                let scenario = self.data_prompt.read(cx).value().trim().to_string();
                let presets = self
                    .data_presets
                    .iter()
                    .filter_map(|index| view::DATA_PRESETS.get(*index))
                    .copied()
                    .collect::<Vec<_>>()
                    .join(", ");
                sections.push((
                    "Scenario",
                    if scenario.is_empty() {
                        "(not described)".into()
                    } else {
                        scenario
                    },
                ));
                if !presets.is_empty() {
                    sections.push(("Presets", presets));
                }
                let mut constraints = Vec::new();
                if self.data_schema_valid {
                    constraints.push("schema-valid values");
                }
                if self.data_unique_keys {
                    constraints.push("unique keys per row");
                }
                if self.stop_on_error {
                    constraints.push("stop on first error");
                }
                sections.push((
                    "Constraints",
                    if constraints.is_empty() {
                        "none".into()
                    } else {
                        constraints.join(", ")
                    },
                ));
                let rows = self.runner_iterations.read(cx).value().trim().to_string();
                let seed = self.runner_seed.read(cx).value().trim().to_string();
                sections.push((
                    "Shape",
                    format!(
                        "rows: {} · seed: {} · format: JSON array of objects (one object per iteration)",
                        if rows.is_empty() { "all data rows" } else { &rows },
                        if seed.is_empty() { "auto" } else { &seed },
                    ),
                ));
                let existing = self
                    .data_source
                    .read(cx)
                    .value()
                    .lines()
                    .take(6)
                    .collect::<Vec<_>>()
                    .join("\n");
                if !existing.trim().is_empty() {
                    sections.push(("Existing data (first lines)", existing));
                }
            }
            assist::AssistIntent::FillEnvironment => {
                let environment = self.active_environment();
                let requests = self
                    .workspace_data
                    .as_ref()
                    .map(|data| data.requests.clone())
                    .unwrap_or_default();
                let defined = environment
                    .map(|env| {
                        env.variables
                            .iter()
                            .map(|v| v.key.clone())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let mut missing = view::variable_references(&requests)
                    .into_iter()
                    .filter(|name| !defined.contains(name))
                    .collect::<Vec<_>>();
                missing.sort();
                sections.push((
                    "Environment",
                    environment
                        .map(|env| env.name.clone())
                        .unwrap_or_else(|| "(unsaved)".into()),
                ));
                sections.push((
                    "Defined keys",
                    if defined.is_empty() {
                        "none".into()
                    } else {
                        defined.join(", ")
                    },
                ));
                sections.push((
                    "Referenced but undefined",
                    if missing.is_empty() {
                        "none".into()
                    } else {
                        missing.join(", ")
                    },
                ));
                let urls = requests
                    .iter()
                    .map(|request| format!("{} {}", request.method.as_str(), request.url))
                    .take(40)
                    .collect::<Vec<_>>()
                    .join("\n");
                if !urls.is_empty() {
                    sections.push(("Requests", urls));
                }
            }
            assist::AssistIntent::ReviewImport => {
                let Some(imported) = self.staged_import.as_ref() else {
                    sections.push(("Import", "Nothing is staged.".into()));
                    return sections;
                };
                sections.push((
                    "Source",
                    format!(
                        "{} · {}",
                        view::import_format_label(&imported.format),
                        self.import_origin
                            .clone()
                            .unwrap_or_else(|| "pasted text".into())
                    ),
                ));
                sections.push((
                    "Collection",
                    format!(
                        "{} · {} folders · {} requests · {} environments · {} examples",
                        imported.collection.name,
                        imported.folders.len(),
                        imported.requests.len(),
                        imported.environments.len(),
                        imported.examples.len()
                    ),
                ));
                let requests = imported
                    .requests
                    .iter()
                    .map(|request| format!("{} {}", request.method.as_str(), request.url))
                    .take(80)
                    .collect::<Vec<_>>()
                    .join("\n");
                if !requests.is_empty() {
                    sections.push(("Requests", requests));
                }
                let environments = imported
                    .environments
                    .iter()
                    .map(|env| {
                        format!(
                            "{}: {}",
                            env.name,
                            env.variables
                                .iter()
                                .map(|v| v.key.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !environments.is_empty() {
                    sections.push(("Environment keys", environments));
                }
                if !imported.warnings.is_empty() {
                    sections.push(("Parser warnings", imported.warnings.join("\n")));
                }
            }
            _ => {}
        }
        sections
    }

    /// Reload the bound workspace from the store. A dirty draft is kept;
    /// the reload is refused with a notice instead of losing edits.
    fn sync_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dirty {
            self.navigation_notice =
                Some("Save or discard the open request before syncing the project.".into());
            cx.notify();
            return;
        }
        let workspace = self.bound_workspace.clone();
        self.start_workspace_hydration(workspace, window, cx);
    }

    /// Clone the active environment into a new, unsaved draft named
    /// "<name> copy"; Save persists it.
    fn duplicate_environment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(environment) = self.active_environment().cloned() else {
            self.navigation_notice = Some("Select an environment to duplicate.".into());
            cx.notify();
            return;
        };
        self.active_environment_id = None;
        let copy = Environment {
            name: format!("{} copy", environment.name),
            ..environment
        };
        self.load_environment_editor(Some(&copy), window, cx);
        self.storage_error = None;
        cx.notify();
    }

    /// Copy the selected data row (the first when none is selected) into the
    /// body editor as a JSON object and switch to Compose → Body.
    fn use_row_as_body(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let source = self.data_source.read(cx).value().to_string();
        let rows = match draft::parse_data_rows(&source) {
            Ok(rows) => rows,
            Err(error) => {
                self.run_status = Some(format!("Data is not parseable: {error}"));
                cx.notify();
                return;
            }
        };
        let index = self.data_selected_row.unwrap_or(0);
        let Some(row) = rows.get(index) else {
            self.run_status = Some("Add at least one data row first.".into());
            cx.notify();
            return;
        };
        let object = row
            .iter()
            .map(|variable| {
                // Columns the table masks stay `{{placeholders}}` the runner
                // fills per iteration, so a credential never lands in the body.
                let value = match &variable.value {
                    VariableValue::Plain(value) if !view::sensitive_column(&variable.key) => {
                        serde_json::Value::String(value.clone())
                    }
                    _ => serde_json::Value::String(format!("{{{{{}}}}}", variable.key)),
                };
                (variable.key.clone(), value)
            })
            .collect::<serde_json::Map<_, _>>();
        let body =
            serde_json::to_string_pretty(&serde_json::Value::Object(object)).unwrap_or_default();
        self.body_mode = draft::BodyMode::Json;
        set_input(&self.body, &body, window, cx);
        self.dirty = true;
        self.tab = Tab::Compose;
        self.composer_tab = ComposerTab::Body;
        self.run_status = Some(format!("Row {} copied into the request body.", index + 1));
        cx.notify();
    }

    fn export_collection(&mut self, postman: bool, cx: &mut Context<Self>) {
        let collection = self.current_collection_id.clone();
        self.export_scope(collection, None, postman, false, cx);
    }

    /// A collection or folder row's "Export": prepare the redacted artifact
    /// for that collection (or just `folder`'s subtree), then ask where to
    /// save it.
    fn export_from_rail(
        &mut self,
        collection: CollectionId,
        folder: Option<FolderId>,
        postman: bool,
        cx: &mut Context<Self>,
    ) {
        self.export_scope(Some(collection), folder, postman, true, cx);
    }

    fn export_scope(
        &mut self,
        collection_id: Option<CollectionId>,
        folder_id: Option<FolderId>,
        postman: bool,
        save_when_ready: bool,
        cx: &mut Context<Self>,
    ) {
        self.tab = Tab::Compose;
        self.composer_tab = ComposerTab::Snippet;
        self.export_generation = self.export_generation.wrapping_add(1);
        let generation = self.export_generation;
        self.prepared_export = None;
        self._export_work = None;
        if self.bound_workspace != current_workspace_id() {
            self.export_output =
                Some("Save or discard changes before exporting after the project switch.".into());
            cx.notify();
            return;
        }
        if self.storage_loading {
            self.export_output =
                Some("Wait for Workbench storage to finish loading before exporting.".into());
            cx.notify();
            return;
        }
        let Some(data) = self.workspace_data.as_ref() else {
            self.export_output = Some("Workbench storage is unavailable".into());
            cx.notify();
            return;
        };
        if data.workspace != self.bound_workspace {
            self.export_output =
                Some("Workbench storage has not finished loading for this project.".into());
            cx.notify();
            return;
        }
        let Some(collection_id) = collection_id else {
            self.export_output = Some("Select a collection before exporting.".into());
            cx.notify();
            return;
        };
        let Some(collection) = data.collection(&collection_id).cloned() else {
            self.export_output = Some("The selected collection no longer exists.".into());
            cx.notify();
            return;
        };
        let workspace = data.workspace.clone();
        let root_folder = match &folder_id {
            Some(id) => match data
                .folders
                .iter()
                .find(|folder| &folder.id == id && folder.collection_id == collection_id)
            {
                Some(folder) => Some(folder.clone()),
                None => {
                    self.export_output = Some("The selected folder no longer exists.".into());
                    cx.notify();
                    return;
                }
            },
            None => None,
        };
        let in_scope = |folder: &FolderId| {
            root_folder
                .as_ref()
                .is_none_or(|root| folder_is_within(folder, &root.id, &data.folders))
        };
        let folders = data
            .folders
            .iter()
            .filter(|folder| folder.collection_id == collection_id && in_scope(&folder.id))
            .map(|folder| {
                let mut folder = folder.clone();
                // The exported folder becomes a top-level folder of the
                // artifact; its parent is not part of the export.
                if root_folder
                    .as_ref()
                    .is_some_and(|root| root.id == folder.id)
                {
                    folder.parent_id = None;
                }
                folder
            })
            .collect::<Vec<_>>();
        let requests = data
            .requests
            .iter()
            .filter(|request| {
                request.collection_id == collection_id
                    && (root_folder.is_none() || request.folder_id.as_ref().is_some_and(&in_scope))
            })
            .cloned()
            .collect::<Vec<_>>();
        let request_ids = requests
            .iter()
            .map(|request| request.id.clone())
            .collect::<HashSet<_>>();
        let environments = data.environments.clone();
        let examples = data
            .examples
            .iter()
            .filter(|example| request_ids.contains(&example.request_id))
            .cloned()
            .collect::<Vec<_>>();
        let secrets = self.session_secrets.clone();
        let base_name = export_file_stem(
            root_folder
                .as_ref()
                .map_or(collection.name.as_str(), |folder| folder.name.as_str()),
        );
        let suggested_name = if postman {
            format!("{base_name}.postman_collection.json")
        } else {
            format!("{base_name}.agentops.json")
        };
        let completion_workspace = workspace.clone();
        self.export_output = Some("Preparing redacted export…".into());
        self._export_work = Some(cx.spawn(async move |this, cx| {
            let result = crate::api::compat::blocking(move || {
                // Postman omits environments from its artifact, but every
                // workspace environment can contribute a credential that
                // must be recognized in captured examples and extensions.
                let prepared = switchyard_api::PreparedCollectionExport::new(
                    &collection,
                    &folders,
                    &requests,
                    &environments,
                    &examples,
                    &secrets,
                )?;
                if postman {
                    switchyard_api::export_prepared_postman_collection(&prepared)
                } else {
                    switchyard_api::export_prepared_agentops_bundle(&prepared)
                }
            })
            .await;
            let _ = this.update(cx, |panel, cx| {
                let current_workspace = current_workspace_id();
                if panel.apply_export_result(
                    &completion_workspace,
                    generation,
                    &current_workspace,
                    suggested_name,
                    result,
                ) {
                    if save_when_ready && panel.prepared_export.is_some() {
                        panel.save_prepared_export(cx);
                    }
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.send_state == SendState::PreparingRun {
            self.send_generation = self.send_generation.wrapping_add(1);
            self.send_state = SendState::Idle;
            self.active_request_id = None;
            self.active_run = None;
            self.run_status = Some("Run canceled before its first request was sent.".into());
            self._request_work = None;
            cx.notify();
            return;
        }
        let Some((request_id, generation)) = self.begin_cancel() else {
            return;
        };
        let workbench_transport = self.transport.clone();
        cx.spawn(async move |this, cx| {
            let cancel_request_id = request_id.clone();
            let result = crate::api::compat::blocking(move || {
                workbench_transport.cancel(&cancel_request_id)
            })
            .await;
            let _ = this.update(cx, |panel, cx| {
                if panel.send_generation != generation
                    || panel.active_request_id.as_deref() != Some(request_id.as_str())
                {
                    return;
                }
                panel.reconcile_cancel(result, cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn begin_cancel(&mut self) -> Option<(String, u64)> {
        if self.send_state != SendState::Sending {
            return None;
        }
        let request_id = self.active_request_id.clone()?;
        self.send_state = SendState::Cancelling;
        self.error = Some("Cancelling upstream request…".into());
        Some((request_id, self.send_generation))
    }

    fn confirm_cancel(&mut self, cx: &mut Context<Self>) {
        self.send_generation = self.send_generation.wrapping_add(1);
        let generation = self.send_generation;
        self.active_request_id = None;
        self.error = Some("Persisting cancelled outcome…".into());
        let pending = self.pending_exchange.take().map(|mut pending| {
            pending.exchange.completed_at = now_millis();
            pending.exchange.error = Some("Request cancelled.".into());
            pending
        });
        let run = self.active_run.take().map(|mut run| {
            run.status = RunStatus::Canceled;
            run.completed_at = Some(now_millis());
            run
        });
        let run_redactions = std::mem::take(&mut self.active_run_redactions);
        let Some(data) = self.workspace_data.as_ref() else {
            self.send_state = SendState::Idle;
            self.error = Some("Request cancelled, but Workbench storage is unavailable.".into());
            return;
        };
        let store = data.store.clone();
        let workspace = data.workspace.clone();
        let mut commands = Vec::new();
        if let Some(pending) = &pending {
            commands.push(coordinator::TerminalCommand::RecordExchange {
                exchange: pending.exchange.clone(),
                redactions: pending.redactions.clone(),
            });
        }
        if let Some(run) = &run {
            commands.push(coordinator::TerminalCommand::UpsertRun {
                run: run.clone(),
                redactions: run_redactions,
            });
        }
        self._request_work = Some(cx.spawn(async move |this, cx| {
            let persisted = crate::api::compat::blocking(move || {
                coordinator::persist_terminal(store, commands)
            })
            .await;
            let _ = this.update(cx, |panel, cx| {
                if panel.send_generation != generation || panel.bound_workspace != workspace {
                    return;
                }
                if let Some(pending) = pending {
                    let safe =
                        switchyard_api::redact_exchange(&pending.exchange, &pending.redactions);
                    let mut entry = HistoryEntry::from_exchange(safe.clone());
                    entry.elapsed_ms = pending
                        .exchange
                        .completed_at
                        .saturating_sub(pending.exchange.started_at)
                        as u128;
                    entry.redactions = pending.redactions;
                    panel.history.insert(0, entry);
                    if let Some(data) = panel.workspace_data.as_mut() {
                        data.history.insert(0, safe);
                        data.history.truncate(1_000);
                    }
                }
                if let Some(run) = run {
                    panel.run_status = Some(format!(
                        "Run canceled after {} completed items.",
                        run.item_results.len()
                    ));
                    if let Some(data) = panel.workspace_data.as_mut() {
                        data.apply_run_snapshot(run);
                    }
                }
                panel.send_state = SendState::Idle;
                panel.error = Some("Request cancelled.".into());
                if let Err(error) = persisted {
                    panel.storage_error =
                        Some(format!("Cancelled outcome could not be persisted: {error}"));
                }
                panel._request_work = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn reconcile_cancel(&mut self, result: Result<bool, String>, cx: &mut Context<Self>) {
        match result {
            Ok(true) => self.confirm_cancel(cx),
            Ok(false) => {
                self.send_state = SendState::Sending;
                self.error =
                    Some("Cancellation was not confirmed; waiting for the request result.".into());
            }
            Err(error) => {
                self.send_state = SendState::Sending;
                self.error = Some(format!(
                    "Cancel failed; the request is still running: {error}"
                ));
            }
        }
    }

    fn toggle_history(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(position) = self
            .selected_history
            .iter()
            .position(|selected| *selected == index)
        {
            self.selected_history.remove(position);
        } else {
            if self.selected_history.len() == 2 {
                self.selected_history.remove(0);
            }
            self.selected_history.push(index);
        }
        self.schedule_diff(cx);
        cx.notify();
    }

    fn schedule_diff(&mut self, cx: &mut Context<Self>) {
        self.diff_generation = self.diff_generation.wrapping_add(1);
        let generation = self.diff_generation;
        self.diff_result = None;
        self._diff_work = None;
        let entries = self
            .selected_history
            .iter()
            .filter_map(|index| self.history.get(*index))
            .collect::<Vec<_>>();
        let [before, after] = entries.as_slice() else {
            return;
        };
        let before = (*before).clone();
        let after = (*after).clone();
        self._diff_work = Some(cx.spawn(async move |this, cx| {
            let changes =
                crate::api::compat::blocking(move || PreparedDiff::new(&before, &after)).await;
            let _ = this.update(cx, |panel, cx| {
                if panel.diff_generation != generation {
                    return;
                }
                panel.diff_result = Some(changes);
                panel._diff_work = None;
                cx.notify();
            });
        }));
    }

    fn replay_history(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.history.get(index).cloned() else {
            return;
        };
        // History is an immutable record of what crossed the wire. Loading the
        // current saved definition here silently changes method/URL/body after
        // that definition is edited, so replay always starts from the snapshot.
        self.load_history_snapshot(&entry.exchange.request, window, cx);
        self.set_shared_response(Some(entry.response.clone()), None, cx);
        self.response_request = Some(entry.exchange.request.clone());
        self.elapsed_ms = Some(entry.elapsed_ms);
        self.error = entry.exchange.error.clone();
        self.tab = Tab::Compose;
        cx.notify();
    }

    fn delete_history_entry(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(exchange_id) = self
            .history
            .get(index)
            .map(|entry| entry.exchange.id.clone())
        else {
            return;
        };
        if self.run_storage_command(coordinator::StorageCommand::DeleteExchange(exchange_id), cx) {
            self.history.remove(index);
            self.selected_history.clear();
            self.diff_result = None;
        }
    }

    fn save_history_as_request(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.history.get(index) else {
            return;
        };
        let Some(collection_id) = self.current_collection_id.clone() else {
            self.storage_error =
                Some("Select a collection before saving history as a request.".into());
            cx.notify();
            return;
        };
        let folder_id = self
            .current_definition
            .as_ref()
            .and_then(|request| request.folder_id.clone());
        let command = coordinator::StorageCommand::SaveExchangeAsRequest {
            exchange_id: entry.exchange.id.clone(),
            collection_id,
            folder_id,
            name: format!("{} history copy", entry.method),
        };
        self.run_storage_command(command, cx);
    }

    fn cycle_history_retention(&mut self, cx: &mut Context<Self>) {
        self.history_retention = match self.history_retention {
            0..=100 => 500,
            101..=500 => 1_000,
            501..=1_000 => 5_000,
            _ => 100,
        };
        self.run_storage_command(
            coordinator::StorageCommand::PruneHistory(self.history_retention),
            cx,
        );
    }

    fn load_history_snapshot(
        &mut self,
        snapshot: &switchyard_api::RedactedRequestSnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A replay needs a tab to land in when none is open.
        self.ensure_request_tab();
        if let Some(id) = self.active_request_tab_id() {
            self.ux.request_settings.remove(&id);
        }
        if let Some(replay) = snapshot.replay.as_ref() {
            let id = RequestId::new();
            let definition = SavedRequest {
                id: id.clone(),
                collection_id: self.current_collection_id.clone().unwrap_or_default(),
                folder_id: None,
                name: replay.name.clone(),
                method: replay.method.clone(),
                url: replay.url.clone(),
                params: replay.params.clone(),
                headers: replay.headers.clone(),
                auth: replay.auth.clone(),
                body: replay.body.clone(),
                variables: replay.variables.clone(),
                scripts: replay.scripts.clone(),
                settings: replay.settings.clone(),
                extensions: Default::default(),
                sort_key: 0,
            };
            self.current_request_id = Some(id);
            self.current_definition = Some(definition);
            self.method = transport::Method::from_label(replay.method.as_str())
                .unwrap_or(transport::Method::Custom);
            set_input(&self.custom_method, replay.method.as_str(), window, cx);
            set_input(&self.request_name, &replay.name, window, cx);
            set_input(&self.url, &replay.url, window, cx);
            set_input(&self.params, &format_rows(&replay.params, '='), window, cx);
            set_input(
                &self.headers,
                &format_rows(&replay.headers, ':'),
                window,
                cx,
            );
            let (mode, body) = format_body(&replay.body);
            self.body_mode = mode;
            set_input(&self.body, &body, window, cx);
            set_input(&self.cookies, "", window, cx);
            self.auth_mode = auth_mode(&replay.auth);
            set_input(
                &self.authorization,
                &draft::format_auth(&replay.auth),
                window,
                cx,
            );
            set_input(
                &self.variables,
                &format_variables(&replay.variables),
                window,
                cx,
            );
            set_input(
                &self.pre_request_script,
                &replay.scripts.pre_request,
                window,
                cx,
            );
            set_input(&self.assertions, &replay.scripts.tests, window, cx);
            self.allow_private_network = replay.settings.allow_private_network;
            self.dirty = true;
            return;
        }
        self.current_request_id = None;
        self.current_definition = None;
        self.method =
            transport::Method::from_label(&snapshot.method).unwrap_or(transport::Method::Custom);
        set_input(&self.custom_method, &snapshot.method, window, cx);
        set_input(&self.request_name, "Replayed request", window, cx);
        set_input(&self.url, &snapshot.url, window, cx);
        set_input(&self.params, "", window, cx);
        set_input(
            &self.headers,
            &snapshot
                .headers
                .iter()
                .map(|(name, value)| format!("{name}: {value}"))
                .collect::<Vec<_>>()
                .join("\n"),
            window,
            cx,
        );
        self.body_mode = if snapshot.body.is_empty() {
            draft::BodyMode::None
        } else {
            draft::BodyMode::Text
        };
        set_input(&self.body, &snapshot.body, window, cx);
        set_input(&self.cookies, "", window, cx);
        self.auth_mode = draft::AuthMode::None;
        set_input(&self.authorization, "", window, cx);
        set_input(&self.variables, "", window, cx);
        set_input(&self.pre_request_script, "", window, cx);
        set_input(&self.assertions, "", window, cx);
        self.allow_private_network = true;
        self.dirty = true;
    }

    fn method_tint_label(&self, method: &str, cx: &App) -> gpui_kit::Hsla {
        let colors = cx.theme().colors;
        match method {
            "GET" | "HEAD" | "OPTIONS" | "TRACE" => colors.success,
            "DELETE" => colors.danger,
            "POST" | "PUT" | "PATCH" => colors.warning,
            _ => colors.primary,
        }
    }

    fn import_from_source(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self
            .workspace_data
            .as_ref()
            .map(|data| data.workspace.clone())
            .unwrap_or_else(current_workspace_id);
        let source = self.import_source.read(cx).value().to_string();
        let remembered_origin = self
            .import_origin_source
            .as_deref()
            .filter(|loaded| *loaded == source.as_str())
            .and(self.import_origin.clone());
        self.import_generation = self.import_generation.wrapping_add(1);
        let generation = self.import_generation;
        self.import_status = Some(if source.trim_start().starts_with("https://") {
            "Fetching remote import through the protected service…".into()
        } else {
            "Parsing import off the frame thread…".into()
        });
        self._import_work = Some(cx.spawn(async move |this, cx| {
            let imported = crate::api::compat::blocking(move || {
                let (source, origin) = if source.trim_start().starts_with("https://") {
                    let origin = source.trim().to_string();
                    (transport::fetch_https_import(&origin)?, Some(origin))
                } else {
                    (source, remembered_origin)
                };
                if let Some(origin) = origin {
                    let origin = switchyard_api::ImportOrigin::new(origin)?;
                    let resolver = transport::ProtectedImportResolver::for_origin(&origin);
                    switchyard_api::import_with_origin(
                        &workspace,
                        source.as_bytes(),
                        origin,
                        &resolver,
                    )
                } else {
                    switchyard_api::import(&workspace, source.as_bytes())
                }
            })
            .await;
            let _ = this.update(cx, |panel, cx| {
                if panel.import_generation != generation {
                    return;
                }
                match imported {
                    Ok(imported) => panel.stage_import(imported),
                    Err(error) => {
                        panel.import_status = Some(format!("Import failed: {error}"));
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn choose_import_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose an API description, collection, or HAR capture".into()),
        });
        self.import_status = Some("Waiting for native file selection…".into());
        cx.spawn_in(window, async move |this, cx| {
            let path = match chosen.await {
                Ok(Ok(Some(mut paths))) if paths.len() == 1 => paths.remove(0),
                Ok(Ok(None)) | Err(_) => return,
                Ok(Ok(Some(_))) => return,
                Ok(Err(error)) => {
                    let _ = this.update(cx, |panel, cx| {
                        panel.import_status = Some(format!("Could not open file picker: {error}"));
                        cx.notify();
                    });
                    return;
                }
            };
            let loaded = crate::api::compat::blocking(move || {
                let metadata = std::fs::metadata(&path)
                    .map_err(|error| format!("inspect import file: {error}"))?;
                if !metadata.is_file() || metadata.len() > 8 * 1024 * 1024 {
                    return Err("import file must be a regular file no larger than 8 MiB".into());
                }
                let source = std::fs::read_to_string(&path)
                    .map_err(|error| format!("read import file as UTF-8: {error}"))?;
                let origin = url::Url::from_file_path(&path)
                    .map_err(|_| {
                        "selected import path cannot be represented as a file URI".to_string()
                    })?
                    .to_string();
                Ok::<_, String>((source, origin))
            })
            .await;
            let _ = this.update_in(cx, |panel, window, cx| match loaded {
                Ok((source, origin)) => {
                    set_input(&panel.import_source, &source, window, cx);
                    panel.import_origin = Some(origin);
                    panel.import_origin_source = Some(source);
                    panel.import_status = Some("File loaded. Parse it for staged review.".into());
                    cx.notify();
                }
                Err(error) => {
                    panel.import_status = Some(error);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn save_export_file(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.save_prepared_export(cx);
    }

    fn save_prepared_export(&mut self, cx: &mut Context<Self>) {
        self.tab = Tab::Compose;
        self.composer_tab = ComposerTab::Snippet;
        let Some(export) = self.prepared_export.clone() else {
            return;
        };
        let workspace = export.workspace.clone();
        let generation = export.generation;
        if !self.owns_export(&workspace, generation, &current_workspace_id()) {
            self.prepared_export = None;
            self.export_output = Some(
                "This export belongs to a previous project. Prepare it again before saving.".into(),
            );
            cx.notify();
            return;
        }
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose a folder for the redacted export".into()),
        });
        cx.spawn(async move |this, cx| {
            let directory = match chosen.await {
                Ok(Ok(Some(mut paths))) if paths.len() == 1 => paths.remove(0),
                Ok(Ok(None)) | Err(_) => return,
                Ok(Ok(Some(_))) => return,
                Ok(Err(error)) => {
                    let _ = this.update(cx, |panel, cx| {
                        let current_workspace = current_workspace_id();
                        if panel.apply_export_picker_error(
                            &workspace,
                            generation,
                            &current_workspace,
                            &error.to_string(),
                        ) {
                            cx.notify();
                        }
                    });
                    return;
                }
            };
            let still_owned = this
                .update(cx, |panel, _| {
                    panel.owns_export(&workspace, generation, &current_workspace_id())
                        && panel.prepared_export.as_ref().is_some_and(|prepared| {
                            prepared.workspace == workspace && prepared.generation == generation
                        })
                })
                .unwrap_or(false);
            if !still_owned {
                return;
            }
            let saved = crate::api::compat::blocking(move || {
                let path = directory.join(&export.suggested_name);
                std::fs::write(&path, export.contents)
                    .map_err(|error| format!("write redacted export: {error}"))?;
                Ok::<_, String>(path)
            })
            .await;
            let _ = this.update(cx, |panel, cx| {
                if !panel.owns_export(&workspace, generation, &current_workspace_id()) {
                    return;
                }
                panel.export_output = Some(match saved {
                    Ok(path) => format!("Saved redacted export to {}", path.display()),
                    Err(error) => error,
                });
                cx.notify();
            });
        })
        .detach();
    }

    fn stage_import(&mut self, imported: ImportResult) {
        let request_count = imported.requests.len();
        let environment_count = imported.environments.len();
        self.import_selection = Some(ImportSelection::all(&imported));
        self.import_origin = imported.origin.as_ref().map(|origin| origin.uri.clone());
        self.staged_import = Some(imported.clone());
        self.import_status = Some(format!(
            "Staged {:?}: {request_count} requests, {} folders, {environment_count} environments, {} examples{}",
            imported.format,
            imported.folders.len(),
            imported.examples.len(),
            if imported.warnings.is_empty() {
                String::new()
            } else {
                format!(" · {} warnings", imported.warnings.len())
            }
        ));
    }

    fn toggle_import_request(&mut self, id: RequestId, cx: &mut Context<Self>) {
        if let Some(selection) = self.import_selection.as_mut() {
            let selecting = !selection.request_ids.remove(&id);
            if selecting {
                selection.include_collection = true;
                selection.request_ids.insert(id.clone());
            }
            if let Some(imported) = self.staged_import.as_ref() {
                for example in imported
                    .examples
                    .iter()
                    .filter(|example| example.request_id == id)
                {
                    if selecting {
                        selection.example_ids.insert(example.id.clone());
                    } else {
                        selection.example_ids.remove(&example.id);
                    }
                }
                if selecting
                    && let Some(folder_id) = imported
                        .requests
                        .iter()
                        .find(|request| request.id == id)
                        .and_then(|request| request.folder_id.as_ref())
                {
                    select_folder_ancestors(imported, folder_id, selection);
                }
            }
            cx.notify();
        }
    }

    fn toggle_import_folder(&mut self, id: FolderId, cx: &mut Context<Self>) {
        if let (Some(selection), Some(imported)) =
            (self.import_selection.as_mut(), self.staged_import.as_ref())
        {
            let selecting = !selection.folder_ids.contains(&id);
            let mut affected = HashSet::from([id.clone()]);
            loop {
                let before = affected.len();
                for folder in &imported.folders {
                    if folder
                        .parent_id
                        .as_ref()
                        .is_some_and(|parent| affected.contains(parent))
                    {
                        affected.insert(folder.id.clone());
                    }
                }
                if affected.len() == before {
                    break;
                }
            }
            for folder_id in &affected {
                if selecting {
                    selection.folder_ids.insert(folder_id.clone());
                } else {
                    selection.folder_ids.remove(folder_id);
                }
            }
            let affected_requests = imported
                .requests
                .iter()
                .filter(|request| {
                    request
                        .folder_id
                        .as_ref()
                        .is_some_and(|folder| affected.contains(folder))
                })
                .map(|request| request.id.clone())
                .collect::<HashSet<_>>();
            for request_id in &affected_requests {
                if selecting {
                    selection.request_ids.insert(request_id.clone());
                } else {
                    selection.request_ids.remove(request_id);
                }
            }
            for example in imported
                .examples
                .iter()
                .filter(|example| affected_requests.contains(&example.request_id))
            {
                if selecting {
                    selection.example_ids.insert(example.id.clone());
                } else {
                    selection.example_ids.remove(&example.id);
                }
            }
            if selecting {
                selection.include_collection = true;
                select_folder_ancestors(imported, &id, selection);
            }
            cx.notify();
        }
    }

    fn toggle_import_environment(&mut self, id: EnvironmentId, cx: &mut Context<Self>) {
        if let Some(selection) = self.import_selection.as_mut() {
            if !selection.environment_ids.remove(&id) {
                selection.environment_ids.insert(id);
            }
            cx.notify();
        }
    }

    fn toggle_import_example(&mut self, id: ExampleId, cx: &mut Context<Self>) {
        if let Some(selection) = self.import_selection.as_mut() {
            let selecting = !selection.example_ids.remove(&id);
            if selecting {
                selection.include_collection = true;
                selection.example_ids.insert(id.clone());
                if let Some(imported) = self.staged_import.as_ref()
                    && let Some(request_id) = imported
                        .examples
                        .iter()
                        .find(|example| example.id == id)
                        .map(|example| example.request_id.clone())
                {
                    selection.request_ids.insert(request_id.clone());
                    if let Some(folder_id) = imported
                        .requests
                        .iter()
                        .find(|request| request.id == request_id)
                        .and_then(|request| request.folder_id.as_ref())
                    {
                        select_folder_ancestors(imported, folder_id, selection);
                    }
                }
            }
            cx.notify();
        }
    }

    fn toggle_import_collection(&mut self, cx: &mut Context<Self>) {
        let (Some(selection), Some(imported)) =
            (self.import_selection.as_mut(), self.staged_import.as_ref())
        else {
            return;
        };
        if selection.include_collection {
            selection.include_collection = false;
            selection.folder_ids.clear();
            selection.request_ids.clear();
            selection.example_ids.clear();
        } else {
            *selection = ImportSelection::all(imported);
        }
        cx.notify();
    }

    fn commit_staged_import(&mut self, cx: &mut Context<Self>) {
        let (Some(imported), Some(selection), Some(data)) = (
            self.staged_import.clone(),
            self.import_selection.clone(),
            self.workspace_data.as_ref(),
        ) else {
            self.import_status = Some("Parse an import before committing it.".into());
            cx.notify();
            return;
        };
        let store = data.store.clone();
        let workspace = data.workspace.clone();
        let imported_collection_id = imported.collection.id.clone();
        let select_imported_collection = selection.include_collection;
        let selected_request_id = imported
            .requests
            .iter()
            .find(|request| selection.request_ids.contains(&request.id))
            .map(|request| request.id.clone());
        let selected_request_count = selection.request_ids.len();
        let imported_environment_ids = imported
            .environments
            .iter()
            .filter(|environment| selection.environment_ids.contains(&environment.id))
            .map(|environment| environment.id.clone())
            .collect::<Vec<_>>();
        self.import_generation = self.import_generation.wrapping_add(1);
        let generation = self.import_generation;
        self.import_status = Some("Committing selected items transactionally…".into());
        self._import_work = Some(cx.spawn(async move |this, cx| {
            let result = crate::api::compat::blocking(move || {
                store
                    .commit_import_selection(&workspace, &imported, &selection)
                    .map_err(|error| error.to_string())?;
                persistence::WorkspaceData::hydrate(store, workspace)
            })
            .await;
            let _ = this.update(cx, |panel, cx| {
                if panel.import_generation != generation {
                    return;
                }
                match result {
                    Ok(data) => {
                        panel.workspace_data = Some(data);
                        if select_imported_collection {
                            panel.current_collection_id = Some(imported_collection_id);
                        }
                        if let Some(request_id) = selected_request_id {
                            panel.pending_request_to_load = Some(request_id);
                            panel.tab = Tab::Compose;
                        }
                        panel.import_status = Some(format!(
                            "Imported {selected_request_count} selected requests transactionally."
                        ));
                        panel.staged_import = None;
                        panel.import_selection = None;
                        panel.storage_error = None;
                        panel
                            .imported_environment_ids
                            .extend(imported_environment_ids.iter().cloned());
                    }
                    Err(error) => panel.storage_error = Some(error),
                }
                panel._import_work = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn save_environment(&mut self, cx: &mut Context<Self>) {
        let name = self.environment_name.read(cx).value().trim().to_string();
        if name.is_empty() {
            self.storage_error = Some("Name the environment before saving.".into());
            cx.notify();
            return;
        }
        let environment_id = self.active_environment_id.clone().unwrap_or_default();
        let scope = format!("environment.{}", environment_id.as_str());
        let parsed =
            draft::parse_session_variables(&self.environment_variables.read(cx).value(), &scope)
                .and_then(|(variables, secrets)| {
                    let (auth, auth_secrets) = draft::parse_auth(
                        self.environment_auth_mode,
                        &self.environment_auth.read(cx).value(),
                        &scope,
                    )?;
                    Ok((variables, auth, secrets, auth_secrets))
                });
        let (variables, mut auth, mut secrets, auth_secrets) = match parsed {
            Ok(parsed) => parsed,
            Err(error) => {
                self.storage_error = Some(error);
                cx.notify();
                return;
            }
        };
        secrets.merge(auth_secrets);
        let Some(data) = self.workspace_data.as_ref() else {
            self.storage_error = Some("Workbench storage is unavailable".into());
            cx.notify();
            return;
        };
        if let Some(saved) = data
            .environments
            .iter()
            .find(|saved| saved.id == environment_id)
        {
            preserve_saved_auth(
                &saved.auth,
                &mut auth,
                &secrets,
                &draft::DraftSecrets::with_store(
                    self.secret_store.clone(),
                    self.bound_workspace.clone(),
                ),
            );
        }
        let environment = Environment {
            id: environment_id,
            workspace_id: data.workspace.clone(),
            name,
            base_url: self
                .environment_base_url
                .read(cx)
                .value()
                .trim()
                .to_string(),
            auth,
            variables,
            active: true,
            extensions: Default::default(),
            label: self.environment_label,
        };
        self.active_environment_id = Some(environment.id.clone());
        self.run_storage_command(
            coordinator::StorageCommand::UpsertEnvironment {
                environment,
                secrets,
                secret_store: self.secret_store.clone(),
            },
            cx,
        );
        cx.notify();
    }

    /// Start an empty draft and put the cursor in its name field.
    fn new_environment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.active_environment_id = None;
        self.load_environment_editor(None, window, cx);
        self.environment_name
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
        self.storage_error = None;
        cx.notify();
    }

    /// Point the Envs editor — name and variables — at `environment`, or at
    /// an empty draft.
    fn load_environment_editor(
        &mut self,
        environment: Option<&Environment>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        set_input(
            &self.environment_name,
            environment.map(|value| value.name.as_str()).unwrap_or(""),
            window,
            cx,
        );
        set_input(
            &self.environment_base_url,
            environment
                .map(|value| value.base_url.as_str())
                .unwrap_or(""),
            window,
            cx,
        );
        self.environment_auth_mode = environment
            .map(|value| auth_mode(&value.auth))
            .unwrap_or(draft::AuthMode::None);
        self.environment_label = environment.map(|value| value.label).unwrap_or_default();
        set_input(
            &self.environment_auth,
            &environment
                .map(|value| draft::format_auth(&value.auth))
                .unwrap_or_default(),
            window,
            cx,
        );
        self.environment_login_status = None;
        set_input(
            &self.environment_variables,
            &environment
                .map(|value| format_variables(&value.variables))
                .unwrap_or_default(),
            window,
            cx,
        );
    }

    fn delete_active_environment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.active_environment_id.clone() else {
            return;
        };
        if self.run_storage_command(coordinator::StorageCommand::DeleteEnvironment(id), cx) {
            self.active_environment_id = None;
            self.load_environment_editor(None, window, cx);
        }
        cx.notify();
    }

    /// Make `id` the active environment. This is the one activation path:
    /// it persists the flag, moves `active_environment_id`, and re-points the
    /// Envs editor, which is what [`Self::environment_base_url_value`] reads
    /// first. Any surface offering an environment switch must call this rather
    /// than set the id, or the base URL keeps the previous environment's value.
    fn activate_environment(
        &mut self,
        id: EnvironmentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let environment = self.workspace_data.as_ref().and_then(|data| {
            data.environments
                .iter()
                .find(|environment| environment.id == id)
                .cloned()
        });
        let Some(environment) = environment else {
            return;
        };
        if self.run_storage_command(
            coordinator::StorageCommand::ActivateEnvironment(Some(id.clone())),
            cx,
        ) {
            self.active_environment_id = Some(id);
            self.load_environment_editor(Some(&environment), window, cx);
        }
        cx.notify();
    }

    /// Clear the active environment. Relative URLs stop being prefixed and
    /// `{{base_url}}` stops resolving, so the Envs editor is emptied with it:
    /// its live text is what [`Self::environment_base_url_value`] reads first,
    /// and leaving it behind would keep the departed base URL in force.
    fn deactivate_environment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_environment_id.is_none() {
            return;
        }
        if self.run_storage_command(coordinator::StorageCommand::ActivateEnvironment(None), cx) {
            self.active_environment_id = None;
            self.load_environment_editor(None, window, cx);
        }
        cx.notify();
    }

    fn toggle_runner_folder(&mut self, id: FolderId, cx: &mut Context<Self>) {
        if self.runner_folder_id.as_ref() == Some(&id) {
            self.runner_folder_id = None;
        } else {
            self.runner_folder_id = Some(id);
        }
        self.runner_request_ids.clear();
        self.runner_request_selection_active = false;
        cx.notify();
    }

    fn toggle_runner_request(&mut self, id: RequestId, cx: &mut Context<Self>) {
        if !self.runner_request_selection_active {
            self.runner_request_selection_active = true;
            self.runner_request_ids = self
                .workspace_data
                .as_ref()
                .into_iter()
                .flat_map(|data| data.requests.iter())
                .filter(|request| {
                    self.current_collection_id.as_ref() == Some(&request.collection_id)
                })
                .map(|request| request.id.clone())
                .collect();
        }
        if !self.runner_request_ids.remove(&id) {
            self.runner_request_ids.insert(id);
        }
        cx.notify();
    }

    fn select_run_result(&mut self, run_id: RunId, index: usize, cx: &mut Context<Self>) {
        self.selected_run_result = Some((run_id, index));
        cx.notify();
    }

    /// A collection or folder row's "Run": scope the runner to it and start.
    fn run_from_rail(
        &mut self,
        collection: CollectionId,
        folder: Option<FolderId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.send_state != SendState::Idle {
            self.run_status = Some("Wait for the current run to finish.".into());
            cx.notify();
            return;
        }
        if !self.focus_collection(collection, window, cx) {
            return;
        }
        self.runner_folder_id = folder;
        self.runner_request_ids.clear();
        self.runner_request_selection_active = false;
        self.tab = Tab::Data;
        self.run_collection_confirmed(window, cx);
    }

    fn run_collection(&mut self, cx: &mut Context<Self>) {
        if self.send_state != SendState::Idle {
            return;
        }
        // The open editor's unsaved changes run in place of its saved copy.
        // A draft that cannot be built only matters when nothing saved is in
        // scope, so a broken open request cannot block running a folder.
        let draft = self.draft_request(cx);
        let Some(collection_id) = self.current_collection_id.clone() else {
            self.run_status = Some(
                draft
                    .err()
                    .unwrap_or_else(|| "Select a collection first.".into()),
            );
            cx.notify();
            return;
        };
        if let Some(folder) = self.runner_folder_id.clone() {
            let in_collection = self.workspace_data.as_ref().is_some_and(|data| {
                data.folders.iter().any(|candidate| {
                    candidate.id == folder && candidate.collection_id == collection_id
                })
            });
            if !in_collection {
                self.runner_folder_id = None;
            }
        }
        let (draft_request, draft_error, draft_secrets) = match draft {
            Ok((request, secrets)) => (Some(request), None, secrets),
            Err(error) => (None, Some(error), self.session_secrets.clone()),
        };
        let Some(data) = self.workspace_data.as_ref() else {
            self.run_status = Some("Workbench storage is unavailable".into());
            cx.notify();
            return;
        };
        let iteration_limit = match self.runner_iterations.read(cx).value().trim() {
            "" => None,
            value => match value.parse::<usize>() {
                Ok(value @ 1..=1_000) => Some(value),
                _ => {
                    self.run_status = Some("Iterations must be between 1 and 1000.".into());
                    cx.notify();
                    return;
                }
            },
        };
        let delay_ms = match self.runner_delay_ms.read(cx).value().trim() {
            "" => 0,
            value => match value.parse::<u64>() {
                Ok(value @ 0..=60_000) => value,
                _ => {
                    self.run_status = Some("Runner delay must be between 0 and 60000 ms.".into());
                    cx.notify();
                    return;
                }
            },
        };
        let collection = data.collection(&collection_id).cloned();
        let folders = data.folders.clone();
        let workspace_id = data.workspace.clone();
        let store = data.store.clone();
        let mut requests = select_run_requests(
            &data.requests,
            &data.folders,
            &collection_id,
            self.runner_folder_id.as_ref(),
            self.runner_request_selection_active
                .then(|| self.runner_request_ids.iter().cloned().collect::<Vec<_>>())
                .as_deref(),
        );
        if let Some(draft_request) = draft_request {
            if requests.is_empty()
                && !self.runner_request_selection_active
                && self.runner_folder_id.is_none()
            {
                requests.push(draft_request);
            } else if let Some(current) = requests
                .iter_mut()
                .find(|request| request.id == draft_request.id)
            {
                *current = draft_request;
            }
        }
        if requests.is_empty() {
            self.run_status = Some(if self.runner_folder_id.is_some() {
                "This folder has no requests to run.".into()
            } else if let Some(error) = draft_error {
                error
            } else {
                "Select at least one request for this run.".into()
            });
            cx.notify();
            return;
        }
        let environment_scope = self
            .active_environment_id
            .as_ref()
            .map(|id| format!("environment.{}", id.as_str()))
            .unwrap_or_else(|| "environment.unsaved".into());
        let preparation = RunPreparationInput {
            workspace_id,
            collection_id,
            environment_id: self.active_environment_id.clone(),
            environment: self.active_environment().cloned(),
            stop_on_error: self.stop_on_error,
            selected_folder_id: self.runner_folder_id.clone(),
            selected_request_ids: if self.runner_request_selection_active {
                self.runner_request_ids.iter().cloned().collect()
            } else {
                Default::default()
            },
            collection,
            folders,
            requests,
            environment_source: self.environment_variables.read(cx).value().to_string(),
            environment_scope,
            environment_base_url: self.environment_base_url_value(cx),
            environment_auth: self
                .active_environment()
                .map(|environment| environment.auth.clone())
                .unwrap_or_default(),
            data_source: self.data_source.read(cx).value().to_string(),
            secrets: draft_secrets,
            iteration_limit,
            delay_ms,
            keep_variables: self.keep_runner_variables,
            store: store.clone(),
            secret_store: self.secret_store.clone(),
            cookie_jar: self.cookie_jar.clone(),
            file_capabilities: self.file_capabilities.clone(),
        };
        self.send_generation = self.send_generation.wrapping_add(1);
        let generation = self.send_generation;
        self.send_state = SendState::PreparingRun;
        self.active_request_id = None;
        self.active_run = None;
        self.active_run_redactions.clear();
        self.run_status = Some("Preparing collection run…".into());
        let runner_transport = self.transport.clone();
        self._request_work = Some(cx.spawn(async move |this, cx| {
            let preparation_transport = runner_transport.clone();
            let prepared = crate::api::compat::blocking(move || {
                prepare_collection_run(preparation, preparation_transport.as_ref())
            })
            .await;
            let prepared = match prepared {
                Ok(prepared) => prepared,
                Err(error) => {
                    let _ = this.update(cx, |panel, cx| {
                        if panel.send_generation == generation {
                            panel.send_state = SendState::Idle;
                            panel.run_status = Some(error);
                            panel._request_work = None;
                            cx.notify();
                        }
                    });
                    return;
                }
            };
            #[allow(clippy::result_large_err)]
            let started = crate::api::compat::blocking(move || RunSession::start(prepared)).await;
            let mut session = match started {
                Ok(session) => session,
                Err(outcome) => {
                    let _ = this.update(cx, |panel, cx| {
                        if panel.send_generation == generation {
                            panel.send_state = SendState::Idle;
                            panel.storage_error = outcome.error;
                            panel._request_work = None;
                            cx.notify();
                        }
                    });
                    return;
                }
            };
            let run_started = this
                .update(cx, |panel, cx| {
                    if panel.send_generation != generation {
                        return false;
                    }
                    panel.send_state = SendState::Sending;
                    panel.active_request_id = None;
                    panel.active_run = Some(session.run().clone());
                    panel.run_status = Some(format!("Running {} requests…", session.item_count()));
                    if let Some(data) = panel.workspace_data.as_mut() {
                        data.apply_run_snapshot(session.run().clone());
                    }
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !run_started {
                let _ = crate::api::compat::blocking(move || session.finish(true)).await;
                return;
            }
            while let Some(item) = session.next_item() {
                let operation_id = item.operation_id;
                let operation_active = this
                    .update(cx, |panel, cx| {
                        if panel.send_generation != generation {
                            return false;
                        }
                        panel.active_request_id = Some(operation_id.clone());
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !operation_active {
                    return;
                }
                if item.delay_ms > 0 {
                    // Install the next operation identity before delaying.
                    // Cancel can tombstone it even before pre-script
                    // registration, so a completed prior item cannot make
                    // the run accidentally continue.
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(item.delay_ms))
                        .await;
                    let delay_still_active = this
                        .update(cx, |panel, _| {
                            panel.send_generation == generation
                                && panel.send_state == SendState::Sending
                                && panel.active_request_id.as_deref() == Some(operation_id.as_str())
                        })
                        .unwrap_or(false);
                    if !delay_still_active {
                        return;
                    }
                }
                let item_transport = runner_transport.clone();
                let (next_session, completed) = crate::api::compat::blocking(move || {
                    let mut session = session;
                    let completed = session.run_item(operation_id, &item_transport).clone();
                    (session, completed)
                })
                .await;
                session = next_session;
                let redactions = session.redactions().to_vec();
                let still_active = this
                    .update(cx, |panel, _| {
                        if panel.send_generation != generation {
                            return false;
                        }
                        if let Some(active) = panel.active_run.as_mut() {
                            active.item_results.push(completed);
                        }
                        panel.active_run_redactions = redactions;
                        true
                    })
                    .unwrap_or(false);
                if !still_active {
                    return;
                }
            }
            let outcome = crate::api::compat::blocking(move || session.finish(false)).await;
            let _ = this.update(cx, |panel, cx| {
                if panel.send_generation != generation {
                    return;
                }
                let failures = outcome
                    .run
                    .item_results
                    .iter()
                    .filter(|result| result.error.is_some())
                    .count();
                panel.send_state = SendState::Idle;
                panel.active_request_id = None;
                panel.active_run = None;
                panel.active_run_redactions.clear();
                panel.cookie_jar = outcome.cookie_jar;
                panel.run_status = Some(format!(
                    "Run complete: {} items, {failures} failed",
                    outcome.run.item_results.len()
                ));
                if let Some(data) = panel.workspace_data.as_mut() {
                    data.apply_run_snapshot(outcome.run);
                }
                if let Some(error) = outcome.error {
                    panel.storage_error = Some(error);
                }
                panel._request_work = None;
                cx.notify();
            });
        }));
        cx.notify();
    }
}

/// A blank secret field on a saved auth of the same type means "keep what is
/// saved": the saved reference is carried over, so an imported or vault-only
/// reference survives a re-save without its value being typed again. Fields
/// that were typed keep the deterministic reference the value was entered
/// under; the managed OAuth and sign-in caches are carried separately.
fn preserve_saved_auth(
    original: &AuthConfig,
    rebuilt: &mut AuthConfig,
    entered: &draft::DraftSecrets,
    stored: &dyn switchyard_api::SecretResolver,
) {
    preserve_saved_secret_refs(original, rebuilt, entered);
    preserve_managed_oauth_state(original, rebuilt);
    if let AuthConfig::Login {
        basic: Some(basic),
        access_token,
        expires_at,
        ..
    } = rebuilt
    {
        // An edited password can retain the same vault reference. Compare its
        // value so repeated sends with unchanged editor text reuse the token.
        if entered.has_value(&basic.password)
            && !matches!(
                (stored.resolve(&basic.password), switchyard_api::SecretResolver::resolve(entered, &basic.password)),
                (Ok(saved), Ok(current)) if saved == current
            )
        {
            *access_token = None;
            *expires_at = None;
        }
    }
}

fn preserve_saved_secret_refs(
    original: &AuthConfig,
    rebuilt: &mut AuthConfig,
    entered: &draft::DraftSecrets,
) {
    let keep = |saved: &SecretRef, current: &mut SecretRef| {
        if !entered.has_value(current)
            && switchyard_api::vault::vault_reference_expression(current).is_none()
            && saved != current
        {
            *current = saved.clone();
        }
    };
    match (original, rebuilt) {
        (
            AuthConfig::ApiKey {
                value: saved_value, ..
            },
            AuthConfig::ApiKey { value, .. },
        ) => {
            keep(saved_value, value);
        }
        (
            AuthConfig::Basic {
                password: saved_password,
                ..
            },
            AuthConfig::Basic { password, .. },
        ) => {
            keep(saved_password, password);
        }
        (
            AuthConfig::Login {
                basic: Some(saved), ..
            },
            AuthConfig::Login {
                basic: Some(current),
                ..
            },
        ) => keep(&saved.password, &mut current.password),
        (
            AuthConfig::Basic {
                password: saved, ..
            },
            AuthConfig::Login {
                basic: Some(current),
                ..
            },
        ) => keep(saved, &mut current.password),
        (
            AuthConfig::Login {
                basic: Some(saved), ..
            },
            AuthConfig::Basic {
                password: current, ..
            },
        ) => keep(&saved.password, current),
        (
            AuthConfig::Bearer {
                token: saved_token, ..
            },
            AuthConfig::Bearer { token, .. },
        ) => {
            keep(saved_token, token);
        }
        (
            AuthConfig::OAuth2 {
                token: saved_token, ..
            },
            AuthConfig::OAuth2 { token, .. },
        ) => {
            keep(saved_token, token);
        }
        (
            AuthConfig::OAuth2ClientCredentials {
                client_secret: saved_client_secret,
                ..
            },
            AuthConfig::OAuth2ClientCredentials { client_secret, .. },
        ) => {
            keep(saved_client_secret, client_secret);
        }
        (
            AuthConfig::OAuth2Password {
                client_secret: saved_client_secret,
                password: saved_pw,
                ..
            },
            AuthConfig::OAuth2Password {
                client_secret,
                password: pw,
                ..
            },
        ) => {
            keep(saved_pw, pw);
            if let Some(saved) = saved_client_secret {
                match client_secret {
                    Some(current) => keep(saved, current),
                    None => *client_secret = Some(saved.clone()),
                }
            }
        }
        (
            AuthConfig::AwsSigV4 {
                access_key: saved_access_key,
                secret_key: saved_secret_key,
                session_token: saved_session_token,
                ..
            },
            AuthConfig::AwsSigV4 {
                access_key,
                secret_key,
                session_token,
                ..
            },
        ) => {
            keep(saved_access_key, access_key);
            keep(saved_secret_key, secret_key);
            if let Some(saved) = saved_session_token {
                match session_token {
                    Some(current) => keep(saved, current),
                    None => *session_token = Some(saved.clone()),
                }
            }
        }
        _ => {}
    }
}

impl EventEmitter<PanelEvent> for WorkbenchPanel {}

/// The Workbench asked for the agent's help with a request. The shell owns
/// the trip to Chat: it opens the surface and fills the composer with
/// `prompt`, which is built only from redacted snapshots — see [`assist`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskAgentRequested {
    pub prompt: String,
}

impl EventEmitter<AskAgentRequested> for WorkbenchPanel {}

/// Open the shared AgentOps secret vault from a request workflow.
pub struct OpenVaultRequested;

impl EventEmitter<OpenVaultRequested> for WorkbenchPanel {}

impl Focusable for WorkbenchPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// How an "Acquire token" button gets its token: a client-credentials or
/// refresh-token exchange, or a PKCE grant that waits for the browser.
enum OAuthAcquisition {
    ClientCredentials {
        request: transport::OAuthTokenRequest,
        secret: SecretRef,
    },
    Password {
        request: transport::OAuthTokenRequest,
        client_secret: Option<SecretRef>,
        password: SecretRef,
    },
    Refresh {
        request: transport::OAuthTokenRequest,
        secret: SecretRef,
        client_secret: Option<SecretRef>,
    },
    Pkce {
        flow: transport::PkceAuthorization,
        request: transport::OAuthTokenRequest,
    },
}

impl OAuthAcquisition {
    /// The page the user has to visit — only a fresh PKCE grant has one.
    fn authorization_url(&self) -> Option<&str> {
        match self {
            Self::Pkce { flow, .. } => Some(&flow.authorization_url),
            _ => None,
        }
    }

    /// The grant as the core send runtime finishes it before compiling.
    fn into_browser_authorization(self) -> BrowserAuthorization {
        BrowserAuthorization {
            complete: Box::new(move |auth, scope, secrets, workspace, secret_store| {
                complete_oauth_acquisition(self, auth, scope, secrets, workspace, secret_store)
            }),
        }
    }
}

/// Work out the exchange `auth` needs for a token, before anything blocks.
fn plan_oauth_acquisition(
    auth: &AuthConfig,
    allow_private_network: bool,
    headers: Vec<(String, String)>,
) -> Result<OAuthAcquisition, String> {
    let token_request = |flow: &str, token_endpoint: &str, client_id: &str, scopes: &[String]| {
        transport::OAuthTokenRequest {
            flow: flow.into(),
            token_url: token_endpoint.to_string(),
            headers: headers.clone(),
            client_id: client_id.to_string(),
            client_secret: None,
            scope: (!scopes.is_empty()).then(|| scopes.join(" ")),
            code: None,
            redirect_uri: None,
            code_verifier: None,
            refresh_token: None,
            username: None,
            password: None,
            expected_state: None,
            callback_state: None,
            allow_private_network,
        }
    };
    match auth {
        AuthConfig::OAuth2ClientCredentials {
            token_endpoint,
            client_id,
            client_secret,
            scopes,
            ..
        } => Ok(OAuthAcquisition::ClientCredentials {
            request: token_request("client_credentials", token_endpoint, client_id, scopes),
            secret: client_secret.clone(),
        }),
        AuthConfig::OAuth2AuthorizationCodePkce {
            token_endpoint,
            client_id,
            scopes,
            refresh_token: Some(refresh_token),
            ..
        } => Ok(OAuthAcquisition::Refresh {
            request: token_request("refresh_token", token_endpoint, client_id, scopes),
            secret: refresh_token.clone(),
            client_secret: None,
        }),
        AuthConfig::OAuth2Password {
            token_endpoint,
            client_id,
            client_secret,
            username,
            password,
            scopes,
            ..
        } => {
            let mut request = token_request("password", token_endpoint, client_id, scopes);
            request.username = Some(username.clone());
            Ok(OAuthAcquisition::Password {
                request,
                client_secret: client_secret.clone(),
                password: password.clone(),
            })
        }
        AuthConfig::OAuth2AuthorizationCodePkce {
            authorization_endpoint,
            token_endpoint,
            client_id,
            scopes,
            redirect_uri,
            refresh_token: None,
            ..
        } => {
            let flow = transport::begin_pkce_authorization(
                authorization_endpoint,
                client_id,
                scopes,
                redirect_uri,
            )?;
            let mut request = token_request("authorization_code", token_endpoint, client_id, scopes);
            request.redirect_uri = Some(redirect_uri.clone());
            Ok(OAuthAcquisition::Pkce { flow, request })
        }
        AuthConfig::OAuth2 { .. } => {
            Err("This already uses a manually supplied OAuth bearer token.".into())
        }
        _ => Err(
            "Choose OAuth 2 and configure flow=client_credentials, flow=password or flow=authorization_code_pkce first."
                .into(),
        ),
    }
}

/// Run a planned acquisition to completion (blocking — a PKCE grant waits
/// up to five minutes for the browser callback) and cache the token on
/// `auth` under `scope`.
fn complete_oauth_acquisition(
    acquisition: OAuthAcquisition,
    auth: &mut AuthConfig,
    scope: &str,
    secrets: &draft::DraftSecrets,
    workspace: &WorkspaceId,
    secret_store: &dyn switchyard_api::SecretStore,
) -> Result<(), String> {
    let token = match acquisition {
        OAuthAcquisition::ClientCredentials {
            mut request,
            secret,
        } => {
            request.client_secret = Some(secrets.resolve(&secret)?);
            transport::exchange_oauth_token(request)
        }
        OAuthAcquisition::Password {
            mut request,
            client_secret,
            password,
        } => {
            request.client_secret = client_secret
                .map(|secret| secrets.resolve(&secret))
                .transpose()?;
            request.password = Some(secrets.resolve(&password)?);
            if let Some(username) = request.username.as_mut()
                && let Some(reference) = switchyard_api::vault::parse_vault_expression(username)?
            {
                *username = secrets.resolve(&reference)?;
            }
            transport::exchange_oauth_token(request)
        }
        OAuthAcquisition::Refresh {
            mut request,
            secret,
            client_secret,
        } => {
            request.client_secret = client_secret
                .map(|secret| secrets.resolve(&secret))
                .transpose()?;
            request.refresh_token = Some(secrets.resolve(&secret)?);
            transport::exchange_oauth_token(request)
        }
        OAuthAcquisition::Pkce { flow, mut request } => {
            let expected_state = flow.expected_state().to_string();
            let (code, verifier, callback_state) =
                flow.wait_for_callback(std::time::Duration::from_secs(300))?;
            request.code = Some(code);
            request.code_verifier = Some(verifier);
            request.expected_state = Some(expected_state);
            request.callback_state = Some(callback_state);
            transport::exchange_oauth_token(request)
        }
    }?;
    store_oauth_token(auth, scope, &token, workspace, secret_store, now_seconds())?;
    Ok(())
}

/// The browser grant a send has to finish first: a PKCE client with no
/// token, or an expired one it cannot refresh. Every other managed cache
/// is fetched or renewed silently by the token endpoint.
fn plan_browser_authorization(
    input: &StandaloneSendInput,
    now: i64,
) -> Result<Option<OAuthAcquisition>, String> {
    let (auth, _) = send_auth_owner(input);
    if !pkce_needs_browser(auth, now) {
        return Ok(None);
    }
    let (environment, variable_secrets) =
        draft::parse_session_variables(&input.environment_source, &input.environment_scope)?;
    let mut secrets = input.secrets.clone();
    secrets.merge(variable_secrets);
    let folders = input.folders.iter().collect::<Vec<_>>();
    let globals = input
        .store
        .global_variables(&input.workspace)
        .map_err(|e| e.to_string())?;
    let variables = OAuthVariables {
        globals: &globals,
        collection: input.collection.as_ref(),
        folders: &folders,
        environment: &environment,
        base_url: Some(&input.environment_base_url),
    };
    let headers = variables.headers(auth, &input.definition.variables, &secrets)?;
    let resolved_auth = variables.resolve_auth(auth, &input.definition.variables, &secrets)?;
    plan_oauth_acquisition(
        &resolved_auth,
        input.definition.settings.allow_private_network,
        headers,
    )
    .map(Some)
}

fn current_workspace_id() -> WorkspaceId {
    workspace_id_for(crate::api::compat::current_project().as_deref())
}

#[cfg(not(test))]
fn workbench_data_dir() -> Option<std::path::PathBuf> {
    crate::api::compat::settings::user_data_dir()
}

#[cfg(test)]
fn workbench_data_dir() -> Option<std::path::PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    Some(std::env::temp_dir().join(format!(
        "agentops-gpui-workbench-panel-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )))
}

fn set_input(
    input: &(impl TextValue + ?Sized),
    value: &str,
    window: &mut Window,
    cx: &mut Context<WorkbenchPanel>,
) {
    input.set_text(value.to_string(), window, cx);
}

fn format_rows(rows: &[switchyard_api::KeyValueRow], separator: char) -> String {
    rows.iter()
        .map(|row| {
            format!(
                "{}{}{}{}",
                if row.enabled { "" } else { "# " },
                row.key,
                separator,
                row.value
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_variables(variables: &[Variable]) -> String {
    variables
        .iter()
        .map(|variable| {
            let (key, value) = match &variable.value {
                switchyard_api::VariableValue::Plain(value) => {
                    (variable.key.clone(), value.clone())
                }
                switchyard_api::VariableValue::Secret(reference) => (
                    format!("secret:{}", variable.key),
                    switchyard_api::vault::vault_reference_expression(reference)
                        .unwrap_or_default(),
                ),
                switchyard_api::VariableValue::MissingSecret(_) => {
                    (format!("secret:{}", variable.key), String::new())
                }
            };
            format!(
                "{}{}={value}",
                if variable.enabled { "" } else { "# " },
                key
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_body(body: &Body) -> (draft::BodyMode, String) {
    match body {
        Body::None => (draft::BodyMode::None, String::new()),
        Body::Raw { media_type, text } => (
            match media_type {
                switchyard_api::RawBodyKind::Json => draft::BodyMode::Json,
                switchyard_api::RawBodyKind::Xml => draft::BodyMode::Xml,
                switchyard_api::RawBodyKind::Text => draft::BodyMode::Text,
            },
            text.clone(),
        ),
        Body::UrlEncoded { rows } => (draft::BodyMode::Form, format_rows(rows, '=')),
        Body::Multipart { rows } => (
            draft::BodyMode::Multipart,
            rows.iter()
                .map(|row| {
                    let value = match &row.value {
                        switchyard_api::MultipartValue::Text(value) => value.clone(),
                        switchyard_api::MultipartValue::File(path) => {
                            format!("file:{path}")
                        }
                    };
                    format!("{}{}={value}", if row.enabled { "" } else { "# " }, row.key)
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        Body::Binary { path } => (draft::BodyMode::Binary, path.clone()),
        Body::GraphQl { query, variables } => (
            draft::BodyMode::GraphQl,
            body_editor::encode_graphql(query, variables),
        ),
    }
}

fn auth_mode(auth: &switchyard_api::AuthConfig) -> draft::AuthMode {
    match auth {
        switchyard_api::AuthConfig::Inherit => draft::AuthMode::Inherit,
        switchyard_api::AuthConfig::None | switchyard_api::AuthConfig::Unsupported { .. } => {
            draft::AuthMode::None
        }
        switchyard_api::AuthConfig::ApiKey { location, .. } => match location {
            switchyard_api::ApiKeyLocation::Header => draft::AuthMode::ApiKeyHeader,
            switchyard_api::ApiKeyLocation::Query => draft::AuthMode::ApiKeyQuery,
        },
        switchyard_api::AuthConfig::Basic { .. }
        | switchyard_api::AuthConfig::Login { basic: Some(_), .. } => draft::AuthMode::Basic,
        switchyard_api::AuthConfig::Bearer { .. } => draft::AuthMode::Bearer,
        switchyard_api::AuthConfig::OAuth2 { .. }
        | switchyard_api::AuthConfig::OAuth2AuthorizationCodePkce { .. }
        | switchyard_api::AuthConfig::OAuth2ClientCredentials { .. }
        | switchyard_api::AuthConfig::OAuth2Password { .. } => draft::AuthMode::OAuth2,
        switchyard_api::AuthConfig::AwsSigV4 { .. } => draft::AuthMode::AwsSigV4,
        switchyard_api::AuthConfig::Login { .. } => draft::AuthMode::Login,
    }
}

/// A rail row the inline rename field can open on — chosen from the row
/// itself, so it never depends on which folder happens to be selected.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RenameTarget {
    Collection(CollectionId),
    Folder(FolderId),
    Request(RequestId),
}

/// One row of the collection rail's request tree.
#[derive(Clone, Copy, Debug, PartialEq)]
enum RailRow<'a> {
    Folder {
        depth: usize,
        folder: &'a switchyard_api::Folder,
    },
    Request {
        depth: usize,
        request: &'a SavedRequest,
    },
}

fn expanded_rail_rows<'a>(
    rows: Vec<RailRow<'a>>,
    collapsed: &HashSet<FolderId>,
) -> Vec<RailRow<'a>> {
    let mut hidden_depth = None;
    rows.into_iter()
        .filter(|row| {
            let depth = match row {
                RailRow::Folder { depth, .. } | RailRow::Request { depth, .. } => *depth,
            };
            if hidden_depth.is_some_and(|hidden| depth > hidden) {
                return false;
            }
            hidden_depth = match row {
                RailRow::Folder { depth, folder } if collapsed.contains(&folder.id) => Some(*depth),
                _ => None,
            };
            true
        })
        .collect()
}

fn folder_request_counts(rows: &[RailRow<'_>]) -> std::collections::HashMap<FolderId, usize> {
    let mut counts = std::collections::HashMap::new();
    let mut ancestors: Vec<(usize, FolderId)> = Vec::new();
    for row in rows {
        let depth = match row {
            RailRow::Folder { depth, .. } | RailRow::Request { depth, .. } => *depth,
        };
        while ancestors
            .last()
            .is_some_and(|(parent_depth, _)| *parent_depth >= depth)
        {
            ancestors.pop();
        }
        match row {
            RailRow::Folder { folder, .. } => {
                counts.entry(folder.id.clone()).or_insert(0);
                ancestors.push((depth, folder.id.clone()));
            }
            RailRow::Request { .. } => {
                for (_, id) in &ancestors {
                    *counts.entry(id.clone()).or_insert(0) += 1;
                }
            }
        }
    }
    counts
}

/// The rail's request tree, scoped to one collection.
///
/// Root requests come first, then a depth-first walk of the folders, each
/// followed by its own requests. Everything is ordered by `sort_key` so the
/// rail matches import-document order. With no collection selected (a fresh
/// workspace) every request is listed. A `parent_id` cycle terminates: a
/// folder is visited at most once.
fn rail_rows<'a>(
    data: &'a persistence::WorkspaceData,
    collection: Option<&CollectionId>,
) -> Vec<RailRow<'a>> {
    let in_scope_request =
        |request: &&'a SavedRequest| collection.is_none_or(|id| &request.collection_id == id);
    let in_scope_folder = |folder: &&'a switchyard_api::Folder| {
        collection.is_none_or(|id| &folder.collection_id == id)
    };
    let mut requests = data
        .requests
        .iter()
        .filter(in_scope_request)
        .collect::<Vec<_>>();
    requests.sort_by_key(|request| request.sort_key);
    let mut folders = data
        .folders
        .iter()
        .filter(in_scope_folder)
        .collect::<Vec<_>>();
    folders.sort_by_key(|folder| folder.sort_key);
    // Index once after sorting. Walking every request and every folder for
    // each expanded folder made hover redraws quadratic in collection size.
    // Buckets keep the stable sort order, including equal sort keys.
    let known_folders: HashSet<_> = folders.iter().map(|folder| &folder.id).collect();
    let mut requests_by_folder = std::collections::HashMap::new();
    for request in &requests {
        let parent = request
            .folder_id
            .as_ref()
            .filter(|id| known_folders.contains(id));
        requests_by_folder
            .entry(parent)
            .or_insert_with(Vec::new)
            .push(*request);
    }
    let mut folders_by_parent = std::collections::HashMap::new();
    for folder in &folders {
        let parent = folder
            .parent_id
            .as_ref()
            .filter(|id| known_folders.contains(id));
        folders_by_parent
            .entry(parent)
            .or_insert_with(Vec::new)
            .push(*folder);
    }

    let mut rows = Vec::with_capacity(requests.len() + folders.len());
    // Requests whose folder is unknown to this collection are shown at the
    // root rather than silently dropped.
    rows.extend(
        requests_by_folder
            .get(&None)
            .into_iter()
            .flatten()
            .map(|request| RailRow::Request { depth: 0, request }),
    );
    let mut visited = std::collections::HashSet::new();
    fn walk<'a>(
        parent: Option<&FolderId>,
        depth: usize,
        folders: &std::collections::HashMap<Option<&'a FolderId>, Vec<&'a switchyard_api::Folder>>,
        requests: &std::collections::HashMap<Option<&'a FolderId>, Vec<&'a SavedRequest>>,
        visited: &mut std::collections::HashSet<FolderId>,
        rows: &mut Vec<RailRow<'a>>,
    ) {
        for folder in folders.get(&parent).into_iter().flatten() {
            if !visited.insert(folder.id.clone()) {
                continue;
            }
            rows.push(RailRow::Folder { depth, folder });
            rows.extend(
                requests
                    .get(&Some(&folder.id))
                    .into_iter()
                    .flatten()
                    .map(|request| RailRow::Request {
                        depth: depth + 1,
                        request,
                    }),
            );
            walk(
                Some(&folder.id),
                depth + 1,
                folders,
                requests,
                visited,
                rows,
            );
        }
    }
    walk(
        None,
        0,
        &folders_by_parent,
        &requests_by_folder,
        &mut visited,
        &mut rows,
    );
    // A folder the walk never reached — only possible through a `parent_id`
    // cycle in a hand-edited database — is still listed, at the root, so its
    // requests are never hidden.
    for folder in folders.iter() {
        if !visited.insert(folder.id.clone()) {
            continue;
        }
        rows.push(RailRow::Folder { depth: 0, folder });
        rows.extend(
            requests_by_folder
                .get(&Some(&folder.id))
                .into_iter()
                .flatten()
                .map(|request| RailRow::Request { depth: 1, request }),
        );
        walk(
            Some(&folder.id),
            1,
            &folders_by_parent,
            &requests_by_folder,
            &mut visited,
            &mut rows,
        );
    }
    rows
}

/// A file-name-safe stem for an export named after its collection or folder.
fn export_file_stem(name: &str) -> String {
    let stem = name
        .trim()
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || "-_.".contains(character) {
                character
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let stem = stem.trim_matches('.');
    if stem.is_empty() {
        "agentops-api-workbench-export".into()
    } else {
        stem.chars().take(80).collect()
    }
}

fn select_folder_ancestors(
    imported: &ImportResult,
    folder_id: &FolderId,
    selection: &mut ImportSelection,
) {
    let mut current = Some(folder_id);
    while let Some(id) = current {
        selection.folder_ids.insert(id.clone());
        current = imported
            .folders
            .iter()
            .find(|folder| &folder.id == id)
            .and_then(|folder| folder.parent_id.as_ref());
    }
}

fn data_summary(input: &str) -> String {
    let input = input.trim();
    if input.is_empty() {
        return "No iteration data loaded.".into();
    }
    if input.starts_with('[') {
        return match serde_json::from_str::<serde_json::Value>(input) {
            Ok(serde_json::Value::Array(rows)) => {
                format!("JSON · {} iteration rows ready", rows.len())
            }
            Ok(_) => "JSON data must be an array of objects.".into(),
            Err(error) => format!("Invalid JSON data: {error}"),
        };
    }
    let rows = input.lines().filter(|line| !line.trim().is_empty()).count();
    if rows < 2 {
        "CSV needs a header and at least one data row.".into()
    } else {
        format!(
            "CSV · {} columns · {} iteration rows ready",
            input
                .lines()
                .next()
                .map(|line| line.split(',').count())
                .unwrap_or(0),
            rows - 1
        )
    }
}

fn response_diff(before: &HistoryEntry, after: &HistoryEntry) -> Vec<String> {
    diff_exchanges(&before.exchange, &after.exchange)
        .entries
        .into_iter()
        .map(|entry| {
            let prefix = match entry.kind {
                switchyard_api::DiffKind::Added => '+',
                switchyard_api::DiffKind::Removed => '-',
                switchyard_api::DiffKind::Changed => '~',
                switchyard_api::DiffKind::Indeterminate => '?',
            };
            format!(
                "{prefix} {}: {} -> {}",
                entry.path,
                entry.before.unwrap_or_else(|| "∅".into()),
                entry.after.unwrap_or_else(|| "∅".into())
            )
        })
        .collect()
}

/// Both presentations of one selected pair are prepared away from rendering.
/// The generation check in `schedule_diff` commits them together.
struct PreparedDiff {
    changes: Vec<String>,
    left: Vec<pretty::DiffLine>,
    right: Vec<pretty::DiffLine>,
    added: usize,
    removed: usize,
}

impl PreparedDiff {
    fn new(before: &HistoryEntry, after: &HistoryEntry) -> Self {
        let changes = response_diff(before, after);
        let (left, right) = pretty::line_diff(&before.response.body, &after.response.body);
        let added = right
            .iter()
            .filter(|line| matches!(line, pretty::DiffLine::Added(_)))
            .count();
        let removed = left
            .iter()
            .filter(|line| matches!(line, pretty::DiffLine::Removed(_)))
            .count();
        Self {
            changes,
            left,
            right,
            added,
            removed,
        }
    }
}

fn history_matches(entry: &HistoryEntry, filter: &str) -> bool {
    filter.is_empty()
        || entry.method.to_lowercase().contains(filter)
        || entry.target.to_lowercase().contains(filter)
        || entry.response.status.to_string().contains(filter)
        || entry
            .exchange
            .error
            .as_deref()
            .is_some_and(|error| error.to_lowercase().contains(filter))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use switchyard_api::runtime::StandaloneSendResult;
    use switchyard_api::runtime::collection::prepare_run_request;
    use switchyard_api::runtime::oauth::{
        refresh_expired_oauth_auth_with, refresh_expired_oauth_token_with,
    };
    use switchyard_api::runtime::send::{
        apply_script_cookie_mutations, carry_shared_script_scopes, cookie_scope_for_url,
        request_script_scopes, script_variables,
    };
    use switchyard_api::{HttpMethod, RequestSettings, RowId, SecretRef};

    #[test]
    fn workbench_exposes_the_six_specified_tabs_in_order() {
        assert_eq!(
            Tab::ALL.map(Tab::id),
            ["compose", "import", "data", "envs", "history", "diff"]
        );
        assert_eq!(
            SNIPPET_LANGUAGES.map(|(_, label)| label),
            [
                "cURL",
                "HTTP",
                "Rust",
                "JavaScript",
                "TypeScript",
                "Python",
                "PowerShell",
                "C#"
            ]
        );
    }

    fn rail_fixture(name: &str) -> (persistence::WorkspaceData, CollectionId, CollectionId) {
        let path = std::env::temp_dir().join(format!(
            "agentops-gpui-workbench-rail-rows-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        let mut data = persistence::WorkspaceData::open(&path, current_workspace_id()).unwrap();
        let first = data.ensure_collection().unwrap();
        let second = Collection {
            id: CollectionId::new(),
            workspace_id: data.workspace.clone(),
            name: "Other".into(),
            description: String::new(),
            auth: AuthConfig::None,
            variables: Vec::new(),
            scripts: Scripts::default(),
            extensions: Default::default(),
        };
        let second_id = second.id.clone();
        data.save_collection(second).unwrap();
        (data, first, second_id)
    }

    fn folder(
        collection: &CollectionId,
        parent: Option<&FolderId>,
        name: &str,
        sort: i64,
    ) -> Folder {
        Folder {
            id: FolderId::new(),
            collection_id: collection.clone(),
            parent_id: parent.cloned(),
            name: name.into(),
            auth: AuthConfig::Inherit,
            variables: Vec::new(),
            scripts: Scripts::default(),
            sort_key: sort,
            extensions: Default::default(),
        }
    }

    fn request(
        collection: &CollectionId,
        folder: Option<&FolderId>,
        name: &str,
        sort: i64,
    ) -> SavedRequest {
        SavedRequest {
            id: RequestId::new(),
            collection_id: collection.clone(),
            folder_id: folder.cloned(),
            name: name.into(),
            method: HttpMethod::new("GET").unwrap(),
            url: format!("https://example.test/{name}"),
            params: Vec::new(),
            headers: Vec::new(),
            auth: AuthConfig::Inherit,
            body: Body::None,
            variables: Vec::new(),
            scripts: Scripts::default(),
            settings: RequestSettings::default(),
            extensions: Default::default(),
            sort_key: sort,
        }
    }

    fn rail_names(rows: &[RailRow<'_>]) -> Vec<(usize, String)> {
        rows.iter()
            .map(|row| match row {
                RailRow::Folder { depth, folder } => (*depth, format!("[{}]", folder.name)),
                RailRow::Request { depth, request } => (*depth, request.name.clone()),
            })
            .collect()
    }

    #[test]
    fn collapsed_folders_hide_descendants_and_keep_nested_expansion_choices() {
        let (mut data, collection, _) = rail_fixture("collapse");
        let parent = folder(&collection, None, "Users", 0);
        let child = folder(&collection, Some(&parent.id), "Admin", 0);
        let sibling = folder(&collection, None, "Orders", 1);
        for folder in [&parent, &child, &sibling] {
            data.save_folder(folder.clone()).unwrap();
        }
        for (folder, name) in [
            (&parent, "List users"),
            (&child, "Ban user"),
            (&sibling, "List orders"),
        ] {
            data.save_request(request(&collection, Some(&folder.id), name, 0))
                .unwrap();
        }
        let rows = rail_rows(&data, Some(&collection));
        let counts = folder_request_counts(&rows);
        assert_eq!(counts[&parent.id], 2);
        assert_eq!(counts[&child.id], 1);
        let mut collapsed = HashSet::from([parent.id.clone(), child.id.clone()]);
        assert_eq!(
            rail_names(&expanded_rail_rows(rows.clone(), &collapsed)),
            vec![
                (0, "[Users]".into()),
                (0, "[Orders]".into()),
                (1, "List orders".into()),
            ]
        );
        collapsed.remove(&parent.id);
        assert_eq!(
            rail_names(&expanded_rail_rows(rows, &collapsed)),
            vec![
                (0, "[Users]".into()),
                (1, "List users".into()),
                (1, "[Admin]".into()),
                (0, "[Orders]".into()),
                (1, "List orders".into()),
            ]
        );
    }

    #[test]
    fn rail_rows_are_scoped_to_the_selected_collection_and_nested_by_folder() {
        let (mut data, first, second) = rail_fixture("scoped");
        let users = folder(&first, None, "Users", 1);
        let admin = folder(&first, Some(&users.id), "Admin", 0);
        let orders = folder(&first, None, "Orders", 0);
        data.save_folder(users.clone()).unwrap();
        data.save_folder(admin.clone()).unwrap();
        data.save_folder(orders.clone()).unwrap();
        data.save_request(request(&first, None, "Health", 5))
            .unwrap();
        data.save_request(request(&first, None, "Ping", 1)).unwrap();
        data.save_request(request(&first, Some(&users.id), "List users", 0))
            .unwrap();
        data.save_request(request(&first, Some(&admin.id), "Ban user", 0))
            .unwrap();
        data.save_request(request(&first, Some(&orders.id), "List orders", 0))
            .unwrap();
        data.save_request(request(&second, None, "Other root", 0))
            .unwrap();

        let rows = rail_rows(&data, Some(&first));
        assert_eq!(
            rail_names(&rows),
            vec![
                (0, "Ping".to_string()),
                (0, "Health".to_string()),
                (0, "[Orders]".to_string()),
                (1, "List orders".to_string()),
                (0, "[Users]".to_string()),
                (1, "List users".to_string()),
                (1, "[Admin]".to_string()),
                (2, "Ban user".to_string()),
            ]
        );
        assert_eq!(
            rail_names(&rail_rows(&data, Some(&second))),
            vec![(0, "Other root".to_string())]
        );
        // No selection: everything, so a fresh workspace never hides a request.
        assert_eq!(rail_rows(&data, None).len(), 9);
    }

    #[test]
    fn rail_rows_keep_stable_order_across_a_large_collection() {
        let (mut data, collection, _) = rail_fixture("large-rail");
        let mut expected = Vec::new();
        // Equal sort keys intentionally exercise stable ordering in the
        // indexed buckets. Payloads are borrowed even for large requests.
        for folder_index in 0..128 {
            let group = folder(&collection, None, &format!("Group {folder_index}"), 0);
            expected.push((0, format!("[{}]", group.name)));
            for request_index in 0..16 {
                let name = format!("Request {folder_index}/{request_index}");
                data.requests
                    .push(request(&collection, Some(&group.id), &name, 0));
                expected.push((1, name));
            }
            data.folders.push(group);
        }
        let rows = rail_rows(&data, Some(&collection));
        assert_eq!(rail_names(&rows), expected);
        for row in rows {
            if let RailRow::Request { request, .. } = row {
                assert!(
                    data.requests
                        .iter()
                        .any(|saved| std::ptr::eq(saved, request))
                );
            }
        }
    }

    #[test]
    fn rail_rows_survive_folder_cycles_and_orphaned_folder_ids() {
        let (mut data, first, _) = rail_fixture("cycles");
        let mut a = folder(&first, None, "A", 0);
        let mut b = folder(&first, Some(&a.id), "B", 0);
        a.parent_id = Some(b.id.clone());
        b.parent_id = Some(a.id.clone());
        data.save_request(request(&first, None, "In A", 0)).unwrap();
        data.save_request(request(&first, None, "Orphan", 1))
            .unwrap();
        // The store refuses to persist a cycle or an unknown parent, so the
        // rail sees them only through a corrupted or hand-edited database;
        // model that by editing the hydrated copy.
        data.folders.push(a.clone());
        data.folders.push(b.clone());
        data.requests[0].folder_id = Some(a.id.clone());
        data.requests[1].folder_id = Some(FolderId::new());

        let rows = rail_rows(&data, Some(&first));
        let names = rail_names(&rows);
        assert_eq!(names[0], (0, "Orphan".to_string()));
        assert_eq!(
            names
                .iter()
                .filter(|(_, name)| name.starts_with('['))
                .count(),
            2,
            "each folder is listed exactly once despite the cycle: {names:?}"
        );
        assert!(
            names.contains(&(1, "In A".to_string())) || names.contains(&(2, "In A".to_string()))
        );
    }

    #[test]
    fn runner_preparation_compiles_each_data_iteration_without_transport_io() {
        let workspace = WorkspaceId::new("/test/runner-preparation").unwrap();
        let path = std::env::temp_dir().join(format!(
            "agentops-gpui-run-preparation-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        let data = persistence::WorkspaceData::open(&path, workspace.clone()).unwrap();
        let run_store = data.store.clone();
        let runner_secret_store: Arc<dyn switchyard_api::SecretStore> =
            Arc::new(switchyard_api::MemorySecretStore::new());
        let collection = Collection {
            id: CollectionId::new(),
            workspace_id: workspace.clone(),
            name: "Runner".into(),
            description: String::new(),
            auth: AuthConfig::None,
            variables: Vec::new(),
            scripts: Scripts::default(),
            extensions: Default::default(),
        };
        let request = SavedRequest {
            id: RequestId::new(),
            collection_id: collection.id.clone(),
            folder_id: None,
            name: "Iteration".into(),
            method: HttpMethod::get(),
            url: "https://example.test/{{iteration}}".into(),
            params: Vec::new(),
            headers: Vec::new(),
            auth: AuthConfig::None,
            body: Body::None,
            variables: Vec::new(),
            scripts: Scripts::default(),
            settings: RequestSettings::default(),
            extensions: Default::default(),
            sort_key: 0,
        };
        let cookie_jar = switchyard_api::CookieJar::new(workspace.clone());
        let prepared = prepare_collection_run(
            RunPreparationInput {
                workspace_id: workspace,
                collection_id: collection.id.clone(),
                environment_id: None,
                environment: None,
                stop_on_error: true,
                selected_folder_id: None,
                selected_request_ids: Vec::new(),
                collection: Some(collection),
                folders: Vec::new(),
                requests: vec![request],
                environment_source: String::new(),
                environment_scope: "test.environment".into(),
                environment_base_url: String::new(),
                environment_auth: AuthConfig::None,
                data_source: r#"[{"iteration":"one"},{"iteration":"two"}]"#.into(),
                secrets: draft::DraftSecrets::default(),
                iteration_limit: None,
                delay_ms: 0,
                keep_variables: true,
                store: data.store,
                secret_store: runner_secret_store,
                cookie_jar,
                file_capabilities: transport::FileCapabilities::default(),
            },
            &transport::NativeWorkbenchTransport::from_env(),
        )
        .unwrap();
        assert_eq!(prepared.run.iteration_count, 2);
        assert!(prepared.run.stop_on_error);
        assert_eq!(prepared.rows.len(), 2);
        assert_eq!(prepared.requests.len(), 1);
        let secret_store: Arc<dyn switchyard_api::SecretStore> =
            Arc::new(switchyard_api::MemorySecretStore::new());
        let cookie_jar = prepared.cookie_jar.clone();
        let runner_scopes = transport::ScriptScopes {
            environment: script_variables(&[&prepared.environment]),
            collection: prepared
                .collection
                .as_ref()
                .map(|collection| script_variables(&[&collection.variables]))
                .unwrap_or_default(),
            ..Default::default()
        };
        let first = prepare_run_request(
            prepared.requests[0].clone(),
            prepared.collection.clone(),
            prepared.folders.clone(),
            (String::new(), AuthConfig::None),
            prepared.rows[0].clone(),
            runner_scopes,
            prepared.secrets.clone(),
            cookie_jar.clone(),
            run_store.clone(),
            secret_store.clone(),
            "runner-operation-one".into(),
            Arc::new(transport::NativeWorkbenchTransport::from_env()),
            Vec::new(),
        )
        .unwrap();
        let second = prepare_run_request(
            prepared.requests[0].clone(),
            prepared.collection.clone(),
            prepared.folders.clone(),
            (String::new(), AuthConfig::None),
            prepared.rows[1].clone(),
            first.2.clone(),
            prepared.secrets.clone(),
            first.5.clone(),
            run_store,
            secret_store,
            "runner-operation-two".into(),
            Arc::new(transport::NativeWorkbenchTransport::from_env()),
            Vec::new(),
        )
        .unwrap();
        assert_eq!(first.0.url, "https://example.test/one");
        assert_eq!(second.0.url, "https://example.test/two");
    }

    #[test]
    fn expired_client_credentials_token_is_refreshed_and_persisted_before_compile() {
        let workspace = WorkspaceId::new("/test/oauth-refresh").unwrap();
        let path = std::env::temp_dir().join(format!(
            "agentops-gpui-oauth-refresh-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        let mut data = persistence::WorkspaceData::open(&path, workspace.clone()).unwrap();
        let collection_id = data.ensure_collection().unwrap();
        let secret_store: Arc<dyn switchyard_api::SecretStore> =
            Arc::new(switchyard_api::MemorySecretStore::new());
        let request_id = RequestId::new();
        let client_secret = SecretRef::new(format!(
            "workbench.request.{}.auth.client_secret",
            request_id.as_str()
        ))
        .unwrap();
        let stale_access = SecretRef::new(format!(
            "workbench.request.{}.auth.token",
            request_id.as_str()
        ))
        .unwrap();
        let refresh_now = now_seconds();
        let mut request = SavedRequest {
            id: request_id.clone(),
            collection_id: collection_id.clone(),
            folder_id: None,
            name: "OAuth".into(),
            method: HttpMethod::get(),
            url: "https://example.test/private".into(),
            params: Vec::new(),
            headers: Vec::new(),
            auth: AuthConfig::OAuth2ClientCredentials {
                headers: Vec::new(),
                token_endpoint: "https://identity.example.test/token".into(),
                client_id: "desktop-client".into(),
                client_secret,
                scopes: vec!["read".into(), "write".into()],
                access_token: Some(stale_access),
                // Inside the 30-second refresh safety window.
                expires_at: Some(refresh_now.saturating_add(25)),
            },
            body: Body::None,
            variables: Vec::new(),
            scripts: Scripts::default(),
            settings: RequestSettings::default(),
            extensions: Default::default(),
            sort_key: 0,
        };
        data.store.upsert_request(&request).unwrap();
        let (_, editor_secrets) = draft::build(draft::DraftInput {
            request_id: Some(request_id),
            collection_id,
            name: "OAuth",
            method: "GET",
            url: "https://example.test/private",
            params: "",
            headers: "",
            cookies: "",
            body_mode: draft::BodyMode::None,
            body: "",
            auth_mode: draft::AuthMode::OAuth2,
            auth: "flow=client_credentials\ntoken_endpoint=https://identity.example.test/token\nclient_id=desktop-client\nclient_secret=client-secret-value\nscopes=read write\ntoken=stale-token",
            variables: "",
            pre_request_script: "",
            test_script: "",
            allow_private_network: false,
        })
        .unwrap();
        let mut secrets = draft::DraftSecrets::with_store(secret_store.clone(), workspace.clone());
        secrets.merge(editor_secrets);
        secrets.persist(secret_store.as_ref(), &workspace).unwrap();

        assert!(
            refresh_expired_oauth_token_with(
                &mut request,
                &mut secrets,
                &workspace,
                data.store.as_ref(),
                secret_store.as_ref(),
                refresh_now,
                |token_request| {
                    assert_eq!(token_request.flow, "client_credentials");
                    assert_eq!(
                        token_request.client_secret.as_deref(),
                        Some("client-secret-value")
                    );
                    assert_eq!(token_request.scope.as_deref(), Some("read write"));
                    Ok(transport::OAuthTokenResponse {
                        access_token: "fresh-token".into(),
                        expires_in: Some(60),
                        refresh_token: None,
                    })
                },
            )
            .unwrap()
        );

        let AuthConfig::OAuth2ClientCredentials {
            access_token: Some(access_ref),
            expires_at,
            ..
        } = &request.auth
        else {
            panic!("expected refreshed client credentials auth");
        };
        assert_eq!(*expires_at, Some(refresh_now.saturating_add(60)));
        assert_eq!(
            secret_store
                .get_secret(&workspace, access_ref)
                .unwrap()
                .expose_secret(),
            "fresh-token"
        );
        let (compiled, _) = switchyard_api::compile_request(
            &request,
            None,
            &switchyard_api::CompileContext {
                global: &[],
                environment: &[],
                data: &[],
                local: &[],
                secrets: &secrets,
                environment_base_url: None,
                environment_auth: None,
            },
        )
        .unwrap();
        assert!(compiled.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("authorization") && value == "Bearer fresh-token"
        }));
        let reopened = persistence::WorkspaceData::hydrate(data.store, workspace).unwrap();
        assert!(reopened.requests.iter().any(|saved| saved == &request));
    }

    #[test]
    fn a_password_grant_signs_the_user_in_on_first_send_and_refreshes_silently() {
        let secret_store: Arc<dyn switchyard_api::SecretStore> =
            Arc::new(switchyard_api::MemorySecretStore::new());
        let workspace = WorkspaceId::new("/test/env-oauth-owner").unwrap();
        let environment_id = EnvironmentId::new();
        let scope = format!("environment.{}", environment_id.as_str());
        let text = "flow=password\ntoken_endpoint=https://identity.example.test/token\nclient_id=desktop-client\nclient_secret=client-secret-value\nusername=jane@example.test\npassword=p&ss w\nscopes=web_api";
        let (mut auth, editor_secrets) =
            draft::parse_auth(draft::AuthMode::OAuth2, text, &scope).unwrap();
        let mut secrets = draft::DraftSecrets::with_store(secret_store.clone(), workspace.clone());
        secrets.merge(editor_secrets);
        secrets.persist(secret_store.as_ref(), &workspace).unwrap();
        let saved = auth.clone();
        let now = now_seconds();

        // No token yet: the first send signs the resource owner in.
        assert!(
            refresh_expired_oauth_auth_with(
                &mut auth,
                &scope,
                false,
                &mut secrets,
                &workspace,
                secret_store.as_ref(),
                now,
                |token_request| {
                    assert_eq!(token_request.flow, "password");
                    assert_eq!(token_request.username.as_deref(), Some("jane@example.test"));
                    assert_eq!(token_request.password.as_deref(), Some("p&ss w"));
                    assert_eq!(
                        token_request.client_secret.as_deref(),
                        Some("client-secret-value")
                    );
                    assert_eq!(token_request.scope.as_deref(), Some("web_api"));
                    assert_eq!(token_request.refresh_token, None);
                    Ok(transport::OAuthTokenResponse {
                        access_token: "user-token".into(),
                        expires_in: Some(3_600),
                        refresh_token: Some("renewal".into()),
                    })
                },
            )
            .unwrap()
        );
        let AuthConfig::OAuth2Password {
            access_token: Some(access_ref),
            refresh_token: Some(refresh_ref),
            expires_at,
            ..
        } = &auth
        else {
            panic!("expected a cached user token, got {auth:?}");
        };
        assert_eq!(
            access_ref.as_str(),
            format!(
                "workbench.environment.{}.auth.token",
                environment_id.as_str()
            )
        );
        assert_eq!(*expires_at, Some(now.saturating_add(3_600)));
        assert_eq!(
            secret_store
                .get_secret(&workspace, access_ref)
                .unwrap()
                .expose_secret(),
            "user-token"
        );
        assert_eq!(
            secret_store
                .get_secret(&workspace, refresh_ref)
                .unwrap()
                .expose_secret(),
            "renewal"
        );
        assert!(same_managed_auth_source(&saved, &auth));
        assert_eq!(
            login::session_status(&auth, now).as_deref(),
            Some("token cached · expires in 1 h")
        );

        // Near expiry the refresh token renews it, still as the confidential client.
        let later = now.saturating_add(3_590);
        assert!(
            refresh_expired_oauth_auth_with(
                &mut auth,
                &scope,
                false,
                &mut secrets,
                &workspace,
                secret_store.as_ref(),
                later,
                |token_request| {
                    assert_eq!(token_request.flow, "refresh_token");
                    assert_eq!(token_request.refresh_token.as_deref(), Some("renewal"));
                    assert_eq!(
                        token_request.client_secret.as_deref(),
                        Some("client-secret-value")
                    );
                    assert_eq!(token_request.password, None);
                    Ok(transport::OAuthTokenResponse {
                        access_token: "user-token-2".into(),
                        expires_in: Some(3_600),
                        refresh_token: None,
                    })
                },
            )
            .unwrap()
        );
        let AuthConfig::OAuth2Password {
            access_token: Some(access_ref),
            expires_at,
            ..
        } = &auth
        else {
            panic!("expected a renewed user token, got {auth:?}");
        };
        assert_eq!(*expires_at, Some(later.saturating_add(3_600)));
        assert_eq!(
            secret_store
                .get_secret(&workspace, access_ref)
                .unwrap()
                .expose_secret(),
            "user-token-2"
        );

        // Re-saving the Envs form with the masked fields left blank keeps the
        // vault references and the cache.
        let blank = "flow=password\ntoken_endpoint=https://identity.example.test/token\nclient_id=desktop-client\nusername=jane@example.test\nscopes=web_api";
        let (mut rebuilt, entered) =
            draft::parse_auth(draft::AuthMode::OAuth2, blank, &scope).unwrap();
        preserve_saved_secret_refs(&auth, &mut rebuilt, &entered);
        preserve_managed_oauth_state(&auth, &mut rebuilt);
        assert_eq!(rebuilt, auth);
        assert!(same_managed_auth_source(&auth, &rebuilt));
    }

    #[test]
    fn an_environment_oauth_client_fetches_its_first_token_under_the_environment_scope() {
        let secret_store: Arc<dyn switchyard_api::SecretStore> =
            Arc::new(switchyard_api::MemorySecretStore::new());
        let workspace = WorkspaceId::new("/test/env-oauth").unwrap();
        let environment_id = EnvironmentId::new();
        let scope = format!("environment.{}", environment_id.as_str());
        let (mut auth, editor_secrets) = draft::parse_auth(
            draft::AuthMode::OAuth2,
            "flow=client_credentials\ntoken_endpoint=https://identity.example.test/token\nclient_id=desktop-client\nclient_secret=client-secret-value\nscopes=read",
            &scope,
        )
        .unwrap();
        let mut secrets = draft::DraftSecrets::with_store(secret_store.clone(), workspace.clone());
        secrets.merge(editor_secrets);
        secrets.persist(secret_store.as_ref(), &workspace).unwrap();
        let saved = auth.clone();
        let now = now_seconds();

        // No token yet: the first send fetches one, no button press needed.
        assert!(
            refresh_expired_oauth_auth_with(
                &mut auth,
                &scope,
                false,
                &mut secrets,
                &workspace,
                secret_store.as_ref(),
                now,
                |token_request| {
                    assert_eq!(token_request.flow, "client_credentials");
                    assert_eq!(
                        token_request.client_secret.as_deref(),
                        Some("client-secret-value")
                    );
                    Ok(transport::OAuthTokenResponse {
                        access_token: "env-token".into(),
                        expires_in: Some(600),
                        refresh_token: None,
                    })
                },
            )
            .unwrap()
        );
        let AuthConfig::OAuth2ClientCredentials {
            access_token: Some(access_ref),
            expires_at,
            ..
        } = &auth
        else {
            panic!("expected a cached client-credentials token, got {auth:?}");
        };
        assert_eq!(
            access_ref.as_str(),
            format!(
                "workbench.environment.{}.auth.token",
                environment_id.as_str()
            )
        );
        assert_eq!(*expires_at, Some(now.saturating_add(600)));
        assert_eq!(
            secret_store
                .get_secret(&workspace, access_ref)
                .unwrap()
                .expose_secret(),
            "env-token"
        );
        // The cache belongs on the saved environment it was fetched for…
        assert!(same_managed_auth_source(&saved, &auth));
        let mut other_client = saved.clone();
        if let AuthConfig::OAuth2ClientCredentials { client_id, .. } = &mut other_client {
            *client_id = "another-client".into();
        }
        assert!(!same_managed_auth_source(&other_client, &auth));
        // …and is left alone while it is good.
        assert!(
            !refresh_expired_oauth_auth_with(
                &mut auth,
                &scope,
                false,
                &mut secrets,
                &workspace,
                secret_store.as_ref(),
                now,
                |_| panic!("a fresh token is not exchanged again"),
            )
            .unwrap()
        );
        assert_eq!(
            login::session_status(&auth, now).as_deref(),
            Some("token cached · expires in 10 min")
        );
        // An inheriting request sends exactly the token the identity server
        // issued — the same `Authorization: Bearer …` header the Bearer type
        // would build from a pasted value.
        let request = SavedRequest {
            id: RequestId::new(),
            collection_id: CollectionId::new(),
            folder_id: None,
            name: "Summary".into(),
            method: HttpMethod::new("POST").unwrap(),
            url: "https://api.example.test/api/v1/dashboard/summary".into(),
            params: Vec::new(),
            headers: Vec::new(),
            auth: AuthConfig::Inherit,
            body: Body::None,
            variables: Vec::new(),
            scripts: Scripts::default(),
            settings: RequestSettings::default(),
            extensions: Default::default(),
            sort_key: 0,
        };
        let context = switchyard_api::CompileContext {
            global: &[],
            environment: &[],
            data: &[],
            local: &[],
            secrets: &secrets,
            environment_base_url: None,
            environment_auth: Some(&auth),
        };
        let (prepared, _) = switchyard_api::compile_request(&request, None, &context).unwrap();
        let authorization = prepared
            .headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .map(|(_, value)| value.as_str())
            .collect::<Vec<_>>();
        assert_eq!(authorization, ["Bearer env-token"]);
        let pasted = SavedRequest {
            auth: AuthConfig::Bearer {
                token: SecretRef::new(format!(
                    "workbench.environment.{}.auth.token",
                    environment_id.as_str()
                ))
                .unwrap(),
            },
            ..request
        };
        let (pasted, _) = switchyard_api::compile_request(&pasted, None, &context).unwrap();
        assert_eq!(pasted.headers, prepared.headers);
    }

    #[test]
    fn only_a_pkce_client_without_a_usable_token_needs_the_browser_on_send() {
        let now = now_seconds();
        let (pkce, _) = draft::parse_auth(
            draft::AuthMode::OAuth2,
            "flow=authorization_code_pkce\nauthorization_endpoint=https://identity.example.test/authorize\ntoken_endpoint=https://identity.example.test/token\nclient_id=desktop-client\nredirect_uri=http://127.0.0.1:0/callback",
            "request.r1",
        )
        .unwrap();
        assert!(pkce_needs_browser(&pkce, now), "no token yet");
        let mut cached = pkce.clone();
        store_oauth_token(
            &mut cached,
            "request.r1",
            &transport::OAuthTokenResponse {
                access_token: "t".into(),
                expires_in: Some(600),
                refresh_token: None,
            },
            &WorkspaceId::new("/test/pkce").unwrap(),
            &switchyard_api::MemorySecretStore::new(),
            now,
        )
        .unwrap();
        assert!(!pkce_needs_browser(&cached, now), "a good token is reused");
        assert!(
            pkce_needs_browser(&cached, now + 600),
            "an expired token with no refresh token goes back to the browser"
        );
        let mut refreshable = pkce.clone();
        store_oauth_token(
            &mut refreshable,
            "request.r1",
            &transport::OAuthTokenResponse {
                access_token: "t".into(),
                expires_in: Some(1),
                refresh_token: Some("r".into()),
            },
            &WorkspaceId::new("/test/pkce").unwrap(),
            &switchyard_api::MemorySecretStore::new(),
            now,
        )
        .unwrap();
        assert!(
            !pkce_needs_browser(&refreshable, now + 600),
            "a refresh token renews silently at the token endpoint"
        );
        let (client, _) = draft::parse_auth(
            draft::AuthMode::OAuth2,
            "flow=client_credentials\ntoken_endpoint=https://identity.example.test/token\nclient_id=c\nclient_secret=s",
            "request.r1",
        )
        .unwrap();
        assert!(!pkce_needs_browser(&client, now));
        assert!(!pkce_needs_browser(&AuthConfig::None, now));
    }

    #[test]
    fn managed_oauth_state_survives_only_an_equivalent_editor_rebuild() {
        let client_secret = SecretRef::new("oauth.client-secret").unwrap();
        let access_token = SecretRef::new("oauth.access-token").unwrap();
        let original = AuthConfig::OAuth2ClientCredentials {
            headers: Vec::new(),
            token_endpoint: "https://identity.example.test/token".into(),
            client_id: "desktop-client".into(),
            client_secret: client_secret.clone(),
            scopes: vec!["read".into()],
            access_token: Some(access_token.clone()),
            expires_at: Some(900),
        };
        let mut equivalent = AuthConfig::OAuth2ClientCredentials {
            headers: Vec::new(),
            token_endpoint: "https://identity.example.test/token".into(),
            client_id: "desktop-client".into(),
            client_secret: client_secret.clone(),
            scopes: vec!["read".into()],
            access_token: None,
            expires_at: None,
        };
        preserve_managed_oauth_state(&original, &mut equivalent);
        assert!(matches!(
            equivalent,
            AuthConfig::OAuth2ClientCredentials {
                access_token: Some(ref token),
                expires_at: Some(900),
                ..
            } if token == &access_token
        ));

        let mut changed_endpoint = AuthConfig::OAuth2ClientCredentials {
            headers: Vec::new(),
            token_endpoint: "https://other.example.test/token".into(),
            client_id: "desktop-client".into(),
            client_secret,
            scopes: vec!["read".into()],
            access_token: None,
            expires_at: None,
        };
        preserve_managed_oauth_state(&original, &mut changed_endpoint);
        assert!(matches!(
            changed_endpoint,
            AuthConfig::OAuth2ClientCredentials {
                access_token: None,
                expires_at: None,
                ..
            }
        ));
    }

    #[test]
    fn consecutive_pkce_refreshes_use_the_rotated_vault_token() {
        let workspace = WorkspaceId::new("/test/oauth-refresh-rotation").unwrap();
        let path = std::env::temp_dir().join(format!(
            "agentops-gpui-oauth-refresh-rotation-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        let mut data = persistence::WorkspaceData::open(&path, workspace.clone()).unwrap();
        let collection_id = data.ensure_collection().unwrap();
        let secret_store: Arc<dyn switchyard_api::SecretStore> =
            Arc::new(switchyard_api::MemorySecretStore::new());
        let refresh_now = now_seconds();
        let (mut request, editor_secrets) = draft::build(draft::DraftInput {
            request_id: Some(RequestId::new()),
            collection_id,
            name: "PKCE rotation",
            method: "GET",
            url: "https://example.test/private",
            params: "",
            headers: "",
            cookies: "",
            body_mode: draft::BodyMode::None,
            body: "",
            auth_mode: draft::AuthMode::OAuth2,
            auth: "flow=authorization_code_pkce\nauthorization_endpoint=https://identity.example.test/authorize\ntoken_endpoint=https://identity.example.test/token\nclient_id=desktop-client\nredirect_uri=http://127.0.0.1/callback\ntoken=stale-access\nrefresh_token=old-refresh",
            variables: "",
            pre_request_script: "",
            test_script: "",
            allow_private_network: false,
        })
        .unwrap();
        let AuthConfig::OAuth2AuthorizationCodePkce { expires_at, .. } = &mut request.auth else {
            panic!("expected PKCE auth");
        };
        *expires_at = Some(refresh_now.saturating_add(25));
        data.store.upsert_request(&request).unwrap();
        let mut secrets = draft::DraftSecrets::with_store(secret_store.clone(), workspace.clone());
        secrets.merge(editor_secrets);
        secrets.persist(secret_store.as_ref(), &workspace).unwrap();

        assert!(
            refresh_expired_oauth_token_with(
                &mut request,
                &mut secrets,
                &workspace,
                data.store.as_ref(),
                secret_store.as_ref(),
                refresh_now,
                |token_request| {
                    assert_eq!(token_request.refresh_token.as_deref(), Some("old-refresh"));
                    Ok(transport::OAuthTokenResponse {
                        access_token: "access-one".into(),
                        expires_in: Some(1),
                        refresh_token: Some("rotated-refresh".into()),
                    })
                },
            )
            .unwrap()
        );
        assert!(
            refresh_expired_oauth_token_with(
                &mut request,
                &mut secrets,
                &workspace,
                data.store.as_ref(),
                secret_store.as_ref(),
                refresh_now.saturating_add(2),
                |token_request| {
                    assert_eq!(
                        token_request.refresh_token.as_deref(),
                        Some("rotated-refresh")
                    );
                    Ok(transport::OAuthTokenResponse {
                        access_token: "access-two".into(),
                        expires_in: Some(60),
                        refresh_token: None,
                    })
                },
            )
            .unwrap()
        );
    }

    #[test]
    fn script_cookie_scopes_do_not_copy_values_across_origins() {
        let workspace = WorkspaceId::new("/test/cookie-origin-scope").unwrap();
        let secret_store = switchyard_api::MemorySecretStore::new();
        let mut jar = switchyard_api::CookieJar::new(workspace);
        jar.set_cookie(
            &secret_store,
            "https://one.example.test/start",
            "session=one; Secure; Path=/",
            now_seconds(),
        )
        .unwrap();

        let first =
            cookie_scope_for_url(&jar, &secret_store, "https://one.example.test/next").unwrap();
        assert_eq!(first.get("session"), Some(&"one".to_string()));
        assert!(
            cookie_scope_for_url(&jar, &secret_store, "https://two.example.test/next",)
                .unwrap()
                .is_empty()
        );

        apply_script_cookie_mutations(
            &mut jar,
            &secret_store,
            "https://two.example.test/next",
            &first,
            &first,
        )
        .unwrap();
        assert!(
            cookie_scope_for_url(&jar, &secret_store, "https://two.example.test/next",)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn runner_request_scopes_preserve_run_local_and_shared_mutations() {
        let collection_id = CollectionId::new();
        let request = |name: &str, key: &str| SavedRequest {
            id: RequestId::new(),
            collection_id: collection_id.clone(),
            folder_id: None,
            name: name.into(),
            method: HttpMethod::get(),
            url: "https://example.test".into(),
            params: Vec::new(),
            headers: Vec::new(),
            auth: AuthConfig::None,
            body: Body::None,
            variables: vec![Variable {
                id: RowId::new(),
                key: key.into(),
                value: VariableValue::Plain(name.into()),
                enabled: true,
                description: String::new(),
            }],
            scripts: Scripts::default(),
            settings: RequestSettings::default(),
            extensions: Default::default(),
            sort_key: 0,
        };
        let first_request = request("first", "first_local");
        let second_request = request("second", "second_local");
        let mut runtime = transport::ScriptScopes {
            environment: BTreeMap::from([("shared".into(), "before".into())]),
            iteration_data: BTreeMap::from([("row".into(), "immutable".into())]),
            ..Default::default()
        };

        let mut first = request_script_scopes(&runtime, &first_request);
        assert_eq!(
            first.local.get("first_local").map(String::as_str),
            Some("first")
        );
        let initial_local = first.local.clone();
        first.environment.insert("shared".into(), "after".into());
        first
            .local
            .insert("run_value".into(), "carried-forward".into());
        carry_shared_script_scopes(&mut runtime, &first, &initial_local);

        let second = request_script_scopes(&runtime, &second_request);
        assert_eq!(
            second.environment.get("shared").map(String::as_str),
            Some("after")
        );
        assert_eq!(
            second.iteration_data.get("row").map(String::as_str),
            Some("immutable")
        );
        assert!(!second.local.contains_key("first_local"));
        assert_eq!(
            second.local.get("run_value").map(String::as_str),
            Some("carried-forward")
        );
        assert_eq!(
            second.local.get("second_local").map(String::as_str),
            Some("second")
        );
    }

    /// Answers the sign-in URL with a fresh token each time and every other
    /// URL with the next status in `statuses`, recording `(url, Authorization)`.
    struct LoginTransport {
        sign_in_url: &'static str,
        statuses: Mutex<Vec<u16>>,
        sign_ins: AtomicUsize,
        sends: Mutex<Vec<(String, Option<String>)>>,
        sign_in_requests: Mutex<Vec<switchyard_api::PreparedRequest>>,
    }

    impl LoginTransport {
        fn new(sign_in_url: &'static str, statuses: &[u16]) -> Arc<Self> {
            Arc::new(Self {
                sign_in_url,
                statuses: Mutex::new(statuses.iter().rev().copied().collect()),
                sign_ins: AtomicUsize::new(0),
                sends: Mutex::default(),
                sign_in_requests: Mutex::default(),
            })
        }

        fn response(status: u16, body: &str) -> transport::Response {
            transport::Response {
                console: Vec::new(),
                status,
                reason: if status == 200 { "OK" } else { "Unauthorized" }.into(),
                headers: vec![("Content-Type".into(), "application/json".into())],
                set_cookies: Vec::new(),
                cookie_mutations: Vec::new(),
                body: body.into(),
                body_base64: String::new(),
                binary: false,
                final_url: String::new(),
                http_version: "HTTP/1.1".into(),
                received_bytes: body.len() as u64,
                stored_bytes: body.len() as u64,
                full_body_sha256: None,
                timings: Default::default(),
                cookies: Vec::new(),
                duration_ms: 1,
                truncated: false,
                redirects: Vec::new(),
                test_results: Vec::new(),
            }
        }
    }

    impl transport::WorkbenchTransport for LoginTransport {
        fn send(
            &self,
            request: &switchyard_api::PreparedRequest,
            _files: &transport::FileCapabilities,
            _phase: &transport::OperationPhase,
        ) -> Result<transport::Response, String> {
            let authorization = request
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                .map(|(_, value)| value.clone());
            self.sends
                .lock()
                .unwrap()
                .push((request.url.clone(), authorization));
            if request.url == self.sign_in_url {
                self.sign_in_requests.lock().unwrap().push(request.clone());
                let nth = self.sign_ins.fetch_add(1, Ordering::SeqCst) + 1;
                return Ok(Self::response(
                    200,
                    &format!(r#"{{"data":{{"token":"t{nth}"}},"expires_in":600}}"#),
                ));
            }
            let status = self.statuses.lock().unwrap().pop().unwrap_or(200);
            Ok(Self::response(status, r#"{"ok":true}"#))
        }

        fn run_script(
            &self,
            _script: &str,
            scopes: transport::ScriptScopes,
            request: transport::ScriptRequestView,
            _response: Option<transport::ScriptResponseView>,
            _request_id: Option<&str>,
            _phase: Option<&transport::OperationPhase>,
        ) -> Result<transport::ScriptResult, String> {
            Ok(transport::ScriptResult {
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
            })
        }

        fn cancel(&self, _operation_id: &str) -> Result<bool, String> {
            Ok(true)
        }
    }

    /// A workspace whose active environment supplies a base URL and a
    /// sign-in request, plus one relative-URL request inheriting it.
    struct LoginFixture {
        data: persistence::WorkspaceData,
        environment: Environment,
        request: SavedRequest,
        secret_store: Arc<dyn switchyard_api::SecretStore>,
    }

    impl LoginFixture {
        fn new(name: &str) -> Self {
            let workspace = WorkspaceId::new(format!("/test/login-{name}")).unwrap();
            let path = std::env::temp_dir()
                .join(format!("agentops-gpui-login-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            let mut data = persistence::WorkspaceData::open(&path, workspace.clone()).unwrap();
            let collection_id = data.ensure_collection().unwrap();
            let environment_id = EnvironmentId::new();
            let scope = format!("environment.{}", environment_id.as_str());
            let (auth, _) = draft::parse_auth(
                draft::AuthMode::Login,
                "url=https://auth.test/login\nmethod=POST\ntoken_path=data.token\nbody={\"user\":\"{{user}}\"}",
                &scope,
            )
            .unwrap();
            let environment = Environment {
                id: environment_id,
                workspace_id: workspace.clone(),
                name: "Staging".into(),
                label: Default::default(),
                base_url: "https://api.test/".into(),
                auth,
                variables: vec![Variable {
                    id: Default::default(),
                    key: "user".into(),
                    value: switchyard_api::VariableValue::Plain("ada".into()),
                    enabled: true,
                    description: String::new(),
                }],
                active: true,
                extensions: Default::default(),
            };
            data.save_environment(environment.clone()).unwrap();
            let request = SavedRequest {
                id: RequestId::new(),
                collection_id,
                folder_id: None,
                name: "Items".into(),
                method: HttpMethod::get(),
                url: "/items".into(),
                params: Vec::new(),
                headers: Vec::new(),
                auth: AuthConfig::Inherit,
                body: Body::None,
                variables: Vec::new(),
                scripts: Scripts::default(),
                settings: RequestSettings::default(),
                extensions: Default::default(),
                sort_key: 0,
            };
            data.store.upsert_request(&request).unwrap();
            Self {
                data,
                environment,
                request,
                secret_store: Arc::new(switchyard_api::MemorySecretStore::new()),
            }
        }

        fn saved_environment(&self) -> Environment {
            self.data
                .store
                .list_environments(&self.data.workspace)
                .unwrap()
                .into_iter()
                .find(|environment| environment.id == self.environment.id)
                .unwrap()
        }

        /// One standalone send the way `WorkbenchPanel::send` assembles it,
        /// with the environment auth as the Envs editor would parse it.
        fn send(
            &self,
            environment: &Environment,
            transport: Arc<dyn transport::WorkbenchTransport>,
        ) -> Result<StandaloneSendResult, String> {
            let workspace = self.data.workspace.clone();
            let scope = format!("environment.{}", environment.id.as_str());
            let input = StandaloneSendInput {
                operation_id: "login-operation".into(),
                definition: self.request.clone(),
                collection: self.data.collection(&self.request.collection_id).cloned(),
                folders: Vec::new(),
                environment: Some(environment.clone()),
                environment_source: format_variables(&environment.variables),
                environment_scope: scope,
                environment_base_url: environment.base_url.clone(),
                environment_auth: environment.auth.clone(),
                browser_authorization: None,
                secrets: draft::DraftSecrets::with_store(
                    self.secret_store.clone(),
                    workspace.clone(),
                ),
                workspace: workspace.clone(),
                store: self.data.store.clone(),
                secret_store: self.secret_store.clone(),
                cookie_jar: switchyard_api::CookieJar::new(workspace),
                manual_cookies: Vec::new(),
                file_capabilities: transport::FileCapabilities::default(),
                transport,
            };
            execute_standalone_send(prepare_standalone_send(input)?)
        }
    }

    fn login_cache_expiry(auth: &AuthConfig) -> Option<i64> {
        match auth {
            AuthConfig::Login { expires_at, .. } => *expires_at,
            _ => None,
        }
    }

    #[test]
    fn environment_sign_in_runs_once_and_prefixes_the_base_url() {
        let fixture = LoginFixture::new("once");
        let transport = LoginTransport::new("https://auth.test/login", &[200, 200]);
        let now = now_seconds();

        let result = fixture
            .send(&fixture.environment, transport.clone())
            .unwrap();
        assert_eq!(result.response.as_ref().unwrap().status, 200);
        assert_eq!(
            transport.sends.lock().unwrap().as_slice(),
            &[
                ("https://auth.test/login".to_string(), None),
                (
                    "https://api.test/items".to_string(),
                    Some("Bearer t1".to_string())
                ),
            ]
        );
        assert!(result.prepared.redactions.iter().any(|value| value == "t1"));
        assert!(
            !result
                .snapshot
                .headers
                .iter()
                .any(|(_, value)| value.contains("t1"))
        );

        // The session lands on the saved environment and in the vault.
        let saved = fixture.saved_environment();
        let expiry = login_cache_expiry(&saved.auth).expect("the sign-in cache is persisted");
        assert!(
            expiry >= now + 590 && expiry <= now + 610,
            "{expiry} vs {now}"
        );
        assert_eq!(
            login_cache_expiry(&result.environment.unwrap().auth),
            Some(expiry)
        );
        let token = fixture
            .secret_store
            .get_secret(
                &fixture.data.workspace,
                &SecretRef::new(format!(
                    "workbench.environment.{}.auth.token",
                    fixture.environment.id.as_str()
                ))
                .unwrap(),
            )
            .unwrap();
        assert_eq!(token.expose_secret(), "t1");

        // A second send reuses the cached session instead of signing in again.
        let result = fixture.send(&saved, transport.clone()).unwrap();
        assert_eq!(result.response.as_ref().unwrap().status, 200);
        assert_eq!(transport.sign_ins.load(Ordering::SeqCst), 1);
        assert_eq!(
            transport.sends.lock().unwrap().last().unwrap(),
            &(
                "https://api.test/items".to_string(),
                Some("Bearer t1".to_string())
            )
        );
    }

    #[test]
    fn a_401_signs_in_again_exactly_once_and_resends() {
        let fixture = LoginFixture::new("retry");
        let transport = LoginTransport::new("https://auth.test/login", &[401, 200]);

        let result = fixture
            .send(&fixture.environment, transport.clone())
            .unwrap();
        assert_eq!(result.response.as_ref().unwrap().status, 200);
        assert_eq!(transport.sign_ins.load(Ordering::SeqCst), 2);
        let sends = transport.sends.lock().unwrap().clone();
        assert_eq!(
            sends
                .iter()
                .map(|(url, auth)| (url.as_str(), auth.as_deref()))
                .collect::<Vec<_>>(),
            [
                ("https://auth.test/login", None),
                ("https://api.test/items", Some("Bearer t1")),
                ("https://auth.test/login", None),
                ("https://api.test/items", Some("Bearer t2")),
            ]
        );
        assert!(result.prepared.redactions.iter().any(|value| value == "t2"));
        let renewed = result.environment.expect("the renewed session is returned");
        assert_eq!(
            login_cache_expiry(&renewed.auth),
            login_cache_expiry(&fixture.saved_environment().auth)
        );
        let token = fixture
            .secret_store
            .get_secret(
                &fixture.data.workspace,
                &SecretRef::new(format!(
                    "workbench.environment.{}.auth.token",
                    fixture.environment.id.as_str()
                ))
                .unwrap(),
            )
            .unwrap();
        assert_eq!(token.expose_secret(), "t2");

        // A 401 that survives the fresh sign-in is reported as-is.
        let transport = LoginTransport::new("https://auth.test/login", &[401, 401]);
        let result = fixture
            .send(&fixture.environment, transport.clone())
            .unwrap();
        assert_eq!(result.response.as_ref().unwrap().status, 401);
        assert_eq!(transport.sign_ins.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_relative_url_without_a_base_url_fails_closed() {
        let fixture = LoginFixture::new("no-base");
        let mut environment = fixture.environment.clone();
        environment.base_url.clear();
        environment.auth = AuthConfig::None;
        let transport = LoginTransport::new("https://auth.test/login", &[200]);
        let error = match fixture.send(&environment, transport.clone()) {
            Ok(_) => panic!("a relative URL must not send without a base URL"),
            Err(error) => error,
        };
        assert!(error.contains("Base URL"), "{error}");
        assert!(transport.sends.lock().unwrap().is_empty());
    }

    #[test]
    fn named_vault_variable_references_survive_editor_roundtrips() {
        let source =
            "secret:access_token={{vault.api_token}}\n# secret:password={{vault.api_password}}";
        let (variables, _) = draft::parse_session_variables(source, "environment.test").unwrap();
        let formatted = format_variables(&variables);
        assert_eq!(formatted, source);
        let (restored, _) = draft::parse_session_variables(&formatted, "environment.test").unwrap();
        for (before, after) in variables.iter().zip(restored.iter()) {
            assert_eq!(before.key, after.key);
            assert_eq!(before.value, after.value);
            assert_eq!(before.enabled, after.enabled);
        }
    }

    #[test]
    fn basic_login_keeps_saved_password_refs_and_invalidates_tokens_after_edits() {
        let scope = "request.basic-login-cache";
        let source = "username=ops\nauth_url=https://auth.test/login";
        let (mut saved, _) = draft::parse_auth(draft::AuthMode::Basic, source, scope).unwrap();
        let imported_password = SecretRef::new("imported.basic.password").unwrap();
        let mut stored = draft::DraftSecrets::default();
        stored.insert(&imported_password, "original-password");
        if let AuthConfig::Login {
            basic: Some(basic),
            access_token,
            expires_at,
            ..
        } = &mut saved
        {
            basic.password = imported_password.clone();
            *access_token = Some(SecretRef::new("cached.basic.token").unwrap());
            *expires_at = Some(i64::MAX);
        }
        let (mut restored, secrets) =
            draft::parse_auth(draft::AuthMode::Basic, source, scope).unwrap();
        preserve_saved_auth(&saved, &mut restored, &secrets, &stored);
        assert_eq!(restored, saved);
        let (mut named, entered) = draft::parse_auth(
            draft::AuthMode::Basic,
            &format!("{source}\npassword={{{{vault.login_password}}}}"),
            scope,
        )
        .unwrap();
        preserve_saved_auth(&saved, &mut named, &entered, &stored);
        let AuthConfig::Login {
            basic: Some(basic),
            access_token,
            ..
        } = &named
        else {
            panic!("expected named Basic login");
        };
        assert_eq!(
            switchyard_api::vault::vault_reference_expression(&basic.password).as_deref(),
            Some("{{vault.login_password}}")
        );
        assert!(access_token.is_none());
        let (mut another, entered) = draft::parse_auth(
            draft::AuthMode::Basic,
            &format!("{source}\npassword={{{{vault.second_password}}}}"),
            scope,
        )
        .unwrap();
        preserve_saved_auth(&named, &mut another, &entered, &stored);
        assert!(draft::format_auth(&another).contains("password={{vault.second_password}}"));
        for edit in [
            format!("{source}\npassword=replacement"),
            source.replace("username=ops", "username=another-user"),
            source.replace("auth.test", "new-auth.test"),
        ] {
            let (mut changed, secrets) =
                draft::parse_auth(draft::AuthMode::Basic, &edit, scope).unwrap();
            preserve_saved_auth(&saved, &mut changed, &secrets, &stored);
            assert!(matches!(
                changed,
                AuthConfig::Login {
                    access_token: None,
                    expires_at: None,
                    ..
                }
            ));
        }
        let (mut direct, secrets) =
            draft::parse_auth(draft::AuthMode::Basic, "username=ops", scope).unwrap();
        preserve_saved_auth(&saved, &mut direct, &secrets, &stored);
        assert!(
            matches!(direct, AuthConfig::Basic { password, .. } if password == imported_password)
        );

        let text = format!("{source}\npassword=original-password");
        let (mut signed_in, secrets) =
            draft::parse_auth(draft::AuthMode::Basic, &text, scope).unwrap();
        if let AuthConfig::Login {
            basic: Some(basic),
            access_token,
            expires_at,
            ..
        } = &mut signed_in
        {
            stored.insert(&basic.password, "original-password");
            *access_token = Some(SecretRef::new("cached.basic.token").unwrap());
            *expires_at = Some(i64::MAX);
        }
        let (mut next_send, _) = draft::parse_auth(draft::AuthMode::Basic, &text, scope).unwrap();
        preserve_saved_auth(&signed_in, &mut next_send, &secrets, &stored);
        assert_eq!(
            next_send, signed_in,
            "unchanged password text must reuse the cached token"
        );
    }
}
