//! Object properties tab (DBX-2b): one table, view or materialized view with its columns,
//! indexes, constraints, foreign keys, triggers, DDL and a first page of data.
//!
//! The tab owns a catalog/query session on its connection. `Detail` comes back through
//! the workspace's event routing ([`ObjectTab::on_catalog`]); the Data page streams into
//! the same [`GridDelegate`] the SQL tab uses, paged, filtered and sorted on the server by
//! a [`Pager`] bar (DBX-3a); a foreign-key cell opens the referenced row (DBX-3c). Every
//! value can be copied: tables have a per-row Copy, the DDL and trigger sources sit in
//! read-only editors.
//!
//! DBX-5a adds a Dependencies page (what the object uses and what uses it; double-click
//! opens one), and the tab also shows non-relations (routines, sequences, packages,
//! Snowflake tasks …) with only their Dependencies and DDL pages.

use std::sync::Arc;

use std::rc::Rc;

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Editor, EditorState, InputEvent};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::table::{DataTable, TableEvent, TableState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Entity, EventEmitter, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, px, relative,
    uniform_list,
};
use switchyard_core::db::{
    CatalogChunk, ColumnMeta, Dependencies, DependencyInfo, Dialect, ForeignKeyInfo,
    IntrospectScope, ObjectDetail, ObjectKind, TriggerInfo, dialect_for,
};
use switchyard_core::store::DbConnection;
use switchyard_core::{
    Command, FetchLimit, QueryEvent, QueryId, RuntimeHandle, SessionId, StatementRequest,
};

use crate::app_state::{SessionState, next_id};
use crate::grid::{
    GridDelegate, PagedView, Pager, PagerAction, pager_action, reference_filter, render_pager,
};
use crate::theme::{MONO, Palette, palette};
use crate::ui::{self, Kind, thousands};
use crate::workspace::{Tab, Workspace};

/// Height of one row in the property tables.
const ROW_H: f32 = 26.;

/// What the tab asks the workspace to do.
pub enum ObjectTabEvent {
    /// Open the properties of another object on the same connection (a foreign key's
    /// referenced table).
    OpenObject {
        /// Schema.
        schema: String,
        /// Name.
        name: String,
        /// Kind.
        kind: ObjectKind,
    },
    /// Show a toast.
    Toast(String),
    /// Open the data view of a table filtered by `filter` (a foreign key's referenced row).
    OpenData {
        /// Schema.
        schema: String,
        /// Table.
        name: String,
        /// WHERE condition.
        filter: String,
    },
}

/// The pages of the tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    /// Columns.
    Columns,
    /// Indexes.
    Indexes,
    /// Constraints.
    Constraints,
    /// Foreign keys.
    ForeignKeys,
    /// Triggers.
    Triggers,
    /// Uses / used by (DBX-5a).
    Dependencies,
    /// DDL.
    Ddl,
    /// First rows.
    Data,
}

impl Page {
    const RELATION: [Page; 8] = [
        Page::Columns,
        Page::Indexes,
        Page::Constraints,
        Page::ForeignKeys,
        Page::Triggers,
        Page::Dependencies,
        Page::Ddl,
        Page::Data,
    ];

    /// The pages of an object of `kind`: every page for a relation, Dependencies and DDL
    /// for anything else; Dependencies only where the engine lists them (`deps`).
    pub fn for_kind(kind: ObjectKind, deps: bool) -> Vec<Page> {
        let deps = deps && kind.has_dependencies();
        let all: &[Page] = if kind.is_relation() {
            &Page::RELATION
        } else {
            &[Page::Dependencies, Page::Ddl]
        };
        all.iter()
            .copied()
            .filter(|p| deps || *p != Page::Dependencies)
            .collect()
    }

    fn label(self) -> &'static str {
        match self {
            Page::Columns => "Columns",
            Page::Indexes => "Indexes",
            Page::Constraints => "Constraints",
            Page::ForeignKeys => "Foreign keys",
            Page::Triggers => "Triggers",
            Page::Dependencies => "Dependencies",
            Page::Ddl => "DDL",
            Page::Data => "Data",
        }
    }
}

/// Which property table a row belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GridRef {
    /// Columns, Indexes, Constraints or Foreign keys (index into `grids`).
    Detail(usize),
    /// Uses (0) or Used by (1).
    Deps(usize),
}

/// A property table: headers, column widths (the last one stretches) and text cells.
struct Grid {
    id: &'static str,
    headers: &'static [&'static str],
    widths: &'static [f32],
    rows: Arc<Vec<Vec<SharedString>>>,
    /// Column whose cells open the referenced object (foreign keys).
    link: Option<usize>,
    /// A double-click on a row opens its object (dependencies).
    open_on_double: bool,
}

/// The Data page: one page (`Dialect::select_page`) streamed into the SQL tab's grid.
#[derive(Default)]
struct DataPage {
    query: Option<QueryId>,
    columns: Vec<String>,
    table: Option<Entity<TableState<GridDelegate>>>,
    rows: usize,
    running: bool,
    error: Option<String>,
    /// Selected cell (view row, table column).
    selected: Option<(usize, usize)>,
    _sub: Option<Subscription>,
}

/// What the Data page's row menu does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DataAction {
    Reference,
    CopyCell,
}

/// An object properties tab.
pub struct ObjectTab {
    /// `<connection id>/<schema>.<name>/<kind>`: which object this tab shows.
    pub key: String,
    /// Tab title: `schema.name`.
    pub title: SharedString,
    /// The connection.
    pub connection: DbConnection,
    schema: String,
    name: String,
    kind: ObjectKind,
    core: RuntimeHandle,
    session: Option<SessionId>,
    session_state: SessionState,
    detail: Option<Box<ObjectDetail>>,
    loading: bool,
    error: Option<String>,
    page: Page,
    /// The pages this object shows ([`Page::for_kind`]).
    pages: Vec<Page>,
    grids: Vec<Grid>,
    /// Uses / used-by answer (loaded when the Dependencies page first shows).
    deps: Option<Box<Dependencies>>,
    deps_loading: bool,
    /// Uses and Used by tables of the Dependencies page.
    dep_grids: Vec<Grid>,
    trigger: usize,
    ddl_editor: Entity<EditorState>,
    trigger_editor: Entity<EditorState>,
    data: DataPage,
    /// Server-side filter, sort and paging of the Data page.
    pager: Pager,
    _pager_sub: Subscription,
}

impl EventEmitter<ObjectTabEvent> for ObjectTab {}

fn code_editor(window: &mut Window, cx: &mut Context<ObjectTab>) -> Entity<EditorState> {
    cx.new(|cx| {
        EditorState::new(window, cx)
            .language("sql")
            .line_number(true)
            .indent_guides(false)
            .soft_wrap(false)
    })
}

impl ObjectTab {
    /// The key of the tab showing `schema.name` on `connection`.
    pub fn key_for(
        connection: &DbConnection,
        schema: &str,
        name: &str,
        kind: ObjectKind,
    ) -> String {
        format!("{}/{schema}.{name}/{kind:?}", connection.id)
    }

    /// A tab for `schema.name` that opens its own session on `connection`.
    pub fn new(
        core: RuntimeHandle,
        connection: DbConnection,
        schema: String,
        name: String,
        kind: ObjectKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let session = next_id();
        core.send(Command::OpenSession {
            session,
            connection: connection.id.clone(),
        });
        let pager = Pager::new(schema.clone(), name.clone(), kind, None, window, cx);
        let pager_sub = cx.subscribe_in(
            &pager.where_input,
            window,
            |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    pager_action(this, PagerAction::Apply, window, cx);
                }
            },
        );
        let pages = Page::for_kind(kind, dialect_for(connection.engine).supports_dependencies());
        let page = pages.first().copied().unwrap_or(Page::Ddl);
        Self {
            key: Self::key_for(&connection, &schema, &name, kind),
            title: if schema.is_empty() {
                name.clone().into()
            } else {
                format!("{schema}.{name}").into()
            },
            connection,
            schema,
            name,
            kind,
            core,
            session: Some(session),
            session_state: SessionState::Connecting,
            detail: None,
            loading: true,
            error: None,
            page,
            pages,
            grids: Vec::new(),
            deps: None,
            deps_loading: false,
            dep_grids: Vec::new(),
            trigger: 0,
            ddl_editor: code_editor(window, cx),
            trigger_editor: code_editor(window, cx),
            data: DataPage::default(),
            pager,
            _pager_sub: pager_sub,
        }
    }

    /// Engine badge for the tab strip.
    pub fn badge(&self) -> &'static str {
        self.connection.engine.badge()
    }

    /// Whether this tab owns `session`.
    pub fn owns_session(&self, session: SessionId) -> bool {
        self.session == Some(session)
    }

    /// Whether this tab owns `query`.
    pub fn owns_query(&self, query: QueryId) -> bool {
        self.data.query == Some(query)
    }

    /// Close the session (the tab is closing).
    pub fn shutdown(&mut self) {
        if let Some(q) = self.data.query.take()
            && self.data.running
        {
            self.core.send(Command::Cancel { query: q });
        }
        if let Some(s) = self.session.take() {
            self.core.send(Command::CloseSession { session: s });
        }
    }

    /// Session lifecycle updates from the workspace.
    pub fn on_session(&mut self, state: SessionState, cx: &mut Context<Self>) {
        let opened = matches!(state, SessionState::Open { .. });
        if let SessionState::Failed(e) = &state {
            self.error = Some(e.clone());
            self.loading = false;
        }
        self.session_state = state;
        if opened {
            self.request_detail(false, cx);
            if self.page == Page::Data {
                self.run_data(cx);
            }
            if self.page == Page::Dependencies {
                self.request_deps(cx);
            }
        }
        cx.notify();
    }

    /// The scope that brings this object's DDL (and, for relations, everything else):
    /// the routine definition for functions, procedures and packages, else `Detail`.
    fn detail_scope(&self) -> IntrospectScope {
        let (schema, name, kind) = (self.schema.clone(), self.name.clone(), self.kind);
        if matches!(
            kind,
            ObjectKind::Function | ObjectKind::Procedure | ObjectKind::Package
        ) {
            IntrospectScope::RoutineDefinition {
                schema,
                name,
                kind,
                signature: None,
            }
        } else {
            IntrospectScope::Detail { schema, name, kind }
        }
    }

    fn request_detail(&mut self, refresh: bool, cx: &mut Context<Self>) {
        let Some(session) = self.session else { return };
        self.loading = true;
        self.core.send(Command::Introspect {
            session,
            scope: self.detail_scope(),
            refresh,
        });
        cx.notify();
    }

    /// Ask for the uses / used-by lists (never cached by core).
    fn request_deps(&mut self, cx: &mut Context<Self>) {
        let (Some(session), SessionState::Open { .. }) = (self.session, &self.session_state) else {
            return;
        };
        self.deps_loading = true;
        self.core.send(Command::Introspect {
            session,
            scope: IntrospectScope::Dependencies {
                schema: self.schema.clone(),
                name: self.name.clone(),
                kind: self.kind,
            },
            refresh: true,
        });
        cx.notify();
    }

    /// Show `page` (when this object has it).
    pub fn show_page(&mut self, page: Page, cx: &mut Context<Self>) {
        if self.pages.contains(&page) {
            self.set_page(page, cx);
        }
    }

    /// Re-read the detail from the server (and the Data page, when it was loaded).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.session_state, SessionState::Open { .. }) {
            if let SessionState::Failed(_) = self.session_state {
                // Reopen a failed session; the detail follows once it is open.
                if let Some(s) = self.session.take() {
                    self.core.send(Command::CloseSession { session: s });
                }
                let session = next_id();
                self.core.send(Command::OpenSession {
                    session,
                    connection: self.connection.id.clone(),
                });
                self.session = Some(session);
                self.session_state = SessionState::Connecting;
                self.error = None;
                self.loading = true;
                cx.notify();
            }
            return;
        }
        self.request_detail(true, cx);
        if self.data.table.is_some() || self.data.error.is_some() {
            self.run_data(cx);
        }
        if self.deps.is_some() || self.page == Page::Dependencies {
            self.request_deps(cx);
        }
    }

    /// A catalog answer on this tab's session.
    pub fn on_catalog(
        &mut self,
        scope: IntrospectScope,
        result: Result<CatalogChunk, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (schema, name, kind) = match scope {
            IntrospectScope::Detail { schema, name, kind }
            | IntrospectScope::RoutineDefinition {
                schema, name, kind, ..
            } => (schema, name, kind),
            IntrospectScope::Dependencies { schema, name, kind } => {
                if (schema, name, kind) == (self.schema.clone(), self.name.clone(), self.kind) {
                    self.on_dependencies(result, cx);
                }
                return;
            }
            _ => return,
        };
        if schema != self.schema || name != self.name || kind != self.kind {
            return;
        }
        self.loading = false;
        let waiting = !self.pager.ready;
        match result {
            Ok(CatalogChunk::Detail(d)) => {
                self.pager.set_detail(&d);
                self.error = None;
                self.grids = grids(&d);
                self.trigger = self.trigger.min(d.trigger_details.len().saturating_sub(1));
                let ddl = d.ddl.clone();
                self.ddl_editor
                    .update(cx, |e, cx| e.set_value(ddl, window, cx));
                self.detail = Some(d);
                self.show_trigger(window, cx);
            }
            Ok(CatalogChunk::Hint(h)) => {
                self.pager.ready = true;
                self.error = Some(h);
            }
            Ok(_) => {}
            Err(e) => {
                // The Data page still pages, without a key order.
                self.pager.ready = true;
                self.error = Some(e);
            }
        }
        if waiting && self.page == Page::Data && self.data.table.is_none() {
            self.run_data(cx);
        }
        cx.notify();
    }

    /// The uses / used-by answer arrived (an error reads like a hint).
    fn on_dependencies(&mut self, result: Result<CatalogChunk, String>, cx: &mut Context<Self>) {
        self.deps_loading = false;
        let deps = match result {
            Ok(CatalogChunk::Dependencies(d)) => d,
            Ok(CatalogChunk::Hint(h)) => Box::new(Dependencies::hint(h)),
            Ok(_) => return,
            Err(e) => Box::new(Dependencies::hint(format!("Dependencies unavailable: {e}"))),
        };
        self.dep_grids = dependency_grids(&deps);
        self.deps = Some(deps);
        cx.notify();
    }

    /// The dependency row `row` of the Uses (0) or Used by (1) list.
    fn dependency(&self, list: usize, row: usize) -> Option<&DependencyInfo> {
        let d = self.deps.as_ref()?;
        if list == 0 {
            d.uses.get(row)
        } else {
            d.used_by.get(row)
        }
    }

    fn show_trigger(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self
            .detail
            .as_ref()
            .and_then(|d| d.trigger_details.get(self.trigger))
            .map(|t| t.definition.clone())
            .unwrap_or_default();
        self.trigger_editor
            .update(cx, |e, cx| e.set_value(text, window, cx));
    }

    fn set_page(&mut self, page: Page, cx: &mut Context<Self>) {
        self.page = page;
        if page == Page::Data && self.data.query.is_none() && self.data.table.is_none() {
            self.run_data(cx);
        }
        if page == Page::Dependencies && self.deps.is_none() && !self.deps_loading {
            self.request_deps(cx);
        }
        cx.notify();
    }

    /// Run the Data page's current page (once the detail brought the key).
    fn run_data(&mut self, cx: &mut Context<Self>) {
        let (Some(session), SessionState::Open { .. }) = (self.session, &self.session_state) else {
            return;
        };
        if !self.pager.ready || !self.kind.is_relation() {
            return;
        }
        if let Some(q) = self.data.query.take()
            && self.data.running
        {
            self.core.send(Command::Cancel { query: q });
        }
        let sql = self.pager.sql(dialect_for(self.connection.engine));
        self.pager.last_sql = sql.clone();
        self.pager.rows = 0;
        let query = next_id();
        self.data = DataPage {
            query: Some(query),
            running: true,
            ..DataPage::default()
        };
        self.core.send(Command::Execute {
            session,
            query,
            statements: vec![StatementRequest {
                sql,
                params: vec![],
                offset: 0,
            }],
            tags: vec![],
            confirmed_destructive: false,
            fetch_limit: FetchLimit::Rows(self.pager.page_size as usize),
        });
        cx.notify();
    }

    /// A query event for the Data page.
    pub fn on_query(&mut self, event: QueryEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            QueryEvent::Columns(cols) => {
                // Only the first result set is shown.
                if self.data.table.is_none() {
                    self.add_grid(cols, window, cx);
                }
            }
            QueryEvent::Rows(batch) => {
                if let Some(t) = &self.data.table {
                    let first = self.data.rows == 0;
                    self.data.rows += batch.len();
                    self.pager.rows = self.data.rows;
                    t.update(cx, |t, cx| {
                        t.delegate_mut().push(batch);
                        if first {
                            t.refresh(cx);
                        }
                        cx.notify();
                    });
                }
            }
            QueryEvent::Failed { error, .. } => self.data.error = Some(error.to_string()),
            QueryEvent::Paused { .. } => {
                // The first page is all this tab shows; stop the rest.
                if let Some(q) = self.data.query {
                    self.core.send(Command::Cancel { query: q });
                }
                self.data.running = false;
            }
            QueryEvent::Finished { .. } => self.data.running = false,
            _ => {}
        }
        cx.notify();
    }

    fn add_grid(&mut self, cols: Arc<[ColumnMeta]>, window: &mut Window, cx: &mut Context<Self>) {
        self.data.columns = cols.iter().map(|c| c.name.clone()).collect();
        let mut delegate = GridDelegate::new(cols.clone()).zoomed(crate::appearance::zoom(cx));
        let weak = cx.entity().downgrade();
        delegate.set_server_sort(
            self.pager.sort_indexes(&cols),
            Rc::new(move |order, _window, cx| {
                let _ = weak.update(cx, |this, cx| {
                    let Some(t) = &this.data.table else { return };
                    let cols = t.read(cx).delegate().columns();
                    this.pager.set_sort(&cols, &order);
                    this.run_data(cx);
                });
            }),
        );
        delegate.set_fk_cols(self.pager.fk_indexes(&cols));
        let weak = cx.entity().downgrade();
        delegate.set_menu(Rc::new(move |_row, menu: PopupMenu, _window, _cx| {
            let item = |label: &'static str, action: DataAction| {
                let weak = weak.clone();
                PopupMenuItem::new(label).on_click(move |_, _, cx| {
                    let _ = weak.update(cx, |this, cx| this.data_action(action, cx));
                })
            };
            menu.item(item("Open referenced row", DataAction::Reference))
                .item(item("Copy cell", DataAction::CopyCell))
        }));
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
            |this, _, ev: &TableEvent, window, cx| match ev {
                TableEvent::SelectCell(r, c) | TableEvent::RightClickedCell(r, c) => {
                    this.data.selected = Some((*r, *c));
                    // Ctrl/Cmd+click follows a foreign key (DBX-3c).
                    if matches!(ev, TableEvent::SelectCell(..))
                        && window.modifiers().secondary()
                        && !window.modifiers().shift
                    {
                        this.data_action(DataAction::Reference, cx);
                    }
                    cx.notify();
                }
                _ => {}
            },
        );
        self.data.table = Some(table);
        self.data._sub = Some(sub);
    }

    /// Text of the selected Data cell, or every loaded row (with a header) as TSV.
    fn copy_data(&self, all: bool, cx: &mut Context<Self>) {
        let Some(table) = &self.data.table else {
            return;
        };
        let t = table.read(cx);
        let d = t.delegate();
        let text = match (all, self.data.selected) {
            (false, Some((r, c))) => d.range_tsv(r..r + 1, c..c + 1),
            (false, None) => return,
            (true, _) => {
                let body = d.range_tsv(0..d.visible_rows(), 1..self.data.columns.len() + 1);
                format!("{}\n{body}", self.data.columns.join("\t"))
            }
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    /// A row-menu or Ctrl/Cmd+click action on the selected Data cell.
    fn data_action(&mut self, action: DataAction, cx: &mut Context<Self>) {
        match action {
            DataAction::CopyCell => self.copy_data(false, cx),
            DataAction::Reference => {
                let (Some(table), Some((r, c))) = (&self.data.table, self.data.selected) else {
                    return;
                };
                let t = table.read(cx);
                let d = t.delegate();
                let Some(col) = d.data_col(c) else { return };
                let cols = d.columns();
                let Some(column) = cols.get(col).map(|m| m.name.clone()) else {
                    return;
                };
                let row = d.data_row(r);
                let found = reference_filter(
                    dialect_for(self.connection.engine),
                    &self.pager.foreign_keys,
                    &column,
                    &self.schema,
                    |name| {
                        let i = cols.iter().position(|m| m.name == name)?;
                        let v = d.data().cell(row, i)?.to_value(cols[i].data_type);
                        Some(v)
                    },
                );
                match found {
                    Ok((schema, name, filter)) => cx.emit(ObjectTabEvent::OpenData {
                        schema,
                        name,
                        filter,
                    }),
                    Err(e) => cx.emit(ObjectTabEvent::Toast(e)),
                }
            }
        }
    }

    /// Open the object a row points at: a foreign key's referenced table, or a
    /// dependency (when the explorer can open its kind).
    fn open_row(&mut self, grid: GridRef, row: usize, cx: &mut Context<Self>) {
        match grid {
            GridRef::Detail(_) => {
                let Some(fk) = self.detail.as_ref().and_then(|d| d.foreign_keys.get(row)) else {
                    return;
                };
                let (schema, name) = split_reference(&fk.references, &self.schema);
                cx.emit(ObjectTabEvent::OpenObject {
                    schema,
                    name,
                    kind: ObjectKind::Table,
                });
            }
            GridRef::Deps(list) => {
                let Some(dep) = self.dependency(list, row).cloned() else {
                    return;
                };
                match dep.kind {
                    Some(kind) => cx.emit(ObjectTabEvent::OpenObject {
                        schema: dep.schema,
                        name: dep.name,
                        kind,
                    }),
                    None => cx.emit(ObjectTabEvent::Toast(format!(
                        "{} {}.{} cannot be opened here",
                        dep.type_label, dep.schema, dep.name
                    ))),
                }
            }
        }
    }

    fn grid(&self, grid: GridRef) -> Option<&Grid> {
        match grid {
            GridRef::Detail(i) => self.grids.get(i),
            GridRef::Deps(i) => self.dep_grids.get(i),
        }
    }

    fn copy_row(&self, grid: GridRef, row: usize, cx: &mut Context<Self>) {
        if let Some(cells) = self.grid(grid).and_then(|g| g.rows.get(row)) {
            let text = cells
                .iter()
                .map(|c| c.as_ref())
                .collect::<Vec<_>>()
                .join("\t");
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            cx.emit(ObjectTabEvent::Toast("Copied row".into()));
        }
    }

    fn count(&self, page: Page) -> Option<usize> {
        if page == Page::Dependencies {
            let deps = self.deps.as_ref()?;
            return Some(deps.uses.len() + deps.used_by.len());
        }
        let d = self.detail.as_ref()?;
        Some(match page {
            Page::Columns => d.columns.len(),
            Page::Indexes => d.indexes.len(),
            Page::Constraints => d.constraints.len(),
            Page::ForeignKeys => d.foreign_keys.len(),
            Page::Triggers => d.triggers.len().max(d.trigger_details.len()),
            Page::Dependencies | Page::Ddl | Page::Data => return None,
        })
    }

    // ------------------------------------------------------------------ render

    fn render_header(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let d = self.detail.as_deref();
        let mut facts: Vec<String> = vec![kind_label(self.kind).into()];
        if let Some(d) = d {
            if let Some(b) = d.size_bytes {
                facts.push(size_label(Some(b)));
            }
            if let Some(n) = d.object.estimated_rows {
                facts.push(rows_label(n));
            }
        }
        let comment = d.and_then(|d| d.comment.clone());
        let qualified = SharedString::from(format!("{}.{}", self.schema, self.name));
        let status: Option<SharedString> = if let Some(e) = &self.error {
            Some(e.clone().into())
        } else if self.loading {
            Some(match self.session_state {
                SessionState::Connecting => "Connecting…".into(),
                _ => "Loading…".into(),
            })
        } else {
            None
        };
        let copy_name = qualified.clone();
        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(4.))
            .px(px(14.))
            .py(px(10.))
            .border_b_1()
            .border_color(p.bd)
            .bg(p.panel)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(ui::monogram(self.kind.icon(), 20., p))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_family(MONO)
                            .text_size(px(13.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.fg)
                            .truncate()
                            .child(qualified),
                    )
                    .children(status.map(|s| {
                        div()
                            .text_size(px(12.))
                            .text_color(if self.error.is_some() { p.prod } else { p.fg3 })
                            .child(s)
                    }))
                    .child(
                        ui::button("obj-copy-name", "Copy name", Kind::Ghost, p).on_click(
                            cx.listener(move |_, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    copy_name.to_string(),
                                ));
                            }),
                        ),
                    )
                    .child(
                        ui::button("obj-refresh", "Refresh", Kind::Secondary, p)
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .text_size(px(12.))
                    .text_color(p.fg2)
                    .child(facts.join(" · "))
                    .children(comment.map(|c| {
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(p.fg3)
                            .child(format!("— {c}"))
                    })),
            )
            .into_any_element()
    }

    fn render_pages(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex_none()
            .h(px(32.))
            .flex()
            .items_end()
            .gap(px(2.))
            .px(px(10.))
            .border_b_1()
            .border_color(p.bd)
            .bg(p.panel)
            .children(self.pages.iter().copied().enumerate().map(|(i, page)| {
                let on = page == self.page;
                let label = match self.count(page) {
                    Some(n) => format!("{} {n}", page.label()),
                    None => page.label().to_owned(),
                };
                div()
                    .id(("obj-page", i))
                    .h(px(28.))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .text_size(px(12.))
                    .cursor_pointer()
                    .border_b_2()
                    .border_color(if on {
                        p.acc
                    } else {
                        gpui_kit::transparent_black()
                    })
                    .text_color(if on { p.fg } else { p.fg2 })
                    .when(on, |d| d.font_weight(FontWeight::MEDIUM))
                    .hover(|s| s.text_color(p.fg))
                    .on_click(cx.listener(move |this, _, _, cx| this.set_page(page, cx)))
                    .child(label)
            }))
            .into_any_element()
    }

    fn render_grid(&self, which: GridRef, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let Some(g) = self.grid(which) else {
            return self.render_empty("Loading…", p);
        };
        if g.rows.is_empty() {
            return self.render_empty("None", p);
        }
        let header = div()
            .flex_none()
            .h(px(ROW_H))
            .flex()
            .items_center()
            .px(px(12.))
            .gap(px(10.))
            .border_b_1()
            .border_color(p.bd)
            .text_size(px(11.5))
            .font_weight(FontWeight::MEDIUM)
            .text_color(p.fg3)
            .children(
                g.headers
                    .iter()
                    .enumerate()
                    .map(|(c, h)| cell_box(g.widths, c).child(*h)),
            )
            .child(div().w(px(44.)).flex_none());
        let rows = g.rows.clone();
        let (id, widths, link, double) = (g.id, g.widths, g.link, g.open_on_double);
        let p2 = *p;
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(header)
            .child(
                uniform_list(
                    id,
                    rows.len(),
                    cx.processor(move |_this, range: std::ops::Range<usize>, _w, cx| {
                        range
                            .map(|r| {
                                let p = &p2;
                                div()
                                    .id(r)
                                    .h(px(ROW_H))
                                    .flex()
                                    .items_center()
                                    .px(px(12.))
                                    .gap(px(10.))
                                    .border_b_1()
                                    .border_color(p.line)
                                    .text_size(px(12.))
                                    .text_color(p.fg)
                                    .hover(|s| s.bg(p.hover))
                                    .when(double, |d| {
                                        d.on_click(cx.listener(
                                            move |this, ev: &gpui_kit::ClickEvent, _, cx| {
                                                if ev.click_count() >= 2 {
                                                    this.open_row(which, r, cx);
                                                }
                                            },
                                        ))
                                    })
                                    .children(rows[r].iter().enumerate().map(|(c, text)| {
                                        let cell = cell_box(widths, c)
                                            .truncate()
                                            .when(c == 0, |d| d.font_family(MONO))
                                            .child(text.clone());
                                        if link == Some(c) {
                                            cell.text_color(p.acc)
                                                .cursor_pointer()
                                                .id(("obj-link", r))
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.open_row(which, r, cx)
                                                }))
                                                .into_any_element()
                                        } else {
                                            cell.into_any_element()
                                        }
                                    }))
                                    .child(
                                        div()
                                            .id(("obj-copy", r))
                                            .w(px(44.))
                                            .flex_none()
                                            .text_size(px(11.))
                                            .text_color(p.fg3)
                                            .cursor_pointer()
                                            .hover(|s| s.text_color(p.acc))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.copy_row(which, r, cx)
                                            }))
                                            .child("Copy"),
                                    )
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .flex_1(),
            )
            .into_any_element()
    }

    /// The Dependencies page: a hint when there is one, then Uses and Used by.
    fn render_dependencies(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let Some(deps) = self.deps.as_deref() else {
            return self.render_empty("Loading…", p);
        };
        let section = |label: &str, n: usize, which: GridRef, cx: &mut Context<Self>| {
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .h(px(28.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .px(px(12.))
                        .gap(px(6.))
                        .border_b_1()
                        .border_color(p.bd)
                        .bg(p.panel)
                        .text_size(px(12.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(p.fg)
                        .child(format!("{label} {n}"))
                        .child(
                            div()
                                .text_color(p.fg3)
                                .font_weight(FontWeight::NORMAL)
                                .child("· double-click to open"),
                        ),
                )
                .child(self.render_grid(which, p, cx))
        };
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .children(deps.hint.clone().map(|h| {
                div()
                    .flex_none()
                    .px(px(12.))
                    .py(px(6.))
                    .border_b_1()
                    .border_color(p.bd)
                    .text_size(px(12.))
                    .text_color(p.fg2)
                    .child(h)
            }))
            .child(section("Uses", deps.uses.len(), GridRef::Deps(0), cx))
            .child(section("Used by", deps.used_by.len(), GridRef::Deps(1), cx))
            .into_any_element()
    }

    fn render_empty(&self, text: &'static str, p: &Palette) -> AnyElement {
        div()
            .flex_1()
            .p(px(16.))
            .text_size(px(12.))
            .text_color(p.fg3)
            .child(text)
            .into_any_element()
    }

    fn render_editor(
        &self,
        id: &'static str,
        editor: &Entity<EditorState>,
        text: String,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(30.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(12.))
                    .border_b_1()
                    .border_color(p.bd)
                    .text_size(px(12.))
                    .child(div().flex_1().text_color(p.fg3).child("Read-only"))
                    .child(
                        ui::button(id, "Copy", Kind::Secondary, p).on_click(cx.listener(
                            move |_, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                            },
                        )),
                    ),
            )
            .child(
                div().flex_1().min_h_0().child(
                    Editor::new(editor)
                        // Read-only, not disabled: a disabled editor swallows mouse input,
                        // so the text could not be selected or copied.
                        .readonly(true)
                        .bordered(false)
                        .appearance(false)
                        .h(relative(1.))
                        .font_family(MONO)
                        .text_size(px(12.5)),
                ),
            )
            .into_any_element()
    }

    fn render_triggers(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let Some(d) = self.detail.as_deref() else {
            return self.render_empty("Loading…", p);
        };
        let list = trigger_list(d);
        if list.is_empty() {
            return self.render_empty("None", p);
        }
        let definition = list
            .get(self.trigger)
            .map(|t| t.definition.clone())
            .unwrap_or_default();
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .child(
                div()
                    .id("obj-trigger-list")
                    .w(px(260.))
                    .flex_none()
                    .overflow_y_scroll()
                    .border_r_1()
                    .border_color(p.bd)
                    .children(list.iter().enumerate().map(|(i, t)| {
                        let on = i == self.trigger;
                        div()
                            .id(("obj-trigger", i))
                            .px(px(12.))
                            .py(px(6.))
                            .flex()
                            .flex_col()
                            .cursor_pointer()
                            .when(on, |d| d.bg(p.sel))
                            .hover(|s| s.bg(p.hover))
                            .on_click(cx.listener(move |this, _, w, cx| {
                                this.trigger = i;
                                this.show_trigger(w, cx);
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .font_family(MONO)
                                    .text_size(px(12.))
                                    .text_color(p.fg)
                                    .truncate()
                                    .child(t.name.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(p.fg3)
                                    .truncate()
                                    .child(trigger_summary(t)),
                            )
                    })),
            )
            .child(self.render_editor("obj-trigger-copy", &self.trigger_editor, definition, p, cx))
            .into_any_element()
    }

    fn render_data(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        if !self.kind.is_relation() {
            return self.render_empty("No rows for this kind of object", p);
        }
        let status = if let Some(e) = &self.data.error {
            e.clone()
        } else if self.data.running || !self.pager.ready {
            "Loading…".into()
        } else if self.data.table.is_some() {
            format!(
                "{} row{} on this page · Ctrl/⌘+click a foreign key to open its row",
                thousands(self.data.rows as u64),
                if self.data.rows == 1 { "" } else { "s" }
            )
        } else {
            String::new()
        };
        let busy = self.data.running || !self.pager.ready;
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(render_pager(&self.pager, busy, p, cx))
            .child(
                div()
                    .h(px(30.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(12.))
                    .border_b_1()
                    .border_color(p.bd)
                    .text_size(px(12.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(if self.data.error.is_some() {
                                p.prod
                            } else {
                                p.fg3
                            })
                            .child(status),
                    )
                    .child(
                        ui::button("obj-data-cell", "Copy cell", Kind::Ghost, p)
                            .on_click(cx.listener(|this, _, _, cx| this.copy_data(false, cx))),
                    )
                    .child(
                        ui::button("obj-data-all", "Copy rows", Kind::Ghost, p)
                            .on_click(cx.listener(|this, _, _, cx| this.copy_data(true, cx))),
                    )
                    .child(
                        ui::button("obj-data-reload", "Reload", Kind::Secondary, p)
                            .on_click(cx.listener(|this, _, _, cx| this.run_data(cx))),
                    ),
            )
            .children(self.data.table.as_ref().map(|t| {
                div().flex_1().min_h_0().child(
                    DataTable::new(t)
                        .bordered(false)
                        .stripe(false)
                        .scrollbar_visible(true, true)
                        .with_size(crate::appearance::table_size(cx)),
                )
            }))
            .into_any_element()
    }
}

impl PagedView for ObjectTab {
    fn pager_mut(&mut self) -> Option<&mut Pager> {
        Some(&mut self.pager)
    }

    fn pager_dialect(&self) -> &'static dyn Dialect {
        dialect_for(self.connection.engine)
    }

    fn reload_page(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.run_data(cx);
    }
}

impl Render for ObjectTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let body = match self.page {
            Page::Columns => self.render_grid(GridRef::Detail(0), &p, cx),
            Page::Indexes => self.render_grid(GridRef::Detail(1), &p, cx),
            Page::Constraints => self.render_grid(GridRef::Detail(2), &p, cx),
            Page::ForeignKeys => self.render_grid(GridRef::Detail(3), &p, cx),
            Page::Triggers => self.render_triggers(&p, cx),
            Page::Dependencies => self.render_dependencies(&p, cx),
            Page::Ddl => {
                let ddl = self
                    .detail
                    .as_ref()
                    .map(|d| d.ddl.clone())
                    .unwrap_or_default();
                let editor = self.ddl_editor.clone();
                self.render_editor("obj-ddl-copy", &editor, ddl, &p, cx)
            }
            Page::Data => self.render_data(&p, cx),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(p.surface)
            .child(self.render_header(&p, cx))
            .child(self.render_pages(&p, cx))
            .child(body)
    }
}

/// A fixed-width cell, or the stretching last one.
fn cell_box(widths: &[f32], c: usize) -> gpui_kit::Div {
    match widths.get(c) {
        Some(w) if c + 1 < widths.len() => div().w(px(*w)).flex_none().min_w_0(),
        _ => div().flex_1().min_w_0(),
    }
}

/// The tables of the Columns, Indexes, Constraints and Foreign keys pages, in that order.
fn grids(d: &ObjectDetail) -> Vec<Grid> {
    let cells = |rows: Vec<Vec<String>>| {
        Arc::new(
            rows.into_iter()
                .map(|r| r.into_iter().map(SharedString::from).collect())
                .collect::<Vec<Vec<SharedString>>>(),
        )
    };
    vec![
        Grid {
            id: "obj-columns",
            headers: &["Name", "Type", "Null", "Default", "Key", "Comment"],
            widths: &[180., 150., 44., 160., 60., 0.],
            rows: cells(column_rows(d)),
            link: None,
            open_on_double: false,
        },
        Grid {
            id: "obj-indexes",
            headers: &["Name", "Columns", "Unique", "Method"],
            widths: &[220., 260., 60., 0.],
            rows: cells(
                d.indexes
                    .iter()
                    .map(|i| {
                        vec![
                            i.name.clone(),
                            i.columns.join(", "),
                            yes(i.is_unique),
                            i.method.clone().unwrap_or_default(),
                        ]
                    })
                    .collect(),
            ),
            link: None,
            open_on_double: false,
        },
        Grid {
            id: "obj-constraints",
            headers: &["Name", "Kind", "Definition"],
            widths: &[220., 110., 0.],
            rows: cells(
                d.constraints
                    .iter()
                    .map(|c| vec![c.name.clone(), c.kind.clone(), c.definition.clone()])
                    .collect(),
            ),
            link: None,
            open_on_double: false,
        },
        Grid {
            id: "obj-fks",
            headers: &["Name", "Columns", "References", "On delete", "On update"],
            widths: &[200., 160., 260., 100., 0.],
            rows: cells(
                d.foreign_keys
                    .iter()
                    .map(|f| {
                        vec![
                            f.name.clone(),
                            f.columns.join(", "),
                            fk_target(f),
                            f.on_delete.clone().unwrap_or_default(),
                            f.on_update.clone().unwrap_or_default(),
                        ]
                    })
                    .collect(),
            ),
            link: Some(2),
            open_on_double: false,
        },
    ]
}

/// The Uses and Used by tables of the Dependencies page: kind, `schema.name`, link.
fn dependency_grids(d: &Dependencies) -> Vec<Grid> {
    let rows = |list: &[DependencyInfo]| {
        Arc::new(
            dependency_rows(list)
                .into_iter()
                .map(|r| r.into_iter().map(SharedString::from).collect())
                .collect::<Vec<Vec<SharedString>>>(),
        )
    };
    let grid = |id, list| Grid {
        id,
        headers: &["Object", "Kind", "Dependency"],
        widths: &[320., 150., 0.],
        rows: rows(list),
        link: None,
        open_on_double: true,
    };
    vec![grid("obj-uses", &d.uses), grid("obj-used-by", &d.used_by)]
}

/// One row per dependency: `schema.name` (just the name without a schema), the engine's
/// type text and the link.
fn dependency_rows(list: &[DependencyInfo]) -> Vec<Vec<String>> {
    list.iter()
        .map(|x| {
            let name = if x.schema.is_empty() {
                x.name.clone()
            } else {
                format!("{}.{}", x.schema, x.name)
            };
            vec![name, x.type_label.clone(), x.dependency.clone()]
        })
        .collect()
}

/// Columns page rows: name, type, nullable, default, key marker, comment.
fn column_rows(d: &ObjectDetail) -> Vec<Vec<String>> {
    let mut cols: Vec<_> = d.columns.iter().collect();
    cols.sort_by_key(|c| c.ordinal);
    let pk_index: Vec<&String> = d
        .indexes
        .iter()
        .filter(|i| i.is_primary)
        .flat_map(|i| &i.columns)
        .collect();
    cols.into_iter()
        .map(|c| {
            let pk = c.is_primary_key || pk_index.contains(&&c.name);
            let fk = d.foreign_keys.iter().any(|f| f.columns.contains(&c.name));
            vec![
                c.name.clone(),
                c.data_type.clone(),
                yes(c.nullable),
                c.default.clone().unwrap_or_default(),
                key_marker(pk, fk).into(),
                c.comment.clone().unwrap_or_default(),
            ]
        })
        .collect()
}

fn yes(b: bool) -> String {
    if b { "YES".into() } else { String::new() }
}

/// `PK`, `FK`, `PK FK` or nothing.
pub fn key_marker(pk: bool, fk: bool) -> &'static str {
    match (pk, fk) {
        (true, true) => "PK FK",
        (true, false) => "PK",
        (false, true) => "FK",
        (false, false) => "",
    }
}

/// `schema.table (a, b)`: what a foreign key points at.
pub fn fk_target(f: &ForeignKeyInfo) -> String {
    if f.referenced_columns.is_empty() {
        f.references.clone()
    } else {
        format!("{} ({})", f.references, f.referenced_columns.join(", "))
    }
}

/// Schema and name of a foreign key's `schema.table` reference; a bare name is taken to
/// be in `default_schema`.
pub fn split_reference(reference: &str, default_schema: &str) -> (String, String) {
    match reference.split_once('.') {
        Some((s, n)) if !s.is_empty() && !n.is_empty() => (s.to_owned(), n.to_owned()),
        _ => (default_schema.to_owned(), reference.to_owned()),
    }
}

/// `12.3 MB`, or `—` when the size is unknown.
pub fn size_label(bytes: Option<i64>) -> String {
    match bytes {
        Some(b) if b >= 0 => ui::bytes(b as u64),
        _ => "—".into(),
    }
}

/// `~1,234 rows`.
pub fn rows_label(n: i64) -> String {
    let n = n.max(0) as u64;
    format!("~{} row{}", thousands(n), if n == 1 { "" } else { "s" })
}

fn kind_label(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Table => "Table",
        ObjectKind::View => "View",
        ObjectKind::MaterializedView => "Materialized view",
        ObjectKind::Function => "Function",
        ObjectKind::Procedure => "Procedure",
        ObjectKind::Sequence => "Sequence",
        ObjectKind::Type => "Type",
        ObjectKind::Synonym => "Synonym",
        ObjectKind::Role => "User / role",
        ObjectKind::Job => "SQL Agent job",
        ObjectKind::Extension => "Extension",
        ObjectKind::Package => "Package",
        ObjectKind::Stage => "Stage",
        ObjectKind::Task => "Task",
        ObjectKind::Pipe => "Pipe",
    }
}

/// Triggers with details; names only (from an older cached detail) when there are none.
fn trigger_list(d: &ObjectDetail) -> Vec<TriggerInfo> {
    if !d.trigger_details.is_empty() || d.triggers.is_empty() {
        return d.trigger_details.clone();
    }
    d.triggers
        .iter()
        .map(|n| TriggerInfo {
            name: n.clone(),
            ..TriggerInfo::default()
        })
        .collect()
}

/// `BEFORE FOR EACH ROW · INSERT OR UPDATE`.
fn trigger_summary(t: &TriggerInfo) -> String {
    [t.timing.as_str(), t.event.as_str()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

impl Workspace {
    /// Open (or focus) the properties tab of `schema.name` on `connection`.
    pub fn open_object_properties(
        &mut self,
        connection: DbConnection,
        schema: String,
        name: String,
        kind: ObjectKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_object_page(connection, schema, name, kind, None, window, cx);
    }

    /// Open (or focus) the properties tab of `schema.name` on `connection`, on `page`
    /// when given (Show dependencies, DBX-5a).
    #[allow(clippy::too_many_arguments)]
    pub fn open_object_page(
        &mut self,
        connection: DbConnection,
        schema: String,
        name: String,
        kind: ObjectKind,
        page: Option<Page>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = ObjectTab::key_for(&connection, &schema, &name, kind);
        if let Some(ix) = self
            .tabs
            .iter()
            .position(|t| matches!(t, Tab::Object(o) if o.read(cx).key == key))
        {
            if let (Some(page), Tab::Object(o)) = (page, &self.tabs[ix]) {
                o.update(cx, |o, cx| o.show_page(page, cx));
            }
            self.activate(ix, cx);
            return;
        }
        let core = self.core.clone();
        let tab = cx.new(|cx| {
            let mut t = ObjectTab::new(core, connection, schema, name, kind, window, cx);
            if let Some(page) = page {
                t.show_page(page, cx);
            }
            t
        });
        cx.subscribe_in(
            &tab,
            window,
            |this, tab, ev: &ObjectTabEvent, window, cx| match ev {
                ObjectTabEvent::OpenObject { schema, name, kind } => {
                    let conn = tab.read(cx).connection.clone();
                    this.open_object_properties(
                        conn,
                        schema.clone(),
                        name.clone(),
                        *kind,
                        window,
                        cx,
                    );
                }
                ObjectTabEvent::Toast(t) => this.toast(t.clone(), cx),
                ObjectTabEvent::OpenData {
                    schema,
                    name,
                    filter,
                } => {
                    let conn = tab.read(cx).connection.clone();
                    let (s, n, f) = (schema.clone(), name.clone(), Some(filter.clone()));
                    this.open_table_data(conn, s, n, ObjectKind::Table, f, window, cx);
                }
            },
        )
        .detach();
        self.tabs.push(Tab::Object(tab));
        self.activate(self.tabs.len() - 1, cx);
    }

    /// Open `schema.name` as a table data view in a new SQL tab: server-side filter, sort
    /// and paging (DBX-3a), `filter` applied from the start (DBX-3c), staged row edits.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open_table_data(
        &mut self,
        connection: DbConnection,
        schema: String,
        name: String,
        kind: ObjectKind,
        filter: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tab = self.open_query_tab(&connection, &name, window, cx);
        tab.update(cx, |t, cx| {
            t.open_data(schema, name, kind, filter, window, cx)
        });
        cx.notify();
    }

    /// The schema tree's "Properties…" (or "Show dependencies", on that `page`) item:
    /// the object on its own connection.
    pub(crate) fn open_tree_object_properties(
        &mut self,
        o: &crate::explorer::ObjRef,
        page: Option<Page>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(conn) = self.profiles.db(&o.conn).cloned() {
            let (schema, name) = (o.schema.clone(), o.name.clone());
            self.open_object_page(conn, schema, name, o.kind, page, window, cx);
        }
    }

    /// The object tab that owns `session`, if any.
    pub(crate) fn object_tab_for_session(
        &self,
        session: SessionId,
        cx: &App,
    ) -> Option<Entity<ObjectTab>> {
        self.tabs.iter().find_map(|t| match t {
            Tab::Object(o) if o.read(cx).owns_session(session) => Some(o.clone()),
            _ => None,
        })
    }

    /// The object tab whose Data page runs `query`, if any.
    pub(crate) fn object_tab_for_query(
        &self,
        query: QueryId,
        cx: &App,
    ) -> Option<Entity<ObjectTab>> {
        self.tabs.iter().find_map(|t| match t {
            Tab::Object(o) if o.read(cx).owns_query(query) => Some(o.clone()),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_core::db::{ColumnInfo, IndexInfo};

    fn fk() -> ForeignKeyInfo {
        ForeignKeyInfo {
            name: "fk".into(),
            columns: vec!["customer_id".into(), "store_id".into()],
            references: "public.customers".into(),
            referenced_columns: vec!["id".into(), "store".into()],
            ..ForeignKeyInfo::default()
        }
    }

    #[test]
    fn sizes_read_as_bytes() {
        assert_eq!(size_label(None), "—");
        assert_eq!(size_label(Some(-1)), "—");
        assert_eq!(size_label(Some(512)), "512 B");
        assert_eq!(size_label(Some(8192)), "8.0 KB");
        assert_eq!(size_label(Some(5 * 1024 * 1024 + 300 * 1024)), "5.3 MB");
        assert_eq!(rows_label(1), "~1 row");
        assert_eq!(rows_label(1_234_567), "~1,234,567 rows");
    }

    #[test]
    fn foreign_key_labels() {
        assert_eq!(fk_target(&fk()), "public.customers (id, store)");
        let bare = ForeignKeyInfo {
            references: "dbo.t".into(),
            ..ForeignKeyInfo::default()
        };
        assert_eq!(fk_target(&bare), "dbo.t");
    }

    #[test]
    fn references_split_into_schema_and_name() {
        assert_eq!(
            split_reference("public.customers", "x"),
            ("public".into(), "customers".into())
        );
        assert_eq!(
            split_reference("customers", "main"),
            ("main".into(), "customers".into())
        );
        assert_eq!(
            split_reference("APP.T.X", "x"),
            ("APP".into(), "T.X".into())
        );
    }

    #[test]
    fn column_rows_mark_keys_in_table_order() {
        let col = |name: &str, ordinal: i32, pk: bool| ColumnInfo {
            name: name.into(),
            data_type: "int".into(),
            nullable: !pk,
            ordinal,
            is_primary_key: pk,
            ..ColumnInfo::default()
        };
        let d = ObjectDetail {
            columns: vec![
                ColumnInfo {
                    comment: Some("who".into()),
                    default: Some("0".into()),
                    ..col("customer_id", 2, false)
                },
                col("id", 1, false),
            ],
            // Engines that do not flag key columns: the primary index says so.
            indexes: vec![IndexInfo {
                name: "pk".into(),
                columns: vec!["id".into()],
                is_primary: true,
                ..IndexInfo::default()
            }],
            foreign_keys: vec![fk()],
            ..ObjectDetail::default()
        };
        let rows = column_rows(&d);
        assert_eq!(rows[0], ["id", "int", "YES", "", "PK", ""]);
        assert_eq!(rows[1], ["customer_id", "int", "YES", "0", "FK", "who"]);
        assert_eq!(key_marker(true, true), "PK FK");
    }

    #[test]
    fn pages_per_kind() {
        assert_eq!(Page::for_kind(ObjectKind::Table, true).len(), 8);
        assert!(!Page::for_kind(ObjectKind::Table, false).contains(&Page::Dependencies));
        assert_eq!(
            Page::for_kind(ObjectKind::Package, true),
            [Page::Dependencies, Page::Ddl]
        );
        // D1 (no dependency catalog) and server-level kinds: DDL only.
        assert_eq!(Page::for_kind(ObjectKind::Function, false), [Page::Ddl]);
        assert_eq!(Page::for_kind(ObjectKind::Role, true), [Page::Ddl]);
    }

    #[test]
    fn dependency_rows_show_kind_name_and_link() {
        let d = Dependencies {
            uses: vec![DependencyInfo {
                schema: "public".into(),
                name: "orders".into(),
                kind: Some(ObjectKind::Table),
                type_label: "table".into(),
                dependency: "foreign key fk".into(),
            }],
            used_by: vec![DependencyInfo {
                name: "x".into(),
                type_label: "trigger".into(),
                ..DependencyInfo::default()
            }],
            hint: None,
        };
        assert_eq!(
            dependency_rows(&d.uses),
            [["public.orders", "table", "foreign key fk"]]
        );
        assert_eq!(dependency_rows(&d.used_by)[0][0], "x");
        let grids = dependency_grids(&d);
        assert!(grids.iter().all(|g| g.open_on_double));
        assert_eq!(grids[1].rows.len(), 1);
    }

    #[test]
    fn trigger_names_from_old_details_still_list() {
        let d = ObjectDetail {
            triggers: vec!["t1".into()],
            ..ObjectDetail::default()
        };
        assert_eq!(trigger_list(&d)[0].name, "t1");
        let t = TriggerInfo {
            name: "t".into(),
            timing: "AFTER".into(),
            event: "INSERT".into(),
            definition: String::new(),
        };
        assert_eq!(trigger_summary(&t), "AFTER · INSERT");
    }
}
