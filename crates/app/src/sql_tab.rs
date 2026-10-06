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
    Command, FetchLimit, QueryEvent, QueryId, RuntimeHandle, SessionId, StatementRequest,
};

use crate::app_state::{SessionState, next_id};
use crate::grid::GridDelegate;
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
    /// Hex dump.
    Hex,
}

/// An SQL editor tab.
pub struct SqlTab {
    core: RuntimeHandle,
    buffer_id: String,
    pub title: SharedString,
    pub connection: Option<DbConnection>,
    pub session: Option<SessionId>,
    pub session_state: SessionState,
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
    pub dirty: bool,
    export_open: bool,
    pub viewer_format: ViewerFormat,
    pub selected: Option<(usize, usize)>,
    fetch_limit: usize,
    notices: usize,
    last_affected: Option<u64>,
    focus: FocusHandle,
    autosave: Option<Task<()>>,
    lint: Option<Task<()>>,
    position: i64,
    filter: Entity<InputState>,
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
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter rows"));
        let filter_sub = cx.subscribe(&filter, |this, input, ev: &InputEvent, cx| {
            if let InputEvent::Change = ev {
                let needle = input.read(cx).value().to_string();
                this.apply_filter(&needle, cx);
            }
        });
        let mut tab = Self {
            core,
            buffer_id: buffer.id,
            title: buffer.title.into(),
            connection: None,
            session: None,
            session_state: SessionState::None,
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
            dirty: false,
            export_open: false,
            viewer_format: ViewerFormat::Json,
            selected: None,
            fetch_limit: DEFAULT_FETCH_LIMIT,
            notices: 0,
            last_affected: None,
            focus: cx.focus_handle(),
            autosave: None,
            lint: None,
            position,
            filter,
            _subs: vec![sub, filter_sub],
        };
        if let Some(c) = connection {
            tab.set_connection(Some(c), cx);
        }
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
        self.session_state = state;
        cx.emit(SqlTabEvent::Changed);
        cx.notify();
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
        self.editor.update(cx, |state, cx| {
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
        let (text, sel, cursor) = {
            let e = self.editor.read(cx);
            (e.value().to_string(), e.selected_range(), e.cursor())
        };
        let dialect = self.dialect();
        let statements: Vec<StatementRequest> = if sel.start < sel.end {
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
        };
        self.start(statements, window, cx);
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
        let Some(session) = self.session else {
            cx.emit(SqlTabEvent::PickConnection);
            return;
        };
        let query = next_id();
        self.results.clear();
        self.active_result = 0;
        self.messages.clear();
        self.error = None;
        self.selected = None;
        self.notices = 0;
        self.last_affected = None;
        self.export_open = false;
        self.current_statements = pending.statements.clone();
        self.run = RunState::Running {
            query,
            started: Instant::now(),
        };
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
            QueryEvent::StatementStarted { .. } => {}
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
        let sub = cx.subscribe(&table, |this, _, ev: &TableEvent, cx| match ev {
            TableEvent::SelectCell(r, c) => {
                this.selected = Some((*r, c.saturating_sub(1)));
                cx.notify();
            }
            TableEvent::SelectRow(r) => {
                this.selected = Some((*r, this.selected.map_or(0, |s| s.1)));
                cx.notify();
            }
            _ => {}
        });
        if self.results.is_empty() {
            self.active_result = 0;
        }
        self.results.push(ResultSet {
            table,
            columns: cols,
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

    /// Copy the selected row in a format.
    pub fn copy_selection(&mut self, fmt: ExportFormat, cx: &mut Context<Self>) {
        self.export_open = false;
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
        let running = matches!(self.run, RunState::Running { .. } | RunState::Paused { .. });
        let env = self.environment();
        let conn_label: SharedString = self
            .connection
            .as_ref()
            .map(|c| c.name.clone().into())
            .unwrap_or_else(|| "Choose connection".into());
        let pill_color = if self.connection.is_some() {
            p.env(env)
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
                let active = i == self.active_result;
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
            .child(div().flex_1())
            .child(
                div()
                    .relative()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .when(!self.results.is_empty(), |d| {
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
                    .child(
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
                    .when(self.export_open, |d| {
                        d.child(self.render_export_menu(p, cx))
                    }),
            );

        let idle = self.results.is_empty()
            && self.error.is_none()
            && self.messages.is_empty()
            && matches!(self.run, RunState::Idle);
        let body: AnyElement = if self.connection.is_none() {
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
        let bar = if self.connection.is_some() && env != EnvironmentLabel::Local {
            Some(p.env(env))
        } else {
            None
        };
        let toolbar = self.render_toolbar(&p, cx);
        let results = self.render_results(&p, window, cx);
        div()
            .id("sql-tab")
            .key_context("SqlTab")
            .track_focus(&self.focus)
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
                    .flex_none()
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
            .child(results)
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
