//! The Runbook tab: a saved, parameterized procedure with an edit mode (title,
//! description, parameters, steps added, reordered and removed) and a run mode (values
//! for this run, each step's resolved text, status, duration and streamed output).
//!
//! The tab owns a [`RunEngine`] and executes the step it names through core
//! ([`Command::RunbookStep`]); nothing here does I/O. SQL steps open a session on their
//! connection and show results in a compact grid; HTTP and shell output streams in as
//! [`switchyard_core::Event::RunbookOutput`]. Shell and Confirm steps wait for the user.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::Sizable as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, FontWeight, Hsla,
    InteractiveElement as _, IntoElement, ParentElement as _, PathPromptOptions, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use secrecy::SecretString;
use switchyard_core::db::{ColumnMeta, Engine, dialect_for};
use switchyard_core::runbook::{
    Next, Outcome, ParamValue, ParamValues, Reveal, RunEngine, RunMode, SQL_ROW_CAP, StepStatus,
    append_capped, header_lines, parse_header_lines, render_sql, render_text,
};
use switchyard_core::store::{
    ApiRequestRef, ProfileId, Runbook, RunbookParam, RunbookStep, ShellTarget, StepKind,
};
use switchyard_core::{Command, QueryEvent, QueryId, RequestId, RuntimeHandle, SessionId};

use crate::app_state::{Profiles, next_id};
use crate::appearance::{rpx, ts};
use crate::grid::GridDelegate;
use crate::theme::{MONO, Palette, palette};
use crate::ui::{self, Kind};

/// What the tab asks of the workspace.
pub enum RunbookTabEvent {
    /// Show a toast.
    Toast(String),
    /// The runbook was deleted: close the tab.
    Close,
}

/// Edit or run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Edit,
    Run,
}

/// Edit fields of one parameter.
struct ParamFields {
    name: Entity<InputState>,
    label: Entity<InputState>,
    /// The default; for a secret parameter, a new stored value (empty keeps it).
    default: Entity<InputState>,
    secret: bool,
}

/// Edit fields of one step (only those of its kind are shown).
struct StepFields {
    title: Entity<InputState>,
    /// Note / Confirm text, SQL, shell command or HTTP body.
    body: Entity<TextareaState>,
    method: Entity<InputState>,
    url: Entity<InputState>,
    headers: Entity<TextareaState>,
    timeout: Entity<InputState>,
}

/// The run state of one step (beside its engine status).
#[derive(Default)]
struct StepRun {
    query: Option<QueryId>,
    started: Option<Instant>,
    duration: Option<Duration>,
    /// The exact text core ran (secrets masked).
    resolved: Option<String>,
    output: String,
    error: Option<String>,
    summary: Option<String>,
    grid: Option<Entity<TableState<GridDelegate>>>,
    rows: usize,
    capped: bool,
    sql_failed: bool,
    /// Destructive statements on Production waiting for "Run anyway".
    confirm: Option<Vec<String>>,
}

/// A database session the tab opened for SQL steps.
struct Session {
    id: SessionId,
    open: bool,
    /// A step waiting for it to open, and whether destructive statements were confirmed.
    waiting: Option<(usize, bool)>,
}

/// The Runbook tab.
pub struct RunbookTab {
    core: RuntimeHandle,
    /// The runbook as last saved or edited (fields are collected into it).
    pub rb: Runbook,
    /// Unsaved edits.
    dirty: bool,
    mode: Mode,
    profiles: Profiles,
    title: Entity<InputState>,
    description: Entity<TextareaState>,
    params: Vec<ParamFields>,
    steps: Vec<StepFields>,
    /// Run-mode values, by parameter (masked for secrets).
    values: Vec<(String, bool, Entity<InputState>)>,
    engine: RunEngine,
    runs: Vec<StepRun>,
    sessions: HashMap<ProfileId, Session>,
    save_request: Option<RequestId>,
    api_request: Option<RequestId>,
    api_requests: Vec<ApiRequestRef>,
    outcome: Option<Outcome>,
    error: Option<SharedString>,
}

impl EventEmitter<RunbookTabEvent> for RunbookTab {}

fn input(
    window: &mut Window,
    cx: &mut Context<RunbookTab>,
    value: &str,
    ph: &str,
) -> Entity<InputState> {
    let (v, ph) = (value.to_owned(), ph.to_owned());
    cx.new(|cx| InputState::new(window, cx).placeholder(ph).default_value(v))
}

fn masked(window: &mut Window, cx: &mut Context<RunbookTab>, ph: &str) -> Entity<InputState> {
    let ph = ph.to_owned();
    cx.new(|cx| InputState::new(window, cx).placeholder(ph).masked(true))
}

fn area(
    window: &mut Window,
    cx: &mut Context<RunbookTab>,
    value: &str,
    ph: &str,
    rows: (usize, usize),
) -> Entity<TextareaState> {
    let (v, ph) = (value.to_owned(), ph.to_owned());
    cx.new(|cx| {
        TextareaState::new(window, cx)
            .auto_grow(rows.0, rows.1)
            .placeholder(ph)
            .default_value(v)
    })
}

fn status_color(s: StepStatus, p: &Palette) -> Hsla {
    match s {
        StepStatus::Pending | StepStatus::Skipped => p.fg3,
        StepStatus::Waiting => p.stg,
        StepStatus::Running => p.acc,
        StepStatus::Ok => p.dev,
        StepStatus::Failed => p.prod,
    }
}

impl RunbookTab {
    /// A tab for `rb` (a new runbook when its id is empty, opened in edit mode).
    pub fn new(
        core: RuntimeHandle,
        rb: Runbook,
        profiles: Profiles,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mode = if rb.id.is_empty() || rb.steps.is_empty() {
            Mode::Edit
        } else {
            Mode::Run
        };
        let mut this = Self {
            core,
            dirty: rb.id.is_empty(),
            mode,
            profiles,
            title: input(window, cx, &rb.name, "Runbook title"),
            description: area(
                window,
                cx,
                &rb.description,
                "What it is for, when to use it",
                (2, 6),
            ),
            params: Vec::new(),
            steps: Vec::new(),
            values: Vec::new(),
            engine: RunEngine::for_steps(rb.steps.iter().map(|s| &s.kind)),
            runs: Vec::new(),
            sessions: HashMap::new(),
            save_request: None,
            api_request: None,
            api_requests: Vec::new(),
            outcome: None,
            error: None,
            rb,
        };
        this.rebuild_fields(window, cx);
        this.rebuild_values(window, cx);
        this.runs = this.rb.steps.iter().map(|_| StepRun::default()).collect();
        if this.mode == Mode::Edit {
            this.load_api_requests();
        }
        this
    }

    /// Tab title.
    pub fn title(&self) -> SharedString {
        if self.rb.name.trim().is_empty() {
            "New runbook".into()
        } else {
            self.rb.name.clone().into()
        }
    }

    /// Whether the tab shows runbook `id`.
    pub fn is(&self, id: &str) -> bool {
        !id.is_empty() && self.rb.id == id
    }

    /// Unsaved edits.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// The saved profiles changed.
    pub fn set_profiles(&mut self, profiles: Profiles, cx: &mut Context<Self>) {
        self.profiles = profiles;
        cx.notify();
    }

    /// Whether `query` is one of this tab's steps.
    pub fn owns_query(&self, query: QueryId) -> bool {
        self.runs.iter().any(|r| r.query == Some(query))
    }

    /// Whether `session` is one of this tab's sessions.
    pub fn owns_session(&self, session: SessionId) -> bool {
        self.sessions.values().any(|s| s.id == session)
    }

    /// Whether `request` is this tab's save or API listing.
    pub fn owns_request(&self, request: RequestId) -> bool {
        self.save_request == Some(request) || self.api_request == Some(request)
    }

    /// Stop everything and close the tab's sessions.
    pub fn shutdown(&mut self) {
        for r in &self.runs {
            if let (Some(q), Some(_)) = (r.query, r.started)
                && r.duration.is_none()
            {
                self.core.send(Command::Cancel { query: q });
            }
        }
        for s in self.sessions.values() {
            self.core.send(Command::CloseSession { session: s.id });
        }
        self.sessions.clear();
    }

    fn load_api_requests(&mut self) {
        let request = next_id();
        self.api_request = Some(request);
        self.core.send(Command::ListApiRequests { request });
    }

    // ------------------------------------------------------------ edit fields

    fn rebuild_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.params = self
            .rb
            .params
            .clone()
            .iter()
            .map(|p| ParamFields {
                name: input(window, cx, &p.name, "name"),
                label: input(window, cx, &p.label, "Label"),
                default: if p.secret {
                    masked(window, cx, "stored value (empty keeps it)")
                } else {
                    input(window, cx, &p.default, "default")
                },
                secret: p.secret,
            })
            .collect();
        self.steps = self
            .rb
            .steps
            .clone()
            .iter()
            .map(|s| {
                let (body, ph, rows) = match &s.kind {
                    StepKind::Note { text } => (text.clone(), "Markdown", (3, 12)),
                    StepKind::Confirm { text } => {
                        (text.clone(), "What to check before going on", (2, 6))
                    }
                    StepKind::Sql { sql, .. } => {
                        (sql.clone(), "SELECT … WHERE id = {{id}}", (3, 14))
                    }
                    StepKind::Shell { command, .. } => {
                        (command.clone(), "systemctl status {{service}}", (2, 8))
                    }
                    StepKind::Http(h) => (h.body.clone(), "Body (optional)", (2, 8)),
                };
                let (method, url, headers, timeout) = match &s.kind {
                    StepKind::Http(h) => (
                        h.method.clone(),
                        h.url.clone(),
                        header_lines(&h.headers),
                        String::new(),
                    ),
                    StepKind::Shell { timeout_secs, .. } => (
                        String::new(),
                        String::new(),
                        String::new(),
                        timeout_secs.to_string(),
                    ),
                    _ => Default::default(),
                };
                StepFields {
                    title: input(window, cx, &s.title, s.kind.label()),
                    body: area(window, cx, &body, ph, rows),
                    method: input(window, cx, &method, "GET"),
                    url: input(window, cx, &url, "https://…/{{param}}"),
                    headers: area(window, cx, &headers, "Name: value (one per line)", (1, 6)),
                    timeout: input(window, cx, &timeout, "300"),
                }
            })
            .collect();
    }

    /// Copy the edit fields into `self.rb`. Returns new values of secret parameters.
    fn collect(&mut self, cx: &mut Context<Self>) -> Vec<(String, SecretString)> {
        if self.params.len() != self.rb.params.len() || self.steps.len() != self.rb.steps.len() {
            return Vec::new();
        }
        let text = |e: &Entity<InputState>, cx: &Context<Self>| e.read(cx).value().to_string();
        let area_text =
            |e: &Entity<TextareaState>, cx: &Context<Self>| e.read(cx).value().to_string();
        self.rb.name = text(&self.title, cx).trim().to_owned();
        self.rb.description = area_text(&self.description, cx);
        let mut secrets = Vec::new();
        for (p, f) in self.rb.params.iter_mut().zip(&self.params) {
            p.name = text(&f.name, cx).trim().to_owned();
            p.label = text(&f.label, cx).trim().to_owned();
            p.secret = f.secret;
            let v = text(&f.default, cx);
            if f.secret {
                p.default.clear();
                if !v.is_empty() {
                    secrets.push((p.name.clone(), SecretString::from(v)));
                }
            } else {
                p.default = v;
            }
        }
        for (s, f) in self.rb.steps.iter_mut().zip(&self.steps) {
            s.title = text(&f.title, cx).trim().to_owned();
            let body = area_text(&f.body, cx);
            match &mut s.kind {
                StepKind::Note { text } | StepKind::Confirm { text } => *text = body,
                StepKind::Sql { sql, .. } => *sql = body,
                StepKind::Shell {
                    command,
                    timeout_secs,
                    ..
                } => {
                    *command = body;
                    *timeout_secs = text(&f.timeout, cx)
                        .trim()
                        .parse()
                        .unwrap_or(switchyard_core::store::runbooks::DEFAULT_SHELL_TIMEOUT_SECS);
                }
                StepKind::Http(h) => {
                    h.body = body;
                    h.method = text(&f.method, cx).trim().to_ascii_uppercase();
                    h.url = text(&f.url, cx).trim().to_owned();
                    h.headers = parse_header_lines(&area_text(&f.headers, cx));
                }
            }
        }
        secrets
    }

    /// Change the structure (add, move, remove): fields are collected first and rebuilt
    /// after, and any run state resets.
    fn restructure(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Runbook),
    ) {
        self.collect(cx);
        f(&mut self.rb);
        self.dirty = true;
        self.rebuild_fields(window, cx);
        self.reset_runs();
        cx.notify();
    }

    fn reset_runs(&mut self) {
        self.engine.reset(
            self.rb
                .steps
                .iter()
                .map(|s| switchyard_core::runbook::Gate::of(&s.kind))
                .collect(),
        );
        self.runs = self.rb.steps.iter().map(|_| StepRun::default()).collect();
        self.outcome = None;
    }

    fn rebuild_values(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let old: HashMap<String, String> = self
            .values
            .iter()
            .map(|(n, _, e)| (n.clone(), e.read(cx).value().to_string()))
            .collect();
        self.values = self
            .rb
            .params
            .clone()
            .iter()
            .map(|p| {
                let e = if p.secret {
                    masked(window, cx, "stored value")
                } else {
                    let v = old
                        .get(&p.name)
                        .cloned()
                        .unwrap_or_else(|| p.default.clone());
                    input(window, cx, &v, "")
                };
                (p.name.clone(), p.secret, e)
            })
            .collect();
    }

    fn set_mode(&mut self, mode: Mode, window: &mut Window, cx: &mut Context<Self>) {
        if mode == self.mode {
            return;
        }
        if self.engine.is_active() {
            self.error = Some("Stop the run before editing".into());
            cx.notify();
            return;
        }
        if mode == Mode::Run {
            self.collect(cx);
            self.reset_runs();
            self.rebuild_values(window, cx);
        } else if self.api_requests.is_empty() {
            self.load_api_requests();
        }
        self.mode = mode;
        self.error = None;
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let secrets = self.collect(cx);
        if let Err(e) = self.rb.validate() {
            self.error = Some(e.message.into());
            cx.notify();
            return;
        }
        let request = next_id();
        self.save_request = Some(request);
        self.core.send(Command::SaveRunbook {
            request,
            runbook: self.rb.clone(),
            secrets,
        });
        self.error = None;
        cx.notify();
    }

    /// Answer to the tab's save.
    pub fn on_saved(
        &mut self,
        result: Result<Runbook, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.save_request = None;
        match result {
            Ok(rb) => {
                // Keep unsaved structure edits made meanwhile out of it: the saved runbook
                // is what the fields were collected into.
                self.rb.id = rb.id;
                self.rb.created_at = rb.created_at;
                self.rb.updated_at = rb.updated_at;
                self.dirty = false;
                for f in &self.params {
                    if f.secret {
                        f.default.update(cx, |i, cx| i.set_value("", window, cx));
                    }
                }
                cx.emit(RunbookTabEvent::Toast(format!("Saved {}", self.rb.name)));
            }
            Err(e) => self.error = Some(e.into()),
        }
        cx.notify();
    }

    /// The API workspace's saved requests arrived.
    pub fn on_api_requests(
        &mut self,
        result: Result<Vec<ApiRequestRef>, String>,
        cx: &mut Context<Self>,
    ) {
        self.api_request = None;
        match result {
            Ok(list) => self.api_requests = list,
            Err(e) => tracing::warn!(error = %e, "API requests not listed"),
        }
        cx.notify();
    }

    fn export(&mut self, cx: &mut Context<Self>) {
        self.collect(cx);
        let dir = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_default();
        let name: String = self
            .rb
            .name
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let rx = cx.prompt_for_new_path(&dir, Some(&format!("{name}.runbook.json")));
        let core = self.core.clone();
        let runbook = self.rb.clone();
        cx.spawn(async move |_, _| {
            if let Ok(Ok(Some(path))) = rx.await {
                core.send(Command::ExportRunbook { runbook, path });
            }
        })
        .detach();
    }

    fn delete(&mut self, cx: &mut Context<Self>) {
        if !self.rb.id.is_empty() {
            self.core.send(Command::DeleteRunbook {
                id: self.rb.id.clone(),
            });
        }
        self.shutdown();
        cx.emit(RunbookTabEvent::Close);
    }

    // ------------------------------------------------------------------- run

    fn run_values(&self, cx: &Context<Self>) -> ParamValues {
        let mut v = ParamValues::new();
        for (name, secret, e) in &self.values {
            let text = e.read(cx).value().to_string();
            if *secret {
                if !text.is_empty() {
                    v.insert(name, ParamValue::Secret(SecretString::from(text)));
                }
            } else {
                v.insert(name, ParamValue::Plain(text));
            }
        }
        v
    }

    /// Values for previews: secrets present (masked) whatever was entered.
    fn preview_values(&self, cx: &Context<Self>) -> ParamValues {
        let mut v = self.run_values(cx).with_defaults(&self.rb);
        for p in self.rb.params.iter().filter(|p| p.secret) {
            if !v.contains(&p.name) {
                v.insert(&p.name, ParamValue::Secret(SecretString::from("x")));
            }
        }
        v
    }

    /// The step's text with values substituted, as it would run now.
    fn preview(&self, i: usize, cx: &Context<Self>) -> Result<String, String> {
        let Some(step) = self.rb.steps.get(i) else {
            return Ok(String::new());
        };
        let v = self.preview_values(cx);
        let t = |s: &str| render_text(s, &v, Reveal::Masked).map_err(|e| e.to_string());
        match &step.kind {
            StepKind::Note { text } | StepKind::Confirm { text } => t(text),
            StepKind::Shell { command, .. } => t(command),
            StepKind::Sql { connection, sql } => {
                let engine = connection
                    .as_ref()
                    .and_then(|c| self.profiles.db(c))
                    .map_or(Engine::Postgres, |d| d.engine);
                render_sql(dialect_for(engine), sql, &v)
                    .map(|r| r.display)
                    .map_err(|e| e.to_string())
            }
            StepKind::Http(h) => match &h.request {
                Some(r) => Ok(format!("{} {}  (saved request)", r.method, r.name)),
                None => {
                    let mut s = format!("{} {}", t(&h.method)?, t(&h.url)?);
                    for (k, val) in &h.headers {
                        s.push_str(&format!("\n{}: {}", t(k)?, t(val)?));
                    }
                    let body = t(&h.body)?;
                    if !body.is_empty() {
                        s.push_str("\n\n");
                        s.push_str(&body);
                    }
                    Ok(s)
                }
            },
        }
    }

    fn start(&mut self, mode: RunMode, cx: &mut Context<Self>) {
        if self.engine.is_active() {
            return;
        }
        let range = match mode {
            RunMode::One(i) => i..i + 1,
            RunMode::FromHere(i) => i..self.runs.len(),
            RunMode::All => 0..self.runs.len(),
        };
        for r in self.runs.iter_mut().skip(range.start).take(range.len()) {
            *r = StepRun::default();
        }
        self.outcome = None;
        self.error = None;
        let next = self.engine.start(mode);
        self.follow(next, cx);
    }

    fn follow(&mut self, next: Next, cx: &mut Context<Self>) {
        match next {
            Next::Execute(i) => self.execute(i, false, cx),
            Next::Finished(o) => {
                self.outcome = Some(o);
                let msg = match o {
                    Outcome::Completed => format!("{}: done", self.rb.name),
                    Outcome::Failed(i) => format!("{}: step {} failed", self.rb.name, i + 1),
                    Outcome::Stopped => format!("{}: stopped", self.rb.name),
                };
                cx.emit(RunbookTabEvent::Toast(msg));
            }
            Next::Wait(_) | Next::Idle => {}
        }
        cx.notify();
    }

    fn execute(&mut self, i: usize, confirmed: bool, cx: &mut Context<Self>) {
        let Some(step) = self.rb.steps.get(i) else {
            return;
        };
        let mut session = None;
        if let StepKind::Sql { connection, .. } = &step.kind {
            let Some(conn) = connection.clone().filter(|c| self.profiles.db(c).is_some()) else {
                return self.step_failed(i, "choose a connection for this step".into(), cx);
            };
            let s = self.sessions.entry(conn.clone()).or_insert_with(|| {
                let id = next_id();
                self.core.send(Command::OpenSession {
                    session: id,
                    connection: conn.clone(),
                });
                Session {
                    id,
                    open: false,
                    waiting: None,
                }
            });
            if !s.open {
                s.waiting = Some((i, confirmed));
                if let Some(r) = self.runs.get_mut(i) {
                    r.started = Some(Instant::now());
                    r.resolved = Some("Connecting…".into());
                }
                cx.notify();
                return;
            }
            session = Some(s.id);
        }
        let query = next_id();
        if let Some(r) = self.runs.get_mut(i) {
            let started = r.started.unwrap_or_else(Instant::now);
            *r = StepRun {
                query: Some(query),
                started: Some(started),
                ..StepRun::default()
            };
        }
        self.core.send(Command::RunbookStep {
            query,
            session,
            runbook: self.rb.clone(),
            step: i,
            values: self.run_values(cx),
            confirmed_destructive: confirmed,
        });
        cx.notify();
    }

    fn step_of(&self, query: QueryId) -> Option<usize> {
        self.runs.iter().position(|r| r.query == Some(query))
    }

    fn finish_step(&mut self, i: usize, ok: bool, cx: &mut Context<Self>) {
        if let Some(r) = self.runs.get_mut(i)
            && r.duration.is_none()
        {
            r.duration = r.started.map(|s| s.elapsed());
        }
        let next = self.engine.completed(i, ok);
        self.follow(next, cx);
    }

    fn step_failed(&mut self, i: usize, error: String, cx: &mut Context<Self>) {
        if let Some(r) = self.runs.get_mut(i) {
            r.error = Some(error);
        }
        self.finish_step(i, false, cx);
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(i) =
            (0..self.runs.len()).find(|i| self.engine.status(*i) == StepStatus::Running)
            && let Some(r) = self.runs.get_mut(i)
        {
            if let Some(q) = r.query {
                self.core.send(Command::Cancel { query: q });
            }
            r.error.get_or_insert_with(|| "stopped".into());
            r.duration = r.started.map(|s| s.elapsed());
            for s in self.sessions.values_mut() {
                s.waiting = None;
            }
        }
        let next = self.engine.stop();
        self.follow(next, cx);
    }

    /// A session of this tab opened or failed.
    pub fn on_session(
        &mut self,
        session: SessionId,
        result: Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        let Some((conn, s)) = self.sessions.iter_mut().find(|(_, s)| s.id == session) else {
            return;
        };
        let conn = conn.clone();
        let waiting = s.waiting.take();
        match result {
            Ok(()) => {
                s.open = true;
                if let Some((i, confirmed)) = waiting {
                    self.execute(i, confirmed, cx);
                }
            }
            Err(e) => {
                self.sessions.remove(&conn);
                if let Some((i, _)) = waiting {
                    self.step_failed(i, format!("connection failed: {e}"), cx);
                }
            }
        }
        cx.notify();
    }

    /// Resolved text of a step.
    pub fn on_resolved(&mut self, query: QueryId, text: String, cx: &mut Context<Self>) {
        if let Some(i) = self.step_of(query) {
            self.runs[i].resolved = Some(text);
            cx.notify();
        }
    }

    /// Output of an HTTP or shell step.
    pub fn on_output(&mut self, query: QueryId, text: &str, cx: &mut Context<Self>) {
        if let Some(i) = self.step_of(query) {
            append_capped(&mut self.runs[i].output, text);
            cx.notify();
        }
    }

    /// An HTTP or shell step ended, or a step could not start.
    pub fn on_step_done(
        &mut self,
        query: QueryId,
        result: Result<String, String>,
        elapsed: Duration,
        cx: &mut Context<Self>,
    ) {
        let Some(i) = self.step_of(query) else {
            return;
        };
        let ok = result.is_ok();
        let r = &mut self.runs[i];
        r.duration = Some(r.started.map_or(elapsed, |s| s.elapsed()));
        match result {
            Ok(s) => r.summary = (!s.is_empty()).then_some(s),
            Err(e) => r.error = Some(e),
        }
        if self.engine.status(i) == StepStatus::Running {
            self.finish_step(i, ok, cx);
        }
        cx.notify();
    }

    /// A query event of a SQL step.
    pub fn on_query(
        &mut self,
        query: QueryId,
        event: QueryEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(i) = self.step_of(query) else {
            return;
        };
        match event {
            QueryEvent::Columns(cols) => self.add_grid(i, cols, window, cx),
            QueryEvent::Rows(batch) => {
                let r = &mut self.runs[i];
                if r.rows >= SQL_ROW_CAP {
                    return;
                }
                r.rows += batch.len();
                if let Some(t) = &r.grid {
                    t.update(cx, |t, cx| {
                        t.delegate_mut().push(batch);
                        t.refresh(cx);
                        cx.notify();
                    });
                }
            }
            QueryEvent::Paused { .. } => {
                // The grid shows the first rows only; stop reading the rest.
                self.runs[i].capped = true;
                self.core.send(Command::Cancel { query });
            }
            QueryEvent::StatementDone { completion, .. } => {
                if let Some(n) = completion.affected {
                    self.runs[i].summary = Some(format!("{} rows affected", ui::thousands(n)));
                }
            }
            QueryEvent::Notice(n) => {
                let r = &mut self.runs[i];
                append_capped(&mut r.output, &format!("{}\n", n.message));
            }
            QueryEvent::NeedsConfirmation { destructive, .. } => {
                self.runs[i].confirm = Some(destructive.iter().map(|d| d.headline()).collect());
            }
            QueryEvent::Failed { error, .. } => {
                let r = &mut self.runs[i];
                r.sql_failed = true;
                r.error = Some(error.to_string());
            }
            QueryEvent::Finished { cancelled, .. } => {
                let r = &mut self.runs[i];
                if r.confirm.is_some() {
                    cx.notify();
                    return;
                }
                let ok = !r.sql_failed && (!cancelled || r.capped);
                if cancelled && !r.capped {
                    r.error.get_or_insert_with(|| "cancelled".into());
                }
                if r.summary.is_none() {
                    r.summary = Some(format!(
                        "{}{} rows",
                        if r.capped { "first " } else { "" },
                        ui::thousands(r.rows.min(SQL_ROW_CAP) as u64)
                    ));
                }
                if self.engine.status(i) == StepStatus::Running {
                    self.finish_step(i, ok, cx);
                }
            }
            QueryEvent::StatementStarted { .. } | QueryEvent::NextResultSet => {}
        }
        cx.notify();
    }

    fn add_grid(
        &mut self,
        i: usize,
        cols: Arc<[ColumnMeta]>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delegate = GridDelegate::new(cols).zoomed(crate::appearance::zoom(cx));
        let table = cx.new(|cx| {
            TableState::new(delegate, window, cx)
                .cell_selectable(true)
                .row_header(false)
                .col_resizable(true)
        });
        let r = &mut self.runs[i];
        r.grid = Some(table);
        r.rows = 0;
    }

    fn confirm_destructive(&mut self, i: usize, run: bool, cx: &mut Context<Self>) {
        if self
            .runs
            .get_mut(i)
            .and_then(|r| r.confirm.take())
            .is_none()
        {
            return;
        }
        if run {
            self.execute(i, true, cx);
        } else {
            self.step_failed(i, "not run: destructive statement declined".into(), cx);
        }
    }

    // ---------------------------------------------------------------- render

    fn render_header(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let mode = self.mode;
        let this = cx.entity().downgrade();
        let opt = |label: &'static str, m: Mode| {
            let this = this.clone();
            let on: ui::OnClick = Box::new(move |_, w, cx| {
                let _ = this.update(cx, |t, cx| t.set_mode(m, w, cx));
            });
            (SharedString::from(label), mode == m, on)
        };
        let running = self.engine.is_active();
        div()
            .h(rpx(40.))
            .flex_none()
            .flex()
            .items_center()
            .gap(rpx(8.))
            .px(rpx(14.))
            .border_b_1()
            .border_color(p.bd)
            .bg(p.panel)
            .child(ui::monogram("RB", 26., p))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(ts::TITLE)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.title()),
            )
            .when(self.dirty, |d| {
                d.child(
                    div()
                        .text_size(ts::LABEL)
                        .text_color(p.stg)
                        .child("unsaved"),
                )
            })
            .child(ui::segmented(
                "rb-mode",
                vec![opt("Edit", Mode::Edit), opt("Run", Mode::Run)],
                22.,
                p,
            ))
            .when(mode == Mode::Run && !running, |d| {
                d.child(
                    ui::button("rb-run-all", "Run all", Kind::Primary, p)
                        .on_click(cx.listener(|t, _, _, cx| t.start(RunMode::All, cx))),
                )
            })
            .when(running, |d| {
                d.child(
                    ui::button("rb-stop", "Stop", Kind::Destructive, p)
                        .on_click(cx.listener(|t, _, _, cx| t.stop(cx))),
                )
            })
            .when(mode == Mode::Edit, |d| {
                d.child(
                    ui::button("rb-save", "Save", Kind::Primary, p)
                        .on_click(cx.listener(|t, _, _, cx| t.save(cx))),
                )
            })
            .child({
                let t = cx.entity().downgrade();
                Button::new("rb-more")
                    .outline()
                    .small()
                    .label("⋯")
                    .dropdown_menu(move |menu, _, _| {
                        let (a, b) = (t.clone(), t.clone());
                        menu.item(PopupMenuItem::new("Export as JSON…").on_click(
                            move |_, _, cx| {
                                let _ = a.update(cx, |t, cx| t.export(cx));
                            },
                        ))
                        .separator()
                        .item(
                            PopupMenuItem::new("Delete runbook").on_click(move |_, _, cx| {
                                let _ = b.update(cx, |t, cx| t.delete(cx));
                            }),
                        )
                    })
            })
            .into_any_element()
    }

    fn field_box(p: &Palette) -> gpui_kit::Div {
        div()
            .px(rpx(8.))
            .py(rpx(4.))
            .border_1()
            .border_color(p.bd2)
            .rounded(px(6.))
            .bg(p.bg)
    }

    fn render_edit(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let mut col = div().flex().flex_col().gap(rpx(10.));
        col = col
            .child(ui::caption("TITLE", p))
            .child(
                Self::field_box(p).child(
                    Input::new(&self.title)
                        .appearance(false)
                        .text_size(ts::BASE),
                ),
            )
            .child(ui::caption("DESCRIPTION", p))
            .child(
                Self::field_box(p).child(
                    Textarea::new(&self.description)
                        .appearance(false)
                        .text_size(ts::BODY),
                ),
            );

        // Parameters.
        col = col.child(
            div()
                .flex()
                .items_center()
                .gap(rpx(8.))
                .pt(rpx(6.))
                .child(ui::caption("PARAMETERS  ·  use as {{name}}", p))
                .child(div().flex_1())
                .child(
                    ui::button("rb-add-param", "Add parameter", Kind::Secondary, p).on_click(
                        cx.listener(|t, _, w, cx| {
                            t.restructure(w, cx, |rb| {
                                let n = rb.params.len() + 1;
                                rb.params.push(RunbookParam::new(&format!("param{n}"), ""));
                            })
                        }),
                    ),
                ),
        );
        for (i, f) in self.params.iter().enumerate() {
            let secret = f.secret;
            col = col.child(
                div()
                    .flex()
                    .items_center()
                    .gap(rpx(6.))
                    .child(
                        Self::field_box(p)
                            .w(rpx(140.))
                            .font_family(MONO)
                            .child(Input::new(&f.name).appearance(false).text_size(ts::BODY)),
                    )
                    .child(
                        Self::field_box(p)
                            .w(rpx(160.))
                            .child(Input::new(&f.label).appearance(false).text_size(ts::BODY)),
                    )
                    .child(
                        Self::field_box(p)
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&f.default).appearance(false).text_size(ts::BODY)),
                    )
                    .child(
                        ui::checkbox(("rb-secret", i), secret, "Secret", p).on_click(cx.listener(
                            move |t, _, w, cx| {
                                t.restructure(w, cx, |rb| {
                                    if let Some(p) = rb.params.get_mut(i) {
                                        p.secret = !p.secret;
                                        p.default.clear();
                                    }
                                })
                            },
                        )),
                    )
                    .child(
                        ui::button(("rb-del-param", i), "✕", Kind::Ghost, p).on_click(cx.listener(
                            move |t, _, w, cx| {
                                t.restructure(w, cx, |rb| {
                                    if i < rb.params.len() {
                                        rb.params.remove(i);
                                    }
                                })
                            },
                        )),
                    ),
            );
        }

        // Steps.
        let this = cx.entity().downgrade();
        col = col.child(
            div()
                .flex()
                .items_center()
                .gap(rpx(8.))
                .pt(rpx(10.))
                .child(ui::caption("STEPS", p))
                .child(div().flex_1())
                .child(
                    Button::new("rb-add-step")
                        .outline()
                        .small()
                        .label("Add step")
                        .dropdown_menu(move |mut menu, _, _| {
                            for (badge, label) in [
                                ("MD", "Note"),
                                ("SQL", "SQL"),
                                ("API", "HTTP request"),
                                ("SH", "Shell command"),
                                ("OK?", "Confirm (manual checkpoint)"),
                            ] {
                                let t = this.clone();
                                menu = menu.item(PopupMenuItem::new(label).on_click(
                                    move |_, w, cx| {
                                        let _ = t.update(cx, |t, cx| {
                                            t.restructure(w, cx, |rb| {
                                                if let Some(k) = StepKind::empty(badge) {
                                                    rb.steps.push(RunbookStep::new("", k));
                                                }
                                            })
                                        });
                                    },
                                ));
                            }
                            menu
                        }),
                ),
        );
        let n = self.steps.len();
        for i in 0..n {
            col = col.child(self.render_edit_step(i, n, p, cx));
        }
        if n == 0 {
            col = col.child(
                div()
                    .text_size(ts::BODY)
                    .text_color(p.fg3)
                    .child("No steps yet. Add a note, SQL, HTTP, shell or confirm step."),
            );
        }
        col.into_any_element()
    }

    fn render_edit_step(
        &self,
        i: usize,
        n: usize,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (Some(step), Some(f)) = (self.rb.steps.get(i), self.steps.get(i)) else {
            return div().into_any_element();
        };
        let kind = step.kind.clone();
        let mut card =
            div()
                .flex_none()
                .flex()
                .flex_col()
                .gap(rpx(6.))
                .p(rpx(10.))
                .border_1()
                .border_color(p.bd)
                .rounded(px(8.))
                .bg(p.surface)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(rpx(6.))
                        .child(
                            div()
                                .w(rpx(22.))
                                .text_color(p.fg3)
                                .text_size(ts::LABEL)
                                .child(format!("{}.", i + 1)),
                        )
                        .child(ui::monogram(kind.badge(), 30., p))
                        .child(
                            Self::field_box(p)
                                .flex_1()
                                .min_w_0()
                                .child(Input::new(&f.title).appearance(false).text_size(ts::BODY)),
                        )
                        .when(i > 0, |d| {
                            d.child(ui::button(("rb-up", i), "↑", Kind::Ghost, p).on_click(
                                cx.listener(move |t, _, w, cx| {
                                    t.restructure(w, cx, |rb| rb.steps.swap(i, i - 1))
                                }),
                            ))
                        })
                        .when(i + 1 < n, |d| {
                            d.child(ui::button(("rb-down", i), "↓", Kind::Ghost, p).on_click(
                                cx.listener(move |t, _, w, cx| {
                                    t.restructure(w, cx, |rb| rb.steps.swap(i, i + 1))
                                }),
                            ))
                        })
                        .child(ui::button(("rb-del", i), "✕", Kind::Ghost, p).on_click(
                            cx.listener(move |t, _, w, cx| {
                                t.restructure(w, cx, |rb| {
                                    if i < rb.steps.len() {
                                        rb.steps.remove(i);
                                    }
                                })
                            }),
                        )),
                );
        let mono_area = |e: &Entity<TextareaState>| {
            Self::field_box(p)
                .font_family(MONO)
                .child(Textarea::new(e).appearance(false).text_size(ts::BODY))
        };
        match &kind {
            StepKind::Note { .. } | StepKind::Confirm { .. } => {
                card = card.child(
                    Self::field_box(p)
                        .child(Textarea::new(&f.body).appearance(false).text_size(ts::BODY)),
                );
            }
            StepKind::Sql { connection, .. } => {
                let current = connection
                    .as_ref()
                    .and_then(|c| self.profiles.db(c))
                    .map_or_else(|| "Choose a connection".to_owned(), |d| d.name.clone());
                let dbs: Vec<(ProfileId, String, &'static str)> = self
                    .profiles
                    .dbs()
                    .filter(|d| d.engine.is_sql())
                    .map(|d| (d.id.clone(), d.name.clone(), d.environment.name()))
                    .collect();
                let chosen = connection.clone();
                let t = cx.entity().downgrade();
                card = card
                    .child(
                        div().flex().items_center().gap(rpx(6.)).child(
                            Button::new(("rb-conn", i))
                                .outline()
                                .small()
                                .label(current)
                                .dropdown_menu(move |mut menu, _, _| {
                                    for (id, name, env) in &dbs {
                                        let t = t.clone();
                                        let id2 = id.clone();
                                        menu = menu.item(
                                            PopupMenuItem::new(format!("{name}  ·  {env}"))
                                                .checked(chosen.as_ref() == Some(id))
                                                .on_click(move |_, w, cx| {
                                                    let id = id2.clone();
                                                    let _ = t.update(cx, |t, cx| {
                                                        t.restructure(w, cx, |rb| {
                                                            if let Some(StepKind::Sql {
                                                                connection,
                                                                ..
                                                            }) = rb
                                                                .steps
                                                                .get_mut(i)
                                                                .map(|s| &mut s.kind)
                                                            {
                                                                *connection = Some(id);
                                                            }
                                                        })
                                                    });
                                                }),
                                        );
                                    }
                                    menu
                                }),
                        ),
                    )
                    .child(mono_area(&f.body));
            }
            StepKind::Shell { target, .. } => {
                let current = match target {
                    ShellTarget::Local => "Local shell".to_owned(),
                    ShellTarget::Host(h) => self.profiles.host(h).map_or_else(
                        || "Missing Host".to_owned(),
                        |h| format!("SSH · {}", h.name),
                    ),
                };
                let hosts: Vec<(ProfileId, String)> = self
                    .profiles
                    .hosts()
                    .map(|h| (h.id.clone(), h.name.clone()))
                    .collect();
                let t = cx.entity().downgrade();
                let set = move |target: ShellTarget| {
                    let t = t.clone();
                    move |_: &gpui_kit::ClickEvent, w: &mut Window, cx: &mut gpui_kit::App| {
                        let target = target.clone();
                        let _ = t.update(cx, |t, cx| {
                            t.restructure(w, cx, |rb| {
                                if let Some(StepKind::Shell { target: tg, .. }) =
                                    rb.steps.get_mut(i).map(|s| &mut s.kind)
                                {
                                    *tg = target;
                                }
                            })
                        });
                    }
                };
                card = card
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(rpx(6.))
                            .child(
                                Button::new(("rb-target", i)).outline().small().label(current).dropdown_menu(
                                    move |mut menu, _, _| {
                                        menu = menu.item(
                                            PopupMenuItem::new("Local shell").on_click(set(ShellTarget::Local)),
                                        );
                                        for (id, name) in &hosts {
                                            menu = menu.item(
                                                PopupMenuItem::new(format!("SSH · {name}"))
                                                    .on_click(set(ShellTarget::Host(id.clone()))),
                                            );
                                        }
                                        menu
                                    },
                                ),
                            )
                            .child(div().text_size(ts::LABEL).text_color(p.fg3).child("Time limit (s)"))
                            .child(Self::field_box(p).w(rpx(70.)).child(Input::new(&f.timeout).appearance(false).text_size(ts::BODY)))
                            .child(
                                div()
                                    .text_size(ts::LABEL)
                                    .text_color(p.fg3)
                                    .child("Runs only after you click Run on it. {{name:quote}} shell-quotes a value."),
                            ),
                    )
                    .child(mono_area(&f.body));
            }
            StepKind::Http(h) => {
                let current = h.request.as_ref().map_or_else(
                    || "Inline request".to_owned(),
                    |r| format!("{} {}", r.method, r.name),
                );
                let saved = self.api_requests.clone();
                let chosen = h.request.clone();
                let t = cx.entity().downgrade();
                let set = move |r: Option<ApiRequestRef>| {
                    let t = t.clone();
                    move |_: &gpui_kit::ClickEvent, w: &mut Window, cx: &mut gpui_kit::App| {
                        let r = r.clone();
                        let _ = t.update(cx, |t, cx| {
                            t.restructure(w, cx, |rb| {
                                if let Some(StepKind::Http(h)) =
                                    rb.steps.get_mut(i).map(|s| &mut s.kind)
                                {
                                    h.request = r;
                                }
                            })
                        });
                    }
                };
                card = card.child(
                    div().flex().items_center().gap(rpx(6.)).child(
                        Button::new(("rb-api", i))
                            .outline()
                            .small()
                            .label(current)
                            .dropdown_menu(move |mut menu, _, _| {
                                menu = menu
                                    .item(
                                        PopupMenuItem::new("Inline request")
                                            .checked(chosen.is_none())
                                            .on_click(set(None)),
                                    )
                                    .separator();
                                if saved.is_empty() {
                                    menu = menu.item(PopupMenuItem::new("No saved API requests"));
                                }
                                for r in &saved {
                                    menu = menu.item(
                                        PopupMenuItem::new(format!("{} {}", r.method, r.name))
                                            .checked(
                                                chosen
                                                    .as_ref()
                                                    .is_some_and(|c| c.request == r.request),
                                            )
                                            .on_click(set(Some(r.clone()))),
                                    );
                                }
                                menu
                            }),
                    ),
                );
                if h.request.is_some() {
                    card = card.child(
                        div()
                            .text_size(ts::LABEL)
                            .text_color(p.fg3)
                            .child("Sent with its collection, folders and active environment; runbook parameters are its {{variables}}."),
                    );
                } else {
                    card = card
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(rpx(6.))
                                .child(Self::field_box(p).w(rpx(80.)).font_family(MONO).child(
                                    Input::new(&f.method).appearance(false).text_size(ts::BODY),
                                ))
                                .child(
                                    Self::field_box(p)
                                        .flex_1()
                                        .min_w_0()
                                        .font_family(MONO)
                                        .child(
                                            Input::new(&f.url)
                                                .appearance(false)
                                                .text_size(ts::BODY),
                                        ),
                                ),
                        )
                        .child(mono_area(&f.headers))
                        .child(mono_area(&f.body));
                }
            }
        }
        card.into_any_element()
    }

    fn render_values(&self, p: &Palette) -> AnyElement {
        if self.values.is_empty() {
            return div().into_any_element();
        }
        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(rpx(6.))
            .p(rpx(10.))
            .border_1()
            .border_color(p.bd)
            .rounded(px(8.))
            .bg(p.surface)
            .child(ui::caption("VALUES FOR THIS RUN", p))
            .children(self.values.iter().map(|(name, secret, e)| {
                let label = self
                    .rb
                    .param(name)
                    .map_or(name.as_str(), RunbookParam::display_label)
                    .to_owned();
                div()
                    .flex()
                    .items_center()
                    .gap(rpx(8.))
                    .child(
                        div()
                            .w(rpx(160.))
                            .flex_none()
                            .truncate()
                            .text_size(ts::BODY)
                            .child(label),
                    )
                    .child(
                        Self::field_box(p)
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(e).appearance(false).text_size(ts::BODY)),
                    )
                    .child(
                        div()
                            .w(rpx(120.))
                            .flex_none()
                            .font_family(MONO)
                            .text_size(ts::LABEL)
                            .text_color(p.fg3)
                            .child(if *secret {
                                format!("{{{{{name}}}}} · secret")
                            } else {
                                format!("{{{{{name}}}}}")
                            }),
                    )
            }))
            .into_any_element()
    }

    fn render_note(text: &str, p: &Palette) -> AnyElement {
        use crate::chat_markdown::{Block, parse};
        div()
            .flex()
            .flex_col()
            .gap(rpx(4.))
            .text_size(ts::BODY)
            .children(parse(text).into_iter().map(|b| {
                match b {
                    Block::Heading { level, text } => div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_size(if level <= 1 { ts::HEADING } else { ts::TITLE })
                        .child(text.text),
                    Block::Item {
                        depth,
                        marker,
                        text,
                    } => div()
                        .pl(rpx(12. * (depth as f32 + 1.)))
                        .child(format!("{marker} {}", text.text)),
                    Block::Code { body, .. } => div()
                        .p(rpx(6.))
                        .rounded(px(4.))
                        .bg(p.bg)
                        .font_family(MONO)
                        .child(body),
                    Block::Quote(t) => div()
                        .pl(rpx(8.))
                        .border_l_2()
                        .border_color(p.bd2)
                        .text_color(p.fg2)
                        .child(t.text),
                    Block::Paragraph(t) => div().child(t.text),
                    Block::Rule => div().h(px(1.)).bg(p.bd),
                    Block::Table { head, rows } => {
                        div()
                            .font_family(MONO)
                            .children(std::iter::once(head).chain(rows).map(|r| {
                                div().child(
                                    r.into_iter()
                                        .map(|c| c.text)
                                        .collect::<Vec<_>>()
                                        .join("  │  "),
                                )
                            }))
                    }
                }
            }))
            .into_any_element()
    }

    fn render_run_step(&self, i: usize, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let Some(step) = self.rb.steps.get(i) else {
            return div().into_any_element();
        };
        let status = self.engine.status(i);
        let run = &self.runs[i];
        let active = self.engine.is_active();
        let color = status_color(status, p);
        let target = match &step.kind {
            StepKind::Sql { connection, .. } => connection
                .as_ref()
                .and_then(|c| self.profiles.db(c))
                .map(|d| (d.name.clone(), Some(d.environment))),
            StepKind::Shell { target, .. } => Some(match target {
                ShellTarget::Local => ("Local shell".to_owned(), None),
                ShellTarget::Host(h) => self
                    .profiles
                    .host(h)
                    .map_or(("Missing Host".to_owned(), None), |h| {
                        (format!("SSH · {}", h.name), Some(h.environment))
                    }),
            }),
            _ => None,
        };
        let header = div()
            .flex()
            .items_center()
            .gap(rpx(8.))
            .child(
                div()
                    .w(rpx(22.))
                    .text_color(p.fg3)
                    .text_size(ts::LABEL)
                    .child(format!("{}.", i + 1)),
            )
            .child(ui::monogram(step.kind.badge(), 30., p))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::MEDIUM)
                    .child(step.display_title().to_owned()),
            )
            .when_some(target, |d, (name, env)| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(rpx(5.))
                        .text_size(ts::LABEL)
                        .text_color(p.fg2)
                        .when_some(env, |d, e| d.child(ui::dot(p.env(e), 6.)))
                        .child(name),
                )
            })
            .when_some(run.duration, |d, dur| {
                d.child(
                    div()
                        .text_size(ts::LABEL)
                        .text_color(p.fg3)
                        .child(ui::duration(dur)),
                )
            })
            .child(
                div()
                    .px(rpx(6.))
                    .rounded(px(4.))
                    .border_1()
                    .border_color(color)
                    .text_color(color)
                    .text_size(ts::SMALL)
                    .child(status.label()),
            )
            .when(status == StepStatus::Running, |d| {
                d.child(
                    ui::button(("rb-cancel", i), "Cancel", Kind::Secondary, p)
                        .on_click(cx.listener(|t, _, _, cx| t.stop(cx))),
                )
            })
            .when(status == StepStatus::Waiting, |d| {
                let label = if matches!(step.kind, StepKind::Shell { .. }) {
                    "Run"
                } else {
                    "Continue"
                };
                d.child(
                    ui::button(("rb-proceed", i), label, Kind::Primary, p).on_click(cx.listener(
                        move |t, _, _, cx| {
                            let next = t.engine.proceed(i);
                            t.follow(next, cx);
                        },
                    )),
                )
                .child(
                    ui::button(("rb-skip", i), "Skip", Kind::Secondary, p).on_click(cx.listener(
                        move |t, _, _, cx| {
                            let next = t.engine.skip(i);
                            t.follow(next, cx);
                        },
                    )),
                )
            })
            .when(
                !active && !matches!(step.kind, StepKind::Note { .. }),
                |d| {
                    d.child(
                        ui::button(("rb-one", i), "Run", Kind::Secondary, p)
                            .on_click(cx.listener(move |t, _, _, cx| t.start(RunMode::One(i), cx))),
                    )
                    .child(
                        ui::button(("rb-from", i), "Run from here", Kind::Ghost, p).on_click(
                            cx.listener(move |t, _, _, cx| t.start(RunMode::FromHere(i), cx)),
                        ),
                    )
                },
            );

        let mut card = div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(rpx(6.))
            .p(rpx(10.))
            .border_1()
            .border_color(
                if matches!(status, StepStatus::Running | StepStatus::Waiting) {
                    color
                } else {
                    p.bd
                },
            )
            .rounded(px(8.))
            .bg(p.surface)
            .child(header);

        let mono_block = |text: String| {
            div()
                .p(rpx(8.))
                .rounded(px(6.))
                .bg(p.bg)
                .font_family(MONO)
                .text_size(ts::BODY)
                .whitespace_normal()
                .child(text)
        };
        match &step.kind {
            StepKind::Note { text } => card = card.child(Self::render_note(text, p)),
            StepKind::Confirm { .. } => {
                if let Ok(t) = self.preview(i, cx) {
                    card = card.child(div().text_size(ts::BODY).child(t));
                }
            }
            _ => {
                let shown = match &run.resolved {
                    Some(r) => Ok(r.clone()),
                    None => self.preview(i, cx),
                };
                card = match shown {
                    Ok(t) => card.child(mono_block(t)),
                    Err(e) => card.child(div().text_size(ts::BODY).text_color(p.prod).child(e)),
                };
            }
        }
        if let Some(c) = &run.confirm {
            card = card.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(rpx(6.))
                    .p(rpx(8.))
                    .rounded(px(6.))
                    .bg(p.prod_bg)
                    .border_1()
                    .border_color(p.prod)
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.prod)
                            .child("Production: this step changes data"),
                    )
                    .children(c.iter().map(|h| div().text_size(ts::BODY).child(h.clone())))
                    .child(
                        div()
                            .flex()
                            .gap(rpx(6.))
                            .child(
                                ui::button(("rb-confirm", i), "Run anyway", Kind::Destructive, p)
                                    .on_click(cx.listener(move |t, _, _, cx| {
                                        t.confirm_destructive(i, true, cx)
                                    })),
                            )
                            .child(
                                ui::button(("rb-decline", i), "Don't run", Kind::Secondary, p)
                                    .on_click(cx.listener(move |t, _, _, cx| {
                                        t.confirm_destructive(i, false, cx)
                                    })),
                            ),
                    ),
            );
        }
        if let Some(e) = &run.error {
            card = card.child(
                div()
                    .text_size(ts::BODY)
                    .text_color(p.prod)
                    .child(e.clone()),
            );
        } else if let Some(s) = &run.summary {
            card = card.child(
                div()
                    .text_size(ts::LABEL)
                    .text_color(p.fg2)
                    .child(s.clone()),
            );
        }
        if let Some(t) = &run.grid {
            card = card.child(
                div()
                    .h(rpx(200.))
                    .flex_none()
                    .border_1()
                    .border_color(p.bd)
                    .rounded(px(6.))
                    .overflow_hidden()
                    .child(
                        DataTable::new(t)
                            .bordered(false)
                            .stripe(false)
                            .scrollbar_visible(true, true)
                            .with_size(crate::appearance::table_size(cx)),
                    ),
            );
        }
        if !run.output.is_empty() {
            // The last lines only: output can be long, and this is not a virtualized list.
            let tail = tail_lines(&run.output, 200);
            card = card.child(
                div()
                    .id(("rb-out", i))
                    .max_h(rpx(240.))
                    .overflow_y_scroll()
                    .child(mono_block(tail)),
            );
        }
        card.into_any_element()
    }

    fn render_run(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let mut col = div().flex().flex_col().gap(rpx(10.));
        if !self.rb.description.trim().is_empty() {
            col = col.child(Self::render_note(&self.rb.description, p));
        }
        col = col.child(self.render_values(p));
        for i in 0..self.rb.steps.len() {
            col = col.child(self.render_run_step(i, p, cx));
        }
        if self.rb.steps.is_empty() {
            col = col.child(
                div()
                    .text_size(ts::BODY)
                    .text_color(p.fg3)
                    .child("No steps: switch to Edit to add some."),
            );
        }
        col.into_any_element()
    }
}

/// The last `n` lines of `text`, with a marker when lines were dropped.
fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= n {
        return text.to_owned();
    }
    format!(
        "… {} earlier lines\n{}",
        lines.len() - n,
        lines[lines.len() - n..].join("\n")
    )
}

impl Render for RunbookTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let header = self.render_header(&p, cx);
        let body = match self.mode {
            Mode::Edit => self.render_edit(&p, cx),
            Mode::Run => self.render_run(&p, cx),
        };
        div()
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(p.bg)
            .child(header)
            .when_some(self.error.clone(), |d, e| {
                d.child(
                    div()
                        .flex_none()
                        .px(rpx(14.))
                        .py(rpx(6.))
                        .bg(p.prod_bg)
                        .text_color(p.prod)
                        .text_size(ts::BODY)
                        .child(e),
                )
            })
            .child(
                div()
                    .id("rb-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(div().max_w(rpx(980.)).p(rpx(16.)).child(body)),
            )
    }
}

impl crate::workspace::Workspace {
    /// Open runbook `id` (focusing its tab when open), or a new runbook.
    pub(crate) fn open_runbook(
        &mut self,
        id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rb = match &id {
            Some(id) => {
                if let Some(ix) = self.tabs.iter().position(
                    |t| matches!(t, crate::workspace::Tab::Runbook(r) if r.read(cx).is(id)),
                ) {
                    self.activate(ix, cx);
                    return;
                }
                match self.runbooks.iter().find(|r| &r.id == id) {
                    Some(r) => r.clone(),
                    None => return self.toast("That runbook no longer exists", cx),
                }
            }
            None => Runbook::new(""),
        };
        let core = self.core.clone();
        let profiles = self.profiles.clone();
        let tab = cx.new(|cx| RunbookTab::new(core, rb, profiles, window, cx));
        let sub =
            cx.subscribe_in(
                &tab,
                window,
                |this, tab, ev: &RunbookTabEvent, _, cx| match ev {
                    RunbookTabEvent::Toast(t) => this.toast(t.clone(), cx),
                    RunbookTabEvent::Close => {
                        if let Some(ix) = this.tabs.iter().position(
                            |t| matches!(t, crate::workspace::Tab::Runbook(r) if r == tab),
                        ) {
                            this.close_tab_now(ix, cx);
                        }
                    }
                },
            );
        self._subs.push(sub);
        self.tabs.push(crate::workspace::Tab::Runbook(tab));
        self.activate(self.tabs.len() - 1, cx);
    }

    /// Ask for a runbook JSON file and import it as a new runbook.
    pub(crate) fn import_runbook(&mut self, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = chosen.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update(cx, |w, _| {
                let request = next_id();
                w.runbook_import = Some(request);
                w.core.send(Command::ImportRunbook { request, path });
            });
        })
        .detach();
    }

    /// Route a runbook event to its tab.
    pub(crate) fn on_runbook_event(
        &mut self,
        ev: switchyard_core::Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use switchyard_core::Event;
        let tabs: Vec<Entity<RunbookTab>> = self
            .tabs
            .iter()
            .filter_map(|t| match t {
                crate::workspace::Tab::Runbook(r) => Some(r.clone()),
                _ => None,
            })
            .collect();
        match ev {
            Event::Runbooks(list) => {
                self.runbooks = list;
                cx.notify();
            }
            Event::RunbookSaved { request, result } => {
                if self.runbook_import == Some(request) {
                    self.runbook_import = None;
                    match result {
                        Ok(rb) => {
                            if !self.runbooks.iter().any(|r| r.id == rb.id) {
                                self.runbooks.push(rb.clone());
                            }
                            self.open_runbook(Some(rb.id), window, cx);
                        }
                        Err(e) => self.toast(format!("Import failed: {e}"), cx),
                    }
                    return;
                }
                if let Some(t) = tabs.iter().find(|t| t.read(cx).owns_request(request)) {
                    t.update(cx, |t, cx| t.on_saved(result, window, cx));
                }
            }
            Event::ApiRequests { request, result } => {
                if let Some(t) = tabs.iter().find(|t| t.read(cx).owns_request(request)) {
                    t.update(cx, |t, cx| t.on_api_requests(result, cx));
                }
            }
            Event::RunbookResolved { query, text } => {
                if let Some(t) = tabs.iter().find(|t| t.read(cx).owns_query(query)) {
                    t.update(cx, |t, cx| t.on_resolved(query, text, cx));
                }
            }
            Event::RunbookOutput { query, text } => {
                if let Some(t) = tabs.iter().find(|t| t.read(cx).owns_query(query)) {
                    t.update(cx, |t, cx| t.on_output(query, &text, cx));
                }
            }
            Event::RunbookStepDone {
                query,
                result,
                elapsed,
                ..
            } => {
                if let Some(t) = tabs.iter().find(|t| t.read(cx).owns_query(query)) {
                    t.update(cx, |t, cx| t.on_step_done(query, result, elapsed, cx));
                }
            }
            _ => {}
        }
    }

    /// A runbook tab that owns `query` (SQL steps), if any.
    pub(crate) fn runbook_tab_for_query(
        &self,
        query: QueryId,
        cx: &gpui_kit::App,
    ) -> Option<Entity<RunbookTab>> {
        self.tabs.iter().find_map(|t| match t {
            crate::workspace::Tab::Runbook(r) if r.read(cx).owns_query(query) => Some(r.clone()),
            _ => None,
        })
    }

    /// A runbook tab that owns `session`, if any.
    pub(crate) fn runbook_tab_for_session(
        &self,
        session: SessionId,
        cx: &gpui_kit::App,
    ) -> Option<Entity<RunbookTab>> {
        self.tabs.iter().find_map(|t| match t {
            crate::workspace::Tab::Runbook(r) if r.read(cx).owns_session(session) => {
                Some(r.clone())
            }
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tails_long_output() {
        assert_eq!(tail_lines("a\nb", 5), "a\nb");
        let long = (0..10)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(tail_lines(&long, 2), "… 8 earlier lines\n8\n9");
    }

    #[test]
    fn statuses_have_colors() {
        let _ = switchyard_core::store::HttpStep::default();
        assert_eq!(StepStatus::Waiting.label(), "waiting");
    }
}
