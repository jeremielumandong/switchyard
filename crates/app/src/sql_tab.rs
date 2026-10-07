//! SQL editor tab: editor on top, streaming results below, toolbar with Run, Run script,
//! Stop, transaction mode and the connection picker.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::highlighter::{Diagnostic, DiagnosticSeverity};
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState, Position};
use gpui_kit::component::table::{DataTable, TableEvent, TableState};
use gpui_kit::component::{Sizable as _, Size};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Entity, EventEmitter, FocusHandle,
    Focusable, FontWeight, Hsla, InteractiveElement as _, IntoElement, MouseButton, MouseMoveEvent,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Task, Window, deferred, div, px, relative,
};
use switchyard_core::db::diagnostics::parse_diagnostics;
use switchyard_core::db::{
    ColumnMeta, Completion, DataType, DbError, Dialect, Engine, Notice, RowBatch, Value,
    dialect_for, format,
};
use switchyard_core::store::{BufferState, DbConnection, EnvironmentLabel};
use switchyard_core::{
    Command, FetchLimit, QueryEvent, QueryId, RequestId, RuntimeHandle, SessionId, StatementRequest,
};

use std::cell::RefCell;
use std::rc::Rc;

use switchyard_core::db::complete::CatalogIndex;
use switchyard_core::db::edit::{EditTable, RowEdit, editable_table, update_statements};

use crate::app_state::{SessionState, next_id};
use crate::completion::{CompletionState, SqlCompletion};
use crate::grid::GridDelegate;
use crate::plan_view::{ExplainRequest, PlanView, PlanViewEvent};
use crate::theme::{MONO, Palette, SANS, palette};
use crate::ui::{self, Kind, thousands};

/// A row as (column name, value, type) triples.
pub type RowValues = Vec<(String, Value, DataType)>;

/// Default rows fetched before pausing (SPEC: 10,000).
pub const DEFAULT_FETCH_LIMIT: usize = 10_000;

/// What the tab asks the workspace to do.
pub enum SqlTabEvent {
    /// Open the quick switcher to pick a connection for this tab.
    PickConnection,
    /// Show a toast.
    Toast(String),
    /// Ask for confirmation of destructive statements on Production.
    ConfirmDestructive(PendingRun),
    /// Ask for parameter values.
    PromptParams(PendingRun),
    /// The tab's title, dirty state or connection changed.
    Changed,
}

/// Statements waiting for a confirmation or parameter values.
#[derive(Clone, Debug)]
pub struct PendingRun {
    /// Statements.
    pub statements: Vec<StatementRequest>,
    /// Parameter names in binding order (per statement).
    pub params: Vec<Vec<String>>,
    /// Destructive findings: (statement text, headline, explanation, object, label).
    pub destructive: Vec<DestructiveInfo>,
}

/// A destructive statement for the safety dialog.
#[derive(Clone, Debug)]
pub struct DestructiveInfo {
    /// 1-based line in the buffer.
    pub line: u32,
    /// Statement text.
    pub sql: String,
    /// Headline question.
    pub headline: String,
    /// Explanation.
    pub explanation: String,
    /// Affected object.
    pub object: String,
    /// Short label (`no WHERE`).
    pub label: String,
}

/// Lifecycle of the current run.
#[derive(Clone, Debug, PartialEq)]
pub enum RunState {
    /// Nothing ran yet.
    Idle,
    /// Streaming.
    Running { query: QueryId, started: Instant },
    /// Paused at the fetch limit.
    Paused { query: QueryId, started: Instant },
    /// Finished.
    Done {
        elapsed: Duration,
        affected: Option<u64>,
    },
    /// Failed.
    Failed { elapsed: Duration },
    /// Cancelled by the user.
    Cancelled { elapsed: Duration },
}

/// A failed statement, for the error view.
#[derive(Clone, Debug)]
pub struct ErrorView {
    code: Option<String>,
    message: String,
    detail: Option<String>,
    hint: Option<String>,
    /// Absolute (line, column) in the buffer, 1-based.
    location: Option<(u32, u32)>,
    /// The offending line's text.
    line_text: Option<String>,
}

/// One result set.
pub struct ResultSet {
    table: Entity<TableState<GridDelegate>>,
    columns: Arc<[ColumnMeta]>,
    sql: String,
    rows: usize,
    completion: Option<Completion>,
    _sub: Subscription,
}

/// Value viewer format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewerFormat {
    /// JSON object of the selected row.
    Json,
    /// Column / value pairs.
    Text,
    /// The selected cell as indented, coloured XML.
    Xml,
    /// Hex dump of the selected cell.
    Hex,
    /// The selected cell drawn as an image (PNG, JPEG, GIF, WebP, BMP, TIFF, SVG).
    Image,
}

/// An SQL editor tab.
pub struct SqlTab {
    core: RuntimeHandle,
    buffer_id: String,
    pub title: SharedString,
    pub connection: Option<DbConnection>,
    pub session: Option<SessionId>,
    pub session_state: SessionState,
    /// This tab's database color (set by the workspace: one color per open database).
    pub accent: Option<Hsla>,
    /// A run asked for while the session was still connecting; sent once it opens.
    queued_run: Option<(PendingRun, bool)>,
    /// An explain asked for while the session was still connecting.
    queued_explain: Option<ExplainRequest>,
    /// The plan view (the "Plan" result tab).
    plan: Entity<PlanView>,
    /// The Plan result tab is showing.
    pub show_plan: bool,
    /// Editor highlight of the selected plan node's tables.
    plan_marks: Option<gpui_kit::component::input::RangeDecorationCollection>,
    editor: Entity<EditorState>,
    editor_height: f32,
    drag: Option<(f32, f32)>,
    pub manual_txn: bool,
    pub txn_open: bool,
    pub txn_statements: u32,
    pub run: RunState,
    pub results: Vec<ResultSet>,
    /// Index into `results`; `results.len()` is the Messages tab.
    pub active_result: usize,
    messages: Vec<(Hsla, String)>,
    error: Option<ErrorView>,
    current_statements: Vec<StatementRequest>,
    current_index: usize,
    edit: Option<EditState>,
    pub dirty: bool,
    export_open: bool,
    pub viewer_format: ViewerFormat,
    pub selected: Option<(usize, usize)>,
    /// Where a Shift-extended range starts: (view row, table column).
    anchor: Option<(usize, usize)>,
    /// The next cell selection extends the range (Shift+arrow).
    extending: bool,
    fetch_limit: usize,
    notices: usize,
    last_affected: Option<u64>,
    focus: FocusHandle,
    autosave: Option<Task<()>>,
    /// Redraws the elapsed time while a query runs (nothing else notifies when no
    /// rows arrive, and retained rendering only redraws what was notified).
    ticker: Option<Task<()>>,
    lint: Option<Task<()>>,
    position: i64,
    filter: Entity<InputState>,
    completion: Rc<RefCell<CompletionState>>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<SqlTabEvent> for SqlTab {}

impl Focusable for SqlTab {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl SqlTab {
    /// A tab restored from (or creating) a buffer.
    pub fn new(
        core: RuntimeHandle,
        buffer: BufferState,
        connection: Option<DbConnection>,
        position: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql")
                .line_number(true)
                .indent_guides(false)
                .soft_wrap(false)
                .default_value(buffer.text.clone())
                .placeholder("-- Write SQL here. ⌘↵ runs the statement at the cursor.")
        });
        let sub = cx.subscribe_in(&editor, window, |this, _, ev: &InputEvent, window, cx| {
            if let InputEvent::Change = ev {
                this.dirty = true;
                this.schedule_autosave(cx);
                this.schedule_lint(window, cx);
                cx.emit(SqlTabEvent::Changed);
            }
        });
        let completion: Rc<RefCell<CompletionState>> = Rc::default();
        let provider = Rc::new(SqlCompletion {
            state: completion.clone(),
        });
        editor.update(cx, |e, _| e.lsp_mut().completion_provider = Some(provider));
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter rows"));
        let filter_sub = cx.subscribe(&filter, |this, input, ev: &InputEvent, cx| {
            if let InputEvent::Change = ev {
                let needle = input.read(cx).value().to_string();
                this.apply_filter(&needle, cx);
            }
        });
        let plan = cx.new(|_| PlanView::new(core.clone()));
        let plan_sub = cx.subscribe_in(&plan, window, |this, _, ev: &PlanViewEvent, window, cx| {
            this.on_plan_event(ev, window, cx)
        });
        let mut tab = Self {
            core,
            buffer_id: buffer.id,
            title: buffer.title.into(),
            connection: None,
            session: None,
            session_state: SessionState::None,
            accent: None,
            queued_run: None,
            queued_explain: None,
            plan,
            show_plan: false,
            plan_marks: None,
            editor,
            editor_height: 300.0,
            drag: None,
            manual_txn: false,
            txn_open: false,
            txn_statements: 0,
            run: RunState::Idle,
            results: Vec::new(),
            active_result: 0,
            messages: Vec::new(),
            error: None,
            current_statements: Vec::new(),
            current_index: 0,
            edit: None,
            dirty: false,
            export_open: false,
            viewer_format: ViewerFormat::Json,
            selected: None,
            anchor: None,
            extending: false,
            fetch_limit: DEFAULT_FETCH_LIMIT,
            notices: 0,
            last_affected: None,
            focus: cx.focus_handle(),
            autosave: None,
            ticker: None,
            lint: None,
            position,
            filter,
            completion,
            _subs: vec![sub, filter_sub, plan_sub],
        };
        if let Some(c) = connection {
            tab.set_connection(Some(c), cx);
        }
        // Folds (and diagnostics) for a restored buffer, which fires no change event.
        tab.schedule_lint(window, cx);
        tab
    }

    /// The editor state.
    pub fn editor(&self) -> &Entity<EditorState> {
        &self.editor
    }

    /// Buffer id.
    pub fn buffer_id(&self) -> &str {
        &self.buffer_id
    }

    /// Whether this tab owns `session`.
    pub fn owns_session(&self, session: SessionId) -> bool {
        self.session == Some(session)
    }

    /// Whether this tab owns `query`.
    pub fn owns_query(&self, query: QueryId) -> bool {
        matches!(self.run, RunState::Running { query: q, .. } | RunState::Paused { query: q, .. } if q == query)
    }

    /// Environment of the bound connection.
    pub fn environment(&self) -> EnvironmentLabel {
        self.connection
            .as_ref()
            .map(|c| c.environment)
            .unwrap_or_default()
    }

    fn dialect(&self) -> &'static dyn Dialect {
        dialect_for(
            self.connection
                .as_ref()
                .map_or(Engine::Postgres, |c| c.engine),
        )
    }

    /// Bind (or rebind) the tab to a connection, opening a new session.
    pub fn set_connection(&mut self, connection: Option<DbConnection>, cx: &mut Context<Self>) {
        if let Some(s) = self.session.take() {
            self.core.send(Command::CloseSession { session: s });
        }
        self.connection = connection;
        let profile = self.connection.as_ref().map(|c| c.id.clone());
        self.plan.update(cx, |v, _| v.set_connection(profile));
        self.txn_open = false;
        self.txn_statements = 0;
        match &self.connection {
            Some(c) => {
                let session = next_id();
                self.session = Some(session);
                self.session_state = SessionState::Connecting;
                self.core.send(Command::OpenSession {
                    session,
                    connection: c.id.clone(),
                });
            }
            None => self.session_state = SessionState::None,
        }
        self.schedule_autosave(cx);
        cx.emit(SqlTabEvent::Changed);
        cx.notify();
    }

    /// Session lifecycle updates from the workspace.
    pub fn on_session(&mut self, state: SessionState, cx: &mut Context<Self>) {
        let opened = matches!(state, SessionState::Open { .. });
        if opened && let Some(session) = self.session {
            // Load every column once for completion (served from the cache when warm).
            self.core.send(Command::Introspect {
                session,
                scope: switchyard_core::db::IntrospectScope::AllColumns,
                refresh: false,
            });
        }
        if let Some(req) = self.queued_explain.take() {
            if opened {
                self.session_state = state.clone();
                self.send_explain(req, cx);
            } else if let SessionState::Failed(why) = &state {
                cx.emit(SqlTabEvent::Toast(format!("Could not connect: {why}")));
            }
        }
        // A run queued while connecting goes now; a failed open drops it and says why.
        if let Some((pending, confirmed)) = self.queued_run.take() {
            if opened {
                self.session_state = state;
                self.execute(pending, confirmed, cx);
                cx.emit(SqlTabEvent::Changed);
                cx.notify();
                return;
            }
            if let SessionState::Failed(why) = &state {
                cx.emit(SqlTabEvent::Toast(format!("Could not connect: {why}")));
            }
        }
        self.session_state = state;
        cx.emit(SqlTabEvent::Changed);
        cx.notify();
    }

    /// A catalog chunk for this tab's session.
    pub fn on_catalog(&mut self, chunk: switchyard_core::db::CatalogChunk, cx: &mut Context<Self>) {
        if let switchyard_core::db::CatalogChunk::Detail(d) = &chunk {
            self.on_table_detail(d, cx);
            return;
        }
        if let switchyard_core::db::CatalogChunk::AllColumns(cols) = chunk {
            tracing::debug!(columns = cols.len(), "completion catalog loaded");
            let mut st = self.completion.borrow_mut();
            st.index = CatalogIndex::from_columns(&cols);
            st.engine = self.connection.as_ref().map(|c| c.engine);
        }
    }

    /// Transaction state updates.
    pub fn on_transaction(&mut self, open: bool, statements: u32, cx: &mut Context<Self>) {
        self.txn_open = open;
        self.txn_statements = statements;
        if open {
            self.manual_txn = true;
        }
        cx.emit(SqlTabEvent::Changed);
        cx.notify();
    }

    /// The buffer for autosave.
    pub fn buffer(&self, cx: &App) -> BufferState {
        let editor = self.editor.read(cx);
        BufferState {
            id: self.buffer_id.clone(),
            title: self.title.to_string(),
            connection_id: self.connection.as_ref().map(|c| c.id.clone()),
            text: editor.value().to_string(),
            cursor: editor.cursor(),
        }
    }

    fn schedule_autosave(&mut self, cx: &mut Context<Self>) {
        self.autosave = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(400))
                .await;
            let _ = this.update(cx, |tab, cx| tab.save_now(cx));
        }));
    }

    /// Persist the buffer immediately.
    pub fn save_now(&mut self, cx: &mut Context<Self>) {
        let buffer = self.buffer(cx);
        self.core.send(Command::SaveBuffer {
            buffer,
            position: self.position,
        });
        self.dirty = false;
        cx.emit(SqlTabEvent::Changed);
    }

    fn schedule_lint(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.lint = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(250))
                .await;
            let _ = this.update_in(cx, |tab, _window, cx| tab.lint_now(cx));
        }));
    }

    fn lint_now(&mut self, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).value().to_string();
        let diags = parse_diagnostics(self.dialect(), &text);
        let folds = crate::folds::sql_folds(self.dialect(), &text);
        self.editor.update(cx, |state, cx| {
            state.apply_highlighter_fold_candidates(folds, cx);
            if let Some(set) = state.diagnostics_mut() {
                set.clear();
                for d in diags {
                    let line = d.line.saturating_sub(1);
                    let col = d.column.saturating_sub(1);
                    set.push(
                        Diagnostic::new(
                            Position::new(line, col)..Position::new(line, col + 1),
                            d.message,
                        )
                        .with_severity(DiagnosticSeverity::Error),
                    );
                }
            }
            cx.notify();
        });
    }

    /// Run the statement at the cursor, or the selection when there is one.
    pub fn run_statement(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let statements = self.statements_at_cursor(cx);
        self.start(statements, window, cx);
    }

    /// The statements in the selection, or the one at the cursor.
    fn statements_at_cursor(&self, cx: &App) -> Vec<StatementRequest> {
        let (text, sel, cursor) = {
            let e = self.editor.read(cx);
            (e.value().to_string(), e.selected_range(), e.cursor())
        };
        let dialect = self.dialect();
        if sel.start < sel.end {
            let slice = &text[sel.clone()];
            dialect
                .split_script(slice)
                .iter()
                .map(|s| StatementRequest {
                    sql: s.text(slice).to_owned(),
                    params: vec![],
                    offset: sel.start + s.start,
                })
                .collect()
        } else {
            dialect
                .statement_at(&text, cursor)
                .map(|s| StatementRequest {
                    sql: s.text(&text).to_owned(),
                    params: vec![],
                    offset: s.start,
                })
                .into_iter()
                .collect()
        }
    }

    /// Capture the plan of the statement at the cursor (or the first selected one):
    /// estimated, or actual when `analyze` (the statement runs; writes are rolled back).
    pub fn explain(&mut self, analyze: bool, _window: &mut Window, cx: &mut Context<Self>) {
        let statements = self.statements_at_cursor(cx);
        let Some(first) = statements.first() else {
            cx.emit(SqlTabEvent::Toast("Nothing to explain".into()));
            return;
        };
        if statements.len() > 1 {
            cx.emit(SqlTabEvent::Toast(format!(
                "Explaining the first of {} selected statements",
                statements.len()
            )));
        }
        let req = ExplainRequest {
            sql: first.sql.clone(),
            offset: Some(first.offset),
            analyze,
            confirmed: false,
        };
        self.send_explain(req, cx);
    }

    fn send_explain(&mut self, req: ExplainRequest, cx: &mut Context<Self>) {
        if self.connection.is_none() {
            cx.emit(SqlTabEvent::PickConnection);
            return;
        }
        self.show_plan = true;
        self.export_open = false;
        // Same as runs: wait for a connecting session, reconnect a failed one.
        match self.session_state {
            SessionState::Connecting => {
                self.queued_explain = Some(req);
                cx.notify();
                return;
            }
            SessionState::Failed(_) => {
                let conn = self.connection.clone();
                self.set_connection(conn, cx);
                self.queued_explain = Some(req);
                return;
            }
            _ => {}
        }
        let Some(session) = self.session else {
            cx.emit(SqlTabEvent::PickConnection);
            return;
        };
        self.plan.update(cx, |v, cx| v.explain(session, req, cx));
        cx.notify();
    }

    /// Show the plan stored with a history entry.
    pub fn open_saved_plan(&mut self, history_id: i64, cx: &mut Context<Self>) {
        self.show_plan = true;
        self.plan.update(cx, |v, cx| v.open_saved(history_id, cx));
        cx.notify();
    }

    /// The plan view.
    pub fn plan_view(&self) -> &Entity<PlanView> {
        &self.plan
    }

    fn on_plan_event(&mut self, ev: &PlanViewEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev {
            PlanViewEvent::Toast(t) => cx.emit(SqlTabEvent::Toast(t.clone())),
            PlanViewEvent::Rerun(req) => self.send_explain(req.clone(), cx),
            PlanViewEvent::Highlight {
                sql,
                offset,
                ranges,
            } => self.highlight_plan(sql, *offset, ranges, window, cx),
        }
    }

    /// Mark where the selected plan node's tables appear in the statement.
    fn highlight_plan(
        &mut self,
        sql: &str,
        offset: Option<usize>,
        ranges: &[std::ops::Range<usize>],
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use gpui_kit::component::input::{RangeDecoration, RangeDecorationStyle};
        let text = self.editor.read(cx).value().to_string();
        // Where the statement is now: where it was explained, else wherever it is found.
        let base = offset
            .filter(|&o| text.get(o..o + sql.len()) == Some(sql))
            .or_else(|| text.find(sql));
        let color = self.accent.unwrap_or_else(|| palette(cx).acc).opacity(0.3);
        let marks: Vec<RangeDecoration> = match base {
            Some(base) => ranges
                .iter()
                .map(|r| {
                    RangeDecoration::new(base + r.start..base + r.end)
                        .with_style(RangeDecorationStyle::Fill)
                        .with_color(color)
                })
                .collect(),
            None => Vec::new(),
        };
        match &self.plan_marks {
            Some(c) => c.set(marks, cx),
            None => {
                let c = self
                    .editor
                    .update(cx, |e, cx| e.create_range_decorations_collection(marks, cx));
                self.plan_marks = Some(c);
            }
        }
    }

    /// Run every statement in the buffer.
    pub fn run_script(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).value().to_string();
        let statements = self
            .dialect()
            .split_script(&text)
            .iter()
            .map(|s| StatementRequest {
                sql: s.text(&text).to_owned(),
                params: vec![],
                offset: s.start,
            })
            .collect();
        self.start(statements, window, cx);
    }

    fn start(
        &mut self,
        statements: Vec<StatementRequest>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if statements.is_empty() {
            cx.emit(SqlTabEvent::Toast("Nothing to run".into()));
            return;
        }
        let Some(conn) = self.connection.clone() else {
            cx.emit(SqlTabEvent::PickConnection);
            return;
        };
        if matches!(self.run, RunState::Running { .. } | RunState::Paused { .. }) {
            self.stop(cx);
        }
        let dialect = self.dialect();
        // Parameters.
        let params: Vec<Vec<String>> = statements
            .iter()
            .map(|s| dialect.bind_params(&s.sql).1)
            .collect();
        // Destructive statements on Production.
        let mut destructive = Vec::new();
        if conn.environment.is_production() {
            let text = self.editor.read(cx).value().to_string();
            for st in &statements {
                for d in switchyard_core::db::guard::classify(dialect, &st.sql).destructive() {
                    destructive.push(DestructiveInfo {
                        line: switchyard_core::db::dialect::line_of_byte(&text, st.offset),
                        sql: st.sql.clone(),
                        headline: d.headline(),
                        explanation: d.explanation().to_owned(),
                        object: d.objects.first().cloned().unwrap_or_default(),
                        label: d.kind.label(),
                    });
                }
            }
        }
        let pending = PendingRun {
            statements,
            params,
            destructive,
        };
        if pending.params.iter().any(|p| !p.is_empty()) {
            cx.emit(SqlTabEvent::PromptParams(pending));
        } else if !pending.destructive.is_empty() {
            cx.emit(SqlTabEvent::ConfirmDestructive(pending));
        } else {
            self.execute(pending, false, cx);
        }
    }

    /// Bind parameter values (by name) into a pending run.
    pub fn bind(&self, mut pending: PendingRun, values: &[(String, String)]) -> PendingRun {
        let dialect = self.dialect();
        for (st, names) in pending.statements.iter_mut().zip(&pending.params) {
            if names.is_empty() {
                continue;
            }
            let (sql, _) = dialect.bind_params(&st.sql);
            st.sql = sql;
            st.params = names
                .iter()
                .map(|n| {
                    values
                        .iter()
                        .find(|(k, _)| k == n)
                        .map(|(_, v)| {
                            if v.eq_ignore_ascii_case("null") {
                                Value::Null
                            } else {
                                Value::Text(v.clone())
                            }
                        })
                        .unwrap_or(Value::Null)
                })
                .collect();
        }
        pending.params = pending.params.iter().map(|_| Vec::new()).collect();
        pending
    }

    /// Send statements to the runtime.
    pub fn execute(&mut self, pending: PendingRun, confirmed: bool, cx: &mut Context<Self>) {
        // The runtime handles commands concurrently: a run sent before the session has
        // opened would find no session ("session closed"). Wait for it, and reconnect a
        // failed one first.
        match self.session_state {
            SessionState::Connecting => {
                self.queued_run = Some((pending, confirmed));
                cx.notify();
                return;
            }
            SessionState::Failed(_) if self.connection.is_some() => {
                let conn = self.connection.clone();
                self.set_connection(conn, cx);
                self.queued_run = Some((pending, confirmed));
                return;
            }
            _ => {}
        }
        let Some(session) = self.session else {
            cx.emit(SqlTabEvent::PickConnection);
            return;
        };
        let query = next_id();
        self.show_plan = false;
        self.results.clear();
        self.active_result = 0;
        self.messages.clear();
        self.error = None;
        self.selected = None;
        self.notices = 0;
        self.last_affected = None;
        self.export_open = false;
        self.current_statements = pending.statements.clone();
        self.current_index = 0;
        {
            // Tables that were just used rank first in completion.
            let dialect = self.dialect();
            let mut st = self.completion.borrow_mut();
            for s in &pending.statements {
                for t in switchyard_core::db::complete::table_refs(dialect, &s.sql) {
                    st.index.touch(&t.name);
                }
            }
        }
        self.edit = None;
        self.run = RunState::Running {
            query,
            started: Instant::now(),
        };
        self.ticker = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let running = this
                    .update(cx, |tab, cx| {
                        let running = matches!(tab.run, RunState::Running { .. });
                        if running {
                            cx.notify();
                        }
                        running
                    })
                    .unwrap_or(false);
                if !running {
                    break;
                }
            }
        }));
        if self.manual_txn && !self.txn_open {
            self.core.send(Command::Begin { session });
        }
        self.core.send(Command::Execute {
            session,
            query,
            statements: pending.statements,
            tags: vec![],
            confirmed_destructive: confirmed,
            fetch_limit: FetchLimit::Rows(self.fetch_limit),
        });
        cx.emit(SqlTabEvent::Changed);
        cx.notify();
    }

    /// Stop the running query.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if let RunState::Running { query, .. } | RunState::Paused { query, .. } = self.run {
            self.core.send(Command::Cancel { query });
        }
        self.plan.update(cx, |v, cx| v.stop(cx));
        cx.notify();
    }

    /// Fetch the remaining rows of a paused query.
    pub fn fetch_all(&mut self, cx: &mut Context<Self>) {
        if let RunState::Paused { query, started } = self.run {
            self.core.send(Command::FetchMore { query, all: true });
            self.run = RunState::Running { query, started };
            cx.emit(SqlTabEvent::Toast(
                "Fetching all rows in the background".into(),
            ));
        }
        cx.notify();
    }

    /// Commit or roll back the manual transaction.
    pub fn end_transaction(&mut self, commit: bool, cx: &mut Context<Self>) {
        if let Some(session) = self.session {
            self.core.send(if commit {
                Command::Commit { session }
            } else {
                Command::Rollback { session }
            });
        }
        cx.notify();
    }

    /// Switch between auto-commit and manual transactions.
    pub fn set_manual(&mut self, manual: bool, cx: &mut Context<Self>) {
        let engine = self.dialect().engine();
        if manual && !engine.supports_transactions() {
            cx.emit(SqlTabEvent::Toast(format!(
                "{} runs every statement on its own; manual transactions are not available",
                engine.display_name()
            )));
            return;
        }
        if !manual && self.txn_open {
            cx.emit(SqlTabEvent::Toast(
                "Commit or roll back the open transaction first".into(),
            ));
            return;
        }
        self.manual_txn = manual;
        cx.emit(SqlTabEvent::Changed);
        cx.notify();
    }

    /// Reformat the buffer.
    pub fn format(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).value().to_string();
        let formatted = format::format_sql(&text, self.dialect().flavor());
        if formatted != text {
            self.editor
                .update(cx, |e, cx| e.replace_all(formatted, window, cx));
        }
    }

    /// Insert text at the cursor (schema explorer actions).
    pub fn insert_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let text = text.to_owned();
        self.editor.update(cx, |e, cx| e.insert(text, window, cx));
    }

    /// Replace the whole buffer and run it.
    pub fn set_text_and_run(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let t = text.to_owned();
        self.editor.update(cx, |e, cx| e.replace_all(t, window, cx));
        self.run_script(window, cx);
    }

    /// Handle a query event from the runtime.
    pub fn on_query(&mut self, event: QueryEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            QueryEvent::StatementStarted { index } => self.current_index = index,
            QueryEvent::Columns(cols) => self.add_result(cols, window, cx),
            QueryEvent::Rows(batch) => self.add_rows(batch, cx),
            QueryEvent::Notice(n) => {
                let c = palette(cx).fg2;
                self.add_notice(n, c);
            }
            QueryEvent::NextResultSet => {}
            QueryEvent::Paused { .. } => {
                if let RunState::Running { query, started } = self.run {
                    self.run = RunState::Paused { query, started };
                }
            }
            QueryEvent::StatementDone { index, completion } => {
                let p = palette(cx);
                let label = match (completion.affected, self.results.last()) {
                    (Some(n), _) => format!(
                        "{} row{} affected",
                        thousands(n),
                        if n == 1 { "" } else { "s" }
                    ),
                    (None, Some(r)) if r.completion.is_none() => {
                        format!("{} rows", thousands(r.rows as u64))
                    }
                    _ => "OK".into(),
                };
                self.messages.push((
                    p.dev,
                    format!(
                        "Statement {} · {label} · {}",
                        index + 1,
                        ui::duration(completion.elapsed)
                    ),
                ));
                if let Some(r) = self.results.last_mut()
                    && r.completion.is_none()
                {
                    r.completion = Some(completion.clone());
                }
                if completion.affected.is_some() {
                    self.last_affected = completion.affected;
                }
            }
            QueryEvent::NeedsConfirmation { .. } => {
                // The UI pre-checks; this only happens if the profile changed meanwhile.
                cx.emit(SqlTabEvent::Toast(
                    "Confirmation required on Production".into(),
                ));
            }
            QueryEvent::Failed {
                index,
                error,
                location,
            } => self.on_failed(index, error, location, cx),
            QueryEvent::Finished { elapsed, cancelled } => {
                let affected = if self.results.is_empty() {
                    self.last_affected
                } else {
                    None
                };
                self.run = if cancelled {
                    RunState::Cancelled { elapsed }
                } else if self.error.is_some() {
                    RunState::Failed { elapsed }
                } else {
                    RunState::Done { elapsed, affected }
                };
                if cancelled {
                    let p = palette(cx);
                    self.messages.push((
                        p.stg,
                        format!("Cancelled after {} by user", ui::duration(elapsed)),
                    ));
                }
                cx.emit(SqlTabEvent::Changed);
            }
        }
        cx.notify();
    }

    fn add_result(&mut self, cols: Arc<[ColumnMeta]>, window: &mut Window, cx: &mut Context<Self>) {
        let delegate = GridDelegate::new(cols.clone());
        let table = cx.new(|cx| {
            TableState::new(delegate, window, cx)
                .cell_selectable(true)
                .row_header(false)
                .col_movable(true)
                .col_resizable(true)
                .sortable(true)
        });
        let sub = cx.subscribe_in(
            &table,
            window,
            |this, table, ev: &TableEvent, window, cx| {
                let shift = window.modifiers().shift || std::mem::take(&mut this.extending);
                match ev {
                    TableEvent::SelectCell(r, c) => {
                        // Shift+click or Shift+arrows extend a rectangle from the anchor.
                        let range = match this.anchor {
                            Some(a) if shift => Some((a, (*r, *c))),
                            _ => {
                                this.anchor = Some((*r, *c));
                                None
                            }
                        };
                        let col = table.update(cx, |t, _| {
                            let d = t.delegate_mut();
                            d.set_range(range);
                            d.data_col(*c).unwrap_or(0)
                        });
                        this.selected = Some((*r, col));
                        cx.notify();
                    }
                    TableEvent::DoubleClickedCell(r, c) => this.edit_cell(*r, *c, cx),
                    TableEvent::SelectRow(r) => {
                        this.selected = Some((*r, this.selected.map_or(0, |s| s.1)));
                        cx.notify();
                    }
                    TableEvent::ClearSelection | TableEvent::MoveColumn(..) => {
                        this.anchor = None;
                        table.update(cx, |t, _| t.delegate_mut().set_range(None));
                    }
                    _ => {}
                }
            },
        );
        if self.results.is_empty() {
            self.active_result = 0;
        }
        let sql = self
            .current_statements
            .get(self.current_index)
            .map(|s| s.sql.clone())
            .unwrap_or_default();
        self.results.push(ResultSet {
            table,
            columns: cols,
            sql,
            rows: 0,
            completion: None,
            _sub: sub,
        });
    }

    fn add_rows(&mut self, batch: RowBatch, cx: &mut Context<Self>) {
        if let Some(r) = self.results.last_mut() {
            r.rows += batch.len();
            let first = r.rows == batch.len();
            r.table.update(cx, |t, cx| {
                t.delegate_mut().push(batch);
                if first {
                    // The row-number column width depends on the row count.
                    t.refresh(cx);
                }
                cx.notify();
            });
            if first && self.selected.is_none() {
                self.selected = Some((0, 0));
            }
        }
    }

    fn add_notice(&mut self, n: Notice, c: Hsla) {
        self.notices += 1;
        self.messages
            .push((c, format!("{}: {}", n.severity, n.message)));
    }

    fn on_failed(
        &mut self,
        index: usize,
        error: DbError,
        location: Option<(u32, u32)>,
        cx: &mut Context<Self>,
    ) {
        let p = palette(cx);
        let text = self.editor.read(cx).value().to_string();
        let st = self.current_statements.get(index).cloned();
        let abs = match (location, &st) {
            (Some((l, c)), Some(st)) => {
                let base_line = switchyard_core::db::dialect::line_of_byte(&text, st.offset);
                let line_start = text[..st.offset].rfind('\n').map_or(0, |i| i + 1);
                let base_col = text[line_start..st.offset].chars().count() as u32;
                Some((base_line + l - 1, if l == 1 { base_col + c } else { c }))
            }
            _ => None,
        };
        let line_text = abs.and_then(|(l, _)| text.lines().nth(l as usize - 1).map(str::to_owned));
        let (code, message, detail, hint) = match &error {
            DbError::Server(s) => (
                s.code.clone(),
                s.message.clone(),
                s.detail.clone(),
                s.hint.clone(),
            ),
            other => (None, other.to_string(), None, None),
        };
        self.messages
            .push((p.prod, format!("Statement {} failed: {message}", index + 1)));
        self.error = Some(ErrorView {
            code,
            message: message.clone(),
            detail,
            hint,
            location: abs,
            line_text,
        });
        if let Some((l, c)) = abs {
            self.editor.update(cx, |state, cx| {
                if let Some(set) = state.diagnostics_mut() {
                    let (l, c) = (l - 1, c - 1);
                    set.push(
                        Diagnostic::new(Position::new(l, c)..Position::new(l, c + 1), message)
                            .with_severity(DiagnosticSeverity::Error),
                    );
                }
                cx.notify();
            });
        }
        self.active_result = self.results.len();
    }

    fn go_to_error(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((l, c)) = self.error.as_ref().and_then(|e| e.location) {
            self.editor.update(cx, |e, cx| {
                e.set_cursor_position(Position::new(l - 1, c - 1), window, cx);
                e.focus(window, cx);
            });
        }
    }

    fn apply_filter(&mut self, needle: &str, cx: &mut Context<Self>) {
        if let Some(r) = self.results.get(self.active_result) {
            r.table.update(cx, |t, cx| {
                t.delegate_mut().set_filter(needle);
                cx.notify();
            });
        }
        self.selected = None;
        cx.notify();
    }

    /// Rows loaded in the active result set.
    pub fn loaded_rows(&self) -> usize {
        self.results.iter().map(|r| r.rows).sum()
    }

    /// Status line text: (label, color, meta).
    pub fn status(&self, p: &Palette) -> (SharedString, Hsla, String, bool) {
        let rows = self.loaded_rows();
        if let Some(e) = self.edit.as_ref().filter(|e| !e.staged.is_empty()) {
            let target = match &e.table.schema {
                Some(s) => format!("{s}.{}", e.table.table),
                None => e.table.table.clone(),
            };
            return (
                "Editing".into(),
                p.stg,
                format!("{} staged · {target}", e.staged.len()),
                false,
            );
        }
        match &self.run {
            RunState::Idle => ("Ready".into(), p.fg3, String::new(), false),
            RunState::Running { started, .. } => (
                "Streaming".into(),
                p.acc,
                format!(
                    "{} rows · {}",
                    thousands(rows as u64),
                    ui::duration(started.elapsed())
                ),
                true,
            ),
            RunState::Paused { .. } => (
                "Paused".into(),
                p.stg,
                format!("{} rows · fetch limit reached", thousands(rows as u64)),
                false,
            ),
            RunState::Done { elapsed, affected } => {
                let meta = match (affected, self.results.is_empty()) {
                    (Some(a), true) => {
                        format!("{} affected · {}", thousands(*a), ui::duration(*elapsed))
                    }
                    _ => format!(
                        "{} rows · {}",
                        thousands(rows as u64),
                        ui::duration(*elapsed)
                    ),
                };
                ("Complete".into(), p.dev, meta, false)
            }
            RunState::Failed { elapsed } => (
                "Failed".into(),
                p.prod,
                format!("after {}", ui::duration(*elapsed)),
                false,
            ),
            RunState::Cancelled { elapsed } => (
                "Cancelled".into(),
                p.stg,
                format!(
                    "{} rows kept · {}",
                    thousands(rows as u64),
                    ui::duration(*elapsed)
                ),
                false,
            ),
        }
    }

    /// The selected row of the active result as (column, value) pairs.
    pub fn selected_row(&self, cx: &App) -> Option<(usize, RowValues)> {
        let r = self.results.get(self.active_result)?;
        let (row, _) = self.selected?;
        let t = r.table.read(cx);
        let d = t.delegate();
        if row >= d.visible_rows() {
            return None;
        }
        Some((
            row,
            r.columns
                .iter()
                .enumerate()
                .map(|(c, m)| {
                    let v = d
                        .cell(row, c)
                        .map(|cell| cell.to_value(m.data_type))
                        .unwrap_or(Value::Null);
                    (m.name.clone(), v, m.data_type)
                })
                .collect(),
        ))
    }

    /// Shift+arrow: move the selected cell by (rows, cols), extending the range.
    fn extend(&mut self, dr: isize, dc: isize, cx: &mut Context<Self>) {
        let Some(r) = self.results.get(self.active_result) else {
            return;
        };
        let table = r.table.clone();
        let (rows, cols) = {
            let t = table.read(cx);
            (t.delegate().visible_rows(), r.columns.len())
        };
        let Some((row, col)) = table.read(cx).selected_cell() else {
            return;
        };
        if rows == 0 {
            return;
        }
        let nr = row.saturating_add_signed(dr).min(rows - 1);
        let nc = col.saturating_add_signed(dc).clamp(1, cols);
        if (nr, nc) == (row, col) {
            return;
        }
        self.extending = true;
        table.update(cx, |t, cx| t.set_selected_cell(nr, nc, cx));
    }

    /// Ctrl/Cmd+C in the grid: the selected range as TSV, or the selected cell.
    pub fn copy_cells(&mut self, cx: &mut Context<Self>) {
        let Some(r) = self.results.get(self.active_result) else {
            return;
        };
        let t = r.table.read(cx);
        let d = t.delegate();
        let (rows, cols) = match d.range() {
            Some(rc) => rc,
            None => match t.selected_cell() {
                Some((row, col)) => (row..row + 1, col..col + 1),
                None => return,
            },
        };
        let cells = rows.len() * cols.len();
        if cells > crate::grid::MAX_COPY_CELLS {
            cx.emit(SqlTabEvent::Toast(format!(
                "{} cells is too many to copy; use Export instead",
                thousands(cells as u64)
            )));
            return;
        }
        let text = d.range_tsv(rows.clone(), cols.clone());
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        if cells > 1 {
            cx.emit(SqlTabEvent::Toast(format!(
                "Copied {} × {} cells",
                thousands(rows.len() as u64),
                cols.len()
            )));
        }
    }

    /// The selected cell of the active result: (view row, column name, value).
    pub fn selected_cell_value(&self, cx: &App) -> Option<(usize, String, Value)> {
        let r = self.results.get(self.active_result)?;
        let (row, col) = self.selected?;
        let d = r.table.read(cx).delegate();
        let meta = r.columns.get(col)?;
        let v = d
            .cell(row, col)
            .map(|c| c.to_value(meta.data_type))
            .unwrap_or(Value::Null);
        Some((row, meta.name.clone(), v))
    }

    /// Copy the selected range (or row) in a format, with column names.
    pub fn copy_selection(&mut self, fmt: ExportFormat, cx: &mut Context<Self>) {
        self.export_open = false;
        if let Some(r) = self.results.get(self.active_result) {
            let d = r.table.read(cx).delegate();
            if let Some((rows, cols)) = d.range()
                && rows.len() * cols.len() <= crate::grid::MAX_COPY_CELLS
            {
                let data_cols: Vec<usize> = cols.filter_map(|c| d.data_col(c)).collect();
                let names: Vec<String> = data_cols
                    .iter()
                    .map(|&c| r.columns[c].name.clone())
                    .collect();
                let values: Vec<Vec<Value>> = rows
                    .clone()
                    .map(|row| {
                        data_cols
                            .iter()
                            .map(|&c| {
                                d.cell(row, c)
                                    .map(|cell| cell.to_value(r.columns[c].data_type))
                                    .unwrap_or(Value::Null)
                            })
                            .collect()
                    })
                    .collect();
                let text = export(fmt, &names, &values, self.dialect(), "result");
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                cx.emit(SqlTabEvent::Toast(format!(
                    "Copied {} rows as {}",
                    thousands(rows.len() as u64),
                    fmt.label()
                )));
                cx.notify();
                return;
            }
        }
        let Some((_, row)) = self.selected_row(cx) else {
            cx.emit(SqlTabEvent::Toast("Select a row first".into()));
            return;
        };
        let names: Vec<String> = row.iter().map(|(n, _, _)| n.clone()).collect();
        let values: Vec<Value> = row.into_iter().map(|(_, v, _)| v).collect();
        let text = export(fmt, &names, &[values], self.dialect(), "result");
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        cx.emit(SqlTabEvent::Toast(format!(
            "Copied 1 row as {}",
            fmt.label()
        )));
        cx.notify();
    }

    /// The whole active result in a format (for "Export to file").
    pub fn export_all(&self, fmt: ExportFormat, cx: &App) -> Option<String> {
        let r = self.results.get(self.active_result)?;
        let t = r.table.read(cx);
        let d = t.delegate();
        let names: Vec<String> = r.columns.iter().map(|c| c.name.clone()).collect();
        let rows: Vec<Vec<Value>> = (0..d.visible_rows())
            .map(|row| {
                r.columns
                    .iter()
                    .enumerate()
                    .map(|(c, m)| {
                        d.cell(row, c)
                            .map(|v| v.to_value(m.data_type))
                            .unwrap_or(Value::Null)
                    })
                    .collect()
            })
            .collect();
        Some(export(fmt, &names, &rows, self.dialect(), "result"))
    }

    fn render_toolbar(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let running = matches!(self.run, RunState::Running { .. } | RunState::Paused { .. })
            || self.plan.read(cx).is_capturing();
        let env = self.environment();
        let conn_label: SharedString = self
            .connection
            .as_ref()
            .map(|c| c.name.clone().into())
            .unwrap_or_else(|| "Choose connection".into());
        let pill_color = if self.connection.is_some() {
            self.accent.unwrap_or_else(|| p.env(env))
        } else {
            p.bd2
        };
        div()
            .h(px(38.))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(6.))
            .px(px(10.))
            .border_b_1()
            .border_color(p.bd)
            .overflow_hidden()
            .whitespace_nowrap()
            .child(
                ui::button_with_key("run", "Run", ui::keys("⌘↵", "Ctrl+Enter"), Kind::Primary, p)
                    .on_click(cx.listener(|this, _, w, cx| this.run_statement(w, cx))),
            )
            .child(
                ui::button_with_key(
                    "run-script",
                    "Run script",
                    ui::keys("⇧⌘↵", "Ctrl+Shift+Enter"),
                    Kind::Secondary,
                    p,
                )
                .on_click(cx.listener(|this, _, w, cx| this.run_script(w, cx))),
            )
            .child(
                ui::button_with_key("stop", "Stop", ui::keys("⌘.", "Ctrl+."), Kind::Secondary, p)
                    .text_color(if running { p.prod } else { p.fg3 })
                    .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
            )
            .child(ui::vdivider(p, 18.))
            .child(
                ui::button_with_key(
                    "explain",
                    "Explain",
                    ui::keys("⌘E", "Ctrl+E"),
                    Kind::Secondary,
                    p,
                )
                .on_click(cx.listener(|this, _, w, cx| this.explain(false, w, cx))),
            )
            .child(
                ui::button_with_key(
                    "explain-analyze",
                    "Analyze",
                    ui::keys("⇧⌘E", "Ctrl+Shift+E"),
                    Kind::Secondary,
                    p,
                )
                .on_click(cx.listener(|this, _, w, cx| this.explain(true, w, cx))),
            )
            .child(ui::vdivider(p, 18.))
            .child(ui::segmented(
                "txn-mode",
                vec![
                    (
                        "Auto-commit".into(),
                        !self.manual_txn,
                        Box::new(cx.listener(|this, _, _, cx| this.set_manual(false, cx))),
                    ),
                    (
                        "Manual".into(),
                        self.manual_txn,
                        Box::new(cx.listener(|this, _, _, cx| this.set_manual(true, cx))),
                    ),
                ],
                20.,
                p,
            ))
            .when(self.txn_open, |d| {
                d.child(
                    ui::button("commit", "Commit", Kind::Secondary, p)
                        .on_click(cx.listener(|this, _, _, cx| this.end_transaction(true, cx))),
                )
                .child(
                    ui::button("rollback", "Rollback", Kind::Secondary, p)
                        .on_click(cx.listener(|this, _, _, cx| this.end_transaction(false, cx))),
                )
            })
            .child(
                ui::button("format", "Format", Kind::Ghost, p)
                    .on_click(cx.listener(|this, _, w, cx| this.format(w, cx))),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("conn-pill")
                    .h(px(26.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(10.))
                    .border_1()
                    .border_color(pill_color)
                    .rounded(px(6.))
                    .when(self.connection.is_some(), |d| d.bg(p.env_bg(env)))
                    .text_size(px(12.))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SqlTabEvent::PickConnection)))
                    .when(self.connection.is_some(), |d| {
                        d.child(ui::dot(p.env(env), 7.))
                    })
                    .child(div().font_weight(FontWeight::MEDIUM).child(conn_label))
                    .when(self.connection.is_some(), |d| {
                        d.child(
                            div()
                                .font_family(MONO)
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_size(px(9.5))
                                .text_color(p.env(env))
                                .child(env.badge()),
                        )
                    })
                    .when(
                        matches!(self.session_state, SessionState::Connecting),
                        |d| d.child(ui::pulse_dot("conn-pulse", p.acc, 6.)),
                    )
                    .when(matches!(self.session_state, SessionState::Failed(_)), |d| {
                        d.child(ui::dot(p.prod, 6.))
                    })
                    .child(div().text_color(p.fg3).text_size(px(10.)).child("▾")),
            )
            .into_any_element()
    }

    fn render_error(&self, e: &ErrorView, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let mut context = String::new();
        if let (Some((l, c)), Some(line)) = (e.location, &e.line_text) {
            let prefix = format!("LINE {l}: ");
            context.push_str(&prefix);
            context.push_str(line);
            context.push('\n');
            context.push_str(&" ".repeat(prefix.len() + c as usize - 1));
            context.push('^');
        }
        if let Some(d) = &e.detail {
            context.push_str(&format!("\nDETAIL:  {d}"));
        }
        if let Some(h) = &e.hint {
            context.push_str(&format!("\nHINT:  {h}"));
        }
        let copy_text = format!(
            "{} {}\n{}",
            e.code.clone().unwrap_or_default(),
            e.message,
            context
        );
        div()
            .flex_1()
            .px(px(22.))
            .py(px(20.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(
                div()
                    .flex()
                    .gap(px(10.))
                    .items_baseline()
                    .when_some(e.code.clone(), |d, code| {
                        d.child(
                            div()
                                .font_family(MONO)
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_size(px(12.))
                                .text_color(p.prod)
                                .child(format!("ERROR {code}")),
                        )
                    })
                    .child(
                        div()
                            .text_size(px(13.5))
                            .font_weight(FontWeight::MEDIUM)
                            .child(e.message.clone()),
                    ),
            )
            .when(!context.is_empty(), |d| {
                d.child(
                    div()
                        .font_family(MONO)
                        .text_size(px(12.))
                        .line_height(px(19.))
                        .text_color(p.fg2)
                        .whitespace_nowrap()
                        .children(context.lines().map(|l| div().child(l.to_owned()))),
                )
            })
            .child(
                div()
                    .flex()
                    .gap(px(6.))
                    .when_some(e.location, |d, (l, c)| {
                        d.child(
                            ui::button(
                                "goto-err",
                                format!("Go to Ln {l}, Col {c}"),
                                Kind::Secondary,
                                p,
                            )
                            .on_click(cx.listener(|this, _, w, cx| this.go_to_error(w, cx))),
                        )
                    })
                    .child(
                        ui::button("copy-err", "Copy error", Kind::Ghost, p).on_click(cx.listener(
                            move |_, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
                                cx.emit(SqlTabEvent::Toast("Error copied".into()));
                            },
                        )),
                    ),
            )
            .into_any_element()
    }

    fn render_results(
        &mut self,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let _ = window;
        let messages_ix = self.results.len();
        let mut tabs: Vec<(usize, String, String)> = self
            .results
            .iter()
            .enumerate()
            .map(|(i, r)| (i, format!("Result {}", i + 1), thousands(r.rows as u64)))
            .collect();
        tabs.push((
            messages_ix,
            "Messages".into(),
            if self.error.is_some() {
                "1".into()
            } else if self.notices > 0 {
                self.notices.to_string()
            } else {
                String::new()
            },
        ));
        let paused = matches!(self.run, RunState::Paused { .. });
        let (plan_tab, plan_busy) = {
            let v = self.plan.read(cx);
            (v.has_content() || self.show_plan, v.is_capturing())
        };
        let header = div()
            .h(px(32.))
            .flex_none()
            .flex()
            .gap(px(2.))
            .px(px(8.))
            .border_b_1()
            .border_color(p.bd)
            .bg(p.panel)
            .children(tabs.into_iter().map(|(i, label, count)| {
                let active = !self.show_plan && i == self.active_result;
                div()
                    .id(("rtab", i))
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(6.))
                    .px(px(10.))
                    .text_size(px(12.))
                    .whitespace_nowrap()
                    .text_color(if active { p.fg } else { p.fg2 })
                    .when(active, |d| d.border_b_2().border_color(p.fg))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.active_result = i;
                        this.show_plan = false;
                        this.selected = None;
                        cx.notify();
                    }))
                    .child(label)
                    .child(
                        div()
                            .font_family(MONO)
                            .text_size(px(11.))
                            .text_color(p.fg3)
                            .child(count),
                    )
            }))
            .when(plan_tab, |d| {
                d.child(
                    div()
                        .id("rtab-plan")
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap(px(6.))
                        .px(px(10.))
                        .text_size(px(12.))
                        .whitespace_nowrap()
                        .text_color(if self.show_plan { p.fg } else { p.fg2 })
                        .when(self.show_plan, |d| d.border_b_2().border_color(p.fg))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_plan = true;
                            this.export_open = false;
                            cx.notify();
                        }))
                        .child("Plan")
                        .when(plan_busy, |d| {
                            d.child(ui::pulse_dot("plan-pulse", p.acc, 6.))
                        }),
                )
            })
            .child(div().flex_1())
            .child(
                div()
                    .relative()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .when(!self.results.is_empty() && !self.show_plan, |d| {
                        d.child(
                            div()
                                .w(px(150.))
                                .h(px(22.))
                                .flex()
                                .items_center()
                                .px(px(6.))
                                .border_1()
                                .border_color(p.bd)
                                .rounded(px(5.))
                                .bg(p.bg)
                                .child(
                                    Input::new(&self.filter)
                                        .appearance(false)
                                        .text_size(px(11.5)),
                                ),
                        )
                    })
                    .when(!self.show_plan, |d| {
                        d.child(
                            ui::button("fetch-all", "Fetch all", Kind::Ghost, p)
                                .h(px(22.))
                                .text_size(px(11.5))
                                .when(!paused, |d| d.text_color(p.fg3))
                                .on_click(cx.listener(|this, _, _, cx| this.fetch_all(cx))),
                        )
                        .child(
                            ui::button("export", "Export ▾", Kind::Secondary, p)
                                .h(px(22.))
                                .px(px(8.))
                                .text_size(px(11.5))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.export_open = !this.export_open;
                                    cx.notify();
                                })),
                        )
                    })
                    .when(self.export_open && !self.show_plan, |d| {
                        d.child(self.render_export_menu(p, cx))
                    }),
            );

        let idle = self.results.is_empty()
            && self.error.is_none()
            && self.messages.is_empty()
            && matches!(self.run, RunState::Idle);
        let body: AnyElement = if self.show_plan {
            div()
                .flex_1()
                .min_h_0()
                .child(self.plan.clone())
                .into_any_element()
        } else if self.connection.is_none() {
            empty_state(
                "No connection",
                "Pick a connection for this tab to run queries.",
                p,
            )
        } else if idle {
            empty_state(
                "Run a statement",
                &format!(
                    "{} runs the statement at the cursor, {} the whole script.",
                    ui::keys("⌘↵", "Ctrl+Enter"),
                    ui::keys("⇧⌘↵", "Ctrl+Shift+Enter")
                ),
                p,
            )
        } else if self.active_result == messages_ix && self.error.is_some() {
            let e = self.error.clone();
            match e {
                Some(e) => self.render_error(&e, p, cx),
                None => div().into_any_element(),
            }
        } else if self.active_result == messages_ix {
            div()
                .id("messages")
                .flex_1()
                .overflow_y_scroll()
                .p(px(12.))
                .font_family(MONO)
                .text_size(px(12.))
                .line_height(px(20.))
                .children(
                    self.messages
                        .iter()
                        .map(|(c, m)| div().text_color(*c).child(m.clone())),
                )
                .when(self.messages.is_empty(), |d| {
                    d.text_color(p.fg3).child("No messages")
                })
                .into_any_element()
        } else if let Some(r) = self.results.get(self.active_result) {
            let cancelled = matches!(self.run, RunState::Cancelled { .. });
            let edit_bar = self.render_edit_bar(p, cx);
            let staged_panel = self.render_staged_panel(p, cx);
            let streaming = matches!(self.run, RunState::Running { .. });
            let (_, _, meta, _) = self.status(p);
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .when(cancelled, |d| {
                    d.child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .px(px(12.))
                            .py(px(6.))
                            .bg(p.stg_bg)
                            .border_b_1()
                            .border_color(p.bd)
                            .text_size(px(12.))
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(p.stg)
                                    .child("Cancelled"),
                            )
                            .child(meta.clone())
                            .child(div().flex_1())
                            .child(
                                div()
                                    .id("run-again")
                                    .text_color(p.acc)
                                    .on_click(
                                        cx.listener(|this, _, w, cx| this.run_statement(w, cx)),
                                    )
                                    .child("Run again"),
                            ),
                    )
                })
                .child(
                    div().flex_1().min_h_0().child(
                        DataTable::new(&r.table)
                            .bordered(false)
                            .stripe(false)
                            .with_size(Size::XSmall),
                    ),
                )
                .children(edit_bar)
                .children(staged_panel)
                .when(streaming, |d| {
                    d.child(
                        div()
                            .h(px(26.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .pl(px(54.))
                            .gap(px(10.))
                            .text_color(p.fg3)
                            .text_size(px(12.))
                            .child(ui::shimmer(120., p))
                            .child("receiving rows…"),
                    )
                })
                .into_any_element()
        } else if let Some(e) = self.error.clone() {
            self.render_error(&e, p, cx)
        } else if matches!(self.run, RunState::Done { .. }) {
            let (_, _, meta, _) = self.status(p);
            empty_state("Statement completed", &meta, p)
        } else if matches!(self.run, RunState::Running { .. }) {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .gap(px(10.))
                .text_color(p.fg3)
                .text_size(px(12.))
                .child(ui::shimmer(120., p))
                .child("running…")
                .into_any_element()
        } else {
            empty_state(
                "Run a statement",
                &format!(
                    "{} runs the statement at the cursor, {} the whole script.",
                    ui::keys("⌘↵", "Ctrl+Enter"),
                    ui::keys("⇧⌘↵", "Ctrl+Shift+Enter")
                ),
                p,
            )
        };

        let (label, color, meta, pulsing) = self.status(p);
        let footer =
            div()
                .h(px(26.))
                .flex_none()
                .flex()
                .items_center()
                .gap(px(14.))
                .px(px(12.))
                .border_t_1()
                .border_color(p.bd)
                .bg(p.panel)
                .text_size(px(11.5))
                .text_color(p.fg2)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .child(if pulsing {
                            ui::pulse_dot("rs-dot", color, 6.)
                        } else {
                            ui::dot(color, 6.).into_any_element()
                        })
                        .child(div().text_color(p.fg).child(label)),
                )
                .child(div().font_family(MONO).child(meta))
                .child(div().flex_1())
                .child(div().whitespace_nowrap().child(
                    match (self.error.is_some(), self.notices) {
                        (true, _) => "1 error".to_owned(),
                        (false, 0) => "no notices".to_owned(),
                        (false, n) => format!("{n} notice{}", if n == 1 { "" } else { "s" }),
                    },
                ))
                .child(
                    div()
                        .font_family(MONO)
                        .text_color(p.fg3)
                        .whitespace_nowrap()
                        .child(format!("limit {}", thousands(self.fetch_limit as u64))),
                );

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(p.surface)
            .child(header)
            .child(body)
            .child(footer)
            .into_any_element()
    }

    fn render_export_menu(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let item = |id: &'static str,
                    label: String,
                    key: SharedString,
                    fmt: Option<ExportFormat>,
                    cx: &mut Context<Self>| {
            div()
                .id(id)
                .h(px(26.))
                .flex()
                .items_center()
                .justify_between()
                .px(px(8.))
                .rounded(px(4.))
                .text_size(px(12.5))
                .hover(|s| s.bg(p.sel))
                .on_click(cx.listener(move |this, _, _, cx| match fmt {
                    Some(f) => this.copy_selection(f, cx),
                    None => {
                        this.export_open = false;
                        cx.emit(SqlTabEvent::Toast("export-file".into()));
                        cx.notify();
                    }
                }))
                .child(label)
                .child(
                    div()
                        .font_family(MONO)
                        .text_size(px(10.5))
                        .text_color(p.fg3)
                        .child(key),
                )
        };
        deferred(
            div()
                .id("export-menu")
                .absolute()
                .top(px(28.))
                .right(px(0.))
                .w(px(220.))
                .p(px(4.))
                .bg(p.elev)
                .rounded(px(7.))
                .shadow(ui::shadow(p))
                .occlude()
                .child(
                    div()
                        .px(px(8.))
                        .py(px(4.))
                        .text_size(px(11.))
                        .text_color(p.fg3)
                        .child("Copy selection as"),
                )
                .child(item(
                    "x-tsv",
                    "TSV".into(),
                    ui::keys("⌘C", "Ctrl+C"),
                    Some(ExportFormat::Tsv),
                    cx,
                ))
                .child(item(
                    "x-csv",
                    "CSV".into(),
                    "".into(),
                    Some(ExportFormat::Csv),
                    cx,
                ))
                .child(item(
                    "x-json",
                    "JSON".into(),
                    "".into(),
                    Some(ExportFormat::Json),
                    cx,
                ))
                .child(item(
                    "x-md",
                    "Markdown table".into(),
                    "".into(),
                    Some(ExportFormat::Markdown),
                    cx,
                ))
                .child(item(
                    "x-sql",
                    "SQL INSERT".into(),
                    ui::keys("⇧⌘C", "Ctrl+Shift+C"),
                    Some(ExportFormat::SqlInsert),
                    cx,
                ))
                .child(div().h(px(1.)).bg(p.bd).my(px(4.)))
                .child(item(
                    "x-file",
                    "Export full result to file…".into(),
                    "".into(),
                    None,
                    cx,
                )),
        )
        .with_priority(1)
        .into_any_element()
    }
}

fn empty_state(title: &str, sub: &str, p: &Palette) -> AnyElement {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .pt(px(40.))
        .gap(px(4.))
        .child(
            div()
                .text_size(px(13.))
                .text_color(p.fg2)
                .child(title.to_owned()),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(p.fg3)
                .child(sub.to_owned()),
        )
        .into_any_element()
}

impl Render for SqlTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let env = self.environment();
        let bar = match (&self.connection, self.accent) {
            (Some(_), Some(c)) => Some(c),
            (Some(_), None) if env != EnvironmentLabel::Local => Some(p.env(env)),
            _ => None,
        };
        let toolbar = self.render_toolbar(&p, cx);
        let results = self.render_results(&p, window, cx);
        div()
            .id("sql-tab")
            .key_context("SqlTab")
            .track_focus(&self.focus)
            .on_action(
                cx.listener(|this, _: &crate::actions::CopyCells, _, cx| this.copy_cells(cx)),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::ExtendUp, _, cx| this.extend(-1, 0, cx)),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::ExtendDown, _, cx| this.extend(1, 0, cx)),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::ExtendLeft, _, cx| this.extend(0, -1, cx)),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::ExtendRight, _, cx| this.extend(0, 1, cx)),
            )
            .size_full()
            .flex()
            .flex_col()
            .font_family(SANS)
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                if let Some((y0, h0)) = this.drag {
                    if ev.pressed_button == Some(MouseButton::Left) {
                        let y: f32 = ev.position.y.into();
                        this.editor_height = (h0 + y - y0).clamp(120.0, 900.0);
                        cx.notify();
                    } else {
                        this.drag = None;
                    }
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.drag = None),
            )
            .child(toolbar)
            .child(
                div()
                    .h(px(self.editor_height))
                    // In a short split pane the editor gives way so the results stay visible.
                    .flex_shrink(1.)
                    .min_h(px(60.))
                    .flex()
                    .relative()
                    .overflow_hidden()
                    .when_some(bar, |d, c| {
                        d.child(div().absolute().left_0().top_0().bottom_0().w(px(2.)).bg(c))
                    })
                    .child(
                        div().flex_1().h_full().pl(px(2.)).child(
                            Editor::new(&self.editor)
                                .bordered(false)
                                .appearance(false)
                                .h(relative(1.))
                                .font_family(MONO)
                                .text_size(px(12.5)),
                        ),
                    ),
            )
            .child(
                div()
                    .id("splitter")
                    .h(px(6.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .border_t_1()
                    .border_color(p.bd)
                    .bg(p.panel)
                    .cursor_row_resize()
                    .hover(|s| s.bg(p.hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, ev: &gpui_kit::MouseDownEvent, _, cx| {
                            this.drag = Some((ev.position.y.into(), this.editor_height));
                            cx.stop_propagation();
                        }),
                    )
                    .child(div().w(px(28.)).h(px(2.)).rounded(px(2.)).bg(p.bd2)),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(120.))
                    .flex()
                    .flex_col()
                    .child(results),
            )
    }
}

/// Inline editing state for one result set.
pub struct EditState {
    result_ix: usize,
    table: EditTable,
    /// Indexes of primary-key columns in the result; `None` while the detail loads.
    key_cols: Option<Vec<usize>>,
    /// Staged values by (data row, column).
    staged: Vec<((usize, usize), Value)>,
    /// The cell being edited.
    cell: Option<(usize, usize)>,
    pending_cell: Option<(usize, usize)>,
    input: Entity<InputState>,
    request: Option<RequestId>,
    _sub: Subscription,
}

impl SqlTab {
    fn default_schema(&self) -> &'static str {
        self.dialect().default_schema()
    }

    /// Start editing a cell (double click).
    fn edit_cell(&mut self, view_row: usize, col_ix: usize, cx: &mut Context<Self>) {
        let Some(r) = self.results.get(self.active_result) else {
            return;
        };
        let Some(col) = r.table.read(cx).delegate().data_col(col_ix) else {
            return;
        };
        let data_row = r.table.read(cx).delegate().data_row(view_row);
        if self.edit.as_ref().map(|e| e.result_ix) != Some(self.active_result) {
            let Some(table) = editable_table(self.dialect(), &r.sql, &r.columns) else {
                cx.emit(SqlTabEvent::Toast(
                    "Only results from a single table can be edited in place".into(),
                ));
                return;
            };
            let Some(window) = cx.windows().first().copied() else {
                return;
            };
            let input = match window.update(cx, |_, window, cx| {
                cx.new(|cx| InputState::new(window, cx).placeholder("New value"))
            }) {
                Ok(i) => i,
                Err(_) => return,
            };
            let sub = cx.subscribe(&input, |this, _, ev: &InputEvent, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.stage_current(false, cx);
                }
            });
            if let Some(session) = self.session {
                self.core.send(Command::Introspect {
                    session,
                    scope: switchyard_core::db::IntrospectScope::Detail {
                        schema: table
                            .schema
                            .clone()
                            .unwrap_or_else(|| self.default_schema().into()),
                        name: table.table.clone(),
                        kind: switchyard_core::db::ObjectKind::Table,
                    },
                    refresh: false,
                });
            }
            self.edit = Some(EditState {
                result_ix: self.active_result,
                table,
                key_cols: None,
                staged: Vec::new(),
                cell: None,
                pending_cell: None,
                input,
                request: None,
                _sub: sub,
            });
        }
        if let Some(e) = self.edit.as_mut() {
            e.pending_cell = Some((data_row, col));
        }
        self.begin_pending(cx);
    }

    fn on_table_detail(&mut self, d: &switchyard_core::db::ObjectDetail, cx: &mut Context<Self>) {
        let Some(e) = self.edit.as_mut() else { return };
        if !d.object.name.eq_ignore_ascii_case(&e.table.table) || e.key_cols.is_some() {
            return;
        }
        let Some(r) = self.results.get(e.result_ix) else {
            return;
        };
        let pk: Vec<&str> = d
            .columns
            .iter()
            .filter(|c| c.is_primary_key)
            .map(|c| c.name.as_str())
            .collect();
        let idx: Vec<usize> = pk
            .iter()
            .filter_map(|k| {
                r.columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(k))
            })
            .collect();
        if pk.is_empty() || idx.len() != pk.len() {
            self.edit = None;
            cx.emit(SqlTabEvent::Toast(if pk.is_empty() {
                "This table has no primary key, so rows cannot be edited safely".into()
            } else {
                format!(
                    "Include the primary key ({}) in the result to edit rows",
                    pk.join(", ")
                )
            }));
            cx.notify();
            return;
        }
        if e.table.schema.is_none() {
            e.table.schema = Some(d.object.schema.clone());
        }
        e.key_cols = Some(idx);
        self.begin_pending(cx);
    }

    fn begin_pending(&mut self, cx: &mut Context<Self>) {
        let Some(e) = self.edit.as_mut() else { return };
        if e.key_cols.is_none() {
            return;
        }
        let Some((row, col)) = e.pending_cell.take() else {
            return;
        };
        if e.key_cols.as_ref().is_some_and(|k| k.contains(&col)) {
            cx.emit(SqlTabEvent::Toast(
                "Primary-key columns are not edited in place".into(),
            ));
            return;
        }
        let current = e
            .staged
            .iter()
            .find(|(k, _)| *k == (row, col))
            .map(|(_, v)| v.clone())
            .or_else(|| {
                let r = self.results.get(e.result_ix)?;
                let t = r.table.read(cx);
                t.delegate()
                    .data()
                    .cell(row, col)
                    .map(|c| c.to_value(r.columns[col].data_type))
            })
            .unwrap_or(Value::Null);
        e.cell = Some((row, col));
        let text = if current.is_null() {
            String::new()
        } else {
            current.to_display()
        };
        let input = e.input.clone();
        if let Some(window) = cx.windows().first().copied() {
            let _ = window.update(cx, |_, window, cx| {
                input.update(cx, |i, cx| i.set_value(text, window, cx));
                // Focus after the table finishes handling the double click.
                window.defer(cx, move |window, cx| {
                    input.update(cx, |i, cx| i.focus(window, cx))
                });
            });
        }
        cx.notify();
    }

    fn stage_current(&mut self, null: bool, cx: &mut Context<Self>) {
        let Some(e) = self.edit.as_mut() else { return };
        let Some((row, col)) = e.cell.take() else {
            return;
        };
        let value = if null {
            Value::Null
        } else {
            Value::Text(e.input.read(cx).value().to_string())
        };
        e.staged.retain(|(k, _)| *k != (row, col));
        let display = if value.is_null() {
            None
        } else {
            Some(SharedString::from(value.to_display()))
        };
        e.staged.push(((row, col), value));
        if let Some(r) = self.results.get(e.result_ix) {
            r.table.update(cx, |t, cx| {
                t.delegate_mut().stage(row, col, display);
                cx.notify();
            });
        }
        cx.notify();
    }

    fn cancel_cell(&mut self, cx: &mut Context<Self>) {
        if let Some(e) = self.edit.as_mut() {
            e.cell = None;
        }
        cx.notify();
    }

    fn discard_edits(&mut self, cx: &mut Context<Self>) {
        if let Some(e) = self.edit.take()
            && let Some(r) = self.results.get(e.result_ix)
        {
            r.table.update(cx, |t, cx| {
                t.delegate_mut().clear_staged();
                cx.notify();
            });
        }
        cx.notify();
    }

    fn edit_statements(&self, cx: &App) -> Vec<String> {
        let Some(e) = &self.edit else {
            return Vec::new();
        };
        let (Some(keys), Some(r)) = (&e.key_cols, self.results.get(e.result_ix)) else {
            return Vec::new();
        };
        let t = r.table.read(cx);
        let data = t.delegate().data();
        let mut rows: Vec<usize> = e.staged.iter().map(|((row, _), _)| *row).collect();
        rows.sort_unstable();
        rows.dedup();
        let edits: Vec<RowEdit> = rows
            .into_iter()
            .map(|row| RowEdit {
                key: keys
                    .iter()
                    .map(|&k| {
                        let v = data
                            .cell(row, k)
                            .map(|c| c.to_value(r.columns[k].data_type))
                            .unwrap_or(Value::Null);
                        (r.columns[k].name.clone(), v)
                    })
                    .collect(),
                set: e
                    .staged
                    .iter()
                    .filter(|((rr, _), _)| *rr == row)
                    .map(|((_, c), v)| (r.columns[*c].name.clone(), v.clone()))
                    .collect(),
            })
            .collect();
        update_statements(self.dialect(), &e.table, &edits)
    }

    fn commit_edits(&mut self, cx: &mut Context<Self>) {
        let statements = self.edit_statements(cx);
        let (Some(session), Some(e)) = (self.session, self.edit.as_mut()) else {
            return;
        };
        if statements.is_empty() {
            return;
        }
        let request = next_id();
        e.request = Some(request);
        self.core.send(Command::ApplyEdits {
            session,
            request,
            statements,
        });
        cx.notify();
    }

    /// Whether this tab is waiting for `request`.
    pub fn owns_edit_request(&self, request: RequestId) -> bool {
        self.edit.as_ref().and_then(|e| e.request) == Some(request)
    }

    /// The runtime applied (or refused) the staged edits.
    pub fn on_edits_applied(
        &mut self,
        result: Result<u64, String>,
        elapsed: Duration,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(n) => {
                cx.emit(SqlTabEvent::Toast(format!(
                    "Committed {n} change{} in 1 transaction · {}",
                    if n == 1 { "" } else { "s" },
                    ui::duration(elapsed)
                )));
                self.discard_edits(cx);
                // Show the saved data.
                let statements = self.current_statements.clone();
                if !statements.is_empty() {
                    let pending = PendingRun {
                        params: statements.iter().map(|_| Vec::new()).collect(),
                        statements,
                        destructive: Vec::new(),
                    };
                    self.execute(pending, false, cx);
                }
                let _ = window;
            }
            Err(e) => {
                if let Some(ed) = self.edit.as_mut() {
                    ed.request = None;
                }
                cx.emit(SqlTabEvent::Toast(format!("Nothing saved: {e}")));
            }
        }
        cx.notify();
    }

    fn render_edit_bar(&self, p: &Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        let e = self.edit.as_ref()?;
        let (row, col) = e.cell?;
        let r = self.results.get(e.result_ix)?;
        let name = r
            .columns
            .get(col)
            .map(|c| c.name.clone())
            .unwrap_or_default();
        Some(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(12.))
                .py(px(6.))
                .bg(p.panel)
                .border_t_1()
                .border_color(p.bd)
                .text_size(px(12.))
                .child(div().w(px(8.)).h(px(8.)).rounded(px(2.)).bg(p.stg))
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(format!("Edit {name}")),
                )
                .child(div().text_color(p.fg3).child(format!("row {}", row + 1)))
                .child(
                    div()
                        .flex_1()
                        .h(px(26.))
                        .flex()
                        .items_center()
                        .px(px(6.))
                        .border_1()
                        .border_color(p.acc)
                        .rounded(px(5.))
                        .bg(p.bg)
                        .font_family(MONO)
                        .child(Input::new(&e.input).appearance(false).text_size(px(12.))),
                )
                .child(
                    ui::button("edit-null", "Set NULL", Kind::Ghost, p)
                        .on_click(cx.listener(|this, _, _, cx| this.stage_current(true, cx))),
                )
                .child(
                    ui::button("edit-stage", "Stage ↵", Kind::Secondary, p)
                        .on_click(cx.listener(|this, _, _, cx| this.stage_current(false, cx))),
                )
                .child(
                    ui::button("edit-cancel", "Cancel", Kind::Ghost, p)
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_cell(cx))),
                )
                .into_any_element(),
        )
    }

    fn render_staged_panel(&self, p: &Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        let e = self.edit.as_ref()?;
        if e.staged.is_empty() {
            return None;
        }
        let statements = self.edit_statements(cx);
        let n = e.staged.len();
        let busy = e.request.is_some();
        let target = match &e.table.schema {
            Some(s) => format!("{s}.{}", e.table.table),
            None => e.table.table.clone(),
        };
        Some(
            div()
                .flex_none()
                .flex()
                .gap(px(14.))
                .items_start()
                .px(px(12.))
                .py(px(10.))
                .border_t_1()
                .border_color(p.bd)
                .bg(p.panel)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .flex()
                                .gap(px(8.))
                                .items_center()
                                .text_size(px(12.))
                                .mb(px(6.))
                                .child(div().w(px(8.)).h(px(8.)).rounded(px(2.)).bg(p.stg))
                                .child(div().font_weight(FontWeight::SEMIBOLD).child(format!(
                                    "{n} staged change{}",
                                    if n == 1 { "" } else { "s" }
                                )))
                                .child(
                                    div().text_color(p.fg3).child(format!(
                                        "· {target} · committed in one transaction"
                                    )),
                                ),
                        )
                        .child(
                            div()
                                .id("staged-sql")
                                .max_h(px(110.))
                                .overflow_y_scroll()
                                .px(px(10.))
                                .py(px(8.))
                                .bg(p.bg)
                                .border_1()
                                .border_color(p.bd)
                                .rounded(px(6.))
                                .font_family(MONO)
                                .text_size(px(11.5))
                                .line_height(px(19.))
                                .text_color(p.fg2)
                                .children(
                                    statements
                                        .into_iter()
                                        .map(|sql| div().whitespace_nowrap().child(sql)),
                                ),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .pt(px(22.))
                        .child(
                            ui::button(
                                "commit-edits",
                                if busy {
                                    "Committing…".to_owned()
                                } else {
                                    format!("Commit {n} change{}", if n == 1 { "" } else { "s" })
                                },
                                Kind::Primary,
                                p,
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.commit_edits(cx))),
                        )
                        .child(
                            ui::button("discard-edits", "Discard", Kind::Secondary, p)
                                .on_click(cx.listener(|this, _, _, cx| this.discard_edits(cx))),
                        ),
                )
                .into_any_element(),
        )
    }
}

/// Copy / export formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportFormat {
    /// Tab-separated.
    Tsv,
    /// Comma-separated.
    Csv,
    /// JSON array of objects.
    Json,
    /// Markdown table.
    Markdown,
    /// INSERT statements.
    SqlInsert,
}

impl ExportFormat {
    /// Label.
    pub fn label(self) -> &'static str {
        match self {
            ExportFormat::Tsv => "TSV",
            ExportFormat::Csv => "CSV",
            ExportFormat::Json => "JSON",
            ExportFormat::Markdown => "Markdown",
            ExportFormat::SqlInsert => "SQL INSERT",
        }
    }

    /// File extension.
    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Tsv => "tsv",
            ExportFormat::Csv => "csv",
            ExportFormat::Json => "json",
            ExportFormat::Markdown => "md",
            ExportFormat::SqlInsert => "sql",
        }
    }
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_owned()
    }
}

fn json_value(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => (*b).into(),
        Value::Int(i) => (*i).into(),
        Value::Float(f) => {
            serde_json::Number::from_f64(*f).map_or(serde_json::Value::Null, Into::into)
        }
        Value::Json(s) => serde_json::from_str(s).unwrap_or_else(|_| s.clone().into()),
        other => other.to_display().into(),
    }
}

/// Render rows in an export format.
pub fn export(
    fmt: ExportFormat,
    names: &[String],
    rows: &[Vec<Value>],
    dialect: &dyn Dialect,
    table: &str,
) -> String {
    let mut out = String::new();
    match fmt {
        ExportFormat::Tsv => {
            out.push_str(&names.join("\t"));
            for r in rows {
                out.push('\n');
                let cells: Vec<String> = r
                    .iter()
                    .map(|v| {
                        if v.is_null() {
                            String::new()
                        } else {
                            v.to_display().replace(['\t', '\n'], " ")
                        }
                    })
                    .collect();
                out.push_str(&cells.join("\t"));
            }
        }
        ExportFormat::Csv => {
            out.push_str(
                &names
                    .iter()
                    .map(|n| csv_field(n))
                    .collect::<Vec<_>>()
                    .join(","),
            );
            for r in rows {
                out.push('\n');
                let cells: Vec<String> = r
                    .iter()
                    .map(|v| {
                        if v.is_null() {
                            String::new()
                        } else {
                            csv_field(&v.to_display())
                        }
                    })
                    .collect();
                out.push_str(&cells.join(","));
            }
        }
        ExportFormat::Json => {
            let arr: Vec<serde_json::Value> = rows
                .iter()
                .map(|r| {
                    serde_json::Value::Object(
                        names
                            .iter()
                            .cloned()
                            .zip(r.iter().map(json_value))
                            .collect(),
                    )
                })
                .collect();
            let v = if arr.len() == 1 {
                arr[0].clone()
            } else {
                serde_json::Value::Array(arr)
            };
            out = serde_json::to_string_pretty(&v).unwrap_or_default();
        }
        ExportFormat::Markdown => {
            out.push_str(&format!("| {} |\n", names.join(" | ")));
            out.push_str(&format!(
                "|{}|",
                names.iter().map(|_| " --- |").collect::<String>()
            ));
            for r in rows {
                let cells: Vec<String> = r
                    .iter()
                    .map(|v| v.to_display().replace('|', "\\|"))
                    .collect();
                out.push_str(&format!("\n| {} |", cells.join(" | ")));
            }
        }
        ExportFormat::SqlInsert => {
            let cols: Vec<String> = names.iter().map(|n| dialect.quote_ident(n)).collect();
            for (i, r) in rows.iter().enumerate() {
                if i > 0 {
                    out.push('\n');
                }
                let vals: Vec<String> = r.iter().map(|v| dialect.literal(v)).collect();
                out.push_str(&format!(
                    "INSERT INTO {table} ({}) VALUES ({});",
                    cols.join(", "),
                    vals.join(", ")
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exports() {
        let names = vec!["id".to_owned(), "email".to_owned()];
        let rows = vec![
            vec![Value::Int(1), Value::Text("a,b".into())],
            vec![Value::Int(2), Value::Null],
        ];
        let d = dialect_for(Engine::Postgres);
        assert_eq!(
            export(ExportFormat::Csv, &names, &rows, d, "t"),
            "id,email\n1,\"a,b\"\n2,"
        );
        assert_eq!(
            export(ExportFormat::Tsv, &names, &rows, d, "t"),
            "id\temail\n1\ta,b\n2\t"
        );
        assert_eq!(
            export(ExportFormat::SqlInsert, &names, &rows[..1], d, "customers"),
            "INSERT INTO customers (id, email) VALUES (1, 'a,b');"
        );
        assert!(
            export(ExportFormat::Markdown, &names, &rows, d, "t").starts_with("| id | email |")
        );
        assert!(export(ExportFormat::Json, &names, &rows, d, "t").contains("\"email\": null"));
    }
}
