//! Left sidebar (connections tree, schema explorer) and the right-hand inspector.

use std::collections::{HashMap, HashSet};

use gpui_kit::component::input::Input;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, ClipboardItem, Context, FocusHandle, FontWeight,
    InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent, ParentElement as _, Pixels,
    Point, ScrollStrategy, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    px, uniform_list,
};
use switchyard_core::db::{
    CatalogChunk, Engine, IntrospectScope, ObjectDetail, ObjectInfo, ObjectKind, SchemaInfo, Value,
    dialect_for,
};
use switchyard_core::store::{DbConnection, Favorite, Host, ProfileId, now_ms};
use switchyard_core::{Command, SessionId};

use crate::actions::{
    TreeCollapse, TreeCopy, TreeDown, TreeExpand, TreeOpen, TreePin, TreeRefresh, TreeUp,
};
use crate::app_state::{SessionState, next_id};
use crate::appearance::{rpx, ts};
use crate::ddl_tab::DdlTab;
pub(crate) use crate::explorer::CoreSink;
use crate::explorer::{ObjRef, RowId};
use crate::explorer_tree::{ConnAction, ConnRow, ExplorerGroup, RowKind, explorer_rows};
use crate::sql_tab::ViewerFormat;
use crate::theme::{MONO, Palette};
use crate::ui;
use crate::workspace::{Tab, Workspace};

/// Which sidebar list is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SideTab {
    /// Every saved source, by place or by type (design v3).
    Explorer,
    /// Schema explorer of the database in front (the Host's files for a terminal).
    Schema,
    /// Tools for servers, databases and cloud accounts.
    Tools,
    /// Sessions, running queries, tunnels and transfers.
    Activity,
}

/// Setting key of the Explorer's grouping (`place` or `type`).
pub(crate) const EXPLORER_GROUP_KEY: &str = "explorer.group";

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

/// Schema explorer state of one connection node (DBX-5e: one per connection, kept in
/// [`crate::explorer::Explorer`]).
#[derive(Default)]
pub struct SchemaState {
    pub connection: Option<DbConnection>,
    pub session: Option<SessionId>,
    pub state: SessionState2,
    pub schemas: Loadable<Vec<SchemaInfo>>,
    pub objects: HashMap<(String, ObjectKind), Loadable<Vec<ObjectInfo>>>,
    /// Folders the server answered with a hint instead of objects (a missing privilege,
    /// such as msdb access for Agent jobs; DBX-5c).
    pub hints: HashMap<(String, ObjectKind), String>,
    pub expanded: HashSet<String>,
    pub cached_at: Option<i64>,
    /// The tree filter while it is scoped to this connection (empty otherwise).
    pub filter: String,
    /// Object actions waiting for their `Detail` or routine definition.
    pending_detail: Vec<PendingDetail>,
    /// Detail of relations expanded in the tree (columns, keys, indexes, FKs, triggers),
    /// by (schema, name, kind). Filled from `IntrospectScope::Detail` (DBX-2a).
    pub details: HashMap<(String, String, ObjectKind), Loadable<Box<ObjectDetail>>>,
    /// Reload everything from the server once the catalog session opens.
    refresh_on_open: bool,
    /// Catalog requests made while the session was still opening; sent once it opens.
    queued: Vec<(IntrospectScope, bool)>,
    /// Server-side object search for the filter (DBX-1d).
    pub search: crate::object_search::ObjectSearch,
}

/// An object action waiting for catalog data: `Detail`, or the routine definition
/// (`IntrospectScope::RoutineDefinition`) when `routine`.
#[derive(Clone, Debug, PartialEq)]
struct PendingDetail {
    action: String,
    schema: String,
    name: String,
    kind: ObjectKind,
    routine: bool,
}

/// Session state of the schema explorer (wrapper so `Default` is `None`).
#[derive(Clone, Debug, Default)]
pub struct SessionState2(pub Option<SessionState>);

impl SchemaState {
    /// A collapsed, not connected node for `conn`; its catalog session opens on the first
    /// expand ([`Self::connect`]).
    pub fn new(conn: DbConnection) -> Self {
        Self {
            connection: Some(conn),
            ..Self::default()
        }
    }

    /// Whether the catalog session is open.
    pub fn is_open(&self) -> bool {
        matches!(self.state.0, Some(SessionState::Open { .. }))
    }

    /// Open the catalog session (closing a previous one) and expand the node. What is
    /// already loaded stays on screen until it is reloaded.
    pub fn connect(&mut self, core: &dyn CoreSink) {
        let Some(conn) = &self.connection else { return };
        if let Some(s) = self.session.take() {
            core.send(Command::CloseSession { session: s });
        }
        let session = next_id();
        core.send(Command::OpenSession {
            session,
            connection: conn.id.clone(),
        });
        self.session = Some(session);
        self.state = SessionState2(Some(SessionState::Connecting));
        self.expanded.insert("db".into());
    }

    /// Close and reopen the catalog session with an empty tree (a changed profile may
    /// point elsewhere).
    pub fn reconnect(&mut self, core: &dyn CoreSink) {
        if let Some(s) = self.session.take() {
            core.send(Command::CloseSession { session: s });
        }
        let conn = self.connection.take();
        *self = SchemaState::default();
        self.connection = conn;
        self.connect(core);
    }

    /// Ask for the detail of an object (`routine`: its routine definition); `action`
    /// runs when it arrives (a cached copy is fine). Returns `false` without a catalog
    /// session.
    pub(crate) fn request_detail(
        &mut self,
        action: &str,
        schema: &str,
        name: &str,
        kind: ObjectKind,
        routine: bool,
        core: &dyn CoreSink,
    ) -> bool {
        if self.session.is_none() {
            return false;
        }
        let entry = PendingDetail {
            action: action.to_owned(),
            schema: schema.to_owned(),
            name: name.to_owned(),
            kind,
            routine,
        };
        let asked = self.pending_detail.iter().any(|p| {
            p.schema == schema && p.name == name && p.kind == kind && p.routine == routine
        });
        if !self.pending_detail.contains(&entry) {
            self.pending_detail.push(entry);
        }
        if !asked {
            let scope = if routine {
                IntrospectScope::RoutineDefinition {
                    schema: schema.to_owned(),
                    name: name.to_owned(),
                    kind,
                    signature: self.signature_of(schema, name, kind),
                }
            } else {
                IntrospectScope::Detail {
                    schema: schema.to_owned(),
                    name: name.to_owned(),
                    kind,
                }
            };
            self.request(scope, false, core);
        }
        true
    }

    /// The tree's signature (`ObjectInfo::detail`) of a loaded object.
    fn signature_of(&self, schema: &str, name: &str, kind: ObjectKind) -> Option<String> {
        match self.objects.get(&(schema.to_owned(), kind)) {
            Some(Loadable::Loaded(objects)) => objects
                .iter()
                .find(|o| o.name == name)
                .and_then(|o| o.detail.clone()),
            _ => None,
        }
    }

    /// The actions waiting for the detail (or routine definition) of `schema.name`.
    pub(crate) fn take_pending(
        &mut self,
        schema: &str,
        name: &str,
        kind: ObjectKind,
        routine: bool,
    ) -> Vec<String> {
        let mut out = Vec::new();
        self.pending_detail.retain(|p| {
            let hit =
                p.schema == schema && p.name == name && p.kind == kind && p.routine == routine;
            if hit {
                out.push(p.action.clone());
            }
            !hit
        });
        out
    }

    /// A relation's detail arrived: keep it for the tree's child rows.
    fn store_detail(
        &mut self,
        schema: &str,
        name: &str,
        kind: ObjectKind,
        result: &Result<CatalogChunk, String>,
    ) {
        if !kind.is_relation() {
            return;
        }
        let state = match result {
            Ok(CatalogChunk::Detail(d)) => Loadable::Loaded(d.clone()),
            Ok(_) => return,
            Err(e) => Loadable::Failed(e.clone()),
        };
        self.details
            .insert((schema.to_owned(), name.to_owned(), kind), state);
    }

    /// The cached detail of a relation, if loaded.
    pub(crate) fn cached_detail(
        &self,
        schema: &str,
        name: &str,
        kind: ObjectKind,
    ) -> Option<&ObjectDetail> {
        match self
            .details
            .get(&(schema.to_owned(), name.to_owned(), kind))
        {
            Some(Loadable::Loaded(d)) => Some(d),
            _ => None,
        }
    }

    /// Reload the detail of a relation from the server (F5 on it or its children).
    fn refresh_detail(&mut self, schema: &str, name: &str, kind: ObjectKind, core: &dyn CoreSink) {
        self.request(
            IntrospectScope::Detail {
                schema: schema.to_owned(),
                name: name.to_owned(),
                kind,
            },
            true,
            core,
        );
    }

    /// The folder (schema, kind) a `f:<schema>:<Kind>` key names; a database-level
    /// folder (`f::<Kind>`, DBX-5c) has an empty schema.
    pub(crate) fn folder_of(&self, key: &str) -> Option<(String, ObjectKind)> {
        let (schema, kind) = key.strip_prefix("f:")?.rsplit_once(':')?;
        let kinds = if schema.is_empty() {
            self.server_folders()
        } else {
            self.folders()
        };
        let kind = kinds.iter().find(|k| format!("{k:?}") == kind)?;
        Some((schema.to_owned(), *kind))
    }

    /// Reload one tree node from the server (F5): a relation's child rows (`owner` is the
    /// relation), a folder, every loaded folder of a schema, an object's folder (tree or
    /// search row) and its loaded detail, or everything for the database row.
    pub(crate) fn refresh_node(
        &mut self,
        key: &str,
        owner: Option<&(String, String, ObjectKind)>,
        core: &dyn CoreSink,
    ) {
        if let Some((schema, name, kind)) = owner {
            if self
                .details
                .contains_key(&(schema.clone(), name.clone(), *kind))
            {
                self.refresh_detail(schema, name, *kind, core);
            }
            // A child row reloads only its relation; the object row also its folder.
            if !key.starts_with("o:") && !key.starts_with("q:") {
                return;
            }
        }
        let folders: Vec<(String, ObjectKind)> = if let Some(f) = self.folder_of(key) {
            vec![f]
        } else if let Some(schema) = key.strip_prefix("s:") {
            self.objects
                .keys()
                .filter(|(s, _)| s == schema)
                .cloned()
                .collect()
        } else if let Some(rest) = key.strip_prefix("o:").or_else(|| key.strip_prefix("q:")) {
            self.objects
                .keys()
                .filter(|(s, k)| rest.starts_with(&format!("{s}:{k:?}:")))
                .cloned()
                .collect()
        } else {
            return self.refresh(core);
        };
        for (schema, kind) in folders {
            self.request(IntrospectScope::Objects { schema, kind }, true, core);
        }
    }

    /// The catalog session opened: load the schemas and send what waited for it.
    pub fn on_open(&mut self, version: String, core: &dyn CoreSink) {
        self.state = SessionState2(Some(SessionState::Open { version }));
        let refresh = std::mem::take(&mut self.refresh_on_open);
        self.request(IntrospectScope::Schemas, refresh, core);
        for (scope, refresh) in std::mem::take(&mut self.queued) {
            self.request(scope, refresh, core);
        }
    }

    /// Reload everything now, or as soon as the catalog session opens (connecting a
    /// node that is not connected).
    pub fn refresh_when_open(&mut self, core: &dyn CoreSink) {
        match self.state.0 {
            Some(SessionState::Connecting) => self.refresh_on_open = true,
            Some(SessionState::Open { .. }) => self.refresh(core),
            _ => {
                self.connect(core);
                self.refresh_on_open = true;
            }
        }
    }

    /// Close the catalog session (Disconnect): the node collapses and reads "not
    /// connected"; its cache stays and the next expand reconnects.
    pub fn disconnect(&mut self, core: &dyn CoreSink) {
        if let Some(s) = self.session.take() {
            core.send(Command::CloseSession { session: s });
        }
        self.pending_detail.clear();
        self.queued.clear();
        self.state = SessionState2(None);
        self.expanded.remove("db");
    }

    /// The catalog session failed.
    pub fn on_failed(&mut self, message: String) {
        self.queued.clear();
        self.state = SessionState2(Some(SessionState::Failed(message)));
    }

    /// Ask the catalog session for `scope`. While the session is still opening, the
    /// request waits for it (core answers "not open" otherwise).
    fn request(&mut self, scope: IntrospectScope, refresh: bool, core: &dyn CoreSink) {
        let Some(session) = self.session else { return };
        let open = self.is_open();
        match &scope {
            IntrospectScope::Schemas => self.schemas = Loadable::Loading,
            IntrospectScope::Objects { schema, kind } => {
                self.objects
                    .insert((schema.clone(), *kind), Loadable::Loading);
            }
            // A loaded detail stays on screen while it reloads.
            IntrospectScope::Detail { schema, name, kind } if kind.is_relation() => {
                let key = (schema.clone(), name.clone(), *kind);
                if !matches!(self.details.get(&key), Some(Loadable::Loaded(_))) {
                    self.details.insert(key, Loadable::Loading);
                }
            }
            _ => {}
        }
        if !open {
            if !self.queued.iter().any(|(s, _)| *s == scope) {
                self.queued.push((scope, refresh));
            }
            return;
        }
        core.send(Command::Introspect {
            session,
            scope,
            refresh,
        });
    }

    /// Load the folder `(schema, kind)` unless it is loaded or loading (pins, reveal).
    pub fn ensure_folder(&mut self, schema: &str, kind: ObjectKind, core: &dyn CoreSink) {
        if !matches!(
            self.objects.get(&(schema.to_owned(), kind)),
            Some(Loadable::Loaded(_) | Loadable::Loading)
        ) {
            self.request(
                IntrospectScope::Objects {
                    schema: schema.to_owned(),
                    kind,
                },
                false,
                core,
            );
        }
    }

    /// Object folders for the connection's dialect.
    pub fn folders(&self) -> &'static [ObjectKind] {
        self.dialect().object_folders()
    }

    /// Database-level folders (users and roles, jobs, extensions; DBX-5c).
    pub fn server_folders(&self) -> &'static [ObjectKind] {
        self.dialect().server_folders()
    }

    /// The connection's dialect (PostgreSQL before one is bound).
    fn dialect(&self) -> &'static dyn switchyard_core::db::Dialect {
        dialect_for(
            self.connection
                .as_ref()
                .map_or(Engine::Postgres, |c| c.engine),
        )
    }

    /// The filter text changed: returns a ticket for [`Self::search_due`] when a debounced
    /// server search should follow.
    pub fn filter_changed(&mut self, filter: String, core: &dyn CoreSink) -> Option<u64> {
        self.filter = filter;
        if !self.filter.is_empty() {
            self.load_all_folders(core);
        }
        self.search.changed(&self.filter)
    }

    /// The search debounce elapsed: send the search unless the filter changed meanwhile.
    pub fn search_due(&mut self, ticket: u64, core: &dyn CoreSink) {
        let Some(session) = self.session.filter(|_| self.is_open()) else {
            return;
        };
        if let Some(scope) = self.search.due(ticket) {
            core.send(Command::Introspect {
                session,
                scope,
                refresh: true,
            });
        }
    }

    /// Load every object folder of every user schema (for search).
    pub fn load_all_folders(&mut self, core: &dyn CoreSink) {
        let schemas: Vec<String> = match &self.schemas {
            Loadable::Loaded(s) => s
                .iter()
                .filter(|s| !s.is_system)
                .map(|s| s.name.clone())
                .collect(),
            _ => return,
        };
        let schema_folders = schemas
            .into_iter()
            .flat_map(|s| self.folders().iter().map(move |k| (s.clone(), *k)));
        let server_folders = self.server_folders().iter().map(|k| (String::new(), *k));
        let folders: Vec<(String, ObjectKind)> = schema_folders.chain(server_folders).collect();
        for (schema, kind) in folders {
            if matches!(
                self.objects.get(&(schema.clone(), kind)),
                None | Some(Loadable::NotLoaded)
            ) {
                self.request(IntrospectScope::Objects { schema, kind }, false, core);
            }
        }
    }

    /// Reload everything from the server.
    pub fn refresh(&mut self, core: &dyn CoreSink) {
        // A session that failed to open (wrong password, server down) is reopened; a
        // node that is not connected connects first.
        match self.state.0 {
            Some(SessionState::Failed(_)) => return self.reconnect(core),
            None | Some(SessionState::None | SessionState::Connecting) => {
                return self.refresh_when_open(core);
            }
            Some(SessionState::Open { .. }) => {}
        }
        let open: Vec<(String, ObjectKind)> = self
            .objects
            .iter()
            .filter(|(_, v)| matches!(v, Loadable::Loaded(_)))
            .map(|(k, _)| k.clone())
            .collect();
        let details: Vec<(String, String, ObjectKind)> = self.details.keys().cloned().collect();
        self.request(IntrospectScope::Schemas, true, core);
        for (schema, kind) in open {
            self.request(IntrospectScope::Objects { schema, kind }, true, core);
        }
        for (schema, name, kind) in details {
            self.refresh_detail(&schema, &name, kind, core);
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
                self.hints.remove(&(schema.clone(), kind));
                self.objects.insert((schema, kind), Loadable::Loaded(o));
            }
            (IntrospectScope::Objects { schema, kind }, Ok(CatalogChunk::Hint(h))) => {
                self.hints.insert((schema.clone(), kind), h);
                self.objects
                    .insert((schema, kind), Loadable::Loaded(Vec::new()));
            }
            (IntrospectScope::Objects { schema, kind }, Err(e)) => {
                self.objects.insert((schema, kind), Loadable::Failed(e));
            }
            _ => {}
        }
    }

    /// Expand or collapse a node. `object` is the relation an object row shows: its
    /// detail (the child rows) loads on the first expand.
    pub(crate) fn toggle(
        &mut self,
        key: &str,
        object: Option<&(String, String, ObjectKind)>,
        core: &dyn CoreSink,
    ) {
        if !self.expanded.remove(key) {
            self.expanded.insert(key.to_owned());
            // The connection node connects on its first expand (and retries a failure).
            if key == "db" {
                if self.session.is_none() || matches!(self.state.0, Some(SessionState::Failed(_))) {
                    self.connect(core);
                }
                return;
            }
            if let Some((schema, name, kind)) = object.filter(|o| o.2.is_relation()) {
                let key = (schema.clone(), name.clone(), *kind);
                if !matches!(
                    self.details.get(&key),
                    Some(Loadable::Loaded(_) | Loadable::Loading)
                ) {
                    self.request(
                        IntrospectScope::Detail {
                            schema: key.0,
                            name: key.1,
                            kind: key.2,
                        },
                        false,
                        core,
                    );
                }
                return;
            }
            if let Some((schema, kind)) = self.folder_of(key)
                && !matches!(
                    self.objects.get(&(schema.clone(), kind)),
                    Some(Loadable::Loaded(_) | Loadable::Loading)
                )
            {
                self.request(IntrospectScope::Objects { schema, kind }, false, core);
            }
        }
    }
}

/// Object actions that need the object's columns, key or DDL.
const NEEDS_DETAIL: &[&str] = &[
    "select",
    "insert",
    "update",
    "delete",
    "ddl",
    "script_create",
    "script_drop_create",
    "script_select",
    "script_insert",
    "script_update",
    "script_delete",
    "script_exec",
];

/// Whether `action` on an object of `kind` needs its routine definition
/// (`IntrospectScope::RoutineDefinition`) rather than its `Detail`.
fn needs_routine(action: &str, kind: ObjectKind) -> bool {
    matches!(
        kind,
        ObjectKind::Function | ObjectKind::Procedure | ObjectKind::Package
    ) && matches!(
        action,
        "script_create" | "script_drop_create" | "script_exec"
    )
}

/// Actions that cannot run without the catalog data they asked for.
const NEEDS_DATA: &[&str] = &["ddl", "script_create", "script_drop_create", "script_exec"];

/// Column names in table order, and the primary key columns.
fn columns_and_key(d: &ObjectDetail) -> (Vec<String>, Vec<String>) {
    let mut cols: Vec<_> = d.columns.iter().collect();
    cols.sort_by_key(|c| c.ordinal);
    let mut pk: Vec<String> = cols
        .iter()
        .filter(|c| c.is_primary_key)
        .map(|c| c.name.clone())
        .collect();
    if pk.is_empty()
        && let Some(ix) = d.indexes.iter().find(|i| i.is_primary)
    {
        pk = ix.columns.clone();
    }
    (cols.into_iter().map(|c| c.name.clone()).collect(), pk)
}

/// A schema object being dragged (into the SQL editor).
#[derive(Clone, Debug)]
pub struct DraggedObject {
    /// Text to insert into a tab on the same connection: an object's quoted, qualified
    /// name, or a child row's (column, index, …) quoted name.
    pub qualified: String,
    /// Text to insert into a tab on another connection: always the qualified name (a
    /// child row's name qualified by its relation).
    pub full: String,
    /// The connection the object belongs to (DBX-5e).
    pub conn: Option<ProfileId>,
}

impl DraggedObject {
    /// What a drop into a tab on `tab_conn` inserts: the qualified name only, unless the
    /// tab is on the object's own connection.
    pub fn text_for(&self, tab_conn: Option<&ProfileId>) -> &str {
        match (&self.conn, tab_conn) {
            (Some(own), Some(tab)) if own != tab => &self.full,
            (Some(_), None) => &self.full,
            _ => &self.qualified,
        }
    }
}

/// One flattened tree row.
#[derive(Clone, Debug)]
pub(crate) struct TreeRow {
    pub(crate) depth: usize,
    pub(crate) caret: &'static str,
    pub(crate) icon: SharedString,
    pub(crate) label: SharedString,
    pub(crate) sub: SharedString,
    pub(crate) loading: bool,
    /// Key of the row within its connection (`db`, `s:<schema>`, `f:…`, `o:…`, …) or
    /// in the Favorites section (`favorites`, `fav:<conn>`, `fav:<id>`).
    pub(crate) key: String,
    pub(crate) object: Option<(String, String, ObjectKind)>,
    pub(crate) dim: bool,
    /// The relation a child row (column, key, index, FK, trigger) belongs to.
    pub(crate) owner: Option<(String, String, ObjectKind)>,
    /// Quoted name a child row copies (Ctrl+C) and drags into the editor.
    pub(crate) leaf: Option<String>,
    /// The connection the row belongs to; `None` for the Favorites header.
    pub(crate) conn: Option<ProfileId>,
    /// Environment of a connection row (its node, or its group in Favorites).
    pub(crate) env: Option<switchyard_core::store::EnvironmentLabel>,
    /// The pin a Favorites row shows.
    pub(crate) fav: Option<i64>,
}

impl TreeRow {
    /// A plain dimmed row at `depth`.
    pub(crate) fn new(depth: usize, key: String) -> Self {
        Self {
            depth,
            caret: "",
            icon: "".into(),
            label: "".into(),
            sub: "".into(),
            loading: false,
            key,
            object: None,
            dim: true,
            owner: None,
            leaf: None,
            conn: None,
            env: None,
            fav: None,
        }
    }

    /// Whether this is a connection node.
    pub(crate) fn is_node(&self) -> bool {
        self.key == "db" && self.conn.is_some()
    }
}

/// One child group under an expanded relation.
struct ChildGroup {
    id: &'static str,
    label: &'static str,
    /// (icon, label, sub, name) per item.
    items: Vec<(&'static str, String, String, String)>,
}

/// The child groups of a relation from its detail: Columns always, then Keys, Indexes,
/// Foreign keys and Triggers when the relation has any (views usually have none).
fn child_groups(d: &ObjectDetail) -> Vec<ChildGroup> {
    let (_, pk) = columns_and_key(d);
    let fk_cols: HashSet<&str> = d
        .foreign_keys
        .iter()
        .flat_map(|f| f.columns.iter().map(String::as_str))
        .collect();
    let mut cols: Vec<_> = d.columns.iter().collect();
    cols.sort_by_key(|c| c.ordinal);
    let columns = cols
        .into_iter()
        .map(|c| {
            let icon = if c.is_primary_key || pk.contains(&c.name) {
                "PK"
            } else if fk_cols.contains(c.name.as_str()) {
                "FK"
            } else {
                "·"
            };
            let null = if c.nullable { "null" } else { "not null" };
            (
                icon,
                c.name.clone(),
                format!("{} · {null}", c.data_type),
                c.name.clone(),
            )
        })
        .collect();
    let mut keys: Vec<(&'static str, String, String, String)> = d
        .constraints
        .iter()
        .filter_map(|c| {
            let icon = match c.kind.as_str() {
                "PRIMARY KEY" => "PK",
                "UNIQUE" => "UQ",
                _ => return None,
            };
            let cols = c
                .definition
                .trim()
                .strip_prefix(c.kind.as_str())
                .unwrap_or(&c.definition)
                .trim();
            Some((icon, c.name.clone(), cols.to_owned(), c.name.clone()))
        })
        .collect();
    // Engines that report keys only through their indexes (SQL Server, Oracle, D1).
    if !keys.iter().any(|k| k.0 == "PK")
        && let Some(ix) = d.indexes.iter().find(|i| i.is_primary)
    {
        let cols = format!("({})", ix.columns.join(", "));
        keys.insert(0, ("PK", ix.name.clone(), cols, ix.name.clone()));
    }
    let indexes = d
        .indexes
        .iter()
        .map(|i| {
            let unique = if i.is_unique { " · unique" } else { "" };
            (
                "IX",
                i.name.clone(),
                format!("({}){unique}", i.columns.join(", ")),
                i.name.clone(),
            )
        })
        .collect();
    let fks = d
        .foreign_keys
        .iter()
        .map(|f| {
            (
                "FK",
                format!("{} ({})", f.name, f.columns.join(", ")),
                format!("→ {}", f.references),
                f.name.clone(),
            )
        })
        .collect();
    let triggers = d
        .triggers
        .iter()
        .map(|t| ("TR", t.clone(), String::new(), t.clone()))
        .collect();
    let groups = [
        ("columns", "Columns", columns),
        ("keys", "Keys", keys),
        ("indexes", "Indexes", indexes),
        ("fks", "Foreign keys", fks),
        ("triggers", "Triggers", triggers),
    ];
    groups
        .into_iter()
        .filter(|(id, _, items)| *id == "columns" || !items.is_empty())
        .map(|(id, label, items)| ChildGroup { id, label, items })
        .collect()
}

/// The child rows of an expanded relation (`okey` is its row key, `depth` its depth).
fn relation_rows(
    s: &SchemaState,
    owner: &(String, String, ObjectKind),
    okey: &str,
    depth: usize,
    quote: &dyn Fn(&str) -> String,
    rows: &mut Vec<TreeRow>,
) {
    let row = |depth: usize, key: String| TreeRow {
        owner: Some(owner.clone()),
        ..TreeRow::new(depth, key)
    };
    let d = match s.details.get(owner) {
        Some(Loadable::Loaded(d)) => d,
        Some(Loadable::Failed(e)) => {
            rows.push(TreeRow {
                icon: "!".into(),
                label: e.clone().into(),
                ..row(depth + 1, format!("{okey}\u{1f}error"))
            });
            return;
        }
        _ => {
            rows.push(TreeRow {
                label: "Columns".into(),
                loading: true,
                ..row(depth + 1, format!("{okey}\u{1f}loading"))
            });
            return;
        }
    };
    for g in child_groups(d) {
        let gkey = format!("{okey}\u{1f}{}", g.id);
        let open = s.expanded.contains(&gkey);
        rows.push(TreeRow {
            caret: if open { "▾" } else { "▸" },
            label: g.label.into(),
            sub: g.items.len().to_string().into(),
            ..row(depth + 1, gkey.clone())
        });
        if !open {
            continue;
        }
        for (icon, label, sub, name) in g.items {
            rows.push(TreeRow {
                icon: icon.into(),
                label: label.into(),
                sub: sub.into(),
                dim: false,
                leaf: Some(quote(&name)),
                ..row(depth + 2, format!("{gkey}\u{1f}{name}"))
            });
        }
    }
}

/// One object folder and, when open, its objects: a schema's folder (`schema` set, at
/// `depth` 2) or a database-level one (`schema` empty, at depth 1; DBX-5c). With a
/// filter, only matching objects show and a folder without any is left out.
fn folder_rows(
    s: &SchemaState,
    schema: &str,
    kind: ObjectKind,
    depth: usize,
    filter: &str,
    quote: &dyn Fn(&str) -> String,
    rows: &mut Vec<TreeRow>,
) {
    let fkey = format!("f:{schema}:{kind:?}");
    let fopen = s.expanded.contains(&fkey);
    let state = s.objects.get(&(schema.to_owned(), kind));
    let hint = s.hints.get(&(schema.to_owned(), kind));
    let count = match state {
        Some(Loadable::Loaded(o)) if hint.is_none() => o.len().to_string(),
        _ => String::new(),
    };
    let matching: Vec<&ObjectInfo> = match state {
        Some(Loadable::Loaded(o)) => o
            .iter()
            .filter(|o| filter.is_empty() || crate::actions::fuzzy(filter, &o.name))
            .collect(),
        _ => Vec::new(),
    };
    if !filter.is_empty() && matching.is_empty() {
        return;
    }
    let show = fopen || (!filter.is_empty() && !matching.is_empty());
    let row = TreeRow::new;
    rows.push(TreeRow {
        caret: if show { "▾" } else { "▸" },
        label: s.dialect().folder_label(kind).into(),
        sub: count.into(),
        loading: matches!(state, Some(Loadable::Loading)),
        ..row(depth, fkey)
    });
    if !show {
        return;
    }
    if let Some(Loadable::Failed(e)) = state {
        rows.push(TreeRow {
            icon: "!".into(),
            label: e.clone().into(),
            ..row(depth + 1, format!("err:{schema}:{kind:?}"))
        });
    }
    if let Some(h) = hint {
        rows.push(TreeRow {
            icon: "i".into(),
            label: h.clone().into(),
            ..row(depth + 1, format!("hint:{schema}:{kind:?}"))
        });
    }
    for o in matching {
        let okey = format!("o:{schema}:{kind:?}:{}", o.name);
        let object = (schema.to_owned(), o.name.clone(), kind);
        let expandable = kind.is_relation();
        let oopen = expandable && s.expanded.contains(&okey);
        // DBX-5c kinds describe themselves on the right (role attributes, job status,
        // version); the others append their signature to the name.
        let (label, sub) = if kind.is_admin() {
            (o.name.clone(), o.detail.clone().unwrap_or_default())
        } else {
            (
                format!("{}{}", o.name, o.detail.clone().unwrap_or_default()),
                o.estimated_rows.map(compact).unwrap_or_default(),
            )
        };
        rows.push(TreeRow {
            caret: match (expandable, oopen) {
                (false, _) => "",
                (true, false) => "▸",
                (true, true) => "▾",
            },
            icon: kind.icon().into(),
            label: label.into(),
            sub: sub.into(),
            object: Some(object.clone()),
            dim: false,
            ..row(depth + 1, okey.clone())
        });
        if oopen {
            relation_rows(s, &object, &okey, depth + 1, quote, rows);
        }
    }
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
            icon: icon.into(),
            label: label.into(),
            loading,
            ..TreeRow::new(1, "search-status".into())
        });
    }
    let hits = merge(filter, local, s.search.hits());
    if hits.is_empty() && matches!(s.search.state, SearchState::Done(_)) {
        rows.push(TreeRow {
            label: "No matching objects".into(),
            ..TreeRow::new(1, "search-empty".into())
        });
    }
    for o in hits {
        rows.push(TreeRow {
            icon: o.kind.icon().into(),
            label: if o.schema.is_empty() {
                o.name.clone().into()
            } else {
                format!(
                    "{}.{}{}",
                    o.schema,
                    o.name,
                    o.detail.as_deref().unwrap_or("")
                )
                .into()
            },
            sub: kind_label(o.kind).into(),
            object: Some((o.schema.clone(), o.name.clone(), o.kind)),
            dim: false,
            ..TreeRow::new(1, format!("q:{}:{:?}:{}", o.schema, o.kind, o.name))
        });
    }
}

/// The short status a connection node shows on the right.
fn node_status(s: &SchemaState) -> String {
    match &s.state.0 {
        Some(SessionState::Open { version }) => version
            .replace("PostgreSQL ", "pg ")
            .split('.')
            .next()
            .unwrap_or("")
            .to_owned(),
        Some(SessionState::Connecting) => "connecting".into(),
        Some(SessionState::Failed(_)) => "failed".into(),
        None | Some(SessionState::None) => "not connected".into(),
    }
}

/// The rows of one connection: its node (depth 0) and, when open, its schemas, folders
/// and objects, or its search results while the filter is scoped to it. With
/// `filtered_out` (the filter is scoped to another connection) only the node shows.
/// Every row is tagged with the connection.
pub(crate) fn connection_rows(s: &SchemaState, filtered_out: bool, rows: &mut Vec<TreeRow>) {
    let Some(conn) = &s.connection else { return };
    let start = rows.len();
    let dialect = dialect_for(conn.engine);
    let quote = |name: &str| dialect.quote_ident(name);
    let filter = s.filter.to_lowercase();
    // A scoped filter shows its matches even under a collapsed (but connected) node.
    let db_open =
        !filtered_out && (s.expanded.contains("db") || (!filter.is_empty() && s.state.0.is_some()));
    rows.push(TreeRow {
        caret: if db_open { "▾" } else { "▸" },
        icon: conn.engine.badge().into(),
        label: conn.name.clone().into(),
        sub: node_status(s).into(),
        loading: matches!(s.state.0, Some(SessionState::Connecting)),
        dim: false,
        env: Some(conn.environment),
        ..TreeRow::new(0, "db".into())
    });
    if db_open {
        if crate::object_search::ObjectSearch::applies(&filter) {
            search_rows(s, &filter, rows);
        } else {
            schema_tree_rows(s, &filter, &quote, rows);
        }
    }
    for r in &mut rows[start..] {
        r.conn = Some(conn.id.clone());
    }
}

/// Schemas, their folders and the database-level folders of an open connection node.
fn schema_tree_rows(
    s: &SchemaState,
    filter: &str,
    quote: &dyn Fn(&str) -> String,
    rows: &mut Vec<TreeRow>,
) {
    // The session failed (wrong password, server down): say why above whatever was
    // loaded before; F5 or Refresh reconnects.
    if let Some(SessionState::Failed(e)) = &s.state.0 {
        rows.push(TreeRow {
            icon: "!".into(),
            label: e.clone().into(),
            ..TreeRow::new(1, "session-failed".into())
        });
        if !matches!(s.schemas, Loadable::Loaded(_)) {
            return;
        }
    }
    match &s.schemas {
        Loadable::Loaded(schemas) => {
            for sc in schemas {
                let key = format!("s:{}", sc.name);
                let open = s.expanded.contains(&key) || !filter.is_empty();
                rows.push(TreeRow {
                    caret: if open { "▾" } else { "▸" },
                    icon: "S".into(),
                    label: sc.name.clone().into(),
                    dim: sc.is_system,
                    ..TreeRow::new(1, key)
                });
                if !open {
                    continue;
                }
                for kind in s.folders() {
                    folder_rows(s, &sc.name, *kind, 2, filter, quote, rows);
                }
            }
            // Database-level folders (users and roles, jobs, extensions; DBX-5c).
            for kind in s.server_folders() {
                folder_rows(s, "", *kind, 1, filter, quote, rows);
            }
        }
        // Not connected yet: the node's first expand connects.
        Loadable::NotLoaded if s.session.is_none() => rows.push(TreeRow {
            label: "Not connected".into(),
            ..TreeRow::new(1, "not-connected".into())
        }),
        Loadable::Loading | Loadable::NotLoaded => rows.push(TreeRow {
            caret: "▾",
            label: "Schemas".into(),
            loading: true,
            ..TreeRow::new(1, "loading".into())
        }),
        Loadable::Failed(e) => rows.push(TreeRow {
            icon: "!".into(),
            label: e.clone().into(),
            ..TreeRow::new(1, "failed".into())
        }),
    }
}

/// Hosts split into those outside any folder (in order) and folders (sorted by name,
/// ignoring case) with their Hosts in order.
pub(crate) fn folder_groups<'a>(
    hosts: impl Iterator<Item = &'a Host>,
) -> (Vec<&'a Host>, Vec<(String, Vec<&'a Host>)>) {
    let mut loose = Vec::new();
    let mut folders: Vec<(String, Vec<&Host>)> = Vec::new();
    for h in hosts {
        match h.folder.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
            None => loose.push(h),
            Some(f) => match folders.iter_mut().find(|(name, _)| name == f) {
                Some((_, list)) => list.push(h),
                None => folders.push((f.to_owned(), vec![h])),
            },
        }
    }
    folders.sort_by_key(|(name, _)| name.to_lowercase());
    (loose, folders)
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

/// Context-menu target.
#[derive(Clone, Debug)]
pub enum CtxTarget {
    /// A schema object on its connection.
    Object(ObjRef),
    /// A saved profile.
    Profile(ProfileId),
    /// A tab in the tab strip (by index).
    Tab(usize),
    /// A session folder of Hosts in the sidebar (MX-6).
    Folder(String),
    /// A schema (its row or one of its folders) on a connection.
    Schema(ProfileId, String),
}

/// The schema of a tree row key: a schema row (`s:<schema>`) or a folder
/// (`f:<schema>:<Kind>`). A database-level folder (`f::<Kind>`) has none.
pub(crate) fn schema_of_row_key(key: &str) -> Option<String> {
    match key.strip_prefix("s:") {
        Some(s) => Some(s.to_owned()),
        None => key
            .strip_prefix("f:")
            .and_then(|f| f.rsplit_once(':'))
            .map(|(s, _)| s.to_owned())
            .filter(|s| !s.is_empty()),
    }
}

/// The text an object copies and drags: its qualified name, or its quoted name alone
/// for one outside any schema (users and roles, jobs, extensions).
pub(crate) fn object_name_text(
    d: &dyn switchyard_core::db::Dialect,
    schema: &str,
    name: &str,
    kind: ObjectKind,
) -> String {
    if kind.is_server_level() || schema.is_empty() {
        d.quote_ident(name)
    } else {
        d.qualified(schema, name)
    }
}

/// Whether `action` may run on an object of `kind`. The DBX-5c kinds (roles, jobs,
/// extensions, packages, stages, tasks, pipes) are browsed read-only: View DDL, copy,
/// dependencies and Script as CREATE where the engine can produce one; never DROP,
/// templates or anything that starts or changes them.
pub(crate) fn action_allowed(action: &str, kind: ObjectKind) -> bool {
    if !kind.is_admin() {
        return true;
    }
    match action {
        "ddl" | "copy" | "deps" => true,
        "properties" => !kind.is_server_level(),
        "script_create" => kind.scripts_create(),
        _ => false,
    }
}

/// What Enter or a double-click does on an object row: open a relation's data, show
/// the DDL of a DBX-5c kind (read-only browsing), nothing for the rest.
fn default_action(kind: ObjectKind) -> Option<&'static str> {
    if kind.is_relation() {
        Some("open")
    } else if kind.is_admin() {
        Some("ddl")
    } else {
        None
    }
}

/// An open context menu.
#[derive(Clone, Debug)]
pub struct CtxMenu {
    /// Position in window coordinates.
    pub at: Point<Pixels>,
    /// Target.
    pub target: CtxTarget,
    /// Highlighted item (mouse hover or arrow keys).
    pub cursor: Option<usize>,
    /// The item whose submenu is open.
    pub sub: Option<usize>,
    /// Highlighted item of the open submenu.
    pub sub_cursor: Option<usize>,
    /// Keyboard focus of the menu, created when it first renders.
    pub focus: Option<FocusHandle>,
}

impl CtxMenu {
    /// A closed-submenu menu at `at` for `target`.
    pub fn new(at: Point<Pixels>, target: CtxTarget) -> Self {
        Self {
            at,
            target,
            cursor: None,
            sub: None,
            sub_cursor: None,
            focus: None,
        }
    }
}

impl Workspace {
    /// Follow the active SQL tab: its connection's node is highlighted and scrolled into
    /// view (added, expanded and connected when it is not in the explorer yet). Other
    /// nodes keep their state.
    pub(crate) fn sync_schema(&mut self, cx: &mut Context<Self>) {
        // The assistant follows the active tab too (databases, Redis, SSH terminals).
        self.sync_assistant(cx);
        let conn = self
            .active_sql()
            .and_then(|t| t.read(cx).connection.clone());
        let id = conn.as_ref().map(|c| c.id.clone());
        let known = id.as_ref().is_some_and(|i| self.explorer.contains(i));
        if id == self.explorer.active && (known || id.is_none()) {
            return;
        }
        self.explorer.active = id.clone();
        let (Some(conn), Some(id)) = (conn, id) else {
            return;
        };
        // While the saved nodes load at startup, nothing connects by itself.
        if self.explorer.ensure(conn) && !self.explorer.restoring {
            let core = self.core.clone();
            if let Some(s) = self.explorer.state_mut(&id) {
                s.connect(&core);
            }
        }
        self.explorer.scroll_to(&RowId::node(&id));
    }

    /// Connection menu "Show in explorer": add the connection's node, expand and connect
    /// it, and select it.
    pub(crate) fn show_in_explorer(&mut self, id: &ProfileId, cx: &mut Context<Self>) {
        let Some(conn) = self.profiles.db(id).cloned() else {
            return;
        };
        let core = self.core.clone();
        self.explorer.ensure_connected(conn, &core);
        self.side_tab = SideTab::Schema;
        let node = RowId::node(id);
        self.explorer.scroll_to(&node);
        self.explorer.cursor = Some(node);
        cx.notify();
    }

    /// Connection menu "Remove from explorer": drop the node and close its session.
    pub(crate) fn remove_from_explorer(&mut self, id: &ProfileId, cx: &mut Context<Self>) {
        let core = self.core.clone();
        self.explorer.remove(id, &core);
        cx.notify();
    }

    /// The tree's Refresh link, F5 without a cursor and the palette's Refresh Schema:
    /// reload the connection in scope ([`Explorer::scope_conn`]).
    pub(crate) fn refresh_explorer(&mut self, cx: &mut Context<Self>) {
        let core = self.core.clone();
        if let Some(id) = self.explorer.scope_conn()
            && let Some(s) = self.explorer.state_mut(&id)
        {
            s.refresh(&core);
        }
        cx.notify();
    }

    /// Connection menu "New query here": a new SQL tab on that connection.
    pub(crate) fn new_query_here(
        &mut self,
        id: &ProfileId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(conn) = self.profiles.db(id).cloned() else {
            return;
        };
        let tab = self.open_query_tab(&conn, &conn.name, window, cx);
        let ed = tab.read(cx).editor().clone();
        ed.update(cx, |e, cx| e.focus(window, cx));
        cx.notify();
    }

    /// Connection menu "Refresh schema": show that connection's node (connecting it)
    /// and reload it from the server once its catalog session has opened.
    pub(crate) fn refresh_connection_schema(
        &mut self,
        id: &ProfileId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_in_explorer(id, cx);
        let core = self.core.clone();
        if let Some(s) = self.explorer.state_mut(id) {
            s.refresh_when_open(&core);
        }
        cx.notify();
    }

    /// Connection menu "Disconnect": close every session on the connection (SQL tabs and
    /// the schema explorer) through core. Unless `confirmed`, asks first when a tab has
    /// an open transaction or staged edits. The tabs stay; their next run reconnects.
    pub(crate) fn disconnect_connection(
        &mut self,
        id: &ProfileId,
        confirmed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(name) = self.profiles.db(id).map(|c| c.name.clone()) else {
            return;
        };
        let tabs: Vec<_> = self
            .tabs
            .iter()
            .filter_map(|t| match t {
                Tab::Sql(s) if s.read(cx).connection.as_ref().is_some_and(|c| &c.id == id) => {
                    Some(s.clone())
                }
                _ => None,
            })
            .collect();
        let reasons: Vec<String> = tabs
            .iter()
            .flat_map(|t| {
                let t = t.read(cx);
                let mut why = Vec::new();
                if t.txn_open {
                    why.push(format!("{}: open transaction (rolled back)", t.title));
                }
                if t.has_staged_edits() {
                    why.push(format!("{}: staged edits not applied", t.title));
                }
                why
            })
            .collect();
        if !confirmed && !reasons.is_empty() {
            self.overlay = Some(crate::overlays::Overlay::ConfirmDisconnect {
                profile: id.clone(),
                name,
                reasons,
            });
            window.focus(&self.overlay_focus, cx);
            cx.notify();
            return;
        }
        let mut closed = 0;
        for t in tabs {
            if t.read(cx).session.is_some() {
                closed += 1;
                t.update(cx, |t, cx| t.disconnect(cx));
            }
        }
        let core = self.core.clone();
        if let Some(s) = self.explorer.state_mut(id)
            && s.session.is_some()
        {
            closed += 1;
            s.disconnect(&core);
        }
        if closed == 0 {
            self.toast(format!("{name} is not connected"), cx);
        } else {
            self.toast(format!("Disconnected {name}"), cx);
        }
        cx.notify();
    }

    /// Bulk edit `hosts` (MX-6) in a dialog.
    pub(crate) fn open_bulk_edit(
        &mut self,
        ids: &[ProfileId],
        title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let hosts: Vec<Host> = ids
            .iter()
            .filter_map(|id| self.profiles.host(id).cloned())
            .collect();
        if hosts.is_empty() {
            return;
        }
        let core = self.core.clone();
        let view = cx.new(|cx| {
            let refs: Vec<&Host> = hosts.iter().collect();
            crate::bulk_edit::BulkEditView::new(core, title, &refs, window, cx)
        });
        cx.subscribe(
            &view,
            |this, _, _: &crate::bulk_edit::BulkEditClosed, cx| {
                this.overlay = None;
                cx.notify();
            },
        )
        .detach();
        self.overlay = Some(crate::overlays::Overlay::BulkEdit(view));
        cx.notify();
    }

    /// The Hosts in folder `name`.
    pub(crate) fn folder_hosts(&self, name: &str) -> Vec<ProfileId> {
        self.profiles
            .hosts()
            .filter(|h| h.folder.as_deref().map(str::trim) == Some(name))
            .map(|h| h.id.clone())
            .collect()
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
                Tab::Redis(r) => {
                    let r = r.read(cx);
                    r.is_open().then(|| r.connection.id.clone())
                }
                Tab::Cloud(c) => {
                    let c = c.read(cx);
                    c.is_open().then(|| c.connection.id.clone())
                }
                _ => None,
            })
            .collect();
        let filter = self.explorer_filter.read(cx).value().to_string();
        explorer_rows(
            &self.profiles,
            &self.collapsed,
            &live,
            self.explorer_group,
            &filter,
        )
    }

    /// Run a context-menu action on a schema object, on the object's own connection
    /// (not the active tab's). Templates and DDL first fetch the object's `Detail`
    /// through that connection's catalog session; they finish in
    /// [`Self::on_schema_detail`].
    pub(crate) fn object_action(
        &mut self,
        action: &str,
        o: ObjRef,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !action_allowed(action, o.kind) {
            return;
        }
        if action == "properties" {
            return self.open_tree_object_properties(&o, None, window, cx);
        }
        if action == "deps" {
            let page = Some(crate::object_tab::Page::Dependencies);
            return self.open_tree_object_properties(&o, page, window, cx);
        }
        if NEEDS_DETAIL.contains(&action) {
            let routine = needs_routine(action, o.kind);
            // An expanded relation's detail is already here.
            if !routine && let Some(d) = self.explorer.cached_detail(&o) {
                let d = d.clone();
                return self.finish_object_action(action, &o, Some(&d), window, cx);
            }
            let core = self.core.clone();
            if self.explorer.request_detail(action, &o, routine, &core) {
                cx.notify();
                return;
            }
        }
        self.finish_object_action(action, &o, None, window, cx);
    }

    /// The detail of an object arrived on the catalog session of node `conn`: keep it
    /// for the tree and run the actions waiting for it.
    pub(crate) fn on_schema_detail(
        &mut self,
        conn: ProfileId,
        scope: IntrospectScope,
        result: Result<CatalogChunk, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (schema, name, kind, routine) = match scope {
            IntrospectScope::Detail { schema, name, kind } => (schema, name, kind, false),
            IntrospectScope::RoutineDefinition {
                schema, name, kind, ..
            } => (schema, name, kind, true),
            _ => return,
        };
        let Some(state) = self.explorer.state_mut(&conn) else {
            return;
        };
        if !routine {
            state.store_detail(&schema, &name, kind, &result);
            cx.notify();
        }
        let actions = state.take_pending(&schema, &name, kind, routine);
        if actions.is_empty() {
            return;
        }
        let detail = match result {
            Ok(CatalogChunk::Detail(d)) => Some(d),
            Ok(CatalogChunk::Hint(h)) => {
                self.toast(h, cx);
                None
            }
            Ok(_) => None,
            Err(e) => {
                self.toast(format!("Could not read {name}: {e}"), cx);
                None
            }
        };
        let o = ObjRef {
            conn,
            schema,
            name,
            kind,
        };
        for action in actions {
            if detail.is_none() && NEEDS_DATA.contains(&action.as_str()) {
                continue;
            }
            self.finish_object_action(&action, &o, detail.as_deref(), window, cx);
        }
    }

    fn finish_object_action(
        &mut self,
        action: &str,
        o: &ObjRef,
        detail: Option<&ObjectDetail>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(conn) = self.profiles.db(&o.conn).cloned() else {
            return;
        };
        let (schema, name, kind) = (o.schema.as_str(), o.name.as_str(), o.kind);
        if !action_allowed(action, kind) {
            return;
        }
        if action == "open" {
            // Table data view: server-side filter, sort and paging (DBX-3a).
            let (s, n) = (schema.to_owned(), name.to_owned());
            self.open_table_data(conn, s, n, kind, None, window, cx);
            return;
        }
        let d = dialect_for(conn.engine);
        let q = object_name_text(d, schema, name, kind);
        let (cols, pk) = detail.map(columns_and_key).unwrap_or_default();
        let text = match action {
            "select" => d.select_template(&q, &cols, 100),
            "insert" => d.insert_template(&q, &cols),
            "update" => d.update_template(&q, &cols, &pk),
            "delete" => d.delete_template(&q, &pk),
            "copy" => {
                cx.write_to_clipboard(ClipboardItem::new_string(q.clone()));
                self.toast(format!("Copied {q}"), cx);
                return;
            }
            "ddl" => {
                let ddl = detail.map(|d| d.ddl.clone()).unwrap_or_default();
                if ddl.trim().is_empty() {
                    self.toast(format!("No DDL available for {q}"), cx);
                } else {
                    self.open_ddl_tab(&conn, name, &q, ddl, window, cx);
                }
                return;
            }
            "truncate" => format!("TRUNCATE TABLE {q};"),
            "drop" => d.script_drop(kind, &q),
            // Script as (DBX-2c).
            "script_create" | "script_drop_create" => {
                let ddl = detail.map(|d| d.ddl.as_str()).unwrap_or_default();
                if ddl.trim().is_empty() {
                    self.toast(format!("No DDL available for {q}"), cx);
                    return;
                }
                if action == "script_create" {
                    d.script_create(kind, ddl)
                } else {
                    d.script_drop_create(kind, &q, ddl)
                }
            }
            "script_drop" => d.script_drop(kind, &q),
            "script_select" => d.select_template(&q, &cols, 100),
            "script_insert" => d.insert_template(&q, &cols),
            "script_update" => d.update_template(&q, &cols, &pk),
            "script_delete" => d.delete_template(&q, &pk),
            "script_exec" => d.script_exec(kind, &q, &cols),
            _ => return,
        };
        // Templates (INSERT/UPDATE/DELETE, Script as) go into the current tab when it is
        // on this connection and database; everything else opens its own tab, so the
        // current query is never replaced.
        let template =
            matches!(action, "insert" | "update" | "delete") || action.starts_with("script_");
        let same_db = self.active_sql().is_some_and(|t| {
            let t = t.read(cx);
            let active = t.connection.as_ref().map(|c| (&c.id, t.current_database()));
            crate::explorer::template_into_active(&conn, active)
        });
        let tab = if template && same_db {
            self.active_sql()
        } else {
            Some(self.open_query_tab(&conn, name, window, cx))
        };
        if let Some(tab) = tab {
            tab.update(cx, |t, cx| match action {
                // Runs once the new tab's session has opened.
                "truncate" | "drop" => t.set_text_and_run(&text, window, cx),
                _ => t.insert_text(&text, window, cx),
            });
        }
        cx.notify();
    }

    /// Show `ddl` in a read-only tab (replacing the tab already showing this object).
    fn open_ddl_tab(
        &mut self,
        conn: &DbConnection,
        name: &str,
        qualified: &str,
        ddl: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = format!("{}/{qualified}", conn.id);
        if let Some(ix) = self
            .tabs
            .iter()
            .position(|t| matches!(t, Tab::Ddl(d) if d.read(cx).key == key))
        {
            self.tabs.remove(ix);
            if self.active > ix {
                self.active -= 1;
            }
        }
        let badge = conn.engine.badge();
        let tab = cx.new(|cx| DdlTab::new(key, name, qualified, badge, ddl, window, cx));
        self.tabs.push(Tab::Ddl(tab));
        self.active = self.tabs.len() - 1;
        cx.notify();
    }

    pub(crate) fn render_sidebar(
        &mut self,
        p: &Palette,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let side = self.side_tab;
        // With an SSH terminal in front, the schema view browses that Host's files.
        let ssh = self.active_ssh_host(cx);
        let list: AnyElement = match (side, &ssh) {
            (SideTab::Explorer, _) => self.render_conn_list(p, cx),
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
            (SideTab::Tools, _) => self.render_tools_pane(p, cx),
            (SideTab::Activity, _) => self.render_activity_pane(p, cx),
        };
        let title = match (side, &ssh) {
            (SideTab::Explorer, _) => "EXPLORER",
            (SideTab::Schema, Some(_)) => "FILES",
            (SideTab::Schema, None) => "SCHEMA",
            (SideTab::Tools, _) => "TOOLS",
            (SideTab::Activity, _) => "ACTIVITY",
        };
        let group = self.explorer_group;
        let explorer = side == SideTab::Explorer;
        let header =
            div()
                .h(rpx(34.))
                .flex_none()
                .flex()
                .items_center()
                .gap(rpx(8.))
                .pl(rpx(12.))
                .pr(rpx(8.))
                .text_size(ts::SMALL)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.fg3)
                .child(div().flex_1().child(title))
                .when(explorer, |d| {
                    d.child(ui::segmented(
                        "explorer-group",
                        [ExplorerGroup::Place, ExplorerGroup::Type]
                            .into_iter()
                            .map(|g| {
                                let label = match g {
                                    ExplorerGroup::Place => "Place",
                                    ExplorerGroup::Type => "Type",
                                };
                                (
                                    SharedString::from(label),
                                    group == g,
                                    Box::new(cx.listener(move |this, _, _, cx| {
                                        this.set_explorer_group(g, cx)
                                    })) as ui::OnClick,
                                )
                            })
                            .collect(),
                        18.,
                        p,
                    ))
                })
                .child(
                    div()
                        .id("side-new")
                        .px(rpx(5.))
                        .rounded(px(4.))
                        .text_size(ts::TITLE_PLUS)
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(p.fg2)
                        .cursor_pointer()
                        .hover(|s| s.bg(p.hover))
                        .tooltip(|w, cx| {
                            gpui_kit::component::tooltip::Tooltip::new("New…").build(w, cx)
                        })
                        .on_click(cx.listener(|this, _, w, cx| this.open_new_chooser(w, cx)))
                        .child("+"),
                );
        let filter = explorer.then(|| {
            let count = if self.explorer_filter.read(cx).value().trim().is_empty() {
                String::new()
            } else {
                crate::explorer_tree::leaf_count(&self.conn_rows(cx)).to_string()
            };
            div().px(rpx(8.)).pb(rpx(4.)).flex_none().child(
                div()
                    .h(rpx(26.))
                    .flex()
                    .items_center()
                    .gap(rpx(8.))
                    .px(rpx(8.))
                    .border_1()
                    .border_color(p.bd)
                    .rounded(px(6.))
                    .bg(p.bg)
                    .text_size(ts::BODY)
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&self.explorer_filter)
                                .appearance(false)
                                .text_size(ts::BODY),
                        ),
                    )
                    .child(
                        div()
                            .font_family(MONO)
                            .text_size(ts::CAPTION_PLUS)
                            .text_color(p.fg3)
                            .child(count),
                    ),
            )
        });
        div()
            .w(rpx(self.sidebar_width))
            .relative()
            .child(
                // Drag the right edge to resize.
                div()
                    .id("side-resize")
                    .absolute()
                    .right(rpx(-3.))
                    .top_0()
                    .bottom_0()
                    .w(rpx(6.))
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
            .child(header)
            .children(filter)
            .child(list)
            .when(explorer, |d| {
                d.child(
                    div()
                        .flex_none()
                        .px(rpx(12.))
                        .py(rpx(7.))
                        .border_t_1()
                        .border_color(p.bd)
                        .text_size(ts::SMALL)
                        .text_color(p.fg3)
                        .child("Right-click anything for its actions"),
                )
            })
            .into_any_element()
    }

    /// Group the Explorer by place or by type, and remember it.
    pub(crate) fn set_explorer_group(&mut self, g: ExplorerGroup, cx: &mut Context<Self>) {
        self.explorer_group = g;
        self.side_tab = SideTab::Explorer;
        self.sidebar_open = true;
        self.core.send(Command::SetSetting {
            key: EXPLORER_GROUP_KEY.into(),
            value: g.key().into(),
        });
        cx.notify();
    }

    fn render_conn_list(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.conn_rows(cx);
        let count = rows.len();
        let p = *p;
        if count == 0 {
            return div()
                .flex_1()
                .px(rpx(12.))
                .py(rpx(16.))
                .text_size(ts::BODY)
                .text_color(p.fg3)
                .child("Nothing matches the filter")
                .into_any_element();
        }
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
        .pb(rpx(8.))
        .into_any_element()
    }

    fn render_conn_row(
        &self,
        r: &ConnRow,
        i: usize,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = r.kind == RowKind::Leaf
            && match (&r.profile, self.active_sql()) {
                (Some(id), Some(t)) => t.read(cx).connection.as_ref().is_some_and(|c| &c.id == id),
                _ => false,
            };
        let head = r.kind == RowKind::Head;
        let key = r.key.clone();
        let action = r.action.clone();
        let profile = r.profile.clone();
        let collapsed = self.collapsed.contains(&r.key);
        let ctx_profile = r.profile.clone();
        let ctx_folder = r.folder.clone();
        let drop_line = p.acc;
        let drag = r
            .drag_group
            .as_ref()
            .zip(r.profile.as_ref())
            .map(|(g, id)| DraggedProfile {
                id: id.clone(),
                group: g.clone(),
                label: r.label.clone(),
            });
        let drop_target = drag.clone();
        let over_target = drag.clone();
        // A row's dot: its environment at the top level of a section.
        let dot = r.env.filter(|_| r.depth == 0 || r.kind == RowKind::Leaf);
        div()
            .id(("conn-row", i))
            .when_some(drag, |d, drag| {
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
            .flex()
            .items_center()
            .gap(rpx(7.))
            .pr(rpx(10.))
            .when(head, |d| {
                d.h(rpx(30.))
                    .pt(rpx(8.))
                    .pl(rpx(12.))
                    .text_size(ts::SMALL)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.fg3)
            })
            .when(!head, |d| {
                d.h(rpx(26.))
                    .pl(rpx(8. + f32::from(r.depth) * 14.))
                    .text_size(ts::UI)
                    .hover(|s| s.bg(p.hover))
            })
            .when(active, |d| d.bg(p.sel))
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
                    // A saved file connection opens its own server (SFTP Host or FTP).
                    ConnAction::Files => match &profile {
                        Some(id) => this.open_profile(id, w, cx),
                        None => this.open_files(w, cx),
                    },
                    ConnAction::Profile => {
                        if let Some(id) = &profile {
                            this.open_profile(id, w, cx);
                        }
                    }
                }
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                    if let Some(id) = &ctx_profile {
                        this.ctx = Some(CtxMenu::new(ev.position, CtxTarget::Profile(id.clone())));
                        cx.notify();
                    } else if let Some(f) = &ctx_folder {
                        this.ctx = Some(CtxMenu::new(ev.position, CtxTarget::Folder(f.clone())));
                        cx.notify();
                    }
                }),
            )
            .when(!head, |d| {
                d.child(
                    div()
                        .w(rpx(10.))
                        .flex_none()
                        .text_color(p.fg3)
                        .text_size(ts::TINY)
                        .child(if r.kind == RowKind::Group {
                            if collapsed { "▸" } else { "▾" }
                        } else {
                            ""
                        }),
                )
            })
            .when_some(dot, |d, e| d.child(ui::dot(p.env(e), 7.)))
            .when(!r.badge.is_empty(), |d| {
                d.child(ui::monogram(r.badge, 26., p))
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .when(r.kind == RowKind::Group && r.depth == 0, |d| {
                        d.font_weight(FontWeight::SEMIBOLD)
                    })
                    .when(r.kind == RowKind::Group && r.depth > 0, |d| {
                        d.text_color(p.fg2)
                    })
                    .child(r.label.clone()),
            )
            .child(
                div()
                    .flex_shrink(1.)
                    .max_w(gpui_kit::relative(0.46))
                    .truncate()
                    .font_family(MONO)
                    .font_weight(FontWeight::NORMAL)
                    .text_size(ts::SMALL)
                    .text_color(p.fg3)
                    .child(r.sub.clone()),
            )
            .when(r.live, |d| d.child(ui::dot(p.dev, 6.)))
            .into_any_element()
    }

    fn render_schema(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.explorer.rows();
        let count = rows.len();
        let p = *p;
        let scope = self.explorer.scope_conn();
        let status = scope.as_ref().and_then(|id| {
            let s = self.explorer.state(id)?;
            let name = s.connection.as_ref().map(|c| c.name.clone())?;
            let cached = match s.cached_at {
                None => "not cached yet".to_owned(),
                Some(at) => match (now_ms() - at).max(0) / 60_000 {
                    0 => "cached just now".to_owned(),
                    mins => format!("cached {mins} min ago"),
                },
            };
            Some(format!("{name} · {cached}"))
        });
        let focus = self
            .explorer
            .focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone();
        let scroll = self.explorer.scroll.clone();
        let empty = rows.is_empty();
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_none()
                    .px(rpx(8.))
                    .pt(rpx(2.))
                    .pb(rpx(6.))
                    .child(
                        div()
                            .id("schema-search")
                            .h(rpx(26.))
                            .flex()
                            .items_center()
                            .gap(rpx(8.))
                            .px(rpx(8.))
                            .border_1()
                            .border_color(p.bd)
                            .rounded(px(6.))
                            .bg(p.bg)
                            .text_color(p.fg3)
                            .text_size(ts::BODY)
                            .child(
                                div().flex_1().child(
                                    Input::new(&self.schema_search)
                                        .appearance(false)
                                        .text_size(ts::BODY),
                                ),
                            )
                            .child(
                                div()
                                    .font_family(MONO)
                                    .text_size(ts::CAPTION_PLUS)
                                    .child(ui::keys("⌘P", "Ctrl+P")),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .gap(rpx(8.))
                            .px(rpx(2.))
                            .pt(rpx(6.))
                            .text_size(ts::SMALL)
                            .text_color(p.fg3)
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .child(status.unwrap_or_default()),
                            )
                            .child(
                                div()
                                    .id("schema-refresh")
                                    .flex_none()
                                    .text_color(p.acc)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.refresh_explorer(cx);
                                    }))
                                    .child("Refresh"),
                            ),
                    ),
            )
            .when(empty, |d| {
                d.child(
                    div()
                        .flex_1()
                        .p(rpx(16.))
                        .text_size(ts::UI)
                        .text_color(p.fg3)
                        .child(
                            "Open a SQL tab, or choose Show in explorer on a database \
                             connection, to browse its schema.",
                        ),
                )
            })
            .when(!empty, |d| {
                d.child(
                    div()
                        .key_context("SchemaTree")
                        .track_focus(&focus)
                        .on_action(cx.listener(|this, _: &TreeUp, _, cx| this.tree_move(-1, cx)))
                        .on_action(cx.listener(|this, _: &TreeDown, _, cx| this.tree_move(1, cx)))
                        .on_action(cx.listener(|this, _: &TreeExpand, _, cx| this.tree_expand(cx)))
                        .on_action(
                            cx.listener(|this, _: &TreeCollapse, _, cx| this.tree_collapse(cx)),
                        )
                        .on_action(cx.listener(|this, _: &TreeOpen, w, cx| this.tree_open(w, cx)))
                        .on_action(cx.listener(|this, _: &TreeCopy, w, cx| this.tree_copy(w, cx)))
                        .on_action(
                            cx.listener(|this, _: &TreeRefresh, _, cx| this.tree_refresh(cx)),
                        )
                        .on_action(cx.listener(|this, _: &TreePin, _, cx| this.tree_pin(cx)))
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .flex_col()
                        .child(
                            uniform_list(
                                "schema-rows",
                                count,
                                cx.processor(
                                    move |this, range: std::ops::Range<usize>, _window, cx| {
                                        range
                                            .map(|i| this.render_schema_row(&rows[i], i, &p, cx))
                                            .collect::<Vec<_>>()
                                    },
                                ),
                            )
                            .track_scroll(&scroll)
                            .flex_1()
                            .pb(rpx(8.)),
                        ),
                )
            })
            .into_any_element()
    }

    /// Index of the keyboard cursor in `rows`.
    fn tree_cursor(&self, rows: &[TreeRow]) -> Option<usize> {
        let c = self.explorer.cursor.as_ref()?;
        rows.iter().position(|r| c.is(r))
    }

    /// Put the keyboard cursor on row `ix` and scroll it into view.
    fn tree_set_cursor(&mut self, rows: &[TreeRow], ix: usize, cx: &mut Context<Self>) {
        let Some(r) = rows.get(ix) else { return };
        self.explorer.cursor = Some(RowId::of(r));
        if let Some(o) = obj_ref(r) {
            self.explorer.selected = Some(o);
        }
        self.explorer
            .scroll
            .scroll_to_item(ix, ScrollStrategy::Nearest);
        cx.notify();
    }

    /// Expand or collapse row `r` (a connection's node or tree row, or the Favorites
    /// header). `object` loads a relation's child rows on its first expand.
    fn tree_toggle(&mut self, r: &TreeRow, with_object: bool, cx: &mut Context<Self>) {
        let core = self.core.clone();
        match &r.conn {
            None if r.key == "favorites" => {
                self.explorer.toggle_favorites();
            }
            Some(id) if r.fav.is_none() && !r.key.starts_with("fav:") => {
                if let Some(s) = self.explorer.state_mut(id) {
                    let object = if with_object { r.object.as_ref() } else { None };
                    s.toggle(&r.key, object, &core);
                }
            }
            _ => {}
        }
        cx.notify();
    }

    /// Up / Down, across connections.
    fn tree_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        let rows = self.explorer.rows();
        if let Some(ix) = crate::explorer::step(&rows, self.explorer.cursor.as_ref(), delta) {
            self.tree_set_cursor(&rows, ix, cx);
        }
    }

    /// Right: expand a collapsed node, or step into an expanded one.
    fn tree_expand(&mut self, cx: &mut Context<Self>) {
        let rows = self.explorer.rows();
        let Some(ix) = self.tree_cursor(&rows) else {
            return self.tree_move(0, cx);
        };
        match rows[ix].caret {
            "▸" => self.tree_toggle(&rows[ix], true, cx),
            "▾" if rows.get(ix + 1).is_some_and(|n| n.depth > rows[ix].depth) => {
                self.tree_set_cursor(&rows, ix + 1, cx);
            }
            _ => {}
        }
    }

    /// Left: collapse an expanded node, else go to the parent.
    fn tree_collapse(&mut self, cx: &mut Context<Self>) {
        let rows = self.explorer.rows();
        let Some(ix) = self.tree_cursor(&rows) else {
            return self.tree_move(0, cx);
        };
        let r = &rows[ix];
        let expanded = match &r.conn {
            None => self.explorer.favorites_open(),
            Some(id) => self
                .explorer
                .state(id)
                .is_some_and(|s| s.expanded.contains(&r.key)),
        };
        if r.caret == "▾" && expanded {
            return self.tree_toggle(r, true, cx);
        }
        if let Some(parent) = crate::explorer::parent(&rows, ix) {
            self.tree_set_cursor(&rows, parent, cx);
        }
    }

    /// Enter: reveal a pin, run what a double-click does (open a relation's data), or
    /// toggle a node.
    fn tree_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.explorer.rows();
        let Some(ix) = self.tree_cursor(&rows) else {
            return;
        };
        let r = rows[ix].clone();
        if let Some(fav) = r.fav {
            return self.reveal_favorite(fav, window, cx);
        }
        match obj_ref(&r) {
            Some(o) => {
                if let Some(action) = default_action(o.kind) {
                    self.object_action(action, o, window, cx);
                }
            }
            None if !r.caret.is_empty() => self.tree_toggle(&r, false, cx),
            None => {}
        }
    }

    /// Ctrl+C / Cmd+C: copy the selected object's qualified name, or a child row's name
    /// (column, key, index, …).
    fn tree_copy(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.explorer.rows();
        let Some(ix) = self.tree_cursor(&rows) else {
            return;
        };
        if let Some(leaf) = rows[ix].leaf.clone() {
            cx.write_to_clipboard(ClipboardItem::new_string(leaf.clone()));
            self.toast(format!("Copied {leaf}"), cx);
        } else if let Some(o) = obj_ref(&rows[ix]) {
            self.object_action("copy", o, window, cx);
        }
    }

    /// F5: reload the selected node from the server, on its own connection (a pin
    /// reloads its folder; no cursor reloads the connection in scope).
    fn tree_refresh(&mut self, cx: &mut Context<Self>) {
        let rows = self.explorer.rows();
        let Some(r) = self.tree_cursor(&rows).map(|ix| rows[ix].clone()) else {
            return self.refresh_explorer(cx);
        };
        let Some(id) = r.conn.clone() else { return };
        let key = match (&r.fav, &r.object) {
            (Some(_), Some((s, n, k))) => format!("o:{s}:{k:?}:{n}"),
            (Some(_), None) => format!("s:{}", r.label),
            (None, _) if r.key.starts_with("fav:") => return,
            (None, _) => r.key.clone(),
        };
        let owner = r.owner.clone().or_else(|| r.object.clone());
        let core = self.core.clone();
        if let Some(s) = self.explorer.state_mut(&id) {
            s.refresh_node(&key, owner.as_ref(), &core);
        }
        cx.notify();
    }

    /// Ctrl+D / Cmd+D: pin or unpin the selected object or schema (unpin on a pin row).
    fn tree_pin(&mut self, cx: &mut Context<Self>) {
        let rows = self.explorer.rows();
        let Some(r) = self.tree_cursor(&rows).map(|ix| rows[ix].clone()) else {
            return;
        };
        if let Some(id) = r.fav {
            self.core.send(Command::RemoveFavorite { id });
            self.toast(format!("Unpinned {}", r.label), cx);
            return;
        }
        let Some(conn) = r.conn.clone() else { return };
        if let Some(o) = obj_ref(&r) {
            return self.toggle_pin(&CtxTarget::Object(o), cx);
        }
        if let Some(schema) = r.key.strip_prefix("s:") {
            self.toggle_pin(&CtxTarget::Schema(conn, schema.to_owned()), cx);
        }
    }

    /// The pin a context-menu target would add, and whether it is pinned already.
    pub(crate) fn pin_of(&self, target: &CtxTarget) -> Option<(Favorite, Option<i64>)> {
        let fav = match target {
            CtxTarget::Object(o) => self.explorer.pin_for_object(o),
            CtxTarget::Schema(conn, schema) => self.explorer.pin_for_schema(conn, schema),
            _ => return None,
        };
        let id = self.explorer.find_pin(&fav);
        Some((fav, id))
    }

    /// Pin, or unpin when pinned, an object or schema (context menu, Ctrl/Cmd+D).
    pub(crate) fn toggle_pin(&mut self, target: &CtxTarget, cx: &mut Context<Self>) {
        let Some((fav, id)) = self.pin_of(target) else {
            return;
        };
        let label = match fav.kind {
            Some(_) if !fav.schema.is_empty() => format!("{}.{}", fav.schema, fav.name),
            Some(_) => fav.name.clone(),
            None => fav.schema.clone(),
        };
        match id {
            Some(id) => {
                self.core.send(Command::RemoveFavorite { id });
                self.toast(format!("Unpinned {label}"), cx);
            }
            None => {
                self.core.send(Command::AddFavorite(fav));
                self.toast(format!("Pinned {label}"), cx);
            }
        }
    }

    /// A pin was clicked: reveal it in its connection's tree, connecting and expanding
    /// as needed; it is selected once its folder has loaded.
    pub(crate) fn reveal_favorite(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(f) = self
            .explorer
            .favorites()
            .iter()
            .find(|f| f.id == id)
            .cloned()
        else {
            return;
        };
        let Some(conn) = self.profiles.db(&f.connection_id).cloned() else {
            self.toast("The connection of this pin no longer exists", cx);
            return;
        };
        if !self.explorer.filter().is_empty() {
            self.schema_search
                .update(cx, |i, cx| i.set_value("", window, cx));
            let core = self.core.clone();
            self.explorer.filter_changed(String::new(), &core);
        }
        let core = self.core.clone();
        self.explorer.ensure(conn);
        self.explorer.reveal = self.explorer.expand_to(&f, &core);
        self.explorer.try_reveal();
        cx.notify();
    }

    fn render_schema_row(
        &self,
        r: &TreeRow,
        i: usize,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let row = r.clone();
        let row2 = r.clone();
        let selected = match &self.explorer.cursor {
            Some(c) => c.is(r),
            None => obj_ref(r).is_some() && self.explorer.selected == obj_ref(r),
        };
        let active_node = r.is_node() && r.conn.is_some() && self.explorer.active == r.conn;
        let focus = self.explorer.focus.clone();
        let engine = r
            .conn
            .as_ref()
            .and_then(|c| self.explorer.connection(c))
            .map(|c| c.engine);
        // Child rows (columns, …) drag their own name; object rows their qualified name.
        // Into a tab on another connection, a child row drops qualified by its relation.
        let drag = engine.and_then(|engine| {
            let d = dialect_for(engine);
            let qualified_of = |(schema, name, kind): &(String, String, ObjectKind)| {
                object_name_text(d, schema, name, *kind)
            };
            match (&r.leaf, &r.owner, &r.object) {
                (Some(leaf), owner, _) => Some(DraggedObject {
                    qualified: leaf.clone(),
                    full: owner
                        .as_ref()
                        .map_or_else(|| leaf.clone(), |o| format!("{}.{leaf}", qualified_of(o))),
                    conn: r.conn.clone(),
                }),
                (None, _, Some(o)) => Some(DraggedObject {
                    qualified: qualified_of(o),
                    full: qualified_of(o),
                    conn: r.conn.clone(),
                }),
                _ => None,
            }
        });
        let has_caret = !r.caret.is_empty();
        let caret_row = r.clone();
        let caret_focus = self.explorer.focus.clone();
        let caret_object = r.object.is_some() && r.fav.is_none();
        div()
            .id(("schema-row", i))
            .when_some(drag, |d, drag| {
                d.on_drag(drag, |d: &DraggedObject, _, _, cx| {
                    let label = d.qualified.clone();
                    cx.new(|_| crate::files_tab::DragPreview(label))
                })
            })
            .w_full()
            .h(rpx(26.))
            .flex()
            .items_center()
            .gap(rpx(7.))
            .pl(rpx(8. + r.depth as f32 * 14.))
            .pr(rpx(10.))
            .text_size(ts::UI)
            .text_color(if r.dim { p.fg2 } else { p.fg })
            .when(r.depth == 0, |d| d.font_weight(FontWeight::SEMIBOLD))
            .when(active_node, |d| d.border_l_2().border_color(p.acc))
            .when(selected, |d| d.bg(p.sel))
            .hover(|s| s.bg(p.hover))
            .on_click(cx.listener(move |this, ev: &gpui_kit::ClickEvent, w, cx| {
                this.explorer.cursor = Some(RowId::of(&row));
                if let Some(f) = &focus {
                    w.focus(f, cx);
                }
                if let Some(fav) = row.fav {
                    this.reveal_favorite(fav, w, cx);
                } else if let Some(o) = obj_ref(&row) {
                    this.explorer.selected = Some(o.clone());
                    if ev.click_count() >= 2
                        && let Some(action) = default_action(o.kind)
                    {
                        this.object_action(action, o, w, cx);
                    }
                } else if has_caret {
                    this.tree_toggle(&row, false, cx);
                }
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                    let r = &row2;
                    let target = if let Some(o) = obj_ref(r) {
                        this.explorer.selected = Some(o.clone());
                        this.explorer.cursor = Some(RowId::of(r));
                        Some(CtxTarget::Object(o))
                    } else if r.is_node() || (r.key.starts_with("fav:") && r.fav.is_none()) {
                        // A connection's node (or its Favorites group): its connection menu.
                        r.conn.clone().map(CtxTarget::Profile)
                    } else if let (Some(conn), Some(schema)) = (
                        r.conn.clone(),
                        schema_of_row_key(&r.key).or_else(|| {
                            // A pinned schema.
                            r.fav.map(|_| r.label.to_string())
                        }),
                    ) {
                        Some(CtxTarget::Schema(conn, schema))
                    } else {
                        None
                    };
                    if let Some(t) = target {
                        this.ctx = Some(CtxMenu::new(ev.position, t));
                        cx.notify();
                    }
                }),
            )
            .child(
                div()
                    .w(rpx(10.))
                    .flex_none()
                    .text_color(p.fg3)
                    .text_size(ts::TINY)
                    // An object row's caret expands its children; the rest of the row
                    // selects it (double-click opens the data).
                    .when(has_caret && caret_object, |d| {
                        d.on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, w, cx| {
                                cx.stop_propagation();
                                this.explorer.cursor = Some(RowId::of(&caret_row));
                                if let Some(f) = &caret_focus {
                                    w.focus(f, cx);
                                }
                                this.tree_toggle(&caret_row, true, cx);
                            }),
                        )
                    })
                    .child(r.caret),
            )
            .child(
                div()
                    .w(rpx(14.))
                    .flex_none()
                    .font_family(MONO)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(ts::CAPTION)
                    .text_color(p.fg3)
                    .child(r.icon.clone()),
            )
            .when_some(r.env, |d, e| d.child(ui::dot(p.env(e), 6.)))
            .child(div().flex_1().min_w_0().truncate().child(r.label.clone()))
            .when(r.loading, |d| d.child(ui::shimmer(64., p)))
            .when(!r.loading, |d| {
                d.child(
                    div()
                        .font_family(MONO)
                        .font_weight(FontWeight::NORMAL)
                        .text_size(ts::SMALL)
                        .text_color(p.fg3)
                        // Long descriptions (job status, role attributes) give way to the name.
                        .flex_shrink(1.)
                        .min_w_0()
                        .max_w(gpui_kit::relative(0.6))
                        .truncate()
                        .child(r.sub.clone()),
                )
            })
            .into_any_element()
    }
}

/// The object a row shows, on the row's connection.
pub(crate) fn obj_ref(r: &TreeRow) -> Option<ObjRef> {
    let (schema, name, kind) = r.object.clone()?;
    Some(ObjRef {
        conn: r.conn.clone()?,
        schema,
        name,
        kind,
    })
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
                // A document store's row is its document: show that, not the flattened
                // columns.
                let document = cols.iter().find_map(|(n, v, _)| match v {
                    Value::Json(s) if n == switchyard_core::db::batch::DOCUMENT_COLUMN => {
                        serde_json::from_str::<serde_json::Value>(s).ok()
                    }
                    _ => None,
                });
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
                let json = serde_json::to_string_pretty(
                    &document.unwrap_or(serde_json::Value::Object(obj)),
                )
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
            .w(rpx(self.inspector_width))
            .relative()
            .child(
                // Drag the left edge to resize.
                div()
                    .id("insp-resize")
                    .absolute()
                    .left(rpx(-3.))
                    .top_0()
                    .bottom_0()
                    .w(rpx(6.))
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
                    .h(rpx(34.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(rpx(8.))
                    .pl(rpx(12.))
                    .pr(rpx(10.))
                    .border_b_1()
                    .border_color(p.bd)
                    .child(
                        div()
                            .text_size(ts::BODY)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Value viewer"),
                    )
                    .when_some(row_no, |d, r| {
                        d.child(
                            div()
                                .font_family(MONO)
                                .text_size(ts::SMALL)
                                .text_color(p.fg3)
                                .child(format!("row {r}")),
                        )
                    })
                    .child(div().flex_1())
                    .child({
                        let wide = self.inspector_width > INSPECTOR_WIDTH + 1.;
                        div()
                            .id("insp-expand")
                            .px(rpx(6.))
                            .py(rpx(2.))
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
                            .px(rpx(6.))
                            .py(rpx(2.))
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
                    .px(rpx(10.))
                    .py(rpx(8.))
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
                    .px(rpx(12.))
                    .pt(rpx(4.))
                    .pb(rpx(12.))
                    .font_family(MONO)
                    .text_size(ts::BODY)
                    .line_height(rpx(19.))
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
                    .px(rpx(12.))
                    .py(rpx(8.))
                    .border_t_1()
                    .border_color(p.bd)
                    .text_size(ts::SMALL)
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
                            .pt(rpx(6.))
                            .flex()
                            .justify_center()
                            .child(
                                gpui_kit::img(img)
                                    .max_w(rpx(self.inspector_width - 24.))
                                    .max_h(rpx(420.)),
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
    use switchyard_core::db::{ColumnInfo, IndexInfo};

    fn column(name: &str, ordinal: i32, pk: bool) -> ColumnInfo {
        ColumnInfo {
            schema: "s".into(),
            table: "t".into(),
            name: name.into(),
            data_type: "int".into(),
            nullable: !pk,
            default: None,
            ordinal,
            is_primary_key: pk,
            ..ColumnInfo::default()
        }
    }

    fn detail(columns: Vec<ColumnInfo>, indexes: Vec<IndexInfo>) -> ObjectDetail {
        ObjectDetail {
            object: ObjectInfo {
                schema: "s".into(),
                name: "t".into(),
                kind: ObjectKind::Table,
                estimated_rows: None,
                detail: None,
            },
            columns,
            indexes,
            constraints: Vec::new(),
            foreign_keys: Vec::new(),
            triggers: Vec::new(),
            ddl: String::new(),
            ..ObjectDetail::default()
        }
    }

    #[test]
    fn templates_use_table_order_and_the_primary_key() {
        let d = detail(
            vec![
                column("b", 2, false),
                column("id", 1, true),
                column("c", 3, false),
            ],
            Vec::new(),
        );
        let (cols, pk) = columns_and_key(&d);
        assert_eq!(cols, ["id", "b", "c"]);
        assert_eq!(pk, ["id"]);
        // Engines that do not flag key columns fall back to the primary index.
        let d = detail(
            vec![column("a", 1, false)],
            vec![IndexInfo {
                name: "pk".into(),
                columns: vec!["a".into()],
                is_unique: true,
                is_primary: true,
                definition: String::new(),
                ..IndexInfo::default()
            }],
        );
        assert_eq!(columns_and_key(&d).1, ["a"]);
    }

    #[test]
    fn pending_detail_actions_are_kept_until_their_object_arrives() {
        let mut s = SchemaState::default();
        let t = ObjectKind::Table;
        let p = |action: &str, name: &str, kind: ObjectKind, routine: bool| PendingDetail {
            action: action.into(),
            schema: "s".into(),
            name: name.into(),
            kind,
            routine,
        };
        s.pending_detail.push(p("select", "t", t, false));
        s.pending_detail.push(p("ddl", "u", t, false));
        s.pending_detail.push(p("insert", "t", t, false));
        let f = ObjectKind::Function;
        s.pending_detail.push(p("ddl", "f", f, false));
        s.pending_detail.push(p("script_exec", "f", f, true));
        assert_eq!(s.take_pending("s", "t", t, false), ["select", "insert"]);
        assert!(s.take_pending("s", "t", t, false).is_empty());
        assert_eq!(s.take_pending("s", "u", t, false), ["ddl"]);
        // A routine definition and a Detail of the same routine are told apart.
        assert_eq!(s.take_pending("s", "f", f, true), ["script_exec"]);
        assert_eq!(s.take_pending("s", "f", f, false), ["ddl"]);
    }

    #[test]
    fn script_actions_on_routines_need_the_routine_definition() {
        assert!(needs_routine("script_exec", ObjectKind::Procedure));
        assert!(needs_routine("script_create", ObjectKind::Function));
        assert!(!needs_routine("script_create", ObjectKind::Table));
        assert!(!needs_routine("script_drop", ObjectKind::Procedure));
        assert!(!needs_routine("ddl", ObjectKind::Function));
    }

    fn summary(groups: &[ChildGroup]) -> Vec<String> {
        groups
            .iter()
            .flat_map(|g| {
                std::iter::once(format!("[{}]", g.label)).chain(
                    g.items
                        .iter()
                        .map(|(icon, label, sub, _)| format!("{icon} {label} | {sub}")),
                )
            })
            .collect()
    }

    #[test]
    fn child_groups_of_a_table() {
        let mut d = detail(
            vec![
                column("customer_id", 2, false),
                column("id", 1, true),
                column("note", 3, false),
            ],
            vec![IndexInfo {
                name: "orders_customer_ix".into(),
                columns: vec!["customer_id".into()],
                is_unique: false,
                is_primary: false,
                definition: String::new(),
                ..Default::default()
            }],
        );
        d.constraints.push(switchyard_core::db::ConstraintInfo {
            name: "orders_pkey".into(),
            kind: "PRIMARY KEY".into(),
            definition: "PRIMARY KEY (id)".into(),
        });
        d.constraints.push(switchyard_core::db::ConstraintInfo {
            name: "orders_note_check".into(),
            kind: "CHECK".into(),
            definition: "CHECK (note <> '')".into(),
        });
        d.foreign_keys.push(switchyard_core::db::ForeignKeyInfo {
            name: "orders_customer_fk".into(),
            columns: vec!["customer_id".into()],
            references: "public.customers".into(),
            referenced_columns: vec!["id".into()],
            ..Default::default()
        });
        d.triggers.push("orders_audit".into());
        assert_eq!(
            summary(&child_groups(&d)),
            [
                "[Columns]",
                "PK id | int · not null",
                "FK customer_id | int · null",
                "· note | int · null",
                "[Keys]",
                "PK orders_pkey | (id)",
                "[Indexes]",
                "IX orders_customer_ix | (customer_id)",
                "[Foreign keys]",
                "FK orders_customer_fk (customer_id) | → public.customers",
                "[Triggers]",
                "TR orders_audit | ",
            ]
        );
    }

    #[test]
    fn views_show_only_their_columns_and_index_keys_count() {
        let mut d = detail(vec![column("a", 1, false)], Vec::new());
        d.object.kind = ObjectKind::View;
        assert_eq!(
            summary(&child_groups(&d)),
            ["[Columns]", "· a | int · null"]
        );
        // SQL Server and Oracle report the key only as the primary index.
        let d = detail(
            vec![column("a", 1, false)],
            vec![IndexInfo {
                name: "PK_t".into(),
                columns: vec!["a".into()],
                is_unique: true,
                is_primary: true,
                definition: String::new(),
                ..Default::default()
            }],
        );
        let s = summary(&child_groups(&d));
        assert!(s.contains(&"PK PK_t | (a)".to_owned()), "{s:?}");
        assert!(s.contains(&"PK a | int · null".to_owned()), "{s:?}");
        assert!(s.contains(&"IX PK_t | (a) · unique".to_owned()), "{s:?}");
    }

    #[test]
    fn expanded_relation_rows_load_then_list_groups() {
        let mut s = SchemaState::default();
        let owner = ("s".to_owned(), "t".to_owned(), ObjectKind::Table);
        let quote = |n: &str| format!("\"{n}\"");
        let mut rows = Vec::new();
        relation_rows(&s, &owner, "o:s:Table:t", 3, &quote, &mut rows);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].loading);
        let d = detail(vec![column("id", 1, true)], Vec::new());
        s.store_detail(
            "s",
            "t",
            ObjectKind::Table,
            &Ok(CatalogChunk::Detail(Box::new(d))),
        );
        let gkey = "o:s:Table:t\u{1f}columns".to_owned();
        s.expanded.insert(gkey.clone());
        let mut rows = Vec::new();
        relation_rows(&s, &owner, "o:s:Table:t", 3, &quote, &mut rows);
        let keys: Vec<(&str, usize, Option<&str>)> = rows
            .iter()
            .map(|r| (r.label.as_ref(), r.depth, r.leaf.as_deref()))
            .collect();
        assert_eq!(keys, [("Columns", 4, None), ("id", 5, Some("\"id\""))]);
        assert!(rows.iter().all(|r| r.owner.as_ref() == Some(&owner)));
        // A failed load shows the error under the relation.
        s.store_detail("s", "t", ObjectKind::Table, &Err("gone".into()));
        let mut rows = Vec::new();
        relation_rows(&s, &owner, "o:s:Table:t", 3, &quote, &mut rows);
        assert_eq!(rows[0].label.as_ref(), "gone");
    }

    /// DBX-5c: a database-level folder (`f::<Kind>`) lists objects without a schema,
    /// with their description on the right, and shows a hint instead of an error.
    #[test]
    fn server_level_folders_list_and_hint() {
        let mut s = SchemaState::default();
        let quote = |n: &str| format!("\"{n}\"");
        s.expanded.insert("f::Role".into());
        s.on_catalog(
            IntrospectScope::Objects {
                schema: String::new(),
                kind: ObjectKind::Role,
            },
            Ok(CatalogChunk::Objects(vec![ObjectInfo {
                name: "app".into(),
                kind: ObjectKind::Role,
                detail: Some("user · create db".into()),
                ..ObjectInfo::default()
            }])),
            1,
        );
        s.on_catalog(
            IntrospectScope::Objects {
                schema: String::new(),
                kind: ObjectKind::Job,
            },
            Ok(CatalogChunk::Hint("needs msdb".into())),
            1,
        );
        s.expanded.insert("f::Job".into());
        let mut rows = Vec::new();
        folder_rows(&s, "", ObjectKind::Role, 1, "", &quote, &mut rows);
        folder_rows(&s, "", ObjectKind::Job, 1, "", &quote, &mut rows);
        let got: Vec<(&str, &str, &str, usize)> = rows
            .iter()
            .map(|r| (r.key.as_str(), r.label.as_ref(), r.sub.as_ref(), r.depth))
            .collect();
        assert_eq!(
            got,
            [
                ("f::Role", "Users & roles", "1", 1),
                ("o::Role:app", "app", "user · create db", 2),
                ("f::Job", "SQL Agent jobs", "", 1),
                ("hint::Job", "needs msdb", "", 2),
            ]
        );
        assert_eq!(
            rows[1].object,
            Some((String::new(), "app".into(), ObjectKind::Role))
        );
        // A filter that matches nothing hides the folder.
        let mut rows = Vec::new();
        folder_rows(&s, "", ObjectKind::Role, 1, "zz", &quote, &mut rows);
        assert!(rows.is_empty());
        // No ER diagram (schema menu) for a database-level folder.
        assert_eq!(schema_of_row_key("f::Role"), None);
        assert_eq!(schema_of_row_key("f:sales:Table"), Some("sales".into()));
        assert_eq!(
            s.folder_of("f::Role"),
            Some((String::new(), ObjectKind::Role))
        );
    }

    #[test]
    fn read_only_kinds_refuse_changes() {
        for k in [
            ObjectKind::Role,
            ObjectKind::Job,
            ObjectKind::Package,
            ObjectKind::Task,
        ] {
            for a in [
                "drop",
                "truncate",
                "script_drop",
                "script_drop_create",
                "open",
                "insert",
            ] {
                assert!(!action_allowed(a, k), "{a} on {k:?}");
            }
            assert!(action_allowed("ddl", k) && action_allowed("copy", k));
        }
        assert!(action_allowed("script_create", ObjectKind::Role));
        assert!(!action_allowed("script_create", ObjectKind::Job));
        assert!(!action_allowed("properties", ObjectKind::Role));
        assert!(action_allowed("properties", ObjectKind::Package));
        assert!(action_allowed("drop", ObjectKind::Table));
        assert_eq!(default_action(ObjectKind::Table), Some("open"));
        assert_eq!(default_action(ObjectKind::Extension), Some("ddl"));
        assert_eq!(default_action(ObjectKind::Function), None);
        assert!(needs_routine("script_create", ObjectKind::Package));
        let d = dialect_for(Engine::SqlServer);
        assert_eq!(
            object_name_text(d, "", "app user", ObjectKind::Role),
            "[app user]"
        );
        assert_eq!(
            object_name_text(d, "dbo", "my t", ObjectKind::Table),
            d.qualified("dbo", "my t")
        );
    }

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

#[cfg(test)]
mod folder_tests {
    use super::*;

    #[test]
    fn hosts_group_by_folder() {
        let mk = |name: &str, folder: Option<&str>| {
            let mut h = Host::new(name, "a", "u");
            h.folder = folder.map(str::to_owned);
            h
        };
        let hosts = [
            mk("a", None),
            mk("b", Some("prod")),
            mk("c", Some(" ")),
            mk("d", Some("Dev")),
            mk("e", Some("prod")),
        ];
        let (loose, folders) = folder_groups(hosts.iter());
        assert_eq!(
            loose.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(),
            ["a", "c"]
        );
        let summary: Vec<(String, Vec<&str>)> = folders
            .into_iter()
            .map(|(f, hs)| (f, hs.iter().map(|h| h.name.as_str()).collect()))
            .collect();
        assert_eq!(
            summary,
            [
                ("Dev".to_owned(), vec!["d"]),
                ("prod".to_owned(), vec!["b", "e"])
            ]
        );
    }
}
