//! The multi-connection schema explorer (DBX-5e, SSMS Object Explorer style).
//!
//! Every database connection the user expanded or connected shows as a top-level node
//! with its own [`SchemaState`] and catalog session. A node opens its session on the first
//! expand, keeps its cache when collapsed, and comes back "not connected" after a restart
//! (the node list is saved as the `explorer.connections` setting). A Favorites section on
//! top lists pinned objects grouped by connection. All rows of all connections are
//! flattened into one list for the sidebar's single virtualized `uniform_list`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gpui_kit::{FocusHandle, ScrollStrategy, UniformListScrollHandle};
use switchyard_core::db::ObjectKind;
use switchyard_core::store::{DbConnection, Favorite, ProfileId};
use switchyard_core::{Command, RuntimeHandle, SessionId};

use crate::sidebar::{Loadable, SchemaState, TreeRow, connection_rows};

/// Where explorer state sends its commands: the core runtime, or a recorder in tests.
pub trait CoreSink {
    /// Send a command to core.
    fn send(&self, command: Command);
}

impl CoreSink for RuntimeHandle {
    fn send(&self, command: Command) {
        RuntimeHandle::send(self, command);
    }
}

/// Setting key holding the explorer's connection nodes (ids, in order).
pub const SAVED_NODES_KEY: &str = "explorer.connections";

/// A schema object on a given connection.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ObjRef {
    /// Connection.
    pub conn: ProfileId,
    /// Schema; empty for a server-level object.
    pub schema: String,
    /// Name.
    pub name: String,
    /// Kind.
    pub kind: ObjectKind,
}

/// Identifies a tree row across connections: its connection and its key there.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RowId {
    /// Connection; `None` for the Favorites header.
    pub conn: Option<ProfileId>,
    /// Row key within the connection (or the Favorites section).
    pub key: String,
}

impl RowId {
    /// The id of `row`.
    pub fn of(row: &TreeRow) -> Self {
        Self {
            conn: row.conn.clone(),
            key: row.key.clone(),
        }
    }

    /// The node row of connection `id`.
    pub fn node(id: &ProfileId) -> Self {
        Self {
            conn: Some(id.clone()),
            key: "db".into(),
        }
    }

    /// Whether `row` is this row.
    pub fn is(&self, row: &TreeRow) -> bool {
        self.key == row.key && self.conn == row.conn
    }
}

/// Whether a pinned object still exists, as far as the loaded catalog tells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinStatus {
    /// Its folder (or the schema list) is not loaded yet.
    Unknown,
    /// Found in its loaded folder.
    Present,
    /// Its folder loaded without it (dropped or renamed). Never removed automatically.
    Missing,
    /// Pinned in another database than the one the connection browses now.
    OtherDatabase,
    /// Its connection profile was deleted.
    NoConnection,
}

/// The explorer: connection nodes, the tree cursor and the Favorites.
pub struct Explorer {
    /// Connection nodes in display order (saved as [`SAVED_NODES_KEY`]). A restored id
    /// whose profile has not loaded yet has no state until it does.
    order: Vec<ProfileId>,
    /// Per-connection tree state and catalog session.
    conns: HashMap<ProfileId, SchemaState>,
    /// Database profiles, to name pins and create nodes.
    profiles: HashMap<ProfileId, DbConnection>,
    profiles_loaded: bool,
    /// Row under the keyboard cursor.
    pub cursor: Option<RowId>,
    /// Last selected object.
    pub selected: Option<ObjRef>,
    /// Tree filter text.
    filter: String,
    /// The connection the filter searches (fixed while the filter is not empty).
    search_conn: Option<ProfileId>,
    /// Connection of the active SQL tab (its node is highlighted).
    pub active: Option<ProfileId>,
    /// Pinned objects in order.
    favorites: Vec<Favorite>,
    /// The Favorites section is expanded.
    favorites_open: bool,
    /// Until the saved node list arrives at startup: nodes added meanwhile do not
    /// connect, and nothing is saved.
    pub restoring: bool,
    /// The node list last saved.
    saved: Vec<ProfileId>,
    /// A row being revealed (a clicked pin): selected once it appears.
    pub reveal: Option<RowId>,
    /// Scroll position of the tree.
    pub scroll: UniformListScrollHandle,
    /// Focus of the tree, for its key bindings (`SchemaTree` context).
    pub focus: Option<FocusHandle>,
    /// [`Self::rows`] as last built. Every `&mut` access that can change the tree (node
    /// state, node list, profiles, pins, filter) clears it, so a render that follows an
    /// unrelated core event reuses it instead of rebuilding and re-matching every row.
    rows_cache: RefCell<Option<Rc<[TreeRow]>>>,
}

impl Default for Explorer {
    fn default() -> Self {
        Self {
            order: Vec::new(),
            conns: HashMap::new(),
            profiles: HashMap::new(),
            profiles_loaded: false,
            cursor: None,
            selected: None,
            filter: String::new(),
            search_conn: None,
            active: None,
            favorites: Vec::new(),
            favorites_open: true,
            restoring: true,
            saved: Vec::new(),
            reveal: None,
            scroll: UniformListScrollHandle::default(),
            focus: None,
            rows_cache: RefCell::new(None),
        }
    }
}

impl Explorer {
    /// The state of connection `id`, if it is a node.
    pub fn state(&self, id: &ProfileId) -> Option<&SchemaState> {
        self.conns.get(id)
    }

    /// The state of connection `id`, if it is a node. Invalidates the cached rows.
    pub fn state_mut(&mut self, id: &ProfileId) -> Option<&mut SchemaState> {
        self.invalidate();
        self.conns.get_mut(id)
    }

    /// Drop the cached [`Self::rows`]; the next call rebuilds them.
    fn invalidate(&mut self) {
        *self.rows_cache.get_mut() = None;
    }

    /// Pinned objects in order.
    pub fn favorites(&self) -> &[Favorite] {
        &self.favorites
    }

    /// Whether the Favorites section is expanded.
    pub fn favorites_open(&self) -> bool {
        self.favorites_open
    }

    /// Expand or collapse the Favorites section.
    pub fn toggle_favorites(&mut self) {
        self.invalidate();
        self.favorites_open = !self.favorites_open;
    }

    /// The tree filter text.
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Whether connection `id` is a node.
    pub fn contains(&self, id: &ProfileId) -> bool {
        self.conns.contains_key(id)
    }

    /// The node whose catalog session is `session`.
    pub fn conn_of_session(&self, session: SessionId) -> Option<ProfileId> {
        self.conns
            .iter()
            .find(|(_, s)| s.session == Some(session))
            .map(|(id, _)| id.clone())
    }

    /// The profile of connection `id` (the node's copy when the profiles are not known).
    pub fn connection(&self, id: &ProfileId) -> Option<&DbConnection> {
        self.profiles
            .get(id)
            .or_else(|| self.conns.get(id).and_then(|s| s.connection.as_ref()))
    }

    /// Add a node for `conn` (collapsed, not connected) unless it has one. Returns
    /// whether it was added.
    pub fn ensure(&mut self, conn: DbConnection) -> bool {
        self.invalidate();
        let id = conn.id.clone();
        if !self.order.contains(&id) {
            self.order.push(id.clone());
        }
        if self.conns.contains_key(&id) {
            return false;
        }
        self.profiles
            .entry(id.clone())
            .or_insert_with(|| conn.clone());
        self.conns.insert(id, SchemaState::new(conn));
        true
    }

    /// Add a node for `conn` if needed and make sure it is expanded and connected.
    pub fn ensure_connected(&mut self, conn: DbConnection, core: &dyn CoreSink) {
        self.invalidate();
        let id = conn.id.clone();
        self.ensure(conn);
        if let Some(s) = self.conns.get_mut(&id) {
            if s.session.is_none() {
                s.connect(core);
            }
            s.expanded.insert("db".into());
        }
    }

    /// Remove the node of `id` and close its catalog session.
    pub fn remove(&mut self, id: &ProfileId, core: &dyn CoreSink) {
        self.invalidate();
        self.order.retain(|o| o != id);
        if let Some(mut s) = self.conns.remove(id) {
            s.disconnect(core);
        }
        if self
            .cursor
            .as_ref()
            .is_some_and(|c| c.conn.as_ref() == Some(id))
        {
            self.cursor = None;
        }
        if self.search_conn.as_ref() == Some(id) {
            self.search_conn = None;
        }
        if self.selected.as_ref().is_some_and(|o| &o.conn == id) {
            self.selected = None;
        }
    }

    /// The database profiles (re)loaded: create restored nodes, refresh each node's copy
    /// of its profile, and drop nodes whose profile was deleted.
    pub fn set_profiles(
        &mut self,
        dbs: impl IntoIterator<Item = DbConnection>,
        core: &dyn CoreSink,
    ) {
        self.invalidate();
        self.profiles = dbs.into_iter().map(|d| (d.id.clone(), d)).collect();
        self.profiles_loaded = true;
        for id in self.order.clone() {
            match self.profiles.get(&id).cloned() {
                Some(conn) => match self.conns.get_mut(&id) {
                    Some(s) => s.connection = Some(conn),
                    None => {
                        self.conns.insert(id, SchemaState::new(conn));
                    }
                },
                None => self.remove(&id, core),
            }
        }
    }

    /// The saved node list arrived at startup: add its nodes, not connected.
    pub fn restore(&mut self, ids: Vec<ProfileId>) {
        self.invalidate();
        self.restoring = false;
        self.saved = ids.clone();
        for id in ids {
            if self.order.contains(&id) {
                continue;
            }
            match self.profiles.get(&id).cloned() {
                Some(conn) => {
                    self.ensure(conn);
                }
                // Profiles are not known yet: [`Self::set_profiles`] creates it.
                None if !self.profiles_loaded => self.order.push(id),
                None => {}
            }
        }
    }

    /// The node list to save, when it changed since the last save (never while the
    /// saved list is still loading).
    pub fn take_save(&mut self) -> Option<Vec<ProfileId>> {
        if self.restoring || self.order == self.saved {
            return None;
        }
        self.saved = self.order.clone();
        Some(self.order.clone())
    }

    /// Every row: the Favorites section (hidden while filtering), then each node. Built
    /// once and shared until something that changes the tree invalidates it.
    pub fn rows(&self) -> Rc<[TreeRow]> {
        if let Some(rows) = self.rows_cache.borrow().as_ref() {
            return rows.clone();
        }
        let rows: Rc<[TreeRow]> = self.build_rows().into();
        *self.rows_cache.borrow_mut() = Some(rows.clone());
        rows
    }

    /// Build [`Self::rows`] from scratch.
    fn build_rows(&self) -> Vec<TreeRow> {
        let mut rows = Vec::new();
        if self.filter.is_empty() && !self.favorites.is_empty() {
            self.favorite_rows(&mut rows);
        }
        for id in &self.order {
            let Some(s) = self.conns.get(id) else {
                continue;
            };
            let filtered_out = !self.filter.is_empty() && self.search_conn.as_ref() != Some(id);
            connection_rows(s, filtered_out, &mut rows);
        }
        rows
    }

    /// The Favorites header and, when open, the pins grouped by connection.
    fn favorite_rows(&self, rows: &mut Vec<TreeRow>) {
        rows.push(TreeRow {
            caret: if self.favorites_open { "▾" } else { "▸" },
            icon: "★".into(),
            label: "Favorites".into(),
            sub: self.favorites.len().to_string().into(),
            dim: false,
            ..TreeRow::new(0, "favorites".into())
        });
        if !self.favorites_open {
            return;
        }
        // Groups in the order of each connection's first pin.
        let mut groups: Vec<&ProfileId> = Vec::new();
        for f in &self.favorites {
            if !groups.contains(&&f.connection_id) {
                groups.push(&f.connection_id);
            }
        }
        for id in groups {
            let conn = self.connection(id);
            rows.push(TreeRow {
                icon: conn.map_or("DB", |c| c.engine.badge()).into(),
                label: conn
                    .map_or_else(|| "Deleted connection".to_owned(), |c| c.name.clone())
                    .into(),
                env: conn.map(|c| c.environment),
                conn: Some(id.clone()),
                ..TreeRow::new(1, format!("fav:{}", id.0))
            });
            for f in self.favorites.iter().filter(|f| &f.connection_id == id) {
                rows.push(self.pin_row(f));
            }
        }
    }

    /// The row of one pin.
    fn pin_row(&self, f: &Favorite) -> TreeRow {
        let status = self.pin_status(f);
        let (icon, label, kind_label) = match f.kind {
            None => ("S", f.schema.clone(), "schema"),
            Some(k) => (
                k.icon(),
                if f.schema.is_empty() {
                    f.name.clone()
                } else {
                    format!("{}.{}", f.schema, f.name)
                },
                crate::object_search::kind_label(k),
            ),
        };
        let sub = match status {
            PinStatus::Missing => "missing".to_owned(),
            PinStatus::NoConnection => "no connection".to_owned(),
            PinStatus::OtherDatabase => format!("in {}", f.database),
            PinStatus::Unknown | PinStatus::Present => kind_label.to_owned(),
        };
        TreeRow {
            icon: icon.into(),
            label: label.into(),
            sub: sub.into(),
            dim: !matches!(status, PinStatus::Unknown | PinStatus::Present),
            object: f.kind.map(|k| (f.schema.clone(), f.name.clone(), k)),
            conn: Some(f.connection_id.clone()),
            fav: Some(f.id),
            ..TreeRow::new(2, format!("fav:{}", f.id))
        }
    }

    /// Whether the object `f` pins still exists, from what its node has loaded.
    pub fn pin_status(&self, f: &Favorite) -> PinStatus {
        if self.profiles_loaded && !self.profiles.contains_key(&f.connection_id) {
            return PinStatus::NoConnection;
        }
        if let Some(c) = self.connection(&f.connection_id)
            && c.database != f.database
        {
            return PinStatus::OtherDatabase;
        }
        let Some(s) = self.conns.get(&f.connection_id) else {
            return PinStatus::Unknown;
        };
        let found = match f.kind {
            None => match &s.schemas {
                Loadable::Loaded(list) => list.iter().any(|x| x.name == f.schema),
                _ => return PinStatus::Unknown,
            },
            Some(k) => {
                let folder = (f.schema.clone(), k);
                // A folder answered with a hint (missing privilege) tells nothing.
                if s.hints.contains_key(&folder) {
                    return PinStatus::Unknown;
                }
                match s.objects.get(&folder) {
                    Some(Loadable::Loaded(list)) => list.iter().any(|o| o.name == f.name),
                    _ => return PinStatus::Unknown,
                }
            }
        };
        if found {
            PinStatus::Present
        } else {
            PinStatus::Missing
        }
    }

    /// A pin of object `o` (in its connection's current database).
    pub fn pin_for_object(&self, o: &ObjRef) -> Favorite {
        let db = self.database_of(&o.conn);
        Favorite::object(o.conn.clone(), &db, &o.schema, &o.name, o.kind)
    }

    /// A pin of `schema` on connection `conn`.
    pub fn pin_for_schema(&self, conn: &ProfileId, schema: &str) -> Favorite {
        Favorite::schema(conn.clone(), &self.database_of(conn), schema)
    }

    fn database_of(&self, conn: &ProfileId) -> String {
        self.connection(conn)
            .map(|c| c.database.clone())
            .unwrap_or_default()
    }

    /// The stored pin of the same object as `f`, if any.
    pub fn find_pin(&self, f: &Favorite) -> Option<i64> {
        self.favorites
            .iter()
            .find(|p| p.same_target(f))
            .map(|p| p.id)
    }

    /// The pins arrived (load, pin, unpin): load the folders they need on open nodes,
    /// so a pin of a dropped object shows as missing.
    pub fn set_favorites(&mut self, list: Vec<Favorite>, core: &dyn CoreSink) {
        self.invalidate();
        self.favorites = list;
        let ids: Vec<ProfileId> = self.conns.keys().cloned().collect();
        for id in ids {
            self.load_pin_folders(&id, core);
        }
    }

    /// Load the folders of the pins on node `id` once its session is open.
    fn load_pin_folders(&mut self, id: &ProfileId, core: &dyn CoreSink) {
        self.invalidate();
        let Some(s) = self.conns.get_mut(id) else {
            return;
        };
        if !s.is_open() {
            return;
        }
        let db = s.connection.as_ref().map(|c| c.database.clone());
        for f in &self.favorites {
            if &f.connection_id == id
                && Some(&f.database) == db.as_ref()
                && let Some(k) = f.kind
            {
                s.ensure_folder(&f.schema, k, core);
            }
        }
    }

    /// A catalog session opened: the node loads its schemas, waiting requests and the
    /// folders of its pins. Returns the node.
    pub fn on_open(
        &mut self,
        session: SessionId,
        version: String,
        core: &dyn CoreSink,
    ) -> Option<ProfileId> {
        self.invalidate();
        let id = self.conn_of_session(session)?;
        self.conns.get_mut(&id)?.on_open(version, core);
        self.load_pin_folders(&id, core);
        Some(id)
    }

    /// The connection a search, F5 or the palette acts on: the selected row's node, else
    /// the selected object's, else the active tab's, else the first connected node.
    pub fn scope_conn(&self) -> Option<ProfileId> {
        let known = |c: &ProfileId| self.conns.contains_key(c);
        self.cursor
            .as_ref()
            .and_then(|c| c.conn.clone())
            .filter(known)
            .or_else(|| self.selected.as_ref().map(|o| o.conn.clone()).filter(known))
            .or_else(|| self.active.clone().filter(known))
            .or_else(|| {
                self.order
                    .iter()
                    .find(|id| self.conns.get(*id).is_some_and(|s| s.session.is_some()))
                    .cloned()
            })
            .or_else(|| self.order.iter().find(|id| known(id)).cloned())
    }

    /// The filter text changed: it searches one connection ([`Self::scope_conn`] when it
    /// starts, kept while it is not empty). Returns that connection and the ticket for
    /// [`Self::search_due`] when a debounced server search should follow.
    pub fn filter_changed(
        &mut self,
        filter: String,
        core: &dyn CoreSink,
    ) -> Option<(ProfileId, u64)> {
        self.invalidate();
        let scope = if filter.is_empty() {
            None
        } else {
            self.search_conn
                .clone()
                .filter(|c| self.conns.contains_key(c))
                .or_else(|| self.scope_conn())
        };
        if let Some(old) = self.search_conn.take()
            && Some(&old) != scope.as_ref()
            && let Some(s) = self.conns.get_mut(&old)
        {
            s.filter_changed(String::new(), core);
        }
        self.filter = filter.clone();
        self.search_conn = scope.clone();
        let id = scope?;
        let ticket = self.conns.get_mut(&id)?.filter_changed(filter, core)?;
        Some((id, ticket))
    }

    /// A search debounce elapsed on node `id`.
    pub fn search_due(&mut self, id: &ProfileId, ticket: u64, core: &dyn CoreSink) {
        self.invalidate();
        if let Some(s) = self.conns.get_mut(id) {
            s.search_due(ticket, core);
        }
    }

    /// Expand the tree down to what `f` pins (connecting its node when needed) and
    /// return the row to reveal once it loads. The node must exist ([`Self::ensure`]).
    pub fn expand_to(&mut self, f: &Favorite, core: &dyn CoreSink) -> Option<RowId> {
        self.invalidate();
        let s = self.conns.get_mut(&f.connection_id)?;
        if s.session.is_none()
            || matches!(s.state.0, Some(crate::app_state::SessionState::Failed(_)))
        {
            s.connect(core);
        }
        s.expanded.insert("db".into());
        let key = match f.kind {
            None => format!("s:{}", f.schema),
            Some(k) => {
                if !f.schema.is_empty() {
                    s.expanded.insert(format!("s:{}", f.schema));
                }
                s.expanded.insert(format!("f:{}:{k:?}", f.schema));
                s.ensure_folder(&f.schema, k, core);
                format!("o:{}:{k:?}:{}", f.schema, f.name)
            }
        };
        Some(RowId {
            conn: Some(f.connection_id.clone()),
            key,
        })
    }

    /// Select the row being revealed if it is there now. Gives up once its folder (or
    /// the schema list) loaded without it. Returns its index, to scroll to.
    pub fn try_reveal(&mut self) -> Option<usize> {
        let target = self.reveal.clone()?;
        let rows = self.rows();
        if let Some(ix) = rows.iter().position(|r| target.is(r)) {
            self.cursor = Some(target);
            self.selected = rows[ix].object.clone().zip(rows[ix].conn.clone()).map(
                |((schema, name, kind), conn)| ObjRef {
                    conn,
                    schema,
                    name,
                    kind,
                },
            );
            self.reveal = None;
            self.scroll.scroll_to_item(ix, ScrollStrategy::Center);
            return Some(ix);
        }
        let s = target.conn.as_ref().and_then(|c| self.conns.get(c))?;
        let settled = match target.key.strip_prefix("o:") {
            Some(rest) => s.objects.iter().any(|((schema, kind), l)| {
                rest.starts_with(&format!("{schema}:{kind:?}:")) && matches!(l, Loadable::Loaded(_))
            }),
            None => matches!(s.schemas, Loadable::Loaded(_)),
        };
        let failed = matches!(s.state.0, Some(crate::app_state::SessionState::Failed(_)));
        if settled || failed {
            self.reveal = None;
        }
        None
    }

    /// Scroll row `id` into view (no-op when it is not shown).
    pub fn scroll_to(&self, id: &RowId) {
        if let Some(ix) = self.rows().iter().position(|r| id.is(r)) {
            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        }
    }

    /// Ask node `o.conn` for the detail (or routine definition) of `o`, for `action`.
    /// Connects the node first when it is not connected. Returns `false` when the
    /// connection is unknown.
    pub fn request_detail(
        &mut self,
        action: &str,
        o: &ObjRef,
        routine: bool,
        core: &dyn CoreSink,
    ) -> bool {
        self.invalidate();
        let Some(conn) = self.connection(&o.conn).cloned() else {
            return false;
        };
        if self.conns.get(&o.conn).is_none_or(|s| s.session.is_none()) {
            self.ensure_connected(conn, core);
        }
        self.conns
            .get_mut(&o.conn)
            .is_some_and(|s| s.request_detail(action, &o.schema, &o.name, o.kind, routine, core))
    }

    /// The cached detail of `o` on its own node.
    pub fn cached_detail(&self, o: &ObjRef) -> Option<&switchyard_core::db::ObjectDetail> {
        self.conns
            .get(&o.conn)?
            .cached_detail(&o.schema, &o.name, o.kind)
    }
}

/// The row `delta` steps from `cursor` (Up / Down), across connection boundaries.
pub fn step(rows: &[TreeRow], cursor: Option<&RowId>, delta: isize) -> Option<usize> {
    if rows.is_empty() {
        return None;
    }
    let at = cursor.and_then(|c| rows.iter().position(|r| c.is(r)));
    Some(match at {
        Some(i) => i.saturating_add_signed(delta).min(rows.len() - 1),
        None => 0,
    })
}

/// The parent row of row `ix` (Left on a collapsed row): the nearest row above it with
/// a smaller depth, which never crosses into another connection's tree.
pub fn parent(rows: &[TreeRow], ix: usize) -> Option<usize> {
    let r = rows.get(ix)?;
    rows[..ix].iter().rposition(|p| p.depth < r.depth)
}

/// Whether a template for an object on `conn` goes into the active tab: only when that
/// tab is on the same connection and database (else it opens a tab on `conn`).
pub fn template_into_active(
    conn: &DbConnection,
    active: Option<(&ProfileId, Option<String>)>,
) -> bool {
    let database = Some(conn.database.clone()).filter(|d| !d.is_empty());
    active.is_some_and(|(id, db)| *id == conn.id && db == database)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use switchyard_core::db::{CatalogChunk, Engine, IntrospectScope, ObjectInfo, SchemaInfo};
    use switchyard_core::store::EnvironmentLabel;

    use super::*;

    /// Records the commands sent to core.
    #[derive(Default)]
    struct Sink(RefCell<Vec<Command>>);

    impl CoreSink for Sink {
        fn send(&self, command: Command) {
            self.0.borrow_mut().push(command);
        }
    }

    impl Sink {
        fn take(&self) -> Vec<Command> {
            std::mem::take(&mut *self.0.borrow_mut())
        }
    }

    fn conn(name: &str, env: EnvironmentLabel) -> DbConnection {
        let mut c = DbConnection::new(name, Engine::Postgres);
        c.database = "shop".into();
        c.environment = env;
        c
    }

    /// Open the node of `c` and load one user schema with a Tables folder.
    fn open_with_tables(ex: &mut Explorer, c: &DbConnection, tables: &[&str], sink: &Sink) {
        ex.ensure_connected(c.clone(), sink);
        let s = ex.state_mut(&c.id).unwrap();
        let session = s.session.unwrap();
        ex.on_open(session, "PostgreSQL 16.2".into(), sink);
        let s = ex.state_mut(&c.id).unwrap();
        s.on_catalog(
            IntrospectScope::Schemas,
            Ok(CatalogChunk::Schemas(vec![SchemaInfo {
                name: "public".into(),
                is_system: false,
            }])),
            1,
        );
        s.expanded.insert("f:public:Table".into());
        s.on_catalog(
            IntrospectScope::Objects {
                schema: "public".into(),
                kind: ObjectKind::Table,
            },
            Ok(CatalogChunk::Objects(
                tables
                    .iter()
                    .map(|t| ObjectInfo {
                        schema: "public".into(),
                        name: (*t).into(),
                        kind: ObjectKind::Table,
                        ..ObjectInfo::default()
                    })
                    .collect(),
            )),
            1,
        );
    }

    fn summary(rows: &[TreeRow], names: &HashMap<ProfileId, &str>) -> Vec<String> {
        rows.iter()
            .map(|r| {
                let c = r.conn.as_ref().map_or("-", |c| names[c]);
                format!("{c} {} {} {}", r.depth, r.key, r.label)
            })
            .collect()
    }

    #[test]
    fn nodes_connect_on_expand_and_keep_their_cache() {
        let sink = Sink::default();
        let mut ex = Explorer::default();
        let a = conn("a", EnvironmentLabel::Development);
        assert!(ex.ensure(a.clone()));
        assert!(!ex.ensure(a.clone()));
        assert!(sink.take().is_empty(), "adding a node opens nothing");
        let rows = ex.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sub.as_ref(), "not connected");
        assert_eq!(rows[0].env, Some(EnvironmentLabel::Development));

        // The first expand opens the session; requests wait until it is open.
        let s = ex.state_mut(&a.id).unwrap();
        s.toggle("db", None, &sink);
        let session = s.session.unwrap();
        assert!(matches!(&sink.take()[..], [Command::OpenSession { .. }]));
        s.toggle("f::Extension", None, &sink);
        assert!(sink.take().is_empty(), "queued while connecting");
        ex.on_open(session, "PostgreSQL 16".into(), &sink);
        let sent = sink.take();
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert!(
            sent.iter()
                .all(|c| matches!(c, Command::Introspect { session: s, .. } if *s == session))
        );

        // Collapse keeps the session and what loaded.
        let s = ex.state_mut(&a.id).unwrap();
        s.on_catalog(
            IntrospectScope::Schemas,
            Ok(CatalogChunk::Schemas(Vec::new())),
            1,
        );
        s.toggle("db", None, &sink);
        assert!(sink.take().is_empty());
        assert!(s.session.is_some() && matches!(s.schemas, Loadable::Loaded(_)));

        // Disconnect closes the session; the node reads "not connected" again.
        s.disconnect(&sink);
        assert!(
            matches!(&sink.take()[..], [Command::CloseSession { session: x }] if *x == session)
        );
        assert_eq!(ex.rows()[0].sub.as_ref(), "not connected");
        assert!(matches!(
            ex.state(&a.id).unwrap().schemas,
            Loadable::Loaded(_)
        ));
    }

    #[test]
    fn rows_flatten_every_connection_with_its_id() {
        let sink = Sink::default();
        let mut ex = Explorer::default();
        let (a, b) = (
            conn("a", EnvironmentLabel::Production),
            conn("b", EnvironmentLabel::Development),
        );
        open_with_tables(&mut ex, &a, &["orders"], &sink);
        ex.ensure(b.clone());
        let names: HashMap<ProfileId, &str> = [(a.id.clone(), "A"), (b.id.clone(), "B")].into();
        let rows = ex.rows();
        assert_eq!(
            summary(&rows, &names),
            [
                "A 0 db a",
                "A 1 s:public public",
                "A 2 f:public:Table Tables",
                "A 3 o:public:Table:orders orders",
                "A 2 f:public:View Views",
                "A 2 f:public:MaterializedView Materialized views",
                "A 2 f:public:Function Functions",
                "A 2 f:public:Procedure Procedures",
                "A 2 f:public:Sequence Sequences",
                "A 2 f:public:Type Types",
                "A 1 f::Role Users & roles",
                "A 1 f::Extension Extensions",
                "B 0 db b",
            ]
        );
        assert_eq!(rows[0].env, Some(EnvironmentLabel::Production));
        assert_eq!(rows[0].sub.as_ref(), "pg 16");
        assert!(rows[0].is_node() && rows[12].is_node());
        assert_eq!(rows[12].env, Some(EnvironmentLabel::Development));
    }

    #[test]
    fn rows_are_cached_until_the_tree_changes() {
        let sink = Sink::default();
        let mut ex = Explorer::default();
        let a = conn("a", EnvironmentLabel::Development);
        open_with_tables(&mut ex, &a, &["orders"], &sink);
        let first = ex.rows();
        // Renders after unrelated events share the rows instead of rebuilding them.
        assert!(Rc::ptr_eq(&first, &ex.rows()));
        ex.cursor = Some(RowId::of(&first[1]));
        ex.active = Some(a.id.clone());
        assert!(Rc::ptr_eq(&first, &ex.rows()));
        // Collapsing the node goes through `state_mut` and rebuilds.
        if let Some(s) = ex.state_mut(&a.id) {
            s.expanded.remove("db");
        }
        let collapsed = ex.rows();
        assert!(!Rc::ptr_eq(&first, &collapsed));
        assert_eq!(collapsed.len(), 1);
        // So do the filter, pins and the node list.
        ex.filter_changed("ord".into(), &sink);
        assert!(!Rc::ptr_eq(&collapsed, &ex.rows()));
        let filtered = ex.rows();
        ex.filter_changed(String::new(), &sink);
        assert!(!Rc::ptr_eq(&filtered, &ex.rows()));
        let before = ex.rows();
        ex.toggle_favorites();
        assert!(!Rc::ptr_eq(&before, &ex.rows()));
        let before = ex.rows();
        ex.ensure(conn("b", EnvironmentLabel::Development));
        assert_eq!(ex.rows().len(), before.len() + 1);
    }

    #[test]
    fn keyboard_moves_across_connection_boundaries() {
        let sink = Sink::default();
        let mut ex = Explorer::default();
        let (a, b) = (
            conn("a", EnvironmentLabel::Development),
            conn("b", EnvironmentLabel::Development),
        );
        open_with_tables(&mut ex, &a, &["orders"], &sink);
        open_with_tables(&mut ex, &b, &["users"], &sink);
        let rows = ex.rows();
        let last_a = rows
            .iter()
            .rposition(|r| r.conn.as_ref() == Some(&a.id))
            .unwrap();
        // Down from A's last row lands on B's node, Up goes back.
        let at = RowId::of(&rows[last_a]);
        let down = step(&rows, Some(&at), 1).unwrap();
        assert!(rows[down].is_node() && rows[down].conn.as_ref() == Some(&b.id));
        assert_eq!(step(&rows, Some(&RowId::of(&rows[down])), -1), Some(last_a));
        // Left from B's table goes up to B's folder, schema and node; never into A.
        let users = rows
            .iter()
            .position(|r| r.key == "o:public:Table:users")
            .unwrap();
        let folder = parent(&rows, users).unwrap();
        let schema = parent(&rows, folder).unwrap();
        let node = parent(&rows, schema).unwrap();
        assert_eq!(
            [
                &rows[folder].key[..],
                &rows[schema].key[..],
                &rows[node].key[..]
            ],
            ["f:public:Table", "s:public", "db"]
        );
        assert_eq!(rows[node].conn.as_ref(), Some(&b.id));
        assert_eq!(parent(&rows, node), None);
        // Clamped at both ends; no cursor starts at the top.
        assert_eq!(step(&rows, None, 1), Some(0));
        let end = RowId::of(rows.last().unwrap());
        assert_eq!(step(&rows, Some(&end), 5), Some(rows.len() - 1));
    }

    #[test]
    fn search_is_scoped_to_the_selected_connection() {
        let sink = Sink::default();
        let mut ex = Explorer::default();
        let (a, b) = (
            conn("a", EnvironmentLabel::Development),
            conn("b", EnvironmentLabel::Development),
        );
        open_with_tables(&mut ex, &a, &["orders"], &sink);
        open_with_tables(&mut ex, &b, &["users", "user_roles"], &sink);
        ex.active = Some(a.id.clone());
        // The cursor (selected row) wins over the active tab.
        ex.cursor = Some(RowId::node(&b.id));
        sink.take();
        let (id, ticket) = ex.filter_changed("user".into(), &sink).unwrap();
        assert_eq!(id, b.id);
        assert_eq!(ex.state(&b.id).unwrap().filter, "user");
        assert_eq!(ex.state(&a.id).unwrap().filter, "");
        let rows = ex.rows();
        let a_rows: Vec<&TreeRow> = rows
            .iter()
            .filter(|r| r.conn.as_ref() == Some(&a.id))
            .collect();
        assert_eq!(a_rows.len(), 1, "only A's node while B is searched");
        let hits: Vec<&str> = rows
            .iter()
            .filter(|r| r.key.starts_with("q:"))
            .map(|r| r.label.as_ref())
            .collect();
        assert_eq!(hits, ["public.users", "public.user_roles"]);
        // The server search runs on B's session only.
        let b_session = ex.state(&b.id).unwrap().session.unwrap();
        // Typing loads B's remaining folders for the local filter, never A's.
        let loads = sink.take();
        assert!(!loads.is_empty());
        assert!(
            loads
                .iter()
                .all(|c| matches!(c, Command::Introspect { session, .. } if *session == b_session))
        );
        ex.search_due(&b.id, ticket, &sink);
        assert!(matches!(
            &sink.take()[..],
            [Command::Introspect { session, scope: IntrospectScope::Search { .. }, .. }] if *session == b_session
        ));
        // Moving the cursor while typing keeps the scope; clearing the filter ends it.
        ex.cursor = Some(RowId::node(&a.id));
        assert_eq!(ex.filter_changed("users".into(), &sink).unwrap().0, b.id);
        ex.filter_changed(String::new(), &sink);
        assert_eq!(ex.search_conn, None);
        assert_eq!(ex.state(&b.id).unwrap().filter, "");
        // Next search follows the cursor to A.
        ex.filter_changed("or".into(), &sink);
        assert_eq!(ex.search_conn, Some(a.id.clone()));
        // Without a cursor or selection the active tab's connection is searched.
        ex.filter_changed(String::new(), &sink);
        ex.cursor = None;
        ex.active = Some(b.id.clone());
        ex.filter_changed("x".into(), &sink);
        assert_eq!(ex.search_conn, Some(b.id));
    }

    #[test]
    fn pins_resolve_and_missing_ones_stay() {
        let sink = Sink::default();
        let mut ex = Explorer::default();
        let (a, b) = (
            conn("a", EnvironmentLabel::Development),
            conn("b", EnvironmentLabel::Production),
        );
        ex.set_profiles([a.clone(), b.clone()], &sink);
        open_with_tables(&mut ex, &a, &["orders"], &sink);
        let pin = |id: i64, f: Favorite| Favorite {
            id,
            position: id,
            ..f
        };
        let orders = pin(
            1,
            Favorite::object(a.id.clone(), "shop", "public", "orders", ObjectKind::Table),
        );
        let gone = pin(
            2,
            Favorite::object(a.id.clone(), "shop", "public", "old", ObjectKind::Table),
        );
        let view = pin(
            3,
            Favorite::object(a.id.clone(), "shop", "public", "v", ObjectKind::View),
        );
        let other_db = pin(
            4,
            Favorite::object(a.id.clone(), "hr", "public", "x", ObjectKind::Table),
        );
        let schema = pin(5, Favorite::schema(a.id.clone(), "shop", "public"));
        let on_b = pin(
            6,
            Favorite::object(b.id.clone(), "shop", "", "app", ObjectKind::Role),
        );
        let deleted = pin(7, Favorite::schema(ProfileId("gone".into()), "", "s"));
        sink.take();
        ex.set_favorites(
            vec![
                orders.clone(),
                gone.clone(),
                view.clone(),
                other_db.clone(),
                schema.clone(),
                on_b.clone(),
                deleted.clone(),
            ],
            &sink,
        );
        // The open node loads the View folder its pin needs (from the cache).
        let sent = sink.take();
        assert!(sent.iter().any(|c| matches!(
            c,
            Command::Introspect {
                scope: IntrospectScope::Objects {
                    kind: ObjectKind::View,
                    ..
                },
                refresh: false,
                ..
            }
        )));
        assert_eq!(ex.pin_status(&orders), PinStatus::Present);
        assert_eq!(ex.pin_status(&gone), PinStatus::Missing);
        assert_eq!(
            ex.pin_status(&view),
            PinStatus::Unknown,
            "folder still loading"
        );
        assert_eq!(ex.pin_status(&other_db), PinStatus::OtherDatabase);
        assert_eq!(ex.pin_status(&schema), PinStatus::Present);
        assert_eq!(ex.pin_status(&on_b), PinStatus::Unknown, "B is not a node");
        assert_eq!(ex.pin_status(&deleted), PinStatus::NoConnection);

        let rows = ex.rows();
        let fav: Vec<(usize, &str, &str, bool)> = rows
            .iter()
            .take_while(|r| !r.is_node())
            .map(|r| (r.depth, r.label.as_ref(), r.sub.as_ref(), r.dim))
            .collect();
        assert_eq!(
            fav,
            [
                (0, "Favorites", "7", false),
                (1, "a", "", true),
                (2, "public.orders", "table", false),
                (2, "public.old", "missing", true),
                (2, "public.v", "view", false),
                (2, "public.x", "in hr", true),
                (2, "public", "schema", false),
                (1, "b", "", true),
                (2, "app", "user / role", false),
                (1, "Deleted connection", "", true),
                (2, "s", "no connection", true),
            ]
        );
        // Pin rows carry their connection and object, for menus, drag and copy.
        assert_eq!(rows[3].fav, Some(2));
        assert_eq!(rows[3].conn.as_ref(), Some(&a.id));
        assert_eq!(
            rows[8].object,
            Some((String::new(), "app".into(), ObjectKind::Role))
        );
        // Filtering hides the section; collapsing it keeps only the header.
        ex.toggle_favorites();
        assert_eq!(ex.rows().iter().filter(|r| r.fav.is_some()).count(), 0);
        // Same object, same pin; another kind is another pin.
        let o = ObjRef {
            conn: a.id.clone(),
            schema: "public".into(),
            name: "orders".into(),
            kind: ObjectKind::Table,
        };
        assert_eq!(ex.find_pin(&ex.pin_for_object(&o)), Some(1));
        let as_view = ObjRef {
            kind: ObjectKind::View,
            ..o
        };
        assert_eq!(ex.find_pin(&ex.pin_for_object(&as_view)), None);
        assert_eq!(ex.find_pin(&ex.pin_for_schema(&a.id, "public")), Some(5));
    }

    #[test]
    fn clicking_a_pin_expands_and_reveals_it() {
        let sink = Sink::default();
        let mut ex = Explorer::default();
        let b = conn("b", EnvironmentLabel::Development);
        ex.set_profiles([b.clone()], &sink);
        let f = Favorite {
            id: 9,
            ..Favorite::object(b.id.clone(), "shop", "sales", "t", ObjectKind::Table)
        };
        ex.favorites = vec![f.clone()];
        ex.ensure(b.clone());
        ex.reveal = ex.expand_to(&f, &sink);
        assert!(
            matches!(&sink.take()[..], [Command::OpenSession { .. }]),
            "connects first"
        );
        assert_eq!(ex.try_reveal(), None, "not loaded yet");
        let session = ex.state(&b.id).unwrap().session.unwrap();
        ex.on_open(session, "PostgreSQL 16".into(), &sink);
        let s = ex.state_mut(&b.id).unwrap();
        s.on_catalog(
            IntrospectScope::Schemas,
            Ok(CatalogChunk::Schemas(vec![SchemaInfo {
                name: "sales".into(),
                is_system: false,
            }])),
            1,
        );
        assert!(s.expanded.contains("s:sales") && s.expanded.contains("f:sales:Table"));
        s.on_catalog(
            IntrospectScope::Objects {
                schema: "sales".into(),
                kind: ObjectKind::Table,
            },
            Ok(CatalogChunk::Objects(vec![ObjectInfo {
                schema: "sales".into(),
                name: "t".into(),
                kind: ObjectKind::Table,
                ..ObjectInfo::default()
            }])),
            1,
        );
        let ix = ex.try_reveal().unwrap();
        let rows = ex.rows();
        assert_eq!(rows[ix].key, "o:sales:Table:t");
        assert_eq!(ex.cursor, Some(RowId::of(&rows[ix])));
        assert_eq!(
            ex.selected.as_ref().map(|o| o.conn.clone()),
            Some(b.id.clone())
        );
        assert!(ex.reveal.is_none());
        // A missing pin's reveal gives up once its folder loaded.
        let gone = Favorite {
            name: "gone".into(),
            ..f
        };
        ex.reveal = ex.expand_to(&gone, &sink);
        assert_eq!(ex.try_reveal(), None);
        assert!(ex.reveal.is_none());
    }

    #[test]
    fn actions_go_to_the_nodes_connection_not_the_active_tab() {
        let sink = Sink::default();
        let mut ex = Explorer::default();
        let (a, b) = (
            conn("a", EnvironmentLabel::Development),
            conn("b", EnvironmentLabel::Development),
        );
        ex.set_profiles([a.clone(), b.clone()], &sink);
        open_with_tables(&mut ex, &a, &["orders"], &sink);
        open_with_tables(&mut ex, &b, &["users"], &sink);
        ex.active = Some(a.id.clone());
        sink.take();
        let o = ObjRef {
            conn: b.id.clone(),
            schema: "public".into(),
            name: "users".into(),
            kind: ObjectKind::Table,
        };
        assert!(ex.request_detail("select", &o, false, &sink));
        let b_session = ex.state(&b.id).unwrap().session.unwrap();
        assert!(matches!(
            &sink.take()[..],
            [Command::Introspect { session, scope: IntrospectScope::Detail { name, .. }, .. }] if *session == b_session && name == "users"
        ));
        // The answer comes back on B's session and finds B's pending action.
        assert_eq!(ex.conn_of_session(b_session), Some(b.id.clone()));
        let s = ex.state_mut(&b.id).unwrap();
        assert_eq!(
            s.take_pending("public", "users", ObjectKind::Table, false),
            ["select"]
        );
        assert!(
            ex.state_mut(&a.id)
                .unwrap()
                .take_pending("public", "users", ObjectKind::Table, false)
                .is_empty()
        );

        // A pin on a connection without a node connects it for the action.
        let c = conn("c", EnvironmentLabel::Development);
        ex.set_profiles([a.clone(), b.clone(), c.clone()], &sink);
        let oc = ObjRef {
            conn: c.id.clone(),
            ..o.clone()
        };
        assert!(ex.request_detail("ddl", &oc, false, &sink));
        assert!(
            matches!(&sink.take()[..], [Command::OpenSession { connection, .. }] if *connection == c.id)
        );
        assert!(ex.contains(&c.id));

        // Templates go into the active tab only when it is on the node's connection
        // and database.
        assert!(!template_into_active(
            &b,
            Some((&a.id, Some("shop".into())))
        ));
        assert!(template_into_active(&b, Some((&b.id, Some("shop".into())))));
        assert!(!template_into_active(&b, Some((&b.id, Some("hr".into())))));
        assert!(!template_into_active(&b, None));
    }

    #[test]
    fn restored_nodes_stay_disconnected_and_are_saved() {
        let sink = Sink::default();
        let mut ex = Explorer::default();
        let (a, b) = (
            conn("a", EnvironmentLabel::Development),
            conn("b", EnvironmentLabel::Development),
        );
        // A tab's connection added before the saved list arrives is not saved over it.
        ex.ensure(a.clone());
        assert_eq!(ex.take_save(), None);
        ex.restore(vec![b.id.clone(), ProfileId("gone".into())]);
        assert_eq!(ex.order.len(), 3);
        ex.set_profiles([a.clone(), b.clone()], &sink);
        assert_eq!(
            ex.order,
            [a.id.clone(), b.id.clone()],
            "deleted profile dropped"
        );
        assert!(
            sink.take()
                .iter()
                .all(|c| !matches!(c, Command::OpenSession { .. }))
        );
        assert!(ex.rows().iter().all(|r| r.sub.as_ref() == "not connected"));
        assert_eq!(ex.take_save(), Some(vec![a.id.clone(), b.id.clone()]));
        assert_eq!(ex.take_save(), None);
        ex.remove(&a.id, &sink);
        assert_eq!(ex.take_save(), Some(vec![b.id]));
    }

    #[test]
    fn a_drop_on_another_connection_inserts_the_qualified_name() {
        let (a, b) = (ProfileId("a".into()), ProfileId("b".into()));
        let d = crate::sidebar::DraggedObject {
            qualified: "\"id\"".into(),
            full: "\"public\".\"t\".\"id\"".into(),
            conn: Some(a.clone()),
        };
        assert_eq!(d.text_for(Some(&a)), "\"id\"");
        assert_eq!(d.text_for(Some(&b)), "\"public\".\"t\".\"id\"");
        assert_eq!(d.text_for(None), "\"public\".\"t\".\"id\"");
    }
}
