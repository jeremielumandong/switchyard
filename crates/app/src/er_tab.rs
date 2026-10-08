//! ER diagram tab (DBX-5d): the tables of one schema as boxes (columns with PK/FK marks)
//! and their foreign keys as lines with crow's-foot ends.
//!
//! The tab owns a catalog session on its connection. It reads the schema's table list,
//! then each selected table's `Detail` (cached by core, so reopening is cheap), at most
//! [`model::MAX_TABLES`] tables, narrowed with a name filter or "only tables related to
//! X". Graph building and layout run on the background executor; each selection's result
//! is cached in the tab. Drag pans, the wheel or a pinch zooms, Fit fits.

mod model;
mod svg;

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Bounds, ClickEvent, ClipboardItem, Context, Entity, EventEmitter,
    FontWeight, InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    ParentElement as _, PathBuilder, PinchEvent, Pixels, Render, ScrollDelta, ScrollWheelEvent,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, canvas,
    div, point, px,
};
use switchyard_core::db::{CatalogChunk, IntrospectScope, ObjectDetail, ObjectKind};
use switchyard_core::store::{DbConnection, ProfileId};
use switchyard_core::{Command, RuntimeHandle, SessionId};

use crate::app_state::{SessionState, next_id};
use crate::theme::{MONO, Palette, palette};
use crate::ui::{self, Kind};
use crate::workspace::{Tab, Workspace};
use model::{ErGraph, ErLayout, HEADER_H, MAX_COLUMNS, MAX_TABLES, ROW_H};

/// Detail requests in flight at once.
const IN_FLIGHT: usize = 6;
const MIN_ZOOM: f32 = 0.15;
const MAX_ZOOM: f32 = 2.0;
/// Below this zoom the boxes show only their names.
const ROWS_ZOOM: f32 = 0.4;
const FIT_MARGIN: f32 = 24.;

/// What the tab asks the workspace to do.
pub enum ErTabEvent {
    /// Open the properties tab of a table (double-click).
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
}

/// A table's detail.
enum DetailState {
    Loading,
    Loaded(Arc<ObjectDetail>),
    Failed,
}

/// A laid-out diagram and the selection it was built from.
struct Diagram {
    key: Vec<String>,
    graph: Arc<ErGraph>,
    layout: Arc<ErLayout>,
}

/// A pan in progress. It stays (with `moved`) after the button is released so a box's
/// click, which fires on mouse-up, can tell a drag from a click.
struct Drag {
    start: (f32, f32),
    pan0: (f32, f32),
    moved: bool,
}

/// An ER diagram tab for one schema.
pub struct ErTab {
    /// `<connection id>/<schema>`: which diagram this tab shows.
    pub key: String,
    /// Tab title.
    pub title: SharedString,
    /// The connection.
    pub connection: DbConnection,
    schema: String,
    core: RuntimeHandle,
    session: Option<SessionId>,
    session_state: SessionState,
    /// Table names of the schema, sorted.
    tables: Option<Vec<String>>,
    error: Option<String>,
    details: HashMap<String, DetailState>,
    queue: VecDeque<String>,
    in_flight: usize,
    /// Read from the server, not the cache (Refresh).
    refresh: bool,
    filter: Entity<InputState>,
    /// "Only tables related to X".
    related: Option<String>,
    selection: Vec<String>,
    /// Tables that matched the filter (the selection is capped).
    matched: usize,
    diagram: Option<Diagram>,
    cache: HashMap<Vec<String>, (Arc<ErGraph>, Arc<ErLayout>)>,
    pending: Option<Vec<String>>,
    _layout_task: Option<Task<()>>,
    /// Highlighted table (schema, name).
    selected: Option<(String, String)>,
    zoom: f32,
    pan: (f32, f32),
    drag: Option<Drag>,
    viewport: Rc<Cell<Bounds<Pixels>>>,
    fit_pending: bool,
    _sub: Subscription,
}

impl EventEmitter<ErTabEvent> for ErTab {}

impl ErTab {
    /// The key of the diagram of `schema` on `connection`.
    pub fn key_for(connection: &DbConnection, schema: &str) -> String {
        format!("{}/{schema}", connection.id)
    }

    /// A diagram of `schema`, optionally limited to the tables related to `focus`; it
    /// opens its own session on `connection`.
    pub fn new(
        core: RuntimeHandle,
        connection: DbConnection,
        schema: String,
        focus: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let session = next_id();
        core.send(Command::OpenSession {
            session,
            connection: connection.id.clone(),
        });
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter tables…"));
        let sub = cx.subscribe(&filter, |this: &mut Self, _, ev: &InputEvent, cx| {
            if let InputEvent::Change = ev {
                this.update_selection(cx);
            }
        });
        Self {
            key: Self::key_for(&connection, &schema),
            title: format!("ER · {schema}").into(),
            connection,
            schema,
            core,
            session: Some(session),
            session_state: SessionState::Connecting,
            tables: None,
            error: None,
            details: HashMap::new(),
            queue: VecDeque::new(),
            in_flight: 0,
            refresh: false,
            filter,
            related: focus,
            selection: Vec::new(),
            matched: 0,
            diagram: None,
            cache: HashMap::new(),
            pending: None,
            _layout_task: None,
            selected: None,
            zoom: 1.0,
            pan: (0., 0.),
            drag: None,
            viewport: Rc::new(Cell::new(Bounds::default())),
            fit_pending: false,
            _sub: sub,
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

    /// Close the session (the tab is closing).
    pub fn shutdown(&mut self) {
        if let Some(s) = self.session.take() {
            self.core.send(Command::CloseSession { session: s });
        }
    }

    /// Limit the diagram to the tables related to `name` (or show all again).
    pub fn set_related(&mut self, name: Option<String>, cx: &mut Context<Self>) {
        self.related = name;
        self.update_selection(cx);
        cx.notify();
    }

    /// Session lifecycle updates from the workspace.
    pub fn on_session(&mut self, state: SessionState, cx: &mut Context<Self>) {
        if let SessionState::Failed(e) = &state {
            self.error = Some(e.clone());
        }
        let opened = matches!(state, SessionState::Open { .. });
        self.session_state = state;
        if opened {
            self.request_tables();
        }
        cx.notify();
    }

    fn request_tables(&mut self) {
        let Some(session) = self.session else { return };
        self.core.send(Command::Introspect {
            session,
            scope: IntrospectScope::Objects {
                schema: self.schema.clone(),
                kind: ObjectKind::Table,
            },
            refresh: self.refresh,
        });
    }

    /// Re-read the table list and details from the server.
    fn reload(&mut self, cx: &mut Context<Self>) {
        if let SessionState::Failed(_) = self.session_state {
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
        }
        self.refresh = true;
        self.error = None;
        self.tables = None;
        self.details.clear();
        self.queue.clear();
        self.in_flight = 0;
        self.cache.clear();
        self.pending = None;
        self.diagram = None;
        if matches!(self.session_state, SessionState::Open { .. }) {
            self.request_tables();
        }
        cx.notify();
    }

    /// A catalog answer on this tab's session.
    pub fn on_catalog(
        &mut self,
        scope: IntrospectScope,
        result: Result<CatalogChunk, String>,
        cx: &mut Context<Self>,
    ) {
        match scope {
            IntrospectScope::Objects { schema, kind } => {
                if schema != self.schema || kind != ObjectKind::Table {
                    return;
                }
                match result {
                    Ok(CatalogChunk::Objects(list)) => {
                        let mut names: Vec<String> = list
                            .into_iter()
                            .filter(|o| o.kind == ObjectKind::Table)
                            .map(|o| o.name)
                            .collect();
                        names.sort();
                        names.dedup();
                        self.tables = Some(names);
                        self.update_selection(cx);
                    }
                    Ok(_) => {}
                    Err(e) => self.error = Some(e),
                }
            }
            IntrospectScope::Detail { schema, name, kind } => {
                if schema != self.schema || kind != ObjectKind::Table {
                    return;
                }
                if !matches!(self.details.get(&name), Some(DetailState::Loading)) {
                    return;
                }
                self.in_flight = self.in_flight.saturating_sub(1);
                let state = match result {
                    Ok(CatalogChunk::Detail(d)) => DetailState::Loaded(Arc::from(d)),
                    Ok(_) => DetailState::Failed,
                    Err(e) => {
                        tracing::warn!(error = %e, "ER diagram: table detail failed");
                        DetailState::Failed
                    }
                };
                self.details.insert(name, state);
                if self.queue.is_empty() && self.in_flight == 0 {
                    self.refresh = false;
                }
                if self.related.is_some() {
                    // A new detail may add related tables.
                    self.update_selection(cx);
                } else {
                    self.pump();
                    self.build(cx);
                }
            }
            _ => return,
        }
        cx.notify();
    }

    /// Recompute the selection from the filter and the related table, ask for missing
    /// details and rebuild when they are all in.
    fn update_selection(&mut self, cx: &mut Context<Self>) {
        let Some(all) = &self.tables else { return };
        let filter = self.filter.read(cx).value().to_string();
        let related = self.related.as_ref().map(|focus| {
            let loaded = self.details.values().filter_map(|d| match d {
                DetailState::Loaded(d) => Some(d.as_ref()),
                _ => None,
            });
            model::related_names(focus, &self.schema, loaded)
        });
        let (mut selection, matched) =
            model::select_tables(all, &filter, related.as_ref(), MAX_TABLES);
        let mut wanted = selection.clone();
        if let Some(focus) = &self.related {
            // The focus table's own foreign keys decide what is related.
            wanted.insert(0, focus.clone());
            if !selection.contains(focus) && all.contains(focus) && filter.trim().is_empty() {
                selection.insert(0, focus.clone());
            }
        }
        self.selection = selection;
        self.matched = matched;
        for name in wanted {
            if !self.details.contains_key(&name) {
                self.details.insert(name.clone(), DetailState::Loading);
                self.queue.push_back(name);
            }
        }
        self.pump();
        self.build(cx);
        cx.notify();
    }

    /// Send queued detail requests, a few at a time.
    fn pump(&mut self) {
        let Some(session) = self.session else { return };
        if !matches!(self.session_state, SessionState::Open { .. }) {
            return;
        }
        while self.in_flight < IN_FLIGHT {
            let Some(name) = self.queue.pop_front() else {
                break;
            };
            self.in_flight += 1;
            self.core.send(Command::Introspect {
                session,
                scope: IntrospectScope::Detail {
                    schema: self.schema.clone(),
                    name,
                    kind: ObjectKind::Table,
                },
                refresh: self.refresh,
            });
        }
    }

    /// Selected tables whose detail is still loading.
    fn loading(&self) -> usize {
        self.selection
            .iter()
            .filter(|n| matches!(self.details.get(*n), Some(DetailState::Loading) | None))
            .count()
    }

    /// Build and lay out the selection on the background executor once its details are
    /// in (or reuse the cached result).
    fn build(&mut self, cx: &mut Context<Self>) {
        if self.tables.is_none() || self.loading() > 0 {
            return;
        }
        let key = self.selection.clone();
        if self.diagram.as_ref().is_some_and(|d| d.key == key)
            || self.pending.as_ref() == Some(&key)
        {
            return;
        }
        if let Some((graph, layout)) = self.cache.get(&key) {
            self.show(key, graph.clone(), layout.clone());
            return;
        }
        let details: Vec<Arc<ObjectDetail>> = key
            .iter()
            .filter_map(|n| match self.details.get(n) {
                Some(DetailState::Loaded(d)) => Some(d.clone()),
                _ => None,
            })
            .collect();
        self.pending = Some(key.clone());
        let task = cx.spawn(async move |this, cx| {
            let (graph, layout) = cx
                .background_executor()
                .spawn(async move {
                    let refs: Vec<&ObjectDetail> = details.iter().map(|d| d.as_ref()).collect();
                    let graph = model::build_graph(&refs, MAX_COLUMNS);
                    let layout = model::layout(&graph);
                    (Arc::new(graph), Arc::new(layout))
                })
                .await;
            let _ = this.update(cx, |t, cx| {
                if t.pending.as_ref() == Some(&key) {
                    t.pending = None;
                    t.cache.insert(key.clone(), (graph.clone(), layout.clone()));
                    t.show(key, graph, layout);
                    cx.notify();
                }
            });
        });
        self._layout_task = Some(task);
    }

    fn show(&mut self, key: Vec<String>, graph: Arc<ErGraph>, layout: Arc<ErLayout>) {
        self.diagram = Some(Diagram { key, graph, layout });
        self.fit_pending = true;
    }

    fn set_zoom(&mut self, z: f32, anchor: Option<(f32, f32)>, cx: &mut Context<Self>) {
        let z = z.clamp(MIN_ZOOM, MAX_ZOOM);
        let b = self.viewport.get();
        let (ax, ay) =
            anchor.unwrap_or((f32::from(b.size.width) / 2., f32::from(b.size.height) / 2.));
        let ratio = z / self.zoom;
        self.pan = (
            ax - (ax - self.pan.0) * ratio,
            ay - (ay - self.pan.1) * ratio,
        );
        self.zoom = z;
        cx.notify();
    }

    /// Fit the diagram into the pane. False until the pane has been measured.
    fn fit(&mut self, cx: &mut Context<Self>) -> bool {
        let b = self.viewport.get();
        let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
        if w <= 0. || h <= 0. {
            return false;
        }
        self.fit_pending = false;
        let Some(d) = &self.diagram else { return true };
        let (lw, lh) = (d.layout.width.max(1.), d.layout.height.max(1.));
        let z = ((w - 2. * FIT_MARGIN) / lw)
            .min((h - 2. * FIT_MARGIN) / lh)
            .clamp(MIN_ZOOM, 1.0);
        self.zoom = z;
        self.pan = (
            ((w - lw * z) / 2.).max(FIT_MARGIN.min(w / 2.)),
            ((h - lh * z) / 2.).max(FIT_MARGIN.min(h / 2.)),
        );
        cx.notify();
        true
    }

    fn on_wheel(&mut self, ev: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let b = self.viewport.get();
        let anchor = (
            f32::from(ev.position.x - b.origin.x),
            f32::from(ev.position.y - b.origin.y),
        );
        match ev.delta {
            // A mouse wheel zooms (Shift scrolls sideways).
            ScrollDelta::Lines(l) if !ev.modifiers.shift => {
                if l.y != 0. {
                    let factor = 1.15_f32.powf(l.y.clamp(-3., 3.));
                    self.set_zoom(self.zoom * factor, Some(anchor), cx);
                }
            }
            ScrollDelta::Lines(l) => {
                self.pan.0 += (l.x + l.y) * 40.;
                cx.notify();
            }
            // A trackpad pans; with Ctrl/Cmd it zooms.
            ScrollDelta::Pixels(p) => {
                let (dx, dy) = (f32::from(p.x), f32::from(p.y));
                if ev.modifiers.secondary() {
                    let factor = (1. + dy / 200.).clamp(0.5, 2.);
                    self.set_zoom(self.zoom * factor, Some(anchor), cx);
                } else {
                    self.pan = (self.pan.0 + dx, self.pan.1 + dy);
                    cx.notify();
                }
            }
        }
    }

    fn svg(&self) -> Option<String> {
        let d = self.diagram.as_ref()?;
        (!d.graph.tables.is_empty()).then(|| svg::to_svg(&d.graph, &d.layout))
    }

    fn copy_svg(&mut self, cx: &mut Context<Self>) {
        match self.svg() {
            Some(s) => {
                cx.write_to_clipboard(ClipboardItem::new_string(s));
                cx.emit(ErTabEvent::Toast("Copied the diagram as SVG".into()));
            }
            None => cx.emit(ErTabEvent::Toast("Nothing to copy yet".into())),
        }
    }

    fn save_svg(&mut self, cx: &mut Context<Self>) {
        let Some(text) = self.svg() else {
            cx.emit(ErTabEvent::Toast("Nothing to save yet".into()));
            return;
        };
        let dir = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_default();
        let name = format!("{}-er.svg", self.schema);
        let rx = cx.prompt_for_new_path(&dir, Some(&name));
        let core = self.core.clone();
        cx.spawn(async move |_, _| {
            if let Ok(Ok(Some(path))) = rx.await {
                core.send(Command::WriteFile {
                    path,
                    contents: text,
                });
            }
        })
        .detach();
    }

    fn status(&self) -> String {
        if let Some(e) = &self.error {
            return format!("Failed: {e}");
        }
        match &self.session_state {
            SessionState::Connecting | SessionState::None => return "Connecting…".into(),
            SessionState::Failed(e) => return format!("Connection failed: {e}"),
            SessionState::Open { .. } => {}
        }
        if self.tables.is_none() {
            return "Reading tables…".into();
        }
        let loading = self.loading();
        if loading > 0 {
            let total = self.selection.len();
            return format!("Reading table details… {}/{total}", total - loading);
        }
        if self.pending.is_some() {
            return "Laying out…".into();
        }
        let Some(d) = &self.diagram else {
            return String::new();
        };
        let tables = d.graph.tables.iter().filter(|t| !t.stub).count();
        let mut s = format!(
            "{tables} {} · {} foreign {}",
            if tables == 1 { "table" } else { "tables" },
            d.graph.edges.len(),
            if d.graph.edges.len() == 1 {
                "key"
            } else {
                "keys"
            }
        );
        if self.matched > self.selection.len() {
            s = format!(
                "Showing {} of {} tables · narrow with the filter or “Only related”",
                self.selection.len(),
                self.matched
            );
        }
        let failed = self
            .selection
            .iter()
            .filter(|n| matches!(self.details.get(*n), Some(DetailState::Failed)))
            .count();
        if failed > 0 {
            s.push_str(&format!(" · {failed} could not be read"));
        }
        s
    }

    fn selected_index(&self) -> Option<usize> {
        let (s, n) = self.selected.as_ref()?;
        self.diagram.as_ref()?.graph.find(s, n)
    }

    fn render_toolbar(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.selected.clone();
        div()
            .h(px(36.))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(6.))
            .px(px(10.))
            .border_b_1()
            .border_color(p.bd)
            .bg(p.panel)
            .text_size(px(12.))
            .child(
                div()
                    .w(px(200.))
                    .flex_none()
                    .h(px(26.))
                    .px(px(6.))
                    .flex()
                    .items_center()
                    .rounded(px(5.))
                    .border_1()
                    .border_color(p.bd2)
                    .bg(p.surface)
                    .child(
                        Input::new(&self.filter)
                            .appearance(false)
                            .text_size(px(12.)),
                    ),
            )
            .map(|d| match &self.related {
                Some(focus) => d.child(
                    ui::button(
                        "er-related-clear",
                        format!("Related to {focus}  ×"),
                        Kind::Secondary,
                        p,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.set_related(None, cx))),
                ),
                None => d.child(
                    ui::button("er-related", "Only related", Kind::Ghost, p)
                        .when(selected.is_none(), |b| b.opacity(0.5))
                        .on_click(cx.listener(move |this, _, _, cx| match &selected {
                            Some((_, name)) => this.set_related(Some(name.clone()), cx),
                            None => cx.emit(ErTabEvent::Toast("Select a table first".into())),
                        })),
                ),
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(p.fg3)
                    .child(self.status()),
            )
            .child(
                ui::button("er-zoom-out", "−", Kind::Ghost, p).on_click(
                    cx.listener(|this, _, _, cx| this.set_zoom(this.zoom / 1.25, None, cx)),
                ),
            )
            .child(
                div()
                    .w(px(40.))
                    .flex()
                    .justify_center()
                    .font_family(MONO)
                    .text_size(px(11.))
                    .text_color(p.fg2)
                    .child(format!("{:.0}%", self.zoom * 100.)),
            )
            .child(
                ui::button("er-zoom-in", "+", Kind::Ghost, p).on_click(
                    cx.listener(|this, _, _, cx| this.set_zoom(this.zoom * 1.25, None, cx)),
                ),
            )
            .child(
                ui::button("er-fit", "Fit", Kind::Ghost, p).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.fit(cx);
                    },
                )),
            )
            .child(ui::vdivider(p, 16.))
            .child(
                ui::button("er-refresh", "Refresh", Kind::Ghost, p)
                    .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
            )
            .child(
                ui::button("er-copy-svg", "Copy as SVG", Kind::Ghost, p)
                    .on_click(cx.listener(|this, _, _, cx| this.copy_svg(cx))),
            )
            .child(
                ui::button("er-save-svg", "Save as SVG…", Kind::Secondary, p)
                    .on_click(cx.listener(|this, _, _, cx| this.save_svg(cx))),
            )
            .into_any_element()
    }

    fn render_diagram(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let viewport = self.viewport.clone();
        let vb = viewport.get();
        let (vw, vh) = (f32::from(vb.size.width), f32::from(vb.size.height));
        let known = vw > 0. && vh > 0.;
        let visible = |x: f32, y: f32, w: f32, h: f32| {
            !known || (x + w >= -40. && x <= vw + 40. && y + h >= -40. && y <= vh + 40.)
        };
        let z = self.zoom;
        let (px0, py0) = self.pan;
        let sel = self.selected_index();
        let mut edges: Vec<(Vec<(f32, f32)>, bool)> = Vec::new();
        let mut nodes: Vec<AnyElement> = Vec::new();
        if let Some(d) = &self.diagram {
            let g = &d.graph;
            let l = &d.layout;
            for (e, route) in g.edges.iter().zip(&l.routes) {
                let pts: Vec<(f32, f32)> = route
                    .iter()
                    .map(|&(x, y)| (px0 + x * z, py0 + y * z))
                    .collect();
                let (x0, x1) = pts
                    .iter()
                    .fold((f32::MAX, f32::MIN), |a, p| (a.0.min(p.0), a.1.max(p.0)));
                let (y0, y1) = pts
                    .iter()
                    .fold((f32::MAX, f32::MIN), |a, p| (a.0.min(p.1), a.1.max(p.1)));
                if !visible(x0, y0, x1 - x0, y1 - y0) {
                    continue;
                }
                let hot = sel.is_some_and(|s| e.from == s || e.to == s);
                edges.push((route.clone(), hot));
            }
            for (i, (t, b)) in g.tables.iter().zip(&l.boxes).enumerate() {
                let (x, y, w, h) = (px0 + b.x * z, py0 + b.y * z, b.w * z, b.h * z);
                if !visible(x, y, w, h) {
                    continue;
                }
                let is_sel = sel == Some(i);
                let linked = sel.is_some_and(|s| {
                    g.edges
                        .iter()
                        .any(|e| (e.from == s && e.to == i) || (e.to == s && e.from == i))
                });
                let (schema, name) = (t.schema.clone(), t.name.clone());
                let rows = z >= ROWS_ZOOM;
                nodes.push(
                    div()
                        .id(("er-table", i))
                        .absolute()
                        .left(px(x))
                        .top(px(y))
                        .w(px(w))
                        .h(px(h))
                        .flex()
                        .flex_col()
                        .overflow_hidden()
                        .rounded(px(5. * z))
                        .border_1()
                        .when(t.stub, |d| d.border_dashed())
                        .border_color(if is_sel {
                            p.acc
                        } else if linked {
                            p.fg3
                        } else {
                            p.bd2
                        })
                        .bg(if t.stub { p.panel } else { p.elev })
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, ev: &ClickEvent, _, cx| {
                            if this.drag.as_ref().is_some_and(|d| d.moved) {
                                return;
                            }
                            if ev.click_count() >= 2 {
                                cx.emit(ErTabEvent::OpenObject {
                                    schema: schema.clone(),
                                    name: name.clone(),
                                    kind: ObjectKind::Table,
                                });
                                return;
                            }
                            let key = (schema.clone(), name.clone());
                            this.selected = (this.selected.as_ref() != Some(&key)).then_some(key);
                            cx.notify();
                        }))
                        .child(
                            div()
                                .h(px(HEADER_H * z))
                                .flex_none()
                                .flex()
                                .items_center()
                                .px(px(8. * z))
                                .gap(px(6. * z))
                                .bg(if is_sel { p.sel } else { p.surface })
                                .border_b_1()
                                .border_color(p.bd)
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_size(px(12. * z))
                                        .text_color(if t.stub { p.fg2 } else { p.fg })
                                        .child(t.name.clone()),
                                )
                                .when(t.stub && t.schema != self.schema, |d| {
                                    d.child(
                                        div()
                                            .flex_none()
                                            .text_size(px(10. * z))
                                            .text_color(p.fg3)
                                            .child(t.schema.clone()),
                                    )
                                }),
                        )
                        .when(rows, |d| {
                            d.children(t.columns.iter().map(|c| {
                                let mark = match (c.pk, c.fk) {
                                    (true, true) => "PF",
                                    (true, false) => "PK",
                                    (false, true) => "FK",
                                    _ => "",
                                };
                                div()
                                    .h(px(ROW_H * z))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .gap(px(4. * z))
                                    .px(px(6. * z))
                                    .font_family(MONO)
                                    .text_size(px(11. * z))
                                    .child(
                                        div()
                                            .w(px(16. * z))
                                            .flex_none()
                                            .text_size(px(8.5 * z))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(if c.pk { p.sx_num } else { p.acc })
                                            .child(mark),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .text_color(p.fg)
                                            .when(c.pk, |d| d.font_weight(FontWeight::SEMIBOLD))
                                            .child(c.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .max_w(px(110. * z))
                                            .truncate()
                                            .text_color(p.fg3)
                                            .child(c.data_type.clone()),
                                    )
                            }))
                            .when(t.hidden > 0, |d| {
                                d.child(
                                    div()
                                        .h(px(ROW_H * z))
                                        .flex_none()
                                        .flex()
                                        .items_center()
                                        .pl(px(26. * z))
                                        .text_size(px(11. * z))
                                        .text_color(p.fg3)
                                        .child(format!("+{} more", t.hidden)),
                                )
                            })
                        })
                        .into_any_element(),
                );
            }
        }
        let (normal, hot) = (p.fg3, p.acc);
        let edges_canvas = canvas(
            move |bounds, _, _| viewport.set(bounds),
            move |bounds, _, window, _| {
                let o = bounds.origin;
                let at = |x: f32, y: f32| o + point(px(px0 + x * z), px(py0 + y * z));
                // Normal edges first so highlighted ones draw on top.
                for pass in [false, true] {
                    for (route, is_hot) in edges.iter().filter(|(_, h)| *h == pass) {
                        let width = if *is_hot { 2.0 } else { 1.2 } * z.clamp(0.6, 1.5);
                        let color = if *is_hot { hot } else { normal };
                        let mut path = PathBuilder::stroke(px(width));
                        for (k, &(x, y)) in route.iter().enumerate() {
                            if k == 0 {
                                path.move_to(at(x, y));
                            } else {
                                path.line_to(at(x, y));
                            }
                        }
                        if let Ok(path) = path.build() {
                            window.paint_path(path, color);
                        }
                        for ((x1, y1), (x2, y2)) in model::edge_marks(route) {
                            let mut m = PathBuilder::stroke(px(width));
                            m.move_to(at(x1, y1));
                            m.line_to(at(x2, y2));
                            if let Ok(m) = m.build() {
                                window.paint_path(m, color);
                            }
                        }
                    }
                }
            },
        )
        .absolute()
        .size_full();
        let empty = self
            .diagram
            .as_ref()
            .is_some_and(|d| d.graph.tables.is_empty());
        div()
            .id("er-canvas")
            .relative()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .bg(p.bg)
            .cursor_grab()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _, _| {
                    this.drag = Some(Drag {
                        start: (f32::from(ev.position.x), f32::from(ev.position.y)),
                        pan0: this.pan,
                        moved: false,
                    });
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                let Some(d) = &mut this.drag else { return };
                if ev.pressed_button != Some(MouseButton::Left) {
                    return;
                }
                let (dx, dy) = (
                    f32::from(ev.position.x) - d.start.0,
                    f32::from(ev.position.y) - d.start.1,
                );
                if dx.abs() + dy.abs() > 3. {
                    d.moved = true;
                }
                this.pan = (d.pan0.0 + dx, d.pan0.1 + dy);
                cx.notify();
            }))
            .on_scroll_wheel(
                cx.listener(|this, ev: &ScrollWheelEvent, _, cx| this.on_wheel(ev, cx)),
            )
            .on_pinch(cx.listener(|this, ev: &PinchEvent, _, cx| {
                let b = this.viewport.get();
                let anchor = (
                    f32::from(ev.position.x - b.origin.x),
                    f32::from(ev.position.y - b.origin.y),
                );
                this.set_zoom(this.zoom * (1. + ev.delta), Some(anchor), cx);
            }))
            .child(edges_canvas)
            .children(nodes)
            .when(empty, |d| {
                d.child(
                    div()
                        .absolute()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(p.fg3)
                        .child("No tables match"),
                )
            })
            .into_any_element()
    }
}

impl Render for ErTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        if self.fit_pending && !self.fit(cx) {
            // Not laid out yet: try again once this frame has measured the pane.
            cx.on_next_frame(window, |this, _, cx| {
                if this.fit_pending {
                    cx.notify();
                }
            });
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(p.surface)
            .child(self.render_toolbar(&p, cx))
            .child(self.render_diagram(&p, cx))
    }
}

impl Workspace {
    /// Open (or focus) the ER diagram of `schema` on the schema tree's connection,
    /// optionally limited to the tables related to `focus`.
    pub(crate) fn open_er_diagram(
        &mut self,
        conn: &ProfileId,
        schema: String,
        focus: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(connection) = self.profiles.db(conn).cloned() else {
            self.toast("Connect to a database first", cx);
            return;
        };
        let key = ErTab::key_for(&connection, &schema);
        if let Some(ix) = self
            .tabs
            .iter()
            .position(|t| matches!(t, Tab::Er(e) if e.read(cx).key == key))
        {
            if let (Some(focus), Tab::Er(e)) = (focus, &self.tabs[ix]) {
                e.update(cx, |e, cx| e.set_related(Some(focus), cx));
            }
            self.activate(ix, cx);
            return;
        }
        let core = self.core.clone();
        let tab = cx.new(|cx| ErTab::new(core, connection, schema, focus, window, cx));
        cx.subscribe_in(
            &tab,
            window,
            |this, tab, ev: &ErTabEvent, window, cx| match ev {
                ErTabEvent::OpenObject { schema, name, kind } => {
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
                ErTabEvent::Toast(t) => this.toast(t.clone(), cx),
            },
        )
        .detach();
        self.tabs.push(Tab::Er(tab));
        self.activate(self.tabs.len() - 1, cx);
    }

    /// The palette's "ER diagram for current schema": the schema under the tree cursor or
    /// of the selected object, else the only (or first) non-system schema.
    pub(crate) fn open_er_for_current_schema(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.current_schema() {
            Some((conn, schema)) => self.open_er_diagram(&conn, schema, None, window, cx),
            None => self.toast("Open a database connection and pick a schema first", cx),
        }
    }

    /// The schema the tree points at, on its connection: under the cursor, of the
    /// selected object, else the only (or first expanded) user schema of the connection
    /// in scope.
    fn current_schema(&self) -> Option<(ProfileId, String)> {
        let ex = &self.explorer;
        if let Some(c) = &ex.cursor
            && let Some(conn) = &c.conn
            && ex.contains(conn)
            && let Some(s) = crate::sidebar::schema_of_row_key(&c.key)
        {
            return Some((conn.clone(), s));
        }
        if let Some(o) = &ex.selected
            && !o.schema.is_empty()
        {
            return Some((o.conn.clone(), o.schema.clone()));
        }
        let conn = ex.scope_conn()?;
        let state = ex.state(&conn)?;
        let crate::sidebar::Loadable::Loaded(list) = &state.schemas else {
            return None;
        };
        let user: Vec<&str> = list
            .iter()
            .filter(|s| !s.is_system)
            .map(|s| s.name.as_str())
            .collect();
        let expanded: Vec<&&str> = user
            .iter()
            .filter(|s| state.expanded.contains(&format!("s:{s}")))
            .collect();
        expanded
            .first()
            .map(|s| s.to_string())
            .or_else(|| user.first().map(|s| s.to_string()))
            .map(|s| (conn, s))
    }

    /// The ER tab that owns `session`, if any.
    pub(crate) fn er_tab_for_session(
        &self,
        session: SessionId,
        cx: &gpui_kit::App,
    ) -> Option<Entity<ErTab>> {
        self.tabs.iter().find_map(|t| match t {
            Tab::Er(e) if e.read(cx).owns_session(session) => Some(e.clone()),
            _ => None,
        })
    }
}
