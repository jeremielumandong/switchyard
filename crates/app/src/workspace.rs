//! The main window: title bar, sidebar, tabbed work area, inspector, status bar and
//! overlays. Owns UI state and routes runtime events to the views that need them.

use std::collections::HashSet;
use std::time::Duration;

use futures::StreamExt as _;
use gpui_kit::component::TitleBar;
use gpui_kit::component::input::InputState;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, FocusHandle, Focusable,
    FontWeight, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div,
    px,
};
use switchyard_core::drivers::Component;
use switchyard_core::store::{
    BufferState, DbConnection, EnvironmentLabel, HistoryEntry, Profile, ProfileId,
    Workspace as SavedWorkspace,
};
use switchyard_core::{Command, Event, EventReceiver, RuntimeHandle};

use crate::actions::{self, CommandId};
use crate::app_state::{Profiles, SessionState, badge_of, describe, next_id};
use crate::conn_editor::{ConnEditor, ConnEditorEvent};
use crate::files_tab::FilesTab;
use crate::overlays::{Overlay, SettingsPage};
use crate::palette::{PaletteEvent, PaletteMode, PaletteView};
use crate::sidebar::{SchemaState, SideTab};
use crate::sql_tab::{SqlTab, SqlTabEvent};
use crate::terminal_tab::TerminalTab;
use crate::theme::{self, MONO, Palette, SANS, palette};
use crate::ui::{self, Kind};

/// A tab in the work area.
pub enum Tab {
    /// Welcome page.
    Welcome,
    /// SQL editor.
    Sql(Entity<SqlTab>),
    /// Terminal.
    Terminal(Entity<TerminalTab>),
    /// File browser.
    Files(Entity<FilesTab>),
}

/// The root view.
pub struct Workspace {
    pub(crate) core: RuntimeHandle,
    pub(crate) profiles: Profiles,
    pub(crate) profiles_loaded: bool,
    pub(crate) tabs: Vec<Tab>,
    pub(crate) active: usize,
    pub(crate) sidebar_open: bool,
    pub(crate) side_tab: SideTab,
    pub(crate) collapsed: HashSet<String>,
    pub(crate) schema: SchemaState,
    pub(crate) inspector_open: bool,
    pub(crate) overlay: Option<Overlay>,
    pub(crate) toast: Option<SharedString>,
    toast_task: Option<Task<()>>,
    pub(crate) tunnels_open: bool,
    pub(crate) workspace_name: String,
    pub(crate) secret_backend: (&'static str, bool),
    pub(crate) components: Vec<Component>,
    pub(crate) history: Vec<HistoryEntry>,
    pub(crate) focus: FocusHandle,
    pub(crate) overlay_focus: FocusHandle,
    pub(crate) ctx: Option<crate::sidebar::CtxMenu>,
    pending_open: Option<ProfileId>,
    rebind: Vec<(Entity<SqlTab>, Option<ProfileId>)>,
    _events: Task<()>,
    _subs: Vec<Subscription>,
}

impl Focusable for Workspace {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Workspace {
    /// Build the workspace and start listening to runtime events.
    pub fn new(
        core: RuntimeHandle,
        mut events: EventReceiver,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let task = cx.spawn_in(window, async move |this, cx| {
            while let Some(ev) = events.next().await {
                if this
                    .update_in(cx, |w, window, cx| w.on_event(ev, window, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        core.send(Command::LoadProfiles);
        core.send(Command::LoadWorkspace);
        core.send(Command::DetectComponents);
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        Self {
            core,
            profiles: Profiles::default(),
            profiles_loaded: false,
            tabs: vec![Tab::Welcome],
            active: 0,
            sidebar_open: true,
            side_tab: SideTab::Connections,
            collapsed: HashSet::new(),
            schema: SchemaState::default(),
            inspector_open: window.bounds().size.width > px(1280.),
            overlay: None,
            toast: None,
            toast_task: None,
            tunnels_open: false,
            workspace_name: "Default".into(),
            secret_backend: ("", false),
            components: Vec::new(),
            history: Vec::new(),
            focus,
            overlay_focus: cx.focus_handle(),
            ctx: None,
            pending_open: None,
            rebind: Vec::new(),
            _events: task,
            _subs: Vec::new(),
        }
    }

    // ---------------------------------------------------------------- events

    fn on_event(&mut self, ev: Event, window: &mut Window, cx: &mut Context<Self>) {
        match ev {
            Event::Pong { .. } => {}
            Event::Profiles(list) => {
                self.profiles = Profiles { all: list };
                self.profiles_loaded = true;
                // Refresh connection details held by tabs.
                for t in &self.tabs {
                    if let Tab::Sql(tab) = t {
                        let id = tab.read(cx).connection.as_ref().map(|c| c.id.clone());
                        if let Some(id) = id {
                            let fresh = self.profiles.db(&id).cloned();
                            tab.update(cx, |t, cx| {
                                if fresh.is_none() {
                                    t.set_connection(None, cx);
                                } else {
                                    t.connection = fresh;
                                    cx.notify();
                                }
                            });
                        }
                    }
                }
                if let Some(Overlay::Palette(p)) = &self.overlay {
                    let profiles = self.profiles.clone();
                    p.update(cx, |p, cx| p.set_profiles(profiles, cx));
                }
            }
            Event::ProfileSaved { request, id } => {
                if let Some(Overlay::ConnEditor(ed)) = &self.overlay
                    && ed.read(cx).request() == Some(request)
                {
                    let connect = ed.read(cx).connect_after_save();
                    self.overlay = None;
                    self.toast("Saved · secrets stored in the secret store", cx);
                    if connect {
                        self.pending_open = Some(id);
                    }
                }
            }
            Event::ProfileError {
                request,
                field,
                message,
            } => {
                if let Some(Overlay::ConnEditor(ed)) = &self.overlay
                    && ed.read(cx).request() == Some(request)
                {
                    ed.update(cx, |ed, cx| ed.set_error(field, message, cx));
                }
            }
            Event::SecretBackend { name, locked } => {
                self.secret_backend = (name, locked);
            }
            Event::TestResult { request, result } => {
                if let Some(Overlay::ConnEditor(ed)) = &self.overlay {
                    ed.update(cx, |ed, cx| ed.on_test_result(request, result, cx));
                }
            }
            Event::SessionOpened {
                session,
                server_version,
            } => {
                if self.schema.session == Some(session) {
                    self.schema.on_open(server_version.clone(), &self.core);
                }
                self.for_sql_session(session, cx, |t, cx| {
                    t.on_session(
                        SessionState::Open {
                            version: server_version.clone(),
                        },
                        cx,
                    )
                });
            }
            Event::SessionFailed { session, message } => {
                if self.schema.session == Some(session) {
                    self.schema.on_failed(message.clone());
                }
                self.for_sql_session(session, cx, |t, cx| {
                    t.on_session(SessionState::Failed(message.clone()), cx)
                });
                self.toast(format!("Connection failed: {message}"), cx);
            }
            Event::Query { query, event } => {
                for t in &self.tabs {
                    if let Tab::Sql(tab) = t
                        && tab.read(cx).owns_query(query)
                    {
                        let tab = tab.clone();
                        tab.update(cx, |t, cx| t.on_query(event, window, cx));
                        break;
                    }
                }
            }
            Event::Transaction {
                session,
                open,
                statements,
            } => {
                self.for_sql_session(session, cx, |t, cx| t.on_transaction(open, statements, cx));
            }
            Event::Catalog {
                session,
                scope,
                result,
                cached_at,
            } => {
                if self.schema.session == Some(session) {
                    self.schema.on_catalog(scope, result, cached_at);
                } else {
                    match result {
                        Ok(chunk) => {
                            self.for_sql_session(session, cx, |t, cx| t.on_catalog(chunk, cx))
                        }
                        Err(e) => tracing::warn!(error = %e, "catalog load failed"),
                    }
                }
            }
            Event::History { entries, .. } => {
                self.history = entries;
            }
            Event::Workspace(w) => self.restore(w, window, cx),
            Event::DirListing {
                request,
                path,
                result,
            } => {
                for t in &self.tabs {
                    if let Tab::Files(f) = t {
                        f.update(cx, |f, cx| {
                            f.on_listing(request, path.clone(), result.clone(), cx)
                        });
                    }
                }
            }
            Event::Components(c) => self.components = c,
            Event::EditsApplied {
                request,
                result,
                elapsed,
            } => {
                for t in &self.tabs {
                    if let Tab::Sql(tab) = t
                        && tab.read(cx).owns_edit_request(request)
                    {
                        let tab = tab.clone();
                        tab.update(cx, |t, cx| t.on_edits_applied(result, elapsed, window, cx));
                        break;
                    }
                }
            }
            Event::Toast(t) => self.toast(t, cx),
            Event::Error { context, message } => self.toast(format!("{context}: {message}"), cx),
        }
        if let Some(id) = self.pending_open.clone()
            && self.profiles.db(&id).is_some()
        {
            self.pending_open = None;
            self.open_connection(&id, window, cx);
        }
        cx.notify();
    }

    fn for_sql_session(
        &self,
        session: u64,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut SqlTab, &mut Context<SqlTab>),
    ) {
        for t in &self.tabs {
            if let Tab::Sql(tab) = t
                && tab.read(cx).owns_session(session)
            {
                tab.update(cx, f);
                return;
            }
        }
    }

    fn restore(&mut self, w: SavedWorkspace, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace_name = w.name.clone();
        self.sidebar_open = !w.sidebar_collapsed;
        let mut active = None;
        for (i, b) in w.buffers.into_iter().enumerate() {
            let conn = b
                .connection_id
                .as_ref()
                .and_then(|id| self.profiles.db(id).cloned());
            let pending = conn.is_none() && b.connection_id.is_some();
            let id = b.id.clone();
            let conn_id = b.connection_id.clone();
            let tab = self.new_sql_tab(b, conn, i as i64, window, cx);
            if pending {
                // Profiles may not be loaded yet; bind once they are.
                self.rebind.push((tab.clone(), conn_id));
            }
            if w.active_buffer.as_deref() == Some(id.as_str()) {
                active = Some(self.tabs.len() - 1);
            }
        }
        if let Some(a) = active {
            self.active = a;
        } else if self.tabs.len() > 1 {
            self.active = 1;
        }
        self.sync_schema(cx);
    }

    // ------------------------------------------------------------------ tabs

    fn new_sql_tab(
        &mut self,
        buffer: BufferState,
        conn: Option<DbConnection>,
        position: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<SqlTab> {
        let core = self.core.clone();
        let tab = cx.new(|cx| SqlTab::new(core, buffer, conn, position, window, cx));
        let sub = cx.subscribe_in(&tab, window, |this, tab, ev: &SqlTabEvent, window, cx| {
            this.on_tab_event(tab.clone(), ev, window, cx)
        });
        self._subs.push(sub);
        self.tabs.push(Tab::Sql(tab.clone()));
        tab
    }

    fn on_tab_event(
        &mut self,
        tab: Entity<SqlTab>,
        ev: &SqlTabEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match ev {
            SqlTabEvent::PickConnection => self.open_palette(PaletteMode::Connections, window, cx),
            SqlTabEvent::Toast(t) if t == "export-file" => self.export_to_file(tab, window, cx),
            SqlTabEvent::Toast(t) => self.toast(t.clone(), cx),
            SqlTabEvent::ConfirmDestructive(p) => {
                let input =
                    cx.new(|cx| InputState::new(window, cx).placeholder("type the object name"));
                input.update(cx, |i, cx| i.focus(window, cx));
                self.overlay = Some(Overlay::Safety {
                    tab,
                    pending: p.clone(),
                    input,
                });
            }
            SqlTabEvent::PromptParams(p) => {
                let mut names: Vec<String> = Vec::new();
                for n in p.params.iter().flatten() {
                    if !names.contains(n) {
                        names.push(n.clone());
                    }
                }
                let inputs: Vec<(String, Entity<InputState>)> = names
                    .into_iter()
                    .map(|n| {
                        let i = cx.new(|cx| {
                            InputState::new(window, cx).placeholder("value (NULL for null)")
                        });
                        (n, i)
                    })
                    .collect();
                if let Some((_, first)) = inputs.first() {
                    first.update(cx, |i, cx| i.focus(window, cx));
                }
                self.overlay = Some(Overlay::Params {
                    tab,
                    pending: p.clone(),
                    inputs,
                });
            }
            SqlTabEvent::Changed => self.sync_schema(cx),
        }
        cx.notify();
    }

    fn export_to_file(
        &mut self,
        tab: Entity<SqlTab>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(text) = tab
            .read(cx)
            .export_all(crate::sql_tab::ExportFormat::Csv, cx)
        else {
            return self.toast("Nothing to export", cx);
        };
        let dir = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_default();
        let name = format!("result.{}", crate::sql_tab::ExportFormat::Csv.extension());
        let rx = cx.prompt_for_new_path(&dir, Some(&name));
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                core.send(Command::WriteFile {
                    path,
                    contents: text,
                });
            } else {
                let _ = this.update(cx, |w, cx| w.toast("Export cancelled", cx));
            }
        })
        .detach();
    }

    pub(crate) fn active_sql(&self) -> Option<Entity<SqlTab>> {
        match self.tabs.get(self.active) {
            Some(Tab::Sql(t)) => Some(t.clone()),
            _ => None,
        }
    }

    pub(crate) fn activate(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.tabs.len() {
            self.active = ix;
            self.save_layout(cx);
            self.sync_schema(cx);
            cx.notify();
        }
    }

    fn save_layout(&self, cx: &App) {
        let active_buffer = match self.tabs.get(self.active) {
            Some(Tab::Sql(t)) => Some(t.read(cx).buffer_id().to_owned()),
            _ => None,
        };
        self.core.send(Command::SaveWorkspace(SavedWorkspace {
            name: self.workspace_name.clone(),
            buffers: Vec::new(),
            pinned: Vec::new(),
            sidebar_collapsed: !self.sidebar_open,
            active_buffer,
        }));
    }

    /// Open (or focus) a SQL tab for a connection.
    pub(crate) fn open_connection(
        &mut self,
        id: &ProfileId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(conn) = self.profiles.db(id).cloned() else {
            return;
        };
        // An active tab without a connection adopts it.
        if let Some(t) = self.active_sql()
            && t.read(cx).connection.is_none()
        {
            t.update(cx, |t, cx| t.set_connection(Some(conn), cx));
            self.sync_schema(cx);
            return;
        }
        if let Some(ix) = self.tabs.iter().position(|t| match t {
            Tab::Sql(s) => s.read(cx).connection.as_ref().is_some_and(|c| &c.id == id),
            _ => false,
        }) {
            return self.activate(ix, cx);
        }
        let n = self
            .tabs
            .iter()
            .filter(|t| matches!(t, Tab::Sql(_)))
            .count();
        let buffer = BufferState {
            id: format!("b{}", next_id()),
            title: format!("{}.sql", conn.name),
            connection_id: Some(id.clone()),
            text: String::new(),
            cursor: 0,
        };
        self.new_sql_tab(buffer, Some(conn), n as i64, window, cx);
        self.active = self.tabs.len() - 1;
        self.save_layout(cx);
        self.sync_schema(cx);
    }

    fn new_query_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let conn = self
            .active_sql()
            .and_then(|t| t.read(cx).connection.clone())
            .or_else(|| self.profiles.dbs().next().cloned());
        let n = self
            .tabs
            .iter()
            .filter(|t| matches!(t, Tab::Sql(_)))
            .count();
        let buffer = BufferState {
            id: format!("b{}", next_id()),
            title: format!("query-{}.sql", n + 1),
            connection_id: conn.as_ref().map(|c| c.id.clone()),
            text: String::new(),
            cursor: 0,
        };
        let tab = self.new_sql_tab(buffer, conn, n as i64, window, cx);
        tab.update(cx, |t, cx| t.save_now(cx));
        self.active = self.tabs.len() - 1;
        self.save_layout(cx);
        self.sync_schema(cx);
        let ed = tab.read(cx).editor().clone();
        ed.update(cx, |e, cx| e.focus(window, cx));
    }

    fn close_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        if let Tab::Sql(t) = &self.tabs[ix] {
            let t = t.read(cx);
            if t.txn_open {
                self.toast(
                    "This tab has an open transaction · commit or roll back first",
                    cx,
                );
                return;
            }
            self.core.send(Command::DeleteBuffer {
                id: t.buffer_id().to_owned(),
            });
            if let Some(s) = t.session {
                self.core.send(Command::CloseSession { session: s });
            }
        }
        self.tabs.remove(ix);
        if self.tabs.is_empty() {
            self.tabs.push(Tab::Welcome);
        }
        self.active = self.active.min(self.tabs.len() - 1);
        self.save_layout(cx);
        self.sync_schema(cx);
        cx.notify();
    }

    pub(crate) fn open_terminal(&mut self, host: Option<ProfileId>, cx: &mut Context<Self>) {
        let name = host
            .as_ref()
            .and_then(|h| self.profiles.host(h))
            .map(|h| h.name.clone())
            .unwrap_or_else(|| "Local shell".into());
        let env = host
            .as_ref()
            .and_then(|h| self.profiles.host(h))
            .map(|h| h.environment)
            .unwrap_or_default();
        let t = cx.new(|_| TerminalTab::new(name, env, host.is_some()));
        self.tabs.push(Tab::Terminal(t));
        self.active = self.tabs.len() - 1;
        cx.notify();
    }

    pub(crate) fn open_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.tabs.iter().position(|t| matches!(t, Tab::Files(_))) {
            return self.activate(ix, cx);
        }
        let core = self.core.clone();
        let f = cx.new(|cx| FilesTab::new(core, window, cx));
        self.tabs.push(Tab::Files(f));
        self.active = self.tabs.len() - 1;
        cx.notify();
    }

    fn show_welcome(&mut self, cx: &mut Context<Self>) {
        if let Some(ix) = self.tabs.iter().position(|t| matches!(t, Tab::Welcome)) {
            return self.activate(ix, cx);
        }
        self.tabs.push(Tab::Welcome);
        self.active = self.tabs.len() - 1;
        cx.notify();
    }

    // --------------------------------------------------------------- toasts

    pub(crate) fn toast(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.toast = Some(text.into());
        self.toast_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(2600))
                .await;
            let _ = this.update(cx, |w, cx| {
                w.toast = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    // ------------------------------------------------------------- overlays

    pub(crate) fn open_palette(
        &mut self,
        mode: PaletteMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let profiles = self.profiles.clone();
        let view = cx.new(|cx| PaletteView::new(mode, profiles, window, cx));
        let sub = cx.subscribe_in(&view, window, |this, _, ev: &PaletteEvent, window, cx| {
            this.overlay = None;
            match ev {
                PaletteEvent::Run(id) => this.run_command(*id, window, cx),
                PaletteEvent::Open(id) => this.open_profile(id, window, cx),
                PaletteEvent::Dismiss => {}
            }
            cx.notify();
        });
        self._subs.push(sub);
        self.overlay = Some(Overlay::Palette(view));
        cx.notify();
    }

    pub(crate) fn open_profile(
        &mut self,
        id: &ProfileId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // From the quick switcher: a SQL tab without a connection picks it up.
        match self.profiles.all.iter().find(|p| p.id() == id).cloned() {
            Some(Profile::Db(d)) => {
                if let Some(t) = self.active_sql() {
                    t.update(cx, |t, cx| t.set_connection(Some(d.clone()), cx));
                    self.sync_schema(cx);
                } else {
                    self.open_connection(id, window, cx);
                }
            }
            Some(Profile::Host(h)) => self.open_terminal(Some(h.id), cx),
            Some(Profile::File(_)) => self.open_files(window, cx),
            Some(Profile::Terminal(t)) => self.open_terminal(t.host_id, cx),
            None => {}
        }
    }

    pub(crate) fn open_conn_editor(
        &mut self,
        kind: crate::conn_editor::ConnKind,
        existing: Option<Profile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let core = self.core.clone();
        let profiles = self.profiles.clone();
        let comps = self.components.clone();
        let ed = cx.new(|cx| ConnEditor::new(core, profiles, kind, existing, comps, window, cx));
        let sub = cx.subscribe_in(
            &ed,
            window,
            |this, _, ev: &ConnEditorEvent, _window, cx| match ev {
                ConnEditorEvent::Close => {
                    this.overlay = None;
                    cx.notify();
                }
                ConnEditorEvent::Toast(t) => this.toast(t.clone(), cx),
            },
        );
        self._subs.push(sub);
        self.overlay = Some(Overlay::ConnEditor(ed));
        cx.notify();
    }

    pub(crate) fn open_settings(
        &mut self,
        page: SettingsPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.core.send(Command::DetectComponents);
        self.overlay = Some(Overlay::Settings(page));
        window.focus(&self.overlay_focus, cx);
        cx.notify();
    }

    pub(crate) fn open_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search history"));
        let core = self.core.clone();
        let sub = cx.subscribe(
            &input,
            move |_, input, ev: &gpui_kit::component::input::InputEvent, cx| {
                if matches!(ev, gpui_kit::component::input::InputEvent::Change) {
                    let q = input.read(cx).value().to_string();
                    core.send(Command::SearchHistory {
                        request: next_id(),
                        query: q,
                        connection: None,
                    });
                }
            },
        );
        self._subs.push(sub);
        input.update(cx, |i, cx| i.focus(window, cx));
        self.core.send(Command::SearchHistory {
            request: next_id(),
            query: String::new(),
            connection: None,
        });
        self.overlay = Some(Overlay::History(input));
        cx.notify();
    }

    pub(crate) fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.overlay = None;
        self.tunnels_open = false;
        self.ctx = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    pub(crate) fn set_theme(&mut self, dark: bool, window: &mut Window, cx: &mut Context<Self>) {
        theme::apply(
            if dark {
                Palette::dark()
            } else {
                Palette::light()
            },
            Some(window),
            cx,
        );
        self.core.send(Command::SetSetting {
            key: "theme".into(),
            value: (if dark { "dark" } else { "light" }).into(),
        });
        cx.notify();
    }

    /// Execute a palette command.
    pub(crate) fn run_command(
        &mut self,
        id: CommandId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::conn_editor::ConnKind;
        match id {
            CommandId::NewConnection => self.open_conn_editor(ConnKind::Postgres, None, window, cx),
            CommandId::NewHost => self.open_conn_editor(ConnKind::Ssh, None, window, cx),
            CommandId::NewTerminal => self.open_terminal(None, cx),
            CommandId::NewQueryTab => self.new_query_tab(window, cx),
            CommandId::RunStatement => {
                if let Some(t) = self.active_sql() {
                    t.update(cx, |t, cx| t.run_statement(window, cx));
                }
            }
            CommandId::RunScript => {
                if let Some(t) = self.active_sql() {
                    t.update(cx, |t, cx| t.run_script(window, cx));
                }
            }
            CommandId::StopQuery => {
                if let Some(t) = self.active_sql() {
                    t.update(cx, |t, cx| t.stop(cx));
                }
            }
            CommandId::FormatSql => {
                if let Some(t) = self.active_sql() {
                    t.update(cx, |t, cx| t.format(window, cx));
                }
            }
            CommandId::CommitTransaction => {
                if let Some(t) = self.active_sql() {
                    t.update(cx, |t, cx| t.end_transaction(true, cx));
                }
            }
            CommandId::RollbackTransaction => {
                if let Some(t) = self.active_sql() {
                    t.update(cx, |t, cx| t.end_transaction(false, cx));
                }
            }
            CommandId::OpenFiles => self.open_files(window, cx),
            CommandId::Settings => self.open_settings(SettingsPage::General, window, cx),
            CommandId::SettingsDrivers => self.open_settings(SettingsPage::Drivers, window, cx),
            CommandId::ToggleTheme => {
                let dark = !palette(cx).dark;
                self.set_theme(dark, window, cx);
            }
            CommandId::ToggleInspector => self.inspector_open = !self.inspector_open,
            CommandId::ToggleSidebar => {
                self.sidebar_open = !self.sidebar_open;
                self.save_layout(cx);
            }
            CommandId::ShowWelcome => self.show_welcome(cx),
            CommandId::OpenComponents => {
                self.overlay = Some(Overlay::Components);
                window.focus(&self.overlay_focus, cx);
            }
            CommandId::RefreshSchema => {
                self.side_tab = SideTab::Schema;
                self.schema.refresh(&self.core);
            }
            CommandId::ImportSshConfig => self.import_ssh_config(cx),
            CommandId::ExportProfiles => {
                let dir = std::env::var_os("HOME")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_default();
                let rx = cx.prompt_for_new_path(&dir, Some("switchyard-profiles.json"));
                let core = self.core.clone();
                cx.spawn(async move |_, _| {
                    if let Ok(Ok(Some(path))) = rx.await {
                        core.send(Command::ExportProfiles { path });
                    }
                })
                .detach();
            }
            CommandId::ShowHistory => self.open_history(window, cx),
        }
        cx.notify();
    }

    fn import_ssh_config(&mut self, cx: &mut Context<Self>) {
        self.core.send(Command::ImportSshConfig);
        self.toast("Importing Hosts from ~/.ssh/config…", cx);
    }

    // --------------------------------------------------------------- render

    fn render_title_bar(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let dark = p.dark;
        TitleBar::new()
            .h(px(38.))
            .bg(p.panel)
            .border_color(p.bd)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .items_center()
                    .gap(px(12.))
                    .pr(px(10.))
                    .font_family(SANS)
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap(px(7.))
                            .h(px(26.))
                            .px(px(8.))
                            .rounded(px(6.))
                            .hover(|s| s.bg(p.hover))
                            .child(
                                div()
                                    .size(px(14.))
                                    .rounded(px(3.))
                                    .bg(p.fg)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(div().size(px(6.)).rounded(px(1.)).bg(p.panel)),
                            )
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_size(px(12.5))
                                    .text_color(p.fg)
                                    .whitespace_nowrap()
                                    .child(self.workspace_name.clone()),
                            )
                            .child(div().text_color(p.fg3).text_size(px(10.)).child("▾")),
                    )
                    .child(
                        ui::button("tb-sidebar", "Sidebar", Kind::Ghost, p).on_click(cx.listener(
                            |this, _, w, cx| this.run_command(CommandId::ToggleSidebar, w, cx),
                        )),
                    )
                    .child(
                        div().flex_1().flex().justify_center().min_w_0().child(
                            div()
                                .id("tb-search")
                                .w_full()
                                .max_w(px(460.))
                                .h(px(26.))
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .pl(px(10.))
                                .pr(px(6.))
                                .border_1()
                                .border_color(p.bd)
                                .rounded(px(6.))
                                .bg(p.bg)
                                .text_color(p.fg3)
                                .text_size(px(12.5))
                                .cursor_text()
                                .hover(|s| s.border_color(p.bd2))
                                .on_click(cx.listener(|this, _, w, cx| {
                                    this.open_palette(PaletteMode::Commands, w, cx)
                                }))
                                .child(
                                    div()
                                        .flex_1()
                                        .truncate()
                                        .child("Search connections, tables, commands…"),
                                )
                                .child(ui::kbd(ui::keys("⇧⌘P", "Ctrl+Shift+P"), p)),
                        ),
                    )
                    .child(
                        ui::button(
                            "tb-theme",
                            if dark { "Light" } else { "Dark" },
                            Kind::Ghost,
                            p,
                        )
                        .on_click(cx.listener(move |this, _, w, cx| this.set_theme(!dark, w, cx))),
                    )
                    .child(
                        ui::button("tb-components", "Components", Kind::Ghost, p).on_click(
                            cx.listener(|this, _, w, cx| {
                                this.run_command(CommandId::OpenComponents, w, cx)
                            }),
                        ),
                    )
                    .child(
                        ui::button("tb-settings", "Settings", Kind::Ghost, p).on_click(
                            cx.listener(|this, _, w, cx| {
                                this.open_settings(SettingsPage::General, w, cx)
                            }),
                        ),
                    ),
            )
            .into_any_element()
    }

    fn tab_info(
        &self,
        tab: &Tab,
        cx: &App,
    ) -> (SharedString, SharedString, Option<EnvironmentLabel>, bool) {
        match tab {
            Tab::Welcome => ("SY".into(), "Welcome".into(), None, false),
            Tab::Sql(t) => {
                let t = t.read(cx);
                let badge = t.connection.as_ref().map_or("SQL", |c| c.engine.badge());
                (
                    badge.into(),
                    t.title.clone(),
                    t.connection.as_ref().map(|c| c.environment),
                    t.dirty,
                )
            }
            Tab::Terminal(t) => {
                let t = t.read(cx);
                ("SSH".into(), t.title.clone(), Some(t.env), false)
            }
            Tab::Files(_) => ("FS".into(), "Files · local".into(), None, false),
        }
    }

    fn render_tab_strip(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("tab-strip")
            .h(px(34.))
            .flex_none()
            .flex()
            .items_stretch()
            .bg(p.panel)
            .border_b_1()
            .border_color(p.bd)
            .overflow_x_scroll()
            .children(self.tabs.iter().enumerate().map(|(i, tab)| {
                let (badge, label, env, dirty) = self.tab_info(tab, cx);
                let active = i == self.active;
                let edge: Hsla = match env {
                    Some(e) if e != EnvironmentLabel::Local || active => {
                        let c = p.env(e);
                        if active { c } else { c.opacity(0.45) }
                    }
                    _ => gpui_kit::transparent_black(),
                };
                div()
                    .id(("tab", i))
                    .relative()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(7.))
                    .pl(px(12.))
                    .pr(px(8.))
                    .min_w_0()
                    .max_w(px(230.))
                    .border_r_1()
                    .border_color(p.bd)
                    .text_size(px(12.5))
                    .bg(if active {
                        p.surface
                    } else {
                        gpui_kit::transparent_black()
                    })
                    .text_color(if active { p.fg } else { p.fg2 })
                    .when(active, |d| d.mb(px(-1.)))
                    .on_click(cx.listener(move |this, _, _, cx| this.activate(i, cx)))
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .top_0()
                            .h(px(2.))
                            .bg(edge),
                    )
                    .child(
                        div()
                            .flex_none()
                            .px(px(4.))
                            .rounded(px(3.))
                            .border_1()
                            .border_color(p.bd)
                            .text_color(p.fg2)
                            .font_family(MONO)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(px(8.5))
                            .line_height(px(14.))
                            .child(badge),
                    )
                    .child(div().truncate().child(label))
                    .child(ui::dot(
                        if dirty {
                            p.fg2
                        } else {
                            gpui_kit::transparent_black()
                        },
                        6.,
                    ))
                    .child(
                        div()
                            .id(("tab-close", i))
                            .flex_none()
                            .px(px(4.))
                            .rounded(px(4.))
                            .text_color(p.fg3)
                            .text_size(px(11.))
                            .hover(|s| s.bg(p.hover).text_color(p.fg))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                cx.stop_propagation();
                                this.close_tab(i, cx);
                            }))
                            .child("×"),
                    )
            }))
            .child(
                div()
                    .id("tab-new")
                    .flex()
                    .items_center()
                    .px(px(10.))
                    .text_color(p.fg3)
                    .font_family(MONO)
                    .text_size(px(13.))
                    .hover(|s| s.text_color(p.fg))
                    .on_click(cx.listener(|this, _, w, cx| this.new_query_tab(w, cx)))
                    .child("+"),
            )
            .child(div().flex_1())
            .into_any_element()
    }

    fn render_welcome(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        use crate::conn_editor::ConnKind;
        let first = self.profiles_loaded && self.profiles.is_empty();
        let card = |id: &'static str, badge: &'static str, title: &'static str, body: String| {
            div()
                .id(id)
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(6.))
                .p(px(14.))
                .border_1()
                .border_color(p.bd)
                .rounded(px(8.))
                .bg(p.panel)
                .hover(|s| s.border_color(p.bd2))
                .child(
                    div().flex().child(
                        div()
                            .px(px(4.))
                            .py(px(1.))
                            .border_1()
                            .border_color(p.bd2)
                            .rounded(px(3.))
                            .font_family(MONO)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(px(9.))
                            .text_color(p.fg2)
                            .child(badge),
                    ),
                )
                .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
                .child(div().text_color(p.fg2).text_size(px(12.)).child(body))
        };
        let ssh_hosts = std::env::var_os("HOME")
            .map(|h| std::path::Path::new(&h).join(".ssh/config"))
            .filter(|p| p.exists())
            .is_some();
        let recents: Vec<&Profile> = self
            .profiles
            .all
            .iter()
            .filter(|p| !matches!(p, Profile::Terminal(_)))
            .take(8)
            .collect();
        div()
            .id("welcome")
            .flex_1()
            .overflow_y_scroll()
            .flex()
            .justify_center()
            .px(px(32.))
            .py(px(64.))
            .child(
                div()
                    .w_full()
                    .max_w(px(680.))
                    .flex()
                    .flex_col()
                    .gap(px(28.))
                    .child(
                        div()
                            .child(
                                div()
                                    .text_size(px(24.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(if first { "Welcome to Switchyard" } else { greeting() }),
                            )
                            .child(div().mt(px(6.)).text_color(p.fg2).text_size(px(13.5)).child(if first {
                                "Databases, terminals and file transfer behind one connection model. Nothing to install — PostgreSQL, SQL Server, SSH, SFTP and FTP drivers are built in."
                            } else {
                                "Pick up where you left off, or open something new."
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(10.))
                            .child(
                                card("w-host", "SSH", "New Host", "A server you reach over SSH. Terminals, files and tunnels reuse it.".into())
                                    .on_click(cx.listener(|this, _, w, cx| this.open_conn_editor(ConnKind::Ssh, None, w, cx))),
                            )
                            .child(
                                card("w-conn", "DB", "New Connection", "PostgreSQL, SQL Server, SFTP or FTP — direct or via a Host.".into())
                                    .on_click(cx.listener(|this, _, w, cx| this.open_conn_editor(ConnKind::Postgres, None, w, cx))),
                            )
                            .child(
                                card(
                                    "w-import",
                                    "~/.ssh",
                                    "Import ssh config",
                                    if ssh_hosts {
                                        "Turn the Host entries in ~/.ssh/config into Hosts, ProxyJump included.".into()
                                    } else {
                                        "No ~/.ssh/config found on this machine.".into()
                                    },
                                )
                                .on_click(cx.listener(|this, _, _, cx| this.import_ssh_config(cx))),
                            ),
                    )
                    .when(!first && !recents.is_empty(), |d| {
                        d.child(
                            div()
                                .child(
                                    div()
                                        .flex()
                                        .justify_between()
                                        .px(px(2.))
                                        .pb(px(6.))
                                        .border_b_1()
                                        .border_color(p.bd)
                                        .child(ui::caption("RECENT", p))
                                        .child(ui::caption(format!("{} to switch", ui::keys("⌘P", "Ctrl+P")), p).font_family(MONO)),
                                )
                                .children(recents.into_iter().enumerate().map(|(i, prof)| {
                                    let id = prof.id().clone();
                                    div()
                                        .id(("recent", i))
                                        .h(px(32.))
                                        .flex()
                                        .items_center()
                                        .gap(px(8.))
                                        .px(px(4.))
                                        .border_b_1()
                                        .border_color(p.line)
                                        .text_size(px(12.5))
                                        .hover(|s| s.bg(p.hover))
                                        .on_click(cx.listener(move |this, _, w, cx| this.open_profile(&id, w, cx)))
                                        .child(div().w(px(14.)).child(ui::dot(p.env(prof.environment()), 7.)))
                                        .child(ui::monogram(badge_of(prof), 34., p))
                                        .child(div().flex_1().font_weight(FontWeight::MEDIUM).child(prof.name().to_owned()))
                                        .child(
                                            div()
                                                .font_family(MONO)
                                                .text_size(px(11.5))
                                                .text_color(p.fg3)
                                                .child(describe(prof, &self.profiles)),
                                        )
                                }))
                                .into_any_element(),
                        )
                    })
                    .when(first, |d| {
                        d.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(4.))
                                .p(px(22.))
                                .border_1()
                                .border_dashed()
                                .border_color(p.bd2)
                                .rounded(px(8.))
                                .text_color(p.fg2)
                                .text_size(px(12.5))
                                .child(div().text_color(p.fg).font_weight(FontWeight::MEDIUM).child("Nothing saved yet"))
                                .child("Start with a Host. Once it's saved, open a terminal, browse its files or tunnel a database through it with one login."),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .gap(px(18.))
                            .text_color(p.fg3)
                            .text_size(px(12.))
                            .child(shortcut_hint("Command palette", ui::keys("⇧⌘P", "Ctrl+Shift+P"), p))
                            .child(shortcut_hint("Quick switch", ui::keys("⌘P", "Ctrl+P"), p))
                            .child(shortcut_hint("Settings", ui::keys("⌘,", "Ctrl+,"), p)),
                    ),
            )
            .into_any_element()
    }

    fn render_status_bar(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let tab = self.tabs.get(self.active);
        let (env, conn, engine, rows, txn) = match tab {
            Some(Tab::Sql(t)) => {
                let t = t.read(cx);
                let env = t.connection.as_ref().map(|c| c.environment);
                let conn = t
                    .connection
                    .as_ref()
                    .map(|c| {
                        let via = c
                            .via_host
                            .as_ref()
                            .and_then(|h| self.profiles.host(h))
                            .map(|h| format!(" @ {}", h.name))
                            .unwrap_or_default();
                        format!("{}{via}", c.name)
                    })
                    .unwrap_or_else(|| "No connection".into());
                let engine = match &t.session_state {
                    SessionState::Open { version } => version.clone(),
                    SessionState::Connecting => "connecting…".into(),
                    SessionState::Failed(_) => "connection failed".into(),
                    SessionState::None => String::new(),
                };
                let (_, _, meta, _) = t.status(p);
                let txn = if t.txn_open {
                    (
                        format!(
                            "Transaction open · {} statement{}",
                            t.txn_statements,
                            if t.txn_statements == 1 { "" } else { "s" }
                        ),
                        p.stg,
                    )
                } else if t.manual_txn {
                    ("Manual · no open transaction".to_owned(), p.fg3)
                } else {
                    ("Auto-commit".to_owned(), p.fg3)
                };
                (env, conn, engine, meta, Some(txn))
            }
            Some(Tab::Terminal(t)) => {
                let t = t.read(cx);
                (
                    Some(t.env),
                    t.title.to_string(),
                    "terminal".into(),
                    String::new(),
                    None,
                )
            }
            Some(Tab::Files(_)) => (
                None,
                "Local files".into(),
                String::new(),
                String::new(),
                None,
            ),
            _ => (
                None,
                self.workspace_name.clone(),
                String::new(),
                String::new(),
                None,
            ),
        };
        let pos = self.active_sql().map(|t| {
            let ed = t.read(cx).editor().read(cx);
            let pos = ed.cursor_position();
            format!("Ln {}, Col {}", pos.line + 1, pos.character + 1)
        });
        let (env_label, env_bg, env_fg) = match env {
            Some(e) => (e.name().to_uppercase(), p.env(e), p.env_on(e)),
            None => ("NO CONNECTION".into(), p.hover, p.fg2),
        };
        div()
            .h(px(24.))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(14.))
            .border_t_1()
            .border_color(p.bd)
            .bg(p.panel)
            .text_size(px(11.5))
            .text_color(p.fg2)
            .whitespace_nowrap()
            .overflow_hidden()
            .child(
                div()
                    .h_full()
                    .flex()
                    .items_center()
                    .px(px(10.))
                    .bg(env_bg)
                    .text_color(env_fg)
                    .font_family(MONO)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(px(10.))
                    .child(env_label),
            )
            .child(div().text_color(p.fg).child(conn))
            .when_some(txn, |d, (label, c)| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(5.))
                        .text_color(c)
                        .child(ui::dot(c, 6.))
                        .child(label),
                )
            })
            .child(
                div()
                    .id("sb-tunnels")
                    .hover(|s| s.text_color(p.fg))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.tunnels_open = !this.tunnels_open;
                        cx.notify();
                    }))
                    .child("0 tunnels"),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("sb-transfers")
                    .hover(|s| s.text_color(p.fg))
                    .on_click(cx.listener(|this, _, w, cx| this.open_files(w, cx)))
                    .child("↑↓ no transfers"),
            )
            .child(div().font_family(MONO).child(rows))
            .when_some(pos, |d, pos| {
                d.child(div().font_family(MONO).text_color(p.fg3).child(pos))
            })
            .child(div().pr(px(12.)).text_color(p.fg3).child(engine))
            .into_any_element()
    }
}

fn greeting() -> &'static str {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Local time is not available without a time-zone database; UTC is close enough
    // for a greeting.
    match (secs / 3600) % 24 {
        5..=11 => "Good morning",
        12..=17 => "Good afternoon",
        _ => "Good evening",
    }
}

fn shortcut_hint(label: &'static str, key: SharedString, p: &Palette) -> AnyElement {
    div()
        .flex()
        .gap(px(6.))
        .child(label)
        .child(div().font_family(MONO).text_color(p.fg2).child(key))
        .into_any_element()
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        // Bind restored tabs whose connection arrived after the workspace.
        if !self.rebind.is_empty() && self.profiles_loaded {
            for (tab, id) in std::mem::take(&mut self.rebind) {
                let conn = id.as_ref().and_then(|id| self.profiles.db(id).cloned());
                tab.update(cx, |t, cx| t.set_connection(conn, cx));
            }
            self.sync_schema(cx);
        }
        let center: AnyElement = match self.tabs.get(self.active) {
            Some(Tab::Sql(t)) => t.clone().into_any_element(),
            Some(Tab::Terminal(t)) => t.clone().into_any_element(),
            Some(Tab::Files(f)) => f.clone().into_any_element(),
            _ => self.render_welcome(&p, cx),
        };
        let show_inspector =
            self.inspector_open && matches!(self.tabs.get(self.active), Some(Tab::Sql(_)));
        let sidebar = self
            .sidebar_open
            .then(|| self.render_sidebar(&p, window, cx));
        let inspector = show_inspector.then(|| self.render_inspector(&p, cx));
        let title = self.render_title_bar(&p, cx);
        let strip = self.render_tab_strip(&p, cx);
        let status = self.render_status_bar(&p, cx);
        let overlays = self.render_overlays(&p, window, cx);

        div()
            .id("workspace")
            .key_context("Workspace")
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(p.bg)
            .text_color(p.fg)
            .font_family(SANS)
            .text_size(px(13.))
            .on_action(cx.listener(|this, _: &actions::OpenPalette, w, cx| {
                this.open_palette(PaletteMode::Commands, w, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::QuickSwitch, w, cx| {
                this.open_palette(PaletteMode::Connections, w, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::RunStatement, w, cx| {
                this.run_command(CommandId::RunStatement, w, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::RunScript, w, cx| {
                this.run_command(CommandId::RunScript, w, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::StopQuery, w, cx| {
                this.run_command(CommandId::StopQuery, w, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::NewTerminal, w, cx| {
                this.run_command(CommandId::NewTerminal, w, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::NewConnection, w, cx| {
                this.run_command(CommandId::NewConnection, w, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::NewQueryTab, w, cx| {
                this.run_command(CommandId::NewQueryTab, w, cx)
            }))
            .on_action(
                cx.listener(|this, _: &actions::CloseTab, _, cx| this.close_tab(this.active, cx)),
            )
            .on_action(cx.listener(|this, _: &actions::OpenSettings, w, cx| {
                this.open_settings(SettingsPage::General, w, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::ToggleSidebar, w, cx| {
                this.run_command(CommandId::ToggleSidebar, w, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::FormatSql, w, cx| {
                this.run_command(CommandId::FormatSql, w, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::ShowHistory, w, cx| {
                this.run_command(CommandId::ShowHistory, w, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::Dismiss, w, cx| this.dismiss(w, cx)))
            .child(title)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .children(sidebar)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .bg(p.surface)
                            .child(strip)
                            .child(div().flex_1().min_h_0().flex().flex_col().child(center)),
                    )
                    .children(inspector),
            )
            .child(status)
            .children(overlays)
    }
}
