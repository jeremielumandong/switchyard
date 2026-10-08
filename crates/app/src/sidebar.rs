//! Left sidebar (connections tree, schema explorer) and the right-hand inspector.

use std::collections::{HashMap, HashSet};

use gpui_kit::component::input::Input;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, ClipboardItem, Context, FontWeight, InteractiveElement as _,
    IntoElement, MouseButton, MouseDownEvent, ParentElement as _, Pixels, Point, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, px, uniform_list,
};
use switchyard_core::db::{
    CatalogChunk, Engine, IntrospectScope, ObjectInfo, ObjectKind, SchemaInfo, Value, dialect_for,
};
use switchyard_core::store::{DbConnection, Profile, ProfileId, now_ms};
use switchyard_core::{Command, RuntimeHandle, SessionId};

use crate::app_state::{SessionState, badge_of, next_id};
use crate::conn_editor::ConnKind;
use crate::sql_tab::ViewerFormat;
use crate::theme::{MONO, Palette};
use crate::ui;
use crate::workspace::{Tab, Workspace};

/// Which sidebar list is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SideTab {
    /// Connections tree.
    Connections,
    /// Schema explorer.
    Schema,
}

/// Loading state of a lazy tree level.
#[derive(Clone, Debug, Default)]
pub enum Loadable<T> {
    /// Not requested.
    #[default]
    NotLoaded,
    /// Requested.
    Loading,
    /// Loaded.
    Loaded(T),
    /// Failed.
    Failed(String),
}

/// Schema explorer state for the active connection.
#[derive(Default)]
pub struct SchemaState {
    pub connection: Option<DbConnection>,
    pub session: Option<SessionId>,
    pub state: SessionState2,
    pub schemas: Loadable<Vec<SchemaInfo>>,
    pub objects: HashMap<(String, ObjectKind), Loadable<Vec<ObjectInfo>>>,
    pub expanded: HashSet<String>,
    pub cached_at: Option<i64>,
    pub selected: Option<(String, String, ObjectKind)>,
    pub filter: String,
    /// Server-side object search for the filter (DBX-1d).
    pub search: crate::object_search::ObjectSearch,
}

/// Session state of the schema explorer (wrapper so `Default` is `None`).
#[derive(Clone, Debug, Default)]
pub struct SessionState2(pub Option<SessionState>);

impl SchemaState {
    /// Bind to a connection (opens a dedicated catalog session).
    pub fn bind(&mut self, conn: Option<DbConnection>, core: &RuntimeHandle) {
        let same = match (&self.connection, &conn) {
            (Some(a), Some(b)) => a.id == b.id,
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        if let Some(s) = self.session.take() {
            core.send(Command::CloseSession { session: s });
        }
        *self = SchemaState::default();
        if let Some(c) = conn {
            let session = next_id();
            core.send(Command::OpenSession {
                session,
                connection: c.id.clone(),
            });
            self.session = Some(session);
            self.state = SessionState2(Some(SessionState::Connecting));
            self.expanded.insert("db".into());
            self.connection = Some(c);
        }
    }

    /// Close and reopen the catalog session (after a failure or a changed profile).
    pub fn reconnect(&mut self, core: &RuntimeHandle) {
        let conn = self.connection.take();
        if let Some(s) = self.session.take() {
            core.send(Command::CloseSession { session: s });
        }
        *self = SchemaState::default();
        self.bind(conn, core);
    }

    /// The catalog session opened.
    pub fn on_open(&mut self, version: String, core: &RuntimeHandle) {
        self.state = SessionState2(Some(SessionState::Open { version }));
        self.request(IntrospectScope::Schemas, false, core);
    }

    /// The catalog session failed.
    pub fn on_failed(&mut self, message: String) {
        self.state = SessionState2(Some(SessionState::Failed(message)));
    }

    fn request(&mut self, scope: IntrospectScope, refresh: bool, core: &RuntimeHandle) {
        let Some(session) = self.session else { return };
        match &scope {
            IntrospectScope::Schemas => self.schemas = Loadable::Loading,
            IntrospectScope::Objects { schema, kind } => {
                self.objects
                    .insert((schema.clone(), *kind), Loadable::Loading);
            }
            _ => {}
        }
        core.send(Command::Introspect {
            session,
            scope,
            refresh,
        });
    }

    /// Object folders for the connection's dialect.
    pub fn folders(&self) -> &'static [ObjectKind] {
        dialect_for(
            self.connection
                .as_ref()
                .map_or(Engine::Postgres, |c| c.engine),
        )
        .object_folders()
    }

    /// The filter text changed: returns a ticket for [`Self::search_due`] when a debounced
    /// server search should follow.
    pub fn filter_changed(&mut self, filter: String, core: &RuntimeHandle) -> Option<u64> {
        self.filter = filter;
        if !self.filter.is_empty() {
            self.load_all_folders(core);
        }
        self.search.changed(&self.filter)
    }

    /// The search debounce elapsed: send the search unless the filter changed meanwhile.
    pub fn search_due(&mut self, ticket: u64, core: &RuntimeHandle) {
        let Some(session) = self.session else { return };
        if let Some(scope) = self.search.due(ticket) {
            core.send(Command::Introspect {
                session,
                scope,
                refresh: true,
            });
        }
    }

    /// Load every object folder of every user schema (for search).
    pub fn load_all_folders(&mut self, core: &RuntimeHandle) {
        let schemas: Vec<String> = match &self.schemas {
            Loadable::Loaded(s) => s
                .iter()
                .filter(|s| !s.is_system)
                .map(|s| s.name.clone())
                .collect(),
            _ => return,
        };
        for schema in schemas {
            for kind in self.folders() {
                if matches!(
                    self.objects.get(&(schema.clone(), *kind)),
                    None | Some(Loadable::NotLoaded)
                ) {
                    self.request(
                        IntrospectScope::Objects {
                            schema: schema.clone(),
                            kind: *kind,
                        },
                        false,
                        core,
                    );
                }
            }
        }
    }

    /// Reload everything from the server.
    pub fn refresh(&mut self, core: &RuntimeHandle) {
        // A session that failed to open (wrong password, server down) is reopened.
        if matches!(self.state.0, Some(SessionState::Failed(_))) {
            return self.reconnect(core);
        }
        let open: Vec<(String, ObjectKind)> = self
            .objects
            .iter()
            .filter(|(_, v)| matches!(v, Loadable::Loaded(_)))
            .map(|(k, _)| k.clone())
            .collect();
        self.request(IntrospectScope::Schemas, true, core);
        for (schema, kind) in open {
            self.request(IntrospectScope::Objects { schema, kind }, true, core);
        }
    }

    /// A catalog chunk arrived.
    pub fn on_catalog(
        &mut self,
        scope: IntrospectScope,
        result: Result<CatalogChunk, String>,
        cached_at: i64,
    ) {
        if let IntrospectScope::Search { pattern, .. } = &scope {
            let hits = result.map(|c| match c {
                CatalogChunk::Objects(o) => o,
                _ => Vec::new(),
            });
            return self.search.on_result(pattern, hits);
        }
        self.cached_at = Some(self.cached_at.map_or(cached_at, |c| c.min(cached_at)));
        match (scope, result) {
            (IntrospectScope::Schemas, Ok(CatalogChunk::Schemas(s))) => {
                // Expand the first user schema by default.
                if self.expanded.len() <= 1
                    && let Some(first) = s.iter().find(|s| !s.is_system)
                {
                    self.expanded.insert(format!("s:{}", first.name));
                }
                self.schemas = Loadable::Loaded(s);
            }
            (IntrospectScope::Schemas, Err(e)) => self.schemas = Loadable::Failed(e),
            (IntrospectScope::Objects { schema, kind }, Ok(CatalogChunk::Objects(o))) => {
                self.objects.insert((schema, kind), Loadable::Loaded(o));
            }
            (IntrospectScope::Objects { schema, kind }, Err(e)) => {
                self.objects.insert((schema, kind), Loadable::Failed(e));
            }
            _ => {}
        }
    }

    fn toggle(&mut self, key: &str, core: &RuntimeHandle) {
        if !self.expanded.remove(key) {
            self.expanded.insert(key.to_owned());
            if let Some(rest) = key.strip_prefix("f:")
                && let Some((schema, kind)) = rest.split_once(':')
                && let Some(kind) = self.folders().iter().find(|k| format!("{k:?}") == kind)
                && !matches!(
                    self.objects.get(&(schema.to_owned(), *kind)),
                    Some(Loadable::Loaded(_) | Loadable::Loading)
                )
            {
                self.request(
                    IntrospectScope::Objects {
                        schema: schema.to_owned(),
                        kind: *kind,
                    },
                    false,
                    core,
                );
            }
        }
    }
}

/// One flattened tree row.
#[derive(Clone)]
struct TreeRow {
    depth: usize,
    caret: &'static str,
    icon: SharedString,
    label: SharedString,
    sub: SharedString,
    loading: bool,
    key: String,
    object: Option<(String, String, ObjectKind)>,
    dim: bool,
}

/// Flat object-search results: local fuzzy matches from loaded folders merged with the
/// server's hits. While the search runs, or if it failed, only the local matches show.
fn search_rows(s: &SchemaState, filter: &str, rows: &mut Vec<TreeRow>) {
    use crate::object_search::{SearchState, kind_label, merge};
    let local = s.objects.values().flat_map(|l| match l {
        Loadable::Loaded(o) => o.as_slice(),
        _ => &[],
    });
    let local = local.filter(|o| crate::actions::fuzzy(filter, &o.name));
    let status = match &s.search.state {
        SearchState::Loading => Some(("", "Searching all schemas…".to_owned(), true)),
        SearchState::Failed(e) => Some(("!", format!("Search failed: {e}"), false)),
        SearchState::Idle | SearchState::Done(_) => None,
    };
    if let Some((icon, label, loading)) = status {
        rows.push(TreeRow {
            depth: 1,
            caret: "",
            icon: icon.into(),
            label: label.into(),
            sub: "".into(),
            loading,
            key: "search-status".into(),
            object: None,
            dim: true,
        });
    }
    let hits = merge(filter, local, s.search.hits());
    if hits.is_empty() && matches!(s.search.state, SearchState::Done(_)) {
        rows.push(TreeRow {
            depth: 1,
            caret: "",
            icon: "".into(),
            label: "No matching objects".into(),
            sub: "".into(),
            loading: false,
            key: "search-empty".into(),
            object: None,
            dim: true,
        });
    }
    for o in hits {
        rows.push(TreeRow {
            depth: 1,
            caret: "",
            icon: o.kind.icon().into(),
            label: format!(
                "{}.{}{}",
                o.schema,
                o.name,
                o.detail.as_deref().unwrap_or("")
            )
            .into(),
            sub: kind_label(o.kind).into(),
            loading: false,
            key: format!("q:{}:{:?}:{}", o.schema, o.kind, o.name),
            object: Some((o.schema.clone(), o.name.clone(), o.kind)),
            dim: false,
        });
    }
}

/// A connections-tree row.
#[derive(Clone)]
struct ConnRow {
    is_group: bool,
    key: String,
    badge: &'static str,
    label: SharedString,
    sub: SharedString,
    env: Option<switchyard_core::store::EnvironmentLabel>,
    live: bool,
    profile: Option<ProfileId>,
    action: ConnAction,
    /// Can be dragged to reorder among the rows of the same group (`group`).
    drag: Option<DraggedProfile>,
}

/// A profile being dragged to a new place in the sidebar.
#[derive(Clone, Debug)]
pub struct DraggedProfile {
    id: ProfileId,
    /// Siblings it may move among: `hosts`, `h:<host id>` or `g:direct`.
    group: String,
    label: SharedString,
}

/// `order` with `dragged` moved onto `target`: after it when moving down, before it when
/// moving up (like dragging in any list).
pub(crate) fn move_to(
    order: &[ProfileId],
    dragged: &ProfileId,
    target: &ProfileId,
) -> Vec<ProfileId> {
    let mut v = order.to_vec();
    let (Some(from), Some(to)) = (
        v.iter().position(|i| i == dragged),
        v.iter().position(|i| i == target),
    ) else {
        return v;
    };
    if from == to {
        return v;
    }
    let item = v.remove(from);
    let t = v.iter().position(|i| i == target).unwrap_or(v.len());
    v.insert(if from < to { t + 1 } else { t }, item);
    v
}

#[derive(Clone)]
enum ConnAction {
    Toggle,
    Open,
    Terminal(Option<ProfileId>),
    Files,
}

/// Context-menu target.
#[derive(Clone, Debug)]
pub enum CtxTarget {
    /// A schema object.
    Object(String, String, ObjectKind),
    /// A saved profile.
    Profile(ProfileId),
    /// A tab in the tab strip (by index).
    Tab(usize),
}

/// An open context menu.
#[derive(Clone, Debug)]
pub struct CtxMenu {
    /// Position in window coordinates.
    pub at: Point<Pixels>,
    /// Target.
    pub target: CtxTarget,
}

impl Workspace {
    /// Point the schema explorer at the active SQL tab's connection.
    pub(crate) fn sync_schema(&mut self, cx: &mut Context<Self>) {
        let conn = self
            .active_sql()
            .and_then(|t| t.read(cx).connection.clone());
        if conn.is_some() || self.schema.connection.is_none() {
            self.schema.bind(conn, &self.core);
        }
    }

    /// Move `dragged` onto `target` in the sidebar and save the order.
    fn reorder_profile(&mut self, dragged: &ProfileId, target: &ProfileId, cx: &mut Context<Self>) {
        let order: Vec<ProfileId> = self.profiles.all.iter().map(|p| p.id().clone()).collect();
        let ids = move_to(&order, dragged, target);
        if ids == order {
            return;
        }
        self.profiles
            .all
            .sort_by_key(|p| ids.iter().position(|i| i == p.id()).unwrap_or(usize::MAX));
        self.core.send(Command::ReorderProfiles { ids });
        cx.notify();
    }

    fn conn_rows(&self, cx: &Context<Self>) -> Vec<ConnRow> {
        let live: HashSet<ProfileId> = self
            .tabs
            .iter()
            .filter_map(|t| match t {
                Tab::Sql(s) => {
                    let s = s.read(cx);
                    matches!(s.session_state, SessionState::Open { .. })
                        .then(|| s.connection.as_ref().map(|c| c.id.clone()))
                        .flatten()
                }
                _ => None,
            })
            .collect();
        let mut rows = Vec::new();
        let leaf = |p: &Profile, group: &str| -> ConnRow {
            let (label, sub, action) = match p {
                Profile::Db(d) => (
                    d.name.clone(),
                    if d.via_host.is_some() {
                        String::new()
                    } else {
                        format!(":{}", d.port)
                    },
                    ConnAction::Open,
                ),
                Profile::File(f) => (f.name.clone(), String::new(), ConnAction::Files),
                Profile::Terminal(t) => (
                    t.name.clone(),
                    String::new(),
                    ConnAction::Terminal(t.host_id.clone()),
                ),
                Profile::Host(h) => (
                    h.name.clone(),
                    String::new(),
                    ConnAction::Terminal(Some(h.id.clone())),
                ),
            };
            let label: SharedString = label.into();
            ConnRow {
                is_group: false,
                key: p.id().0.clone(),
                badge: badge_of(p),
                label: label.clone(),
                sub: sub.into(),
                env: None,
                live: live.contains(p.id()),
                profile: Some(p.id().clone()),
                action,
                drag: Some(DraggedProfile {
                    id: p.id().clone(),
                    group: group.to_owned(),
                    label,
                }),
            }
        };
        for h in self.profiles.hosts() {
            let key = format!("h:{}", h.id);
            let collapsed = self.collapsed.contains(&key);
            let kids = self.profiles.host_children(&h.id);
            let any_live = kids.iter().any(|k| live.contains(k.id()));
            rows.push(ConnRow {
                is_group: true,
                key: key.clone(),
                badge: "",
                label: h.name.clone().into(),
                sub: h.address.clone().into(),
                env: Some(h.environment),
                live: any_live,
                profile: Some(h.id.clone()),
                action: ConnAction::Toggle,
                drag: Some(DraggedProfile {
                    id: h.id.clone(),
                    group: "hosts".into(),
                    label: h.name.clone().into(),
                }),
            });
            if !collapsed {
                rows.push(ConnRow {
                    is_group: false,
                    key: format!("{key}:term"),
                    badge: "SSH",
                    label: "Terminal".into(),
                    sub: "".into(),
                    env: None,
                    live: false,
                    profile: Some(h.id.clone()),
                    action: ConnAction::Terminal(Some(h.id.clone())),
                    drag: None,
                });
                rows.extend(kids.into_iter().map(|k| leaf(k, &key)));
            }
        }
        let direct = self.profiles.direct();
        let key = "g:direct".to_owned();
        rows.push(ConnRow {
            is_group: true,
            key: key.clone(),
            badge: "",
            label: "Local & direct".into(),
            sub: "".into(),
            env: Some(switchyard_core::store::EnvironmentLabel::Local),
            live: direct.iter().any(|k| live.contains(k.id())),
            profile: None,
            action: ConnAction::Toggle,
            drag: None,
        });
        if !self.collapsed.contains(&key) {
            rows.push(ConnRow {
                is_group: false,
                key: "local-shell".into(),
                badge: "SH",
                label: "Local shell".into(),
                sub: "".into(),
                env: None,
                live: false,
                profile: None,
                action: ConnAction::Terminal(None),
                drag: None,
            });
            rows.extend(direct.into_iter().map(|p| {
                let mut r = leaf(p, "g:direct");
                if let Profile::Db(d) = p {
                    r.env = Some(d.environment);
                }
                r
            }));
            rows.push(ConnRow {
                is_group: false,
                key: "local-files".into(),
                badge: "FS",
                label: "Local files".into(),
                sub: "".into(),
                env: None,
                live: false,
                profile: None,
                action: ConnAction::Files,
                drag: None,
            });
        }
        rows
    }

    fn schema_rows(&self) -> Vec<TreeRow> {
        let s = &self.schema;
        let mut rows = Vec::new();
        let Some(conn) = &s.connection else {
            return rows;
        };
        let version = match &s.state.0 {
            Some(SessionState::Open { version }) => version
                .replace("PostgreSQL ", "pg ")
                .split('.')
                .next()
                .unwrap_or("")
                .to_owned(),
            Some(SessionState::Connecting) => "connecting".into(),
            Some(SessionState::Failed(_)) => "failed".into(),
            _ => String::new(),
        };
        let db_open = s.expanded.contains("db");
        rows.push(TreeRow {
            depth: 0,
            caret: if db_open { "▾" } else { "▸" },
            icon: "DB".into(),
            label: if conn.database.is_empty() {
                conn.name.clone().into()
            } else {
                conn.database.clone().into()
            },
            sub: version.into(),
            loading: matches!(s.state.0, Some(SessionState::Connecting)),
            key: "db".into(),
            object: None,
            dim: false,
        });
        if !db_open {
            return rows;
        }
        let filter = s.filter.to_lowercase();
        if crate::object_search::ObjectSearch::applies(&filter) {
            search_rows(s, &filter, &mut rows);
            return rows;
        }
        match &s.schemas {
            Loadable::Loaded(schemas) => {
                for sc in schemas {
                    let key = format!("s:{}", sc.name);
                    let open = s.expanded.contains(&key) || !filter.is_empty();
                    rows.push(TreeRow {
                        depth: 1,
                        caret: if open { "▾" } else { "▸" },
                        icon: "S".into(),
                        label: sc.name.clone().into(),
                        sub: "".into(),
                        loading: false,
                        key: key.clone(),
                        object: None,
                        dim: sc.is_system,
                    });
                    if !open {
                        continue;
                    }
                    for kind in s.folders() {
                        let fkey = format!("f:{}:{kind:?}", sc.name);
                        let fopen = s.expanded.contains(&fkey);
                        let state = s.objects.get(&(sc.name.clone(), *kind));
                        let count = match state {
                            Some(Loadable::Loaded(o)) => o.len().to_string(),
                            _ => String::new(),
                        };
                        let matching: Vec<&ObjectInfo> = match state {
                            Some(Loadable::Loaded(o)) => o
                                .iter()
                                .filter(|o| {
                                    filter.is_empty() || crate::actions::fuzzy(&filter, &o.name)
                                })
                                .collect(),
                            _ => Vec::new(),
                        };
                        if !filter.is_empty() && matching.is_empty() {
                            continue;
                        }
                        let show = fopen || (!filter.is_empty() && !matching.is_empty());
                        rows.push(TreeRow {
                            depth: 2,
                            caret: if show { "▾" } else { "▸" },
                            icon: "".into(),
                            label: kind.folder_label().into(),
                            sub: count.into(),
                            loading: matches!(state, Some(Loadable::Loading)),
                            key: fkey,
                            object: None,
                            dim: true,
                        });
                        if !show {
                            continue;
                        }
                        if let Some(Loadable::Failed(e)) = state {
                            rows.push(TreeRow {
                                depth: 3,
                                caret: "",
                                icon: "!".into(),
                                label: e.clone().into(),
                                sub: "".into(),
                                loading: false,
                                key: format!("err:{}", sc.name),
                                object: None,
                                dim: true,
                            });
                        }
                        for o in matching {
                            rows.push(TreeRow {
                                depth: 3,
                                caret: "",
                                icon: kind.icon().into(),
                                label: format!(
                                    "{}{}",
                                    o.name,
                                    o.detail.clone().unwrap_or_default()
                                )
                                .into(),
                                sub: o.estimated_rows.map(compact).unwrap_or_default().into(),
                                loading: false,
                                key: format!("o:{}:{kind:?}:{}", sc.name, o.name),
                                object: Some((sc.name.clone(), o.name.clone(), *kind)),
                                dim: false,
                            });
                        }
                    }
                }
            }
            Loadable::Loading | Loadable::NotLoaded => rows.push(TreeRow {
                depth: 1,
                caret: "▾",
                icon: "".into(),
                label: "Schemas".into(),
                sub: "".into(),
                loading: true,
                key: "loading".into(),
                object: None,
                dim: true,
            }),
            Loadable::Failed(e) => rows.push(TreeRow {
                depth: 1,
                caret: "",
                icon: "!".into(),
                label: e.clone().into(),
                sub: "".into(),
                loading: false,
                key: "failed".into(),
                object: None,
                dim: true,
            }),
        }
        rows
    }

    /// Run a context-menu action on a schema object.
    pub(crate) fn object_action(
        &mut self,
        action: &str,
        schema: String,
        name: String,
        kind: ObjectKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(conn) = self.schema.connection.clone() else {
            return;
        };
        let d = dialect_for(conn.engine);
        let q = d.qualified(&schema, &name);
        let columns: Vec<String> = Vec::new();
        let cols = if columns.is_empty() {
            "*".to_owned()
        } else {
            columns.join(", ")
        };
        let text = match action {
            "open" => d.select_rows(&q, 100),
            "select" => format!("SELECT {cols}\nFROM {q}\nLIMIT 100;"),
            "insert" => format!(
                "INSERT INTO {q} (column1, column2)\nVALUES ({}, {});",
                d.literal(&Value::Null),
                d.literal(&Value::Null)
            ),
            "update" => format!(
                "UPDATE {q}\nSET column1 = {}\nWHERE id = {};",
                d.literal(&Value::Null),
                d.literal(&Value::Int(0))
            ),
            "copy" => {
                cx.write_to_clipboard(ClipboardItem::new_string(q.clone()));
                self.toast(format!("Copied {q}"), cx);
                return;
            }
            "ddl" => format!("-- DDL for {q} is loading…"),
            "truncate" => format!("TRUNCATE {q};"),
            "drop" => format!(
                "DROP {} {q};",
                match kind {
                    ObjectKind::View => "VIEW",
                    ObjectKind::MaterializedView => "MATERIALIZED VIEW",
                    ObjectKind::Function => "FUNCTION",
                    ObjectKind::Procedure => "PROCEDURE",
                    ObjectKind::Sequence => "SEQUENCE",
                    ObjectKind::Type => "TYPE",
                    _ => "TABLE",
                }
            ),
            _ => return,
        };
        // Templates (INSERT/UPDATE) go into the current tab when it is on this database;
        // everything else opens its own tab, so the current query is never replaced.
        let template = matches!(action, "insert" | "update");
        let same_db = self.active_sql().is_some_and(|t| {
            t.read(cx)
                .connection
                .as_ref()
                .is_some_and(|c| c.id == conn.id)
        });
        let tab = if template && same_db {
            self.active_sql()
        } else {
            Some(self.open_query_tab(&conn, &name, window, cx))
        };
        if let Some(tab) = tab {
            tab.update(cx, |t, cx| match action {
                // Runs once the new tab's session has opened.
                "open" | "truncate" | "drop" => t.set_text_and_run(&text, window, cx),
                _ => t.insert_text(&text, window, cx),
            });
        }
        if action == "ddl" {
            self.toast(
                "View DDL: generated from the catalog in the object detail",
                cx,
            );
        }
        cx.notify();
    }

    pub(crate) fn render_sidebar(
        &mut self,
        p: &Palette,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let side = self.side_tab;
        // With an SSH terminal in front, the second tab browses that Host's files.
        let ssh = self.active_ssh_host(cx);
        let list: AnyElement = match (side, &ssh) {
            (SideTab::Connections, _) => self.render_conn_list(p, cx),
            (SideTab::Schema, Some(host)) => {
                let name = self
                    .profiles
                    .host(host)
                    .map(|h| h.name.clone())
                    .unwrap_or_default();
                let panel = self.remote_files_for(host, cx);
                panel.update(cx, |f, cx| f.render_panel(&name, cx))
            }
            (SideTab::Schema, None) => self.render_schema(p, cx),
        };
        let second = if ssh.is_some() { "Files" } else { "Schema" };
        div()
            .w(px(self.sidebar_width))
            .relative()
            .child(
                // Drag the right edge to resize.
                div()
                    .id("side-resize")
                    .absolute()
                    .right(px(-3.))
                    .top_0()
                    .bottom_0()
                    .w(px(6.))
                    .cursor_col_resize()
                    .hover(|s| s.bg(p.acc.opacity(0.35)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                            this.sidebar_drag = Some((ev.position.x.into(), this.sidebar_width));
                            cx.stop_propagation();
                        }),
                    ),
            )
            .flex_none()
            .flex()
            .flex_col()
            .bg(p.panel)
            .border_r_1()
            .border_color(p.bd)
            .min_h_0()
            .child(
                div()
                    .px(px(8.))
                    .pt(px(8.))
                    .pb(px(6.))
                    .flex_none()
                    .child(ui::segmented(
                        "side-tabs",
                        vec![
                            (
                                "Connections".into(),
                                side == SideTab::Connections,
                                Box::new(cx.listener(|this, _, _, cx| {
                                    this.side_tab = SideTab::Connections;
                                    cx.notify();
                                })),
                            ),
                            (
                                second.into(),
                                side == SideTab::Schema,
                                Box::new(cx.listener(|this, _, _, cx| {
                                    this.side_tab = SideTab::Schema;
                                    cx.notify();
                                })),
                            ),
                        ],
                        22.,
                        p,
                    )),
            )
            .child(list)
            .child(
                div()
                    .flex_none()
                    .p(px(8.))
                    .border_t_1()
                    .border_color(p.bd)
                    .flex()
                    .gap(px(6.))
                    .child(
                        ui::button("new-conn", "New connection", ui::Kind::Secondary, p)
                            .flex_1()
                            .on_click(cx.listener(|this, _, w, cx| {
                                this.open_conn_editor(ConnKind::Postgres, None, w, cx)
                            })),
                    )
                    .child(
                        ui::button("new-host", "New host", ui::Kind::Secondary, p).on_click(
                            cx.listener(|this, _, w, cx| {
                                this.open_conn_editor(ConnKind::Ssh, None, w, cx)
                            }),
                        ),
                    ),
            )
            .into_any_element()
    }

    fn render_conn_list(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.conn_rows(cx);
        let count = rows.len();
        let p = *p;
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(12.))
                    .pt(px(6.))
                    .pb(px(4.))
                    .child(ui::caption("HOSTS", &p))
                    .child(ui::caption("by host", &p).font_family(MONO)),
            )
            .child(
                uniform_list(
                    "conn-rows",
                    count,
                    cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                        range
                            .map(|i| this.render_conn_row(&rows[i], i, &p, cx))
                            .collect::<Vec<_>>()
                    }),
                )
                .flex_1()
                .pb(px(8.)),
            )
            .into_any_element()
    }

    fn render_conn_row(
        &self,
        r: &ConnRow,
        i: usize,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = !r.is_group
            && match (&r.profile, self.active_sql()) {
                (Some(id), Some(t)) => t.read(cx).connection.as_ref().is_some_and(|c| &c.id == id),
                _ => false,
            };
        let key = r.key.clone();
        let action = r.action.clone();
        let profile = r.profile.clone();
        let collapsed = self.collapsed.contains(&r.key);
        let ctx_profile = r.profile.clone();
        let drop_line = p.acc;
        let drop_target = r.drag.clone();
        let over_target = r.drag.clone();
        div()
            .id(("conn-row", i))
            .when_some(r.drag.clone(), |d, drag| {
                d.on_drag(drag, |d: &DraggedProfile, _, _, cx| {
                    let label = d.label.to_string();
                    cx.new(|_| crate::files_tab::DragPreview(label))
                })
            })
            .drag_over::<DraggedProfile>(move |s, d, _, _| match &over_target {
                Some(t) if t.group == d.group && t.id != d.id => {
                    s.bg(drop_line.opacity(0.18)).border_color(drop_line)
                }
                _ => s,
            })
            .on_drop(cx.listener(move |this, d: &DraggedProfile, _, cx| {
                if let Some(t) = &drop_target
                    && t.group == d.group
                {
                    this.reorder_profile(&d.id, &t.id, cx);
                }
            }))
            .w_full()
            .h(px(26.))
            .flex()
            .items_center()
            .gap(px(7.))
            .pl(px(if r.is_group { 8. } else { 26. }))
            .pr(px(10.))
            .text_size(px(12.5))
            .when(active, |d| d.bg(p.sel))
            .hover(|s| s.bg(p.hover))
            .on_click(cx.listener(move |this, _, w, cx| {
                match &action {
                    ConnAction::Toggle => {
                        if !this.collapsed.remove(&key) {
                            this.collapsed.insert(key.clone());
                        }
                    }
                    ConnAction::Open => {
                        if let Some(id) = &profile {
                            this.open_connection(id, w, cx);
                        }
                    }
                    ConnAction::Terminal(h) => this.open_terminal(h.clone(), cx),
                    ConnAction::Files => this.open_files(w, cx),
                }
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                    if let Some(id) = &ctx_profile {
                        this.ctx = Some(CtxMenu {
                            at: ev.position,
                            target: CtxTarget::Profile(id.clone()),
                        });
                        cx.notify();
                    }
                }),
            )
            .child(
                div()
                    .w(px(10.))
                    .flex_none()
                    .text_color(p.fg3)
                    .text_size(px(9.))
                    .child(if r.is_group {
                        if collapsed { "▸" } else { "▾" }
                    } else {
                        ""
                    }),
            )
            .when(r.is_group, |d| {
                d.child(ui::dot(r.env.map_or(p.loc, |e| p.env(e)), 7.))
            })
            .when(!r.is_group, |d| {
                d.child(ui::monogram(r.badge, 28., p))
                    .when_some(r.env, |d, e| d.child(ui::dot(p.env(e), 5.)))
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .when(r.is_group, |d| d.font_weight(FontWeight::SEMIBOLD))
                    .child(r.label.clone()),
            )
            .child(
                div()
                    .font_family(MONO)
                    .text_size(px(11.))
                    .text_color(p.fg3)
                    .whitespace_nowrap()
                    .child(r.sub.clone()),
            )
            .child(ui::dot(
                if r.live {
                    p.dev
                } else {
                    gpui_kit::transparent_black()
                },
                6.,
            ))
            .into_any_element()
    }

    fn render_schema(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.schema_rows();
        let count = rows.len();
        let p = *p;
        let has_conn = self.schema.connection.is_some();
        let cached = self.schema.cached_at.map(|at| {
            let mins = (now_ms() - at).max(0) / 60_000;
            if mins == 0 {
                "Cached just now".to_owned()
            } else {
                format!("Cached {mins} min ago")
            }
        });
        if !has_conn {
            return div()
                .flex_1()
                .p(px(16.))
                .text_size(px(12.5))
                .text_color(p.fg3)
                .child("Open a SQL tab with a connection to browse its schema.")
                .into_any_element();
        }
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_none()
                    .px(px(8.))
                    .pt(px(2.))
                    .pb(px(6.))
                    .child(
                        div()
                            .id("schema-search")
                            .h(px(26.))
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .px(px(8.))
                            .border_1()
                            .border_color(p.bd)
                            .rounded(px(6.))
                            .bg(p.bg)
                            .text_color(p.fg3)
                            .text_size(px(12.))
                            .child(
                                div().flex_1().child(
                                    Input::new(&self.schema_search)
                                        .appearance(false)
                                        .text_size(px(12.)),
                                ),
                            )
                            .child(
                                div()
                                    .font_family(MONO)
                                    .text_size(px(10.5))
                                    .child(ui::keys("⌘P", "Ctrl+P")),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .px(px(2.))
                            .pt(px(6.))
                            .text_size(px(11.))
                            .text_color(p.fg3)
                            .child(cached.unwrap_or_else(|| "Not cached yet".into()))
                            .child(
                                div()
                                    .id("schema-refresh")
                                    .text_color(p.acc)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.schema.refresh(&this.core);
                                        this.schema.filter.clear();
                                        cx.notify();
                                    }))
                                    .child("Refresh"),
                            ),
                    ),
            )
            .child(
                uniform_list(
                    "schema-rows",
                    count,
                    cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                        range
                            .map(|i| this.render_schema_row(&rows[i], i, &p, cx))
                            .collect::<Vec<_>>()
                    }),
                )
                .flex_1()
                .pb(px(8.)),
            )
            .into_any_element()
    }

    fn render_schema_row(
        &self,
        r: &TreeRow,
        i: usize,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = r.key.clone();
        let object = r.object.clone();
        let object2 = r.object.clone();
        let selected = r.object.is_some() && self.schema.selected == r.object;
        div()
            .id(("schema-row", i))
            .w_full()
            .h(px(26.))
            .flex()
            .items_center()
            .gap(px(7.))
            .pl(px(8. + r.depth as f32 * 14.))
            .pr(px(10.))
            .text_size(px(12.5))
            .text_color(if r.dim { p.fg2 } else { p.fg })
            .when(selected, |d| d.bg(p.sel))
            .hover(|s| s.bg(p.hover))
            .on_click(cx.listener(move |this, ev: &gpui_kit::ClickEvent, w, cx| {
                if let Some((s, n, k)) = &object {
                    this.schema.selected = Some((s.clone(), n.clone(), *k));
                    if ev.click_count() >= 2 && k.is_relation() {
                        this.object_action("open", s.clone(), n.clone(), *k, w, cx);
                    }
                } else {
                    let core = this.core.clone();
                    this.schema.toggle(&key, &core);
                }
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                    if let Some((s, n, k)) = &object2 {
                        this.schema.selected = Some((s.clone(), n.clone(), *k));
                        this.ctx = Some(CtxMenu {
                            at: ev.position,
                            target: CtxTarget::Object(s.clone(), n.clone(), *k),
                        });
                        cx.notify();
                    }
                }),
            )
            .child(
                div()
                    .w(px(10.))
                    .flex_none()
                    .text_color(p.fg3)
                    .text_size(px(9.))
                    .child(r.caret),
            )
            .child(
                div()
                    .w(px(14.))
                    .flex_none()
                    .font_family(MONO)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(px(10.))
                    .text_color(p.fg3)
                    .child(r.icon.clone()),
            )
            .child(div().flex_1().min_w_0().truncate().child(r.label.clone()))
            .when(r.loading, |d| d.child(ui::shimmer(64., p)))
            .when(!r.loading, |d| {
                d.child(
                    div()
                        .font_family(MONO)
                        .text_size(px(11.))
                        .text_color(p.fg3)
                        .child(r.sub.clone()),
                )
            })
            .into_any_element()
    }
}

/// Default and smallest width of the left sidebar.
pub(crate) const SIDEBAR_WIDTH: f32 = 264.;
pub(crate) const SIDEBAR_MIN: f32 = 200.;

/// Default and smallest width of the right panel.
pub(crate) const INSPECTOR_WIDTH: f32 = 300.;
pub(crate) const INSPECTOR_MIN: f32 = 240.;

impl Workspace {
    /// The right-hand value viewer.
    ///
    /// Its left edge drags to resize it; the header button toggles a wide view.
    pub(crate) fn render_inspector(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let Some(tab) = self.active_sql() else {
            return div().into_any_element();
        };
        let (fmt, row, cell) = {
            let t = tab.read(cx);
            (
                t.viewer_format,
                t.selected_row(cx),
                t.selected_cell_value(cx),
            )
        };
        let per_cell = matches!(
            fmt,
            ViewerFormat::Xml | ViewerFormat::Hex | ViewerFormat::Image
        );
        let mut lines: Vec<Vec<(String, gpui_kit::Hsla)>> = Vec::new();
        let mut size_label = String::new();
        let mut image: Option<AnyElement> = None;
        if per_cell {
            self.render_cell_view(
                fmt,
                cell.as_ref(),
                tab.entity_id().as_u64(),
                &mut lines,
                &mut size_label,
                &mut image,
                p,
            );
        }
        match &row {
            _ if per_cell => {}
            None => lines.push(vec![("Select a row in the results".into(), p.fg3)]),
            Some((_, cols)) => {
                let obj: serde_json::Map<String, serde_json::Value> = cols
                    .iter()
                    .map(|(n, v, _)| {
                        let j = match v {
                            Value::Null => serde_json::Value::Null,
                            Value::Bool(b) => (*b).into(),
                            Value::Int(i) => (*i).into(),
                            Value::Float(f) => serde_json::Number::from_f64(*f)
                                .map_or(serde_json::Value::Null, Into::into),
                            Value::Json(s) => {
                                serde_json::from_str(s).unwrap_or_else(|_| s.clone().into())
                            }
                            other => other.to_display().into(),
                        };
                        (n.clone(), j)
                    })
                    .collect();
                let json = serde_json::to_string_pretty(&serde_json::Value::Object(obj))
                    .unwrap_or_default();
                size_label = format!("{} bytes · UTF-8", json.len());
                match fmt {
                    ViewerFormat::Json => {
                        for l in json.lines() {
                            lines.push(highlight_json_line(l, p));
                        }
                    }
                    ViewerFormat::Text => {
                        for (n, v, _) in cols {
                            lines.push(vec![
                                (format!("{n:<14} "), p.fg3),
                                (v.to_display(), if v.is_null() { p.fg3 } else { p.fg }),
                            ]);
                        }
                    }
                    ViewerFormat::Xml | ViewerFormat::Hex | ViewerFormat::Image => {}
                }
            }
        }
        let row_no = row.as_ref().map(|(r, _)| r + 1);
        let row_no = if per_cell {
            cell.as_ref().map(|(r, _, _)| r + 1)
        } else {
            row_no
        };
        let copy_text: String = lines
            .iter()
            .map(|l| l.iter().map(|(t, _)| t.as_str()).collect::<String>() + "\n")
            .collect();
        let set_fmt = |f: ViewerFormat, tab: gpui_kit::Entity<crate::sql_tab::SqlTab>| {
            move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut gpui_kit::App| {
                tab.update(cx, |t, cx| {
                    t.viewer_format = f;
                    cx.notify();
                });
            }
        };
        div()
            .w(px(self.inspector_width))
            .relative()
            .child(
                // Drag the left edge to resize.
                div()
                    .id("insp-resize")
                    .absolute()
                    .left(px(-3.))
                    .top_0()
                    .bottom_0()
                    .w(px(6.))
                    .cursor_col_resize()
                    .hover(|s| s.bg(p.acc.opacity(0.35)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                            this.inspector_drag =
                                Some((ev.position.x.into(), this.inspector_width));
                            cx.stop_propagation();
                        }),
                    ),
            )
            .flex_none()
            .flex()
            .flex_col()
            .bg(p.panel)
            .border_l_1()
            .border_color(p.bd)
            .min_h_0()
            .child(
                div()
                    .h(px(34.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .pl(px(12.))
                    .pr(px(10.))
                    .border_b_1()
                    .border_color(p.bd)
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Value viewer"),
                    )
                    .when_some(row_no, |d, r| {
                        d.child(
                            div()
                                .font_family(MONO)
                                .text_size(px(11.))
                                .text_color(p.fg3)
                                .child(format!("row {r}")),
                        )
                    })
                    .child(div().flex_1())
                    .child({
                        let wide = self.inspector_width > INSPECTOR_WIDTH + 1.;
                        div()
                            .id("insp-expand")
                            .px(px(6.))
                            .py(px(2.))
                            .rounded(px(4.))
                            .text_color(p.fg3)
                            .hover(|s| s.bg(p.hover).text_color(p.fg))
                            .tooltip(move |w, cx| {
                                gpui_kit::component::tooltip::Tooltip::new(if wide {
                                    "Restore width"
                                } else {
                                    "Expand"
                                })
                                .build(w, cx)
                            })
                            .on_click(cx.listener(move |this, _, w, cx| {
                                this.inspector_width = if wide {
                                    INSPECTOR_WIDTH
                                } else {
                                    // About half the window, leaving room for the editor.
                                    (f32::from(w.bounds().size.width) * 0.5).max(INSPECTOR_WIDTH)
                                };
                                this.save_inspector_width();
                                cx.notify();
                            }))
                            .child(if wide { "⇥" } else { "⇤" })
                    })
                    .child(
                        div()
                            .id("insp-close")
                            .px(px(6.))
                            .py(px(2.))
                            .rounded(px(4.))
                            .text_color(p.fg3)
                            .hover(|s| s.bg(p.hover))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.inspector_open = false;
                                cx.notify();
                            }))
                            .child("×"),
                    ),
            )
            .child(
                div()
                    .px(px(10.))
                    .py(px(8.))
                    .flex_none()
                    .child(ui::segmented(
                        "viewer-fmt",
                        vec![
                            (
                                "JSON".into(),
                                fmt == ViewerFormat::Json,
                                Box::new(set_fmt(ViewerFormat::Json, tab.clone())),
                            ),
                            (
                                "Text".into(),
                                fmt == ViewerFormat::Text,
                                Box::new(set_fmt(ViewerFormat::Text, tab.clone())),
                            ),
                            (
                                "XML".into(),
                                fmt == ViewerFormat::Xml,
                                Box::new(set_fmt(ViewerFormat::Xml, tab.clone())),
                            ),
                            (
                                "Hex".into(),
                                fmt == ViewerFormat::Hex,
                                Box::new(set_fmt(ViewerFormat::Hex, tab.clone())),
                            ),
                            (
                                "Image".into(),
                                fmt == ViewerFormat::Image,
                                Box::new(set_fmt(ViewerFormat::Image, tab.clone())),
                            ),
                        ],
                        20.,
                        p,
                    )),
            )
            .child(
                div()
                    .id("insp-body")
                    .flex_1()
                    .min_h_0()
                    // Long XML or text lines scroll sideways instead of being cut.
                    .overflow_scroll()
                    .px(px(12.))
                    .pt(px(4.))
                    .pb(px(12.))
                    .font_family(MONO)
                    .text_size(px(12.))
                    .line_height(px(19.))
                    .children(image)
                    .children(lines.into_iter().map(|segs| {
                        div().flex().whitespace_nowrap().children(
                            segs.into_iter()
                                .map(|(t, c)| div().text_color(c).child(SharedString::from(t))),
                        )
                    })),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .justify_between()
                    .px(px(12.))
                    .py(px(8.))
                    .border_t_1()
                    .border_color(p.bd)
                    .text_size(px(11.))
                    .text_color(p.fg3)
                    .child(size_label)
                    .child(
                        div()
                            .id("insp-copy")
                            .text_color(p.acc)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
                                this.toast("Copied value", cx);
                            }))
                            .child("Copy"),
                    ),
            )
            .into_any_element()
    }
}

impl Workspace {
    /// The XML, hex or image view of the selected cell.
    #[allow(clippy::too_many_arguments)]
    fn render_cell_view(
        &mut self,
        fmt: ViewerFormat,
        cell: Option<&(usize, String, switchyard_core::db::Value)>,
        tab_id: u64,
        lines: &mut Vec<Vec<(String, gpui_kit::Hsla)>>,
        size_label: &mut String,
        image: &mut Option<AnyElement>,
        p: &Palette,
    ) {
        use crate::viewer::{self, Tok};
        /// Lines shown before "showing the first …".
        const MAX_LINES: usize = 4000;
        let Some((row, name, value)) = cell else {
            lines.push(vec![("Select a cell in the results".into(), p.fg3)]);
            return;
        };
        if value.is_null() {
            lines.push(vec![(format!("{name} is NULL"), p.fg3)]);
            return;
        }
        let bytes = viewer::value_bytes(value);
        *size_label = format!("{name} · {}", human_bytes(bytes.len()));
        match fmt {
            ViewerFormat::Xml => {
                let text = String::from_utf8_lossy(&bytes);
                match viewer::xml_lines(&text, MAX_LINES) {
                    Ok((xml, cut)) => {
                        lines.extend(xml.into_iter().map(|l| {
                            l.into_iter()
                                .map(|(s, t)| {
                                    let c = match t {
                                        Tok::Punct => p.fg3,
                                        Tok::Tag => p.sx_kw,
                                        Tok::Attr => p.sx_fn,
                                        Tok::Value => p.sx_str,
                                        Tok::Text => p.fg,
                                        Tok::Meta => p.sx_cm,
                                    };
                                    (s, c)
                                })
                                .collect()
                        }));
                        if cut {
                            lines.push(vec![(format!("… first {MAX_LINES} lines"), p.fg3)]);
                        }
                    }
                    Err(e) => {
                        lines.push(vec![(format!("Not XML ({e}); shown as text:"), p.stg)]);
                        lines.extend(
                            text.lines()
                                .take(MAX_LINES)
                                .map(|l| vec![(l.to_owned(), p.fg)]),
                        );
                    }
                }
            }
            ViewerFormat::Hex => {
                for (off, hex, ascii) in viewer::hex_rows(&bytes, MAX_LINES) {
                    lines.push(vec![
                        (format!("{off}  "), p.fg3),
                        (format!("{hex:<24}  "), p.fg),
                        (ascii, p.fg2),
                    ]);
                }
                if bytes.len() > MAX_LINES * 8 {
                    lines.push(vec![(
                        format!(
                            "… first {} of {}",
                            human_bytes(MAX_LINES * 8),
                            human_bytes(bytes.len())
                        ),
                        p.fg3,
                    )]);
                }
            }
            _ => match viewer::image_format(&bytes) {
                None => lines.push(vec![(
                    "Not an image (PNG, JPEG, GIF, WebP, BMP, TIFF or SVG)".into(),
                    p.fg3,
                )]),
                Some(f) => {
                    *size_label = format!(
                        "{name} · {} · {}",
                        viewer::format_name(f),
                        human_bytes(bytes.len())
                    );
                    // Decode once per selected cell, not every frame.
                    let key = {
                        use std::hash::{Hash, Hasher};
                        let mut h = std::collections::hash_map::DefaultHasher::new();
                        (tab_id, row, name, bytes.len()).hash(&mut h);
                        h.finish()
                    };
                    let img = match &self.viewer_image {
                        Some((k, img)) if *k == key => img.clone(),
                        _ => {
                            let img = std::sync::Arc::new(gpui_kit::Image::from_bytes(f, bytes));
                            self.viewer_image = Some((key, img.clone()));
                            img
                        }
                    };
                    *image = Some(
                        div()
                            .pt(px(6.))
                            .flex()
                            .justify_center()
                            .child(
                                gpui_kit::img(img)
                                    .max_w(px(self.inspector_width - 24.))
                                    .max_h(px(420.)),
                            )
                            .into_any_element(),
                    );
                }
            },
        }
    }
}

/// `1.2 KB`, `3.4 MB`.
fn human_bytes(n: usize) -> String {
    match n {
        0..1024 => format!("{n} bytes"),
        1024..1_048_576 => format!("{:.1} KB", n as f64 / 1024.0),
        _ => format!("{:.1} MB", n as f64 / 1_048_576.0),
    }
}

fn highlight_json_line(line: &str, p: &Palette) -> Vec<(String, gpui_kit::Hsla)> {
    let mut out = Vec::new();
    let b = line.as_bytes();
    let mut i = 0;
    let mut plain = String::new();
    let flush = |plain: &mut String, out: &mut Vec<(String, gpui_kit::Hsla)>| {
        if !plain.is_empty() {
            out.push((std::mem::take(plain), p.fg2));
        }
    };
    while i < b.len() {
        let c = b[i];
        if c == b'"' {
            flush(&mut plain, &mut out);
            let mut j = i + 1;
            while j < b.len() && b[j] != b'"' {
                if b[j] == b'\\' {
                    j += 1;
                }
                j += 1;
            }
            let end = (j + 1).min(b.len());
            let s = &line[i..end];
            let is_key = line[end..].trim_start().starts_with(':');
            out.push((s.to_owned(), if is_key { p.sx_fn } else { p.sx_str }));
            i = end;
        } else if c.is_ascii_digit() || (c == b'-' && i + 1 < b.len() && b[i + 1].is_ascii_digit())
        {
            flush(&mut plain, &mut out);
            let mut j = i + 1;
            while j < b.len()
                && (b[j].is_ascii_digit() || b[j] == b'.' || b[j] == b'e' || b[j] == b'-')
            {
                j += 1;
            }
            out.push((line[i..j].to_owned(), p.sx_num));
            i = j;
        } else if line[i..].starts_with("null") {
            flush(&mut plain, &mut out);
            out.push(("null".into(), p.fg3));
            i += 4;
        } else {
            let ch = line[i..].chars().next().unwrap_or(' ');
            plain.push(ch);
            i += ch.len_utf8();
        }
    }
    flush(&mut plain, &mut out);
    out
}

fn compact(n: i64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1_000_000.0),
        n if n >= 1_000 => format!("{:.1}K", n as f64 / 1_000.0).replace(".0K", "K"),
        n => n.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dragging_moves_after_going_down_and_before_going_up() {
        let ids: Vec<ProfileId> = ["a", "b", "c", "d"].map(|s| ProfileId(s.into())).to_vec();
        let id = |s: &str| ProfileId(s.into());
        let names = |v: Vec<ProfileId>| v.into_iter().map(|i| i.0).collect::<Vec<_>>().join("");
        assert_eq!(names(move_to(&ids, &id("a"), &id("c"))), "bcad");
        assert_eq!(names(move_to(&ids, &id("d"), &id("b"))), "adbc");
        assert_eq!(
            names(move_to(&ids, &id("a"), &id("d"))),
            "bcda",
            "to the end"
        );
        assert_eq!(
            names(move_to(&ids, &id("c"), &id("a"))),
            "cabd",
            "to the start"
        );
        assert_eq!(names(move_to(&ids, &id("b"), &id("b"))), "abcd");
        assert_eq!(
            names(move_to(&ids, &id("x"), &id("b"))),
            "abcd",
            "unknown id"
        );
    }
}
