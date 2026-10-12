//! Schema compare tab: pick a source and a target (two connections, or two databases or
//! schemas on one), compare them, review the differences grouped as Only in source /
//! Only in target / Different with a side-by-side DDL diff, tick what to include, and
//! open the migration script (target → source) in a new SQL tab.
//!
//! Each side reads its catalog on a session of its own through the core
//! ([`Command::CompareCatalog`], [`Command::CompareSnapshot`]); the diff and the script
//! come from `switchyard_db::compare` behind the engine's `Dialect::schema_scripter`.
//! Nothing here runs the script: opening it in the editor is the contract.

use std::sync::Arc;

use gpui_kit::component::Sizable as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, EventEmitter, FontWeight, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Window, div, px, uniform_list,
};
use switchyard_core::db::compare::{
    self, DiffStatus, ObjectDiff, SchemaDiff, SchemaSnapshot, ScriptHeader,
};
use switchyard_core::db::{Engine, dialect_for};
use switchyard_core::store::{DbConnection, ProfileId};
use switchyard_core::{Command, CompareCatalog, RequestId, RuntimeHandle, SessionId};

use crate::api::workbench::{DiffLine, line_diff};
use crate::app_state::next_id;
use crate::appearance::{rpx, ts};
use crate::theme::{MONO, Palette, SANS, palette};
use crate::ui::{self, Kind};
use crate::workspace::{Tab, Workspace};

/// Row height of the difference list and the DDL panes.
const ROW_H: f32 = 24.;

/// What the tab asks the workspace to do.
pub enum CompareTabEvent {
    /// Open `script` in a new SQL tab on `connection`.
    OpenScript {
        /// The target connection.
        connection: Box<DbConnection>,
        /// Tab title.
        title: String,
        /// The migration script.
        script: String,
    },
    /// Show a toast.
    Toast(String),
}

/// DDL diff of the selected object: (object index, target pane, source pane).
type DdlPanes = (usize, Arc<Vec<DiffLine>>, Arc<Vec<DiffLine>>);

/// One side (source or target) of the compare.
#[derive(Default)]
struct Side {
    connection: Option<DbConnection>,
    /// The side's own session in the core (a new one per connection).
    session: SessionId,
    catalog_request: Option<RequestId>,
    catalog: Option<CompareCatalog>,
    database: Option<String>,
    schema: Option<String>,
    /// Schema to pick once the catalog arrives (the one the user started from).
    wanted_schema: Option<String>,
    snapshot_request: Option<RequestId>,
    progress: (usize, usize),
    snapshot: Option<Arc<SchemaSnapshot>>,
    error: Option<String>,
}

impl Side {
    /// `connection · database · schema` for the script header (names only).
    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(c) = &self.connection {
            parts.push(c.name.clone());
        }
        if let Some(d) = &self.database {
            parts.push(d.clone());
        }
        if let Some(s) = &self.schema {
            parts.push(s.clone());
        }
        parts.join(" · ")
    }

    /// The database to switch to, when it is not the connection's default.
    fn switched_database(&self) -> Option<String> {
        let conn = self.connection.as_ref()?;
        self.database
            .clone()
            .filter(|d| !d.is_empty() && *d != conn.database)
    }
}

/// A row of the difference list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    /// Group heading with its object count.
    Group(DiffStatus, usize),
    /// An object (index into [`SchemaDiff::objects`]).
    Object(usize),
}

/// The list rows of `diff`: each status group's heading, then its objects.
pub fn rows_for(diff: &SchemaDiff) -> Vec<Row> {
    let mut rows = Vec::new();
    for status in [
        DiffStatus::OnlyInSource,
        DiffStatus::OnlyInTarget,
        DiffStatus::Different,
    ] {
        let ix: Vec<usize> = diff
            .objects
            .iter()
            .enumerate()
            .filter(|(_, o)| o.status == status)
            .map(|(i, _)| i)
            .collect();
        if ix.is_empty() {
            continue;
        }
        rows.push(Row::Group(status, ix.len()));
        rows.extend(ix.into_iter().map(Row::Object));
    }
    rows
}

/// Which objects the script includes at first: everything except drops of objects only
/// in the target, which the user ticks on purpose.
pub fn default_selection(diff: &SchemaDiff) -> Vec<bool> {
    diff.objects
        .iter()
        .map(|o| o.status != DiffStatus::OnlyInTarget)
        .collect()
}

/// Why the two picked connections cannot be compared, if they cannot.
pub fn compare_blocker(
    source: Option<&DbConnection>,
    target: Option<&DbConnection>,
) -> Option<String> {
    let (Some(a), Some(b)) = (source, target) else {
        return Some("Pick a source and a target".into());
    };
    if a.engine != b.engine {
        return Some(format!(
            "Both sides must be the same engine ({} vs {})",
            a.engine.display_name(),
            b.engine.display_name()
        ));
    }
    if dialect_for(a.engine).schema_scripter().is_none() {
        return Some(compare::unsupported_message(a.engine));
    }
    None
}

/// The schema compare tab.
pub struct CompareTab {
    core: RuntimeHandle,
    /// Tab title.
    pub title: SharedString,
    /// SQL connections offered in the pickers.
    connections: Vec<DbConnection>,
    sides: [Side; 2],
    diff: Option<Arc<SchemaDiff>>,
    rows: Vec<Row>,
    included: Vec<bool>,
    selected: Option<usize>,
    /// DDL diff of the selected object.
    ddl: Option<DdlPanes>,
    error: Option<String>,
}

impl EventEmitter<CompareTabEvent> for CompareTab {}

impl CompareTab {
    /// A tab comparing `source` (schema `schema` when given) with a target to pick.
    pub fn new(
        core: RuntimeHandle,
        connections: Vec<DbConnection>,
        source: Option<DbConnection>,
        schema: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut t = Self {
            core,
            title: "Schema compare".into(),
            connections,
            sides: [Side::default(), Side::default()],
            diff: None,
            rows: Vec::new(),
            included: Vec::new(),
            selected: None,
            ddl: None,
            error: None,
        };
        if let Some(c) = source {
            t.sides[0].wanted_schema = schema.clone();
            t.sides[1].wanted_schema = schema;
            t.pick_connection(0, c.clone(), cx);
            t.pick_connection(1, c, cx);
        }
        t
    }

    /// The connection the tab belongs to (the source), for the tab strip.
    pub fn connection(&self) -> Option<&DbConnection> {
        self.sides[0].connection.as_ref()
    }

    /// Whether `request` is one of this tab's.
    pub fn owns(&self, request: RequestId) -> bool {
        self.sides
            .iter()
            .any(|s| s.catalog_request == Some(request) || s.snapshot_request == Some(request))
    }

    /// Close both sides' sessions (the tab is closing).
    pub fn shutdown(&mut self) {
        for s in &self.sides {
            if s.connection.is_some() {
                self.core.send(Command::CloseSession { session: s.session });
            }
        }
    }

    fn running(&self) -> bool {
        self.sides.iter().any(|s| s.snapshot_request.is_some())
    }

    fn clear_result(&mut self) {
        self.diff = None;
        self.rows.clear();
        self.included.clear();
        self.selected = None;
        self.ddl = None;
        for s in &mut self.sides {
            s.snapshot = None;
        }
    }

    fn pick_connection(&mut self, side: usize, conn: DbConnection, cx: &mut Context<Self>) {
        let s = &mut self.sides[side];
        if s.connection.is_some() {
            self.core.send(Command::CloseSession { session: s.session });
        }
        s.session = next_id();
        s.database = Some(conn.database.clone()).filter(|d| !d.is_empty());
        if s.schema.is_some() {
            s.wanted_schema = s.schema.take();
        }
        s.catalog = None;
        s.error = None;
        s.connection = Some(conn);
        self.clear_result();
        self.load_catalog(side, None);
        cx.notify();
    }

    fn pick_database(&mut self, side: usize, db: String, cx: &mut Context<Self>) {
        let s = &mut self.sides[side];
        s.database = Some(db.clone());
        s.wanted_schema = s.schema.take();
        self.clear_result();
        self.load_catalog(side, Some(db));
        cx.notify();
    }

    fn pick_schema(&mut self, side: usize, schema: String, cx: &mut Context<Self>) {
        self.sides[side].schema = Some(schema);
        self.clear_result();
        cx.notify();
    }

    fn load_catalog(&mut self, side: usize, database: Option<String>) {
        let s = &mut self.sides[side];
        let Some(conn) = &s.connection else { return };
        let request = next_id();
        s.catalog_request = Some(request);
        self.core.send(Command::CompareCatalog {
            session: s.session,
            connection: conn.id.clone(),
            request,
            database,
        });
    }

    /// Swap source and target.
    fn swap(&mut self, cx: &mut Context<Self>) {
        self.sides.swap(0, 1);
        self.clear_result();
        cx.notify();
    }

    /// Read both sides.
    fn start(&mut self, cx: &mut Context<Self>) {
        if let Some(why) = compare_blocker(
            self.sides[0].connection.as_ref(),
            self.sides[1].connection.as_ref(),
        ) {
            self.error = Some(why);
            cx.notify();
            return;
        }
        if self.sides.iter().any(|s| s.schema.is_none()) {
            self.error = Some("Pick a schema on both sides".into());
            cx.notify();
            return;
        }
        self.error = None;
        self.clear_result();
        for s in &mut self.sides {
            let (Some(conn), Some(schema)) = (&s.connection, &s.schema) else {
                continue;
            };
            let request = next_id();
            s.snapshot_request = Some(request);
            s.progress = (0, 0);
            s.error = None;
            self.core.send(Command::CompareSnapshot {
                session: s.session,
                connection: conn.id.clone(),
                request,
                database: s.database.clone(),
                schema: schema.clone(),
            });
        }
        cx.notify();
    }

    /// Stop reading: closing the sessions cancels the reads; the next request opens
    /// them again.
    fn cancel(&mut self, cx: &mut Context<Self>) {
        for s in &mut self.sides {
            if s.snapshot_request.take().is_some() {
                self.core.send(Command::CloseSession { session: s.session });
                s.session = next_id();
                s.catalog = None;
            }
        }
        self.error = Some("Cancelled".into());
        for side in 0..2 {
            let db = self.sides[side].database.clone();
            self.load_catalog(side, db);
        }
        cx.notify();
    }

    /// [`switchyard_core::Event::CompareCatalog`].
    pub fn on_catalog(
        &mut self,
        request: RequestId,
        result: Result<CompareCatalog, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(s) = self
            .sides
            .iter_mut()
            .find(|s| s.catalog_request == Some(request))
        else {
            return;
        };
        s.catalog_request = None;
        match result {
            Ok(c) => {
                if s.database.is_none() {
                    s.database = c.database.clone();
                }
                let engine = s.connection.as_ref().map_or(Engine::Postgres, |c| c.engine);
                let default = dialect_for(engine).default_schema();
                let wanted = s.wanted_schema.take();
                s.schema = wanted
                    .filter(|w| c.schemas.contains(w))
                    .or_else(|| c.schemas.iter().find(|n| *n == default).cloned())
                    .or_else(|| c.schemas.first().cloned());
                s.catalog = Some(c);
                s.error = None;
            }
            Err(e) => s.error = Some(e),
        }
        cx.notify();
    }

    /// [`switchyard_core::Event::CompareProgress`].
    pub fn on_progress(
        &mut self,
        request: RequestId,
        done: usize,
        total: usize,
        cx: &mut Context<Self>,
    ) {
        if let Some(s) = self
            .sides
            .iter_mut()
            .find(|s| s.snapshot_request == Some(request))
        {
            s.progress = (done, total);
            cx.notify();
        }
    }

    /// [`switchyard_core::Event::CompareSnapshot`].
    pub fn on_snapshot(
        &mut self,
        request: RequestId,
        result: Result<Arc<SchemaSnapshot>, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(s) = self
            .sides
            .iter_mut()
            .find(|s| s.snapshot_request == Some(request))
        else {
            return;
        };
        s.snapshot_request = None;
        match result {
            Ok(snap) => s.snapshot = Some(snap),
            Err(e) => s.error = Some(e),
        }
        if let (Some(a), Some(b)) = (&self.sides[0].snapshot, &self.sides[1].snapshot)
            && let Some(scripter) = dialect_for(a.engine).schema_scripter()
        {
            let diff = Arc::new(compare::compare(a, b, scripter));
            self.rows = rows_for(&diff);
            self.included = default_selection(&diff);
            self.selected = None;
            self.ddl = None;
            self.diff = Some(diff);
            if let Some(Row::Object(first)) = self.rows.iter().find(|r| matches!(r, Row::Object(_)))
            {
                let first = *first;
                self.select(first, cx);
            }
        }
        cx.notify();
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.selected = Some(ix);
        let Some(diff) = &self.diff else {
            return;
        };
        let (Some(o), Some(scripter)) = (
            diff.objects.get(ix),
            dialect_for(diff.engine).schema_scripter(),
        ) else {
            return;
        };
        let (src, tgt) = compare::display_ddl(diff, o, scripter);
        let (left, right) = line_diff(&tgt, &src);
        self.ddl = Some((ix, Arc::new(left), Arc::new(right)));
        cx.notify();
    }

    fn toggle(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(v) = self.included.get_mut(ix) {
            *v = !*v;
        }
        cx.notify();
    }

    fn toggle_group(&mut self, status: DiffStatus, cx: &mut Context<Self>) {
        let Some(diff) = &self.diff else { return };
        let ix: Vec<usize> = diff
            .objects
            .iter()
            .enumerate()
            .filter(|(_, o)| o.status == status)
            .map(|(i, _)| i)
            .collect();
        let all = ix
            .iter()
            .all(|i| self.included.get(*i).copied().unwrap_or(false));
        for i in ix {
            if let Some(v) = self.included.get_mut(i) {
                *v = !all;
            }
        }
        cx.notify();
    }

    /// Build the script and hand it to the workspace for a new SQL tab.
    fn open_script(&mut self, cx: &mut Context<Self>) {
        let (Some(diff), Some(target)) = (&self.diff, self.sides[1].connection.clone()) else {
            return;
        };
        let Some(scripter) = dialect_for(diff.engine).schema_scripter() else {
            return;
        };
        if !self.included.iter().any(|v| *v) {
            cx.emit(CompareTabEvent::Toast("Tick at least one object".into()));
            return;
        }
        let header = ScriptHeader {
            source: self.sides[0].describe(),
            target: self.sides[1].describe(),
            database: self.sides[1].switched_database(),
        };
        let included = self.included.clone();
        let script = compare::migration_script(
            diff,
            &|i| included.get(i).copied().unwrap_or(false),
            scripter,
            &header,
        );
        let title = format!(
            "migrate-{}",
            self.sides[1].schema.clone().unwrap_or_default()
        );
        cx.emit(CompareTabEvent::OpenScript {
            connection: Box::new(target),
            title,
            script,
        });
    }

    // ------------------------------------------------------------------ render

    fn render_side(&self, side: usize, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let s = &self.sides[side];
        let this = cx.entity().downgrade();
        let conns = self.connections.clone();
        let chosen = s.connection.as_ref().map(|c| c.id.clone());
        let conn_label = s
            .connection
            .as_ref()
            .map_or("Choose a connection".to_owned(), |c| c.name.clone());
        let conn_picker = Button::new(("cmp-conn", side))
            .outline()
            .small()
            .label(conn_label)
            .dropdown_menu(move |mut menu, _, _| {
                for c in &conns {
                    let this = this.clone();
                    let conn = c.clone();
                    let checked = chosen.as_ref() == Some(&c.id);
                    let label = format!("{} · {}", c.name, c.engine.display_name());
                    menu = menu.item(PopupMenuItem::new(label).checked(checked).on_click(
                        move |_, _, cx| {
                            let conn = conn.clone();
                            let _ = this.update(cx, |t, cx| t.pick_connection(side, conn, cx));
                        },
                    ));
                }
                menu
            });
        let databases = s
            .catalog
            .as_ref()
            .map(|c| c.databases.clone())
            .unwrap_or_default();
        let db_picker = (databases.len() > 1).then(|| {
            let this = cx.entity().downgrade();
            let chosen = s.database.clone();
            Button::new(("cmp-db", side))
                .outline()
                .small()
                .label(chosen.clone().unwrap_or_else(|| "Database".into()))
                .dropdown_menu(move |mut menu, _, _| {
                    for d in &databases {
                        let this = this.clone();
                        let db = d.clone();
                        menu = menu.item(
                            PopupMenuItem::new(d.clone())
                                .checked(chosen.as_ref() == Some(d))
                                .on_click(move |_, _, cx| {
                                    let db = db.clone();
                                    let _ = this.update(cx, |t, cx| t.pick_database(side, db, cx));
                                }),
                        );
                    }
                    menu
                })
        });
        let schemas = s
            .catalog
            .as_ref()
            .map(|c| c.schemas.clone())
            .unwrap_or_default();
        let schema_picker = {
            let this = cx.entity().downgrade();
            let chosen = s.schema.clone();
            Button::new(("cmp-schema", side))
                .outline()
                .small()
                .label(chosen.clone().unwrap_or_else(|| "Schema".into()))
                .dropdown_menu(move |mut menu, _, _| {
                    for n in &schemas {
                        let this = this.clone();
                        let name = n.clone();
                        menu = menu.item(
                            PopupMenuItem::new(n.clone())
                                .checked(chosen.as_ref() == Some(n))
                                .on_click(move |_, _, cx| {
                                    let name = name.clone();
                                    let _ = this.update(cx, |t, cx| t.pick_schema(side, name, cx));
                                }),
                        );
                    }
                    menu
                })
        };
        let status: Option<AnyElement> = if let Some(e) = &s.error {
            Some(
                div()
                    .text_size(ts::SMALL)
                    .text_color(p.prod)
                    .child(e.clone())
                    .into_any_element(),
            )
        } else if s.snapshot_request.is_some() {
            let (done, total) = s.progress;
            Some(
                div()
                    .font_family(MONO)
                    .text_size(ts::SMALL)
                    .text_color(p.fg3)
                    .child(if total == 0 {
                        "Listing objects…".to_owned()
                    } else {
                        format!("Reading {done} / {total}")
                    })
                    .into_any_element(),
            )
        } else if s.catalog_request.is_some() {
            Some(ui::shimmer(80., p))
        } else {
            s.snapshot.as_ref().map(|snap| {
                let skipped = snap.skipped.len();
                div()
                    .font_family(MONO)
                    .text_size(ts::SMALL)
                    .text_color(if skipped > 0 { p.stg } else { p.fg3 })
                    .child(if skipped > 0 {
                        format!("{} objects · {skipped} unreadable", snap.objects.len())
                    } else {
                        format!("{} objects", snap.objects.len())
                    })
                    .into_any_element()
            })
        };
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(rpx(6.))
            .child(
                div()
                    .text_size(ts::CAPTION_PLUS)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.fg3)
                    .child(if side == 0 {
                        "SOURCE (wanted state)"
                    } else {
                        "TARGET (changed by the script)"
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(rpx(6.))
                    .child(conn_picker)
                    .children(db_picker)
                    .child(schema_picker),
            )
            .children(status)
            .into_any_element()
    }

    fn render_list(
        &self,
        diff: &Arc<SchemaDiff>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rows = self.rows.clone();
        let diff = diff.clone();
        let included = self.included.clone();
        let selected = self.selected;
        let p2 = *p;
        uniform_list(
            "cmp-rows",
            rows.len(),
            cx.processor(move |_this, range: std::ops::Range<usize>, _w, cx| {
                let p = &p2;
                range
                    .map(|r| match rows[r] {
                        Row::Group(status, n) => {
                            let all = diff
                                .objects
                                .iter()
                                .enumerate()
                                .filter(|(_, o)| o.status == status)
                                .all(|(i, _)| included.get(i).copied().unwrap_or(false));
                            div()
                                .id(("cmp-group", r))
                                .w_full()
                                .h(rpx(ROW_H))
                                .flex()
                                .items_center()
                                .gap(rpx(6.))
                                .px(rpx(8.))
                                .bg(p.panel)
                                .border_b_1()
                                .border_color(p.line)
                                .child(ui::checkbox(("cmp-group-check", r), all, "", p).on_click(
                                    cx.listener(move |this, _, _, cx| {
                                        this.toggle_group(status, cx)
                                    }),
                                ))
                                .child(
                                    div()
                                        .text_size(ts::CAPTION_PLUS)
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(p.fg2)
                                        .child(format!("{} ({n})", status.label().to_uppercase())),
                                )
                                .into_any_element()
                        }
                        Row::Object(ix) => {
                            let o = &diff.objects[ix];
                            let is_sel = selected == Some(ix);
                            let on = included.get(ix).copied().unwrap_or(false);
                            div()
                                .id(("cmp-row", r))
                                .w_full()
                                .h(rpx(ROW_H))
                                .flex()
                                .items_center()
                                .gap(rpx(6.))
                                .px(rpx(8.))
                                .border_b_1()
                                .border_color(p.line)
                                .text_size(ts::BODY)
                                .when(is_sel, |d| d.bg(p.sel))
                                .when(!is_sel, |d| d.hover(|st| st.bg(p.hover)))
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, _, cx| this.select(ix, cx)))
                                .child(ui::checkbox(("cmp-check", r), on, "", p).on_click(
                                    cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.toggle(ix, cx)
                                    }),
                                ))
                                .child(
                                    div()
                                        .w(rpx(18.))
                                        .flex_none()
                                        .font_family(MONO)
                                        .text_size(ts::SMALL)
                                        .text_color(p.fg3)
                                        .child(o.kind.icon()),
                                )
                                .child(div().flex_1().min_w_0().truncate().child(o.label()))
                                .when(o.is_destructive(), |d| {
                                    d.child(
                                        div()
                                            .flex_none()
                                            .text_size(ts::TINY_PLUS)
                                            .text_color(p.prod)
                                            .child("drops data"),
                                    )
                                })
                                .when(!o.changes.is_empty(), |d| {
                                    d.child(
                                        div()
                                            .flex_none()
                                            .font_family(MONO)
                                            .text_size(ts::SMALL)
                                            .text_color(p.fg3)
                                            .child(o.changes.len().to_string()),
                                    )
                                })
                                .into_any_element()
                        }
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .flex_1()
        .into_any_element()
    }

    fn render_pane(
        id: &'static str,
        title: &str,
        lines: Arc<Vec<DiffLine>>,
        p: &Palette,
    ) -> AnyElement {
        let p2 = *p;
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(p.bd)
            .child(
                div()
                    .flex_none()
                    .h(rpx(ROW_H))
                    .flex()
                    .items_center()
                    .px(rpx(8.))
                    .border_b_1()
                    .border_color(p.bd)
                    .text_size(ts::CAPTION_PLUS)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.fg3)
                    .child(title.to_owned()),
            )
            .child(
                uniform_list(
                    id,
                    lines.len(),
                    move |range: std::ops::Range<usize>, _w, _cx| {
                        let p = &p2;
                        range
                            .map(|i| {
                                let (text, bg, fg) = match &lines[i] {
                                    DiffLine::Same(t) => (t.clone(), None, p.fg2),
                                    DiffLine::Removed(t) => (t.clone(), Some(p.prod_bg), p.fg),
                                    DiffLine::Added(t) => (t.clone(), Some(p.dev_bg), p.fg),
                                    DiffLine::Elided(n) => {
                                        (format!("… {n} more lines"), None, p.fg3)
                                    }
                                };
                                div()
                                    .h(rpx(18.))
                                    .px(rpx(8.))
                                    .font_family(MONO)
                                    .text_size(ts::SMALL)
                                    .text_color(fg)
                                    .whitespace_nowrap()
                                    .when_some(bg, |d, bg| d.bg(bg))
                                    .child(if text.is_empty() {
                                        " ".to_owned()
                                    } else {
                                        text
                                    })
                                    .into_any_element()
                            })
                            .collect::<Vec<_>>()
                    },
                )
                .flex_1(),
            )
            .into_any_element()
    }

    fn render_detail(&self, diff: &Arc<SchemaDiff>, p: &Palette) -> AnyElement {
        let Some((ix, left, right)) = self.ddl.clone() else {
            return div()
                .flex_1()
                .p(rpx(16.))
                .text_size(ts::BODY)
                .text_color(p.fg3)
                .child("Select an object to see its DDL on both sides.")
                .into_any_element();
        };
        let Some(o): Option<&ObjectDiff> = diff.objects.get(ix) else {
            return div().flex_1().into_any_element();
        };
        let changes = o.changes.iter().map(|c| {
            div()
                .font_family(MONO)
                .text_size(ts::SMALL)
                .text_color(p.fg2)
                .child(c.label())
        });
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(rpx(3.))
                    .px(rpx(12.))
                    .py(rpx(8.))
                    .border_b_1()
                    .border_color(p.bd)
                    .child(
                        div()
                            .flex()
                            .gap(rpx(8.))
                            .items_center()
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_size(ts::BASE)
                                    .child(o.label()),
                            )
                            .child(
                                div()
                                    .text_size(ts::SMALL)
                                    .text_color(p.fg3)
                                    .child(o.status.label()),
                            ),
                    )
                    .children(changes),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(Self::render_pane("cmp-ddl-target", "TARGET (now)", left, p))
                    .child(Self::render_pane(
                        "cmp-ddl-source",
                        "SOURCE (after the script)",
                        right,
                        p,
                    )),
            )
            .into_any_element()
    }
}

impl Render for CompareTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let running = self.running();
        let blocker = compare_blocker(
            self.sides[0].connection.as_ref(),
            self.sides[1].connection.as_ref(),
        );
        let pickers = div()
            .flex_none()
            .flex()
            .items_start()
            .gap(rpx(12.))
            .px(rpx(12.))
            .py(rpx(10.))
            .border_b_1()
            .border_color(p.bd)
            .child(self.render_side(0, &p, cx))
            .child(
                div()
                    .pt(rpx(20.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(rpx(6.))
                    .child(
                        ui::button("cmp-swap", "⇄ Swap", Kind::Ghost, &p)
                            .on_click(cx.listener(|t, _, _, cx| t.swap(cx))),
                    ),
            )
            .child(self.render_side(1, &p, cx))
            .child(div().pt(rpx(20.)).flex_none().child(if running {
                ui::button("cmp-cancel", "Cancel", Kind::Secondary, &p)
                    .on_click(cx.listener(|t, _, _, cx| t.cancel(cx)))
                    .into_any_element()
            } else {
                ui::button("cmp-run", "Compare", Kind::Primary, &p)
                    .when(blocker.is_some(), |b| b.opacity(0.6))
                    .on_click(cx.listener(|t, _, _, cx| t.start(cx)))
                    .into_any_element()
            }));
        let message = self.error.clone().or_else(|| {
            // Explain a disabled Compare once both sides are chosen.
            blocker.filter(|_| self.sides.iter().all(|s| s.connection.is_some()))
        });
        let message = message.map(|m| {
            div()
                .flex_none()
                .px(rpx(12.))
                .py(rpx(6.))
                .text_size(ts::BODY)
                .text_color(p.prod)
                .child(m)
        });
        let body = match self.diff.clone() {
            Some(diff) if diff.objects.is_empty() => div()
                .flex_1()
                .p(rpx(16.))
                .text_size(ts::BODY)
                .text_color(p.fg2)
                .child(format!(
                    "No differences: {} objects are identical.",
                    diff.identical
                ))
                .into_any_element(),
            Some(diff) => {
                let chosen = self.included.iter().filter(|v| **v).count();
                let footer = div()
                    .flex_none()
                    .h(rpx(36.))
                    .flex()
                    .items_center()
                    .gap(rpx(8.))
                    .px(rpx(10.))
                    .border_t_1()
                    .border_color(p.bd)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(ts::SMALL)
                            .text_color(p.fg3)
                            .child(format!(
                                "{chosen} of {} selected · {} identical",
                                diff.objects.len(),
                                diff.identical
                            )),
                    )
                    .child(
                        ui::button("cmp-script", "Open migration script", Kind::Primary, &p)
                            .when(chosen == 0, |b| b.opacity(0.6))
                            .on_click(cx.listener(|t, _, _, cx| t.open_script(cx))),
                    );
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(
                        div()
                            .w(rpx(360.))
                            .flex_none()
                            .flex()
                            .flex_col()
                            .border_r_1()
                            .border_color(p.bd)
                            .child(self.render_list(&diff, &p, cx))
                            .child(footer),
                    )
                    .child(self.render_detail(&diff, &p))
                    .into_any_element()
            }
            None => div()
                .flex_1()
                .p(rpx(16.))
                .text_size(ts::BODY)
                .text_color(p.fg3)
                .child(if running {
                    "Reading both schemas…"
                } else {
                    "Pick a source and a target, then Compare. The migration script makes the \
                     target match the source; it opens in a SQL tab and is never run for you."
                })
                .into_any_element(),
        };
        div()
            .id("compare-tab")
            .size_full()
            .flex()
            .flex_col()
            .font_family(SANS)
            .bg(p.bg)
            .text_color(p.fg)
            .child(
                div()
                    .flex_none()
                    .h(rpx(38.))
                    .flex()
                    .items_center()
                    .gap(rpx(10.))
                    .px(rpx(12.))
                    .border_b_1()
                    .border_color(p.bd)
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(ts::BASE)
                            .child("Schema compare"),
                    )
                    .child(
                        div()
                            .px(rpx(5.))
                            .rounded(px(3.))
                            .text_size(ts::SMALL)
                            .text_color(p.fg3)
                            .child("Tables · columns · keys · indexes · checks · views · routines"),
                    ),
            )
            .child(pickers)
            .children(message)
            .child(body)
    }
}

impl Workspace {
    /// Open a schema compare tab, the source preset to `source` / `schema` when given.
    pub(crate) fn open_compare(
        &mut self,
        source: Option<ProfileId>,
        schema: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let connections: Vec<DbConnection> = self
            .profiles
            .dbs()
            .filter(|d| d.engine.is_sql())
            .cloned()
            .collect();
        if connections.is_empty() {
            return self.toast("Add a database connection first", cx);
        }
        let source = source.and_then(|id| self.profiles.db(&id).cloned());
        if let Some(c) = &source
            && dialect_for(c.engine).schema_scripter().is_none()
        {
            return self.toast(compare::unsupported_message(c.engine), cx);
        }
        let core = self.core.clone();
        let tab = cx.new(|cx| CompareTab::new(core, connections, source, schema, cx));
        cx.subscribe_in(
            &tab,
            window,
            |this, _tab, ev: &CompareTabEvent, window, cx| match ev {
                CompareTabEvent::Toast(t) => this.toast(t.clone(), cx),
                // Opened as text only: the user reviews and runs it (or not).
                CompareTabEvent::OpenScript {
                    connection,
                    title,
                    script,
                } => {
                    let tab = this.open_query_tab(connection, title, window, cx);
                    let script = script.clone();
                    tab.update(cx, |t, cx| t.insert_text(&script, window, cx));
                }
            },
        )
        .detach();
        self.tabs.push(Tab::Compare(tab));
        self.activate(self.tabs.len() - 1, cx);
    }

    /// The palette's "Compare Schemas…": the source preset to the active query tab's
    /// connection, else the schema tree's.
    pub(crate) fn open_compare_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let conn = self
            .active_sql()
            .and_then(|t| t.read(cx).connection.clone())
            .map(|c| c.id)
            .or_else(|| self.explorer.scope_conn());
        self.open_compare(conn, None, window, cx);
    }

    /// Route the compare events to their tab.
    pub(crate) fn compare_tab(
        &self,
        request: RequestId,
        cx: &gpui_kit::App,
    ) -> Option<gpui_kit::Entity<CompareTab>> {
        self.tabs.iter().find_map(|t| match t {
            Tab::Compare(c) if c.read(cx).owns(request) => Some(c.clone()),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_core::db::ObjectKind;
    use switchyard_core::db::compare::{ObjectDiff, SchemaDiff};

    fn obj(name: &str, status: DiffStatus) -> ObjectDiff {
        ObjectDiff {
            kind: ObjectKind::Table,
            name: name.into(),
            signature: None,
            status,
            changes: Vec::new(),
            source: None,
            target: None,
        }
    }

    fn diff() -> SchemaDiff {
        SchemaDiff {
            engine: Engine::Postgres,
            source_schema: "a".into(),
            target_schema: "b".into(),
            objects: vec![
                obj("n1", DiffStatus::OnlyInSource),
                obj("n2", DiffStatus::OnlyInSource),
                obj("old", DiffStatus::OnlyInTarget),
                obj("t", DiffStatus::Different),
            ],
            identical: 3,
            unchanged_views: Vec::new(),
        }
    }

    #[test]
    fn rows_are_grouped_with_headings() {
        assert_eq!(
            rows_for(&diff()),
            [
                Row::Group(DiffStatus::OnlyInSource, 2),
                Row::Object(0),
                Row::Object(1),
                Row::Group(DiffStatus::OnlyInTarget, 1),
                Row::Object(2),
                Row::Group(DiffStatus::Different, 1),
                Row::Object(3),
            ]
        );
    }

    #[test]
    fn drops_are_not_selected_by_default() {
        assert_eq!(default_selection(&diff()), [true, true, false, true]);
    }

    #[test]
    fn only_one_supported_engine_family_compares() {
        let conn = |engine| DbConnection::new("x", engine);
        assert!(compare_blocker(None, Some(&conn(Engine::Postgres))).is_some());
        assert_eq!(
            compare_blocker(Some(&conn(Engine::Postgres)), Some(&conn(Engine::MySql))).as_deref(),
            Some("Both sides must be the same engine (PostgreSQL vs MySQL)")
        );
        assert_eq!(
            compare_blocker(Some(&conn(Engine::Oracle)), Some(&conn(Engine::Oracle))).as_deref(),
            Some("Schema compare isn't supported for Oracle yet")
        );
        assert_eq!(
            compare_blocker(
                Some(&conn(Engine::SqlServer)),
                Some(&conn(Engine::SqlServer))
            ),
            None
        );
    }
}
