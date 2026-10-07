//! Plan view, shown as the SQL tab's "Plan" result tab: a graph of the plan (colour by
//! share of time or cost, edge width by rows) or a flame view, the hotspot list, the
//! selected node's details, and side-by-side comparison with another plan from this tab
//! or from history.

pub mod layout;

use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Bounds, ClickEvent, Context, Div, EventEmitter, FontWeight, Hsla,
    InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    ParentElement as _, PathBuilder, Pixels, Render, ScrollDelta, ScrollWheelEvent, SharedString,
    StatefulInteractiveElement as _, Styled as _, Task, Window, canvas, deferred, div, point, px,
    relative,
};
use gpui_kit::{AppContext as _, Entity};
use switchyard_core::plan::whatif::WhatIf;
use switchyard_core::plan::{
    Comparison, Finding, Pair, Plan, PlanKind, PlanNode, PlanSource, Severity, Thresholds,
};
use switchyard_core::store::{HistoryEntry, ProfileId};
use switchyard_core::{Command, QueryId, RequestId, RuntimeHandle, SessionId};

use crate::app_state::next_id;
use crate::theme::{MONO, Palette, SANS, palette};
use crate::ui::{self, Kind, thousands};
use layout::{FlameBar, GraphLayout, Heat, NODE_H, NODE_W};

/// What the plan view asks its SQL tab to do.
pub enum PlanViewEvent {
    /// Highlight `ranges` of the plan's statement `sql` in the editor (empty clears).
    /// `offset` is where the statement started in the buffer when it was explained.
    Highlight {
        sql: String,
        offset: Option<usize>,
        ranges: Vec<Range<usize>>,
    },
    /// Capture again (Retry, or the Production confirmation) on the tab's session.
    Rerun(ExplainRequest),
    /// Show a toast.
    Toast(String),
    /// Ask the assistant to optimize the plan's statement (with the plan's findings).
    Optimize {
        /// The statement.
        sql: String,
        /// The findings, one line each.
        findings: Vec<String>,
    },
}

/// What the assistant's suggestion cards ask the plan view to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AfterPlan {
    /// Plan this rewrite and compare it with the current plan.
    Compare(String),
    /// Plan the current statement with these hypothetical indexes and compare.
    WhatIf(Vec<String>),
    /// Plan the current statement again and compare (statistics changed).
    Replan,
}

/// Whitespace- and semicolon-insensitive statement equality.
fn same_sql(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        s.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .trim_end_matches(';')
            .trim()
            .to_lowercase()
    };
    norm(a) == norm(b)
}

/// A plan capture the tab asked for.
#[derive(Clone, Debug)]
pub struct ExplainRequest {
    /// The statement.
    pub sql: String,
    /// Where it starts in the editor buffer.
    pub offset: Option<usize>,
    /// Actual plan instead of an estimate.
    pub analyze: bool,
    /// Confirmed for a writing statement on Production.
    pub confirmed: bool,
}

/// Graph or flame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Operator graph, root on the left.
    Graph,
    /// Icicle chart, widths by inclusive time (or cost).
    Flame,
}

/// What a pending history load is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Purpose {
    /// Show it.
    Show,
    /// Compare the current plan against it.
    Compare,
}

enum Status {
    Idle,
    Capturing {
        query: QueryId,
        req: ExplainRequest,
        started: Instant,
        /// Compare the result with the plan that was current when it started.
        against: Option<usize>,
    },
    Loading {
        request: RequestId,
        purpose: Purpose,
    },
    Failed {
        message: String,
        req: Option<ExplainRequest>,
    },
    /// An actual plan of a writing statement on Production waits for confirmation.
    Confirm {
        req: ExplainRequest,
    },
    /// Planning with hypothetical indexes.
    WhatIf {
        query: QueryId,
        started: Instant,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Menu {
    Plans,
    Compare,
    /// Hypothetical indexes (HypoPG).
    WhatIf,
}

/// One captured or loaded plan.
struct Entry {
    plan: Arc<Plan>,
    findings: Vec<Finding>,
    history_id: Option<i64>,
    offset: Option<usize>,
    label: SharedString,
    graph: GraphLayout,
    flame: Vec<FlameBar>,
    pan: (f32, f32),
}

struct CompareState {
    /// The baseline entry (`a`); the current entry is `b`.
    base: usize,
    cmp: Comparison,
    base_selected: Option<u32>,
}

/// An edge to paint: from, to (pane coordinates), stroke width, colour.
type Edge = ((f32, f32), (f32, f32), f32, Hsla);

struct Drag {
    entry: usize,
    start: (f32, f32),
    pan0: (f32, f32),
    moved: bool,
}

/// The plan view of one SQL tab.
pub struct PlanView {
    core: RuntimeHandle,
    connection: Option<ProfileId>,
    entries: Vec<Entry>,
    current: Option<usize>,
    status: Status,
    mode: Mode,
    zoom: f32,
    selected: Option<u32>,
    compare: Option<CompareState>,
    menu: Option<Menu>,
    /// Saved plans from history for the compare menu.
    saved: Vec<HistoryEntry>,
    saved_request: Option<RequestId>,
    drag: Option<Drag>,
    /// Viewport of the main pane (0) and the compare baseline pane (1).
    viewports: [Rc<Cell<Bounds<Pixels>>>; 2],
    /// Width of the whole view (last frame), to collapse the hotspot list when narrow.
    width: Rc<Cell<f32>>,
    /// Hotspot list shown (`None`: automatic by width).
    hotspots: Option<bool>,
    /// Fit the graph once the pane's size is known.
    fit_pending: bool,
    ticker: Option<Task<()>>,
    /// The session of the last capture (hypothetical indexes run there).
    session: Option<SessionId>,
    /// `CREATE INDEX` statements for the what-if panel, one per line.
    what_if_input: Option<Entity<TextareaState>>,
    /// An assistant comparison waiting for the original statement's plan.
    pending: Option<AfterPlan>,
}

impl EventEmitter<PlanViewEvent> for PlanView {}

const MIN_ZOOM: f32 = 0.3;
const FIT_MIN_ZOOM: f32 = 0.8;
const MAX_ZOOM: f32 = 1.6;
const MARGIN: f32 = 24.;
const FLAME_ROW: f32 = 24.;

impl PlanView {
    /// An empty view.
    pub fn new(core: RuntimeHandle) -> Self {
        Self {
            core,
            connection: None,
            entries: Vec::new(),
            current: None,
            status: Status::Idle,
            mode: Mode::Graph,
            zoom: 1.0,
            selected: None,
            compare: None,
            menu: None,
            saved: Vec::new(),
            saved_request: None,
            drag: None,
            viewports: [Rc::default(), Rc::default()],
            width: Rc::default(),
            hotspots: None,
            fit_pending: false,
            ticker: None,
            session: None,
            what_if_input: None,
            pending: None,
        }
    }

    /// The tab's connection (scopes the saved plans offered for comparison).
    pub fn set_connection(&mut self, connection: Option<ProfileId>) {
        if self.connection != connection {
            self.connection = connection;
            self.saved.clear();
        }
    }

    /// Whether there is anything to show (a plan, a capture, an error).
    pub fn has_content(&self) -> bool {
        !self.entries.is_empty() || !matches!(self.status, Status::Idle)
    }

    /// Whether a capture is running.
    pub fn is_capturing(&self) -> bool {
        matches!(
            self.status,
            Status::Capturing { .. } | Status::WhatIf { .. }
        )
    }

    /// Whether `request` is this view's capture or history load.
    pub fn owns(&self, request: u64) -> bool {
        match self.status {
            Status::Capturing { query, .. } | Status::WhatIf { query, .. } => query == request,
            Status::Loading { request: r, .. } => r == request,
            _ => false,
        }
    }

    /// Whether `request` is this view's saved-plans search.
    pub fn owns_history(&self, request: RequestId) -> bool {
        self.saved_request == Some(request)
    }

    /// Capture a plan on `session`.
    pub fn explain(&mut self, session: SessionId, req: ExplainRequest, cx: &mut Context<Self>) {
        self.pending = None;
        self.capture(session, req, None, cx);
    }

    /// For an assistant suggestion: plan the rewrite (or the statement with hypothetical
    /// indexes, or again) and compare with the plan of `base`, capturing that first when it
    /// is not the current plan.
    pub fn assistant_compare(
        &mut self,
        session: SessionId,
        base: Option<String>,
        then: AfterPlan,
        cx: &mut Context<Self>,
    ) {
        let current_sql = self
            .current
            .and_then(|i| self.entries.get(i))
            .map(|e| e.plan.sql.clone());
        let ready = match (&base, &current_sql) {
            (Some(b), Some(c)) => same_sql(b, c),
            (None, Some(_)) => true,
            _ => false,
        };
        if ready {
            self.session = Some(session);
            self.run_after(session, then, cx);
            return;
        }
        match base {
            Some(b) => {
                self.capture(
                    session,
                    ExplainRequest {
                        sql: b,
                        offset: None,
                        analyze: false,
                        confirmed: false,
                    },
                    None,
                    cx,
                );
                self.pending = Some(then);
            }
            // Nothing to compare with: just show the suggestion's plan.
            None => {
                if let AfterPlan::Compare(sql) = then {
                    self.capture(
                        session,
                        ExplainRequest {
                            sql,
                            offset: None,
                            analyze: false,
                            confirmed: false,
                        },
                        None,
                        cx,
                    );
                } else {
                    cx.emit(PlanViewEvent::Toast("Explain the statement first".into()));
                }
            }
        }
    }

    fn run_after(&mut self, session: SessionId, then: AfterPlan, cx: &mut Context<Self>) {
        let Some(cur) = self.current else {
            return;
        };
        let sql = self.entries[cur].plan.sql.clone();
        let estimated = |sql: String| ExplainRequest {
            sql,
            offset: None,
            analyze: false,
            confirmed: false,
        };
        match then {
            AfterPlan::Compare(rewrite) => self.capture(session, estimated(rewrite), Some(cur), cx),
            AfterPlan::Replan => self.capture(session, estimated(sql), Some(cur), cx),
            AfterPlan::WhatIf(indexes) => {
                self.stop(cx);
                let query = next_id();
                self.core.send(Command::WhatIf {
                    session,
                    query,
                    sql,
                    indexes,
                });
                self.status = Status::WhatIf {
                    query,
                    started: Instant::now(),
                };
                self.menu = None;
                self.tick(cx);
                cx.notify();
            }
        }
    }

    fn capture(
        &mut self,
        session: SessionId,
        req: ExplainRequest,
        against: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        self.stop(cx);
        self.session = Some(session);
        let query = next_id();
        self.core.send(Command::Explain {
            session,
            query,
            sql: req.sql.clone(),
            analyze: req.analyze,
            confirmed: req.confirmed,
            tags: Vec::new(),
        });
        self.status = Status::Capturing {
            query,
            req,
            started: Instant::now(),
            against,
        };
        self.menu = None;
        self.tick(cx);
        cx.notify();
    }

    /// Repaint while a capture runs, for its elapsed time.
    fn tick(&mut self, cx: &mut Context<Self>) {
        self.ticker = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let busy = this
                    .update(cx, |v, cx| {
                        cx.notify();
                        v.is_capturing()
                    })
                    .unwrap_or(false);
                if !busy {
                    break;
                }
            }
        }));
    }

    /// Cancel a running capture.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if let Status::Capturing { query, .. } | Status::WhatIf { query, .. } = self.status {
            self.core.send(Command::Cancel { query });
            cx.notify();
        }
    }

    /// Show the plan stored with history entry `history_id`.
    pub fn open_saved(&mut self, history_id: i64, cx: &mut Context<Self>) {
        if let Some(ix) = self
            .entries
            .iter()
            .position(|e| e.history_id == Some(history_id))
        {
            self.set_current(ix, cx);
            return;
        }
        self.load(history_id, Purpose::Show, cx);
    }

    fn load(&mut self, history_id: i64, purpose: Purpose, cx: &mut Context<Self>) {
        self.stop(cx);
        let request = next_id();
        self.core.send(Command::LoadPlan {
            request,
            history_id,
        });
        self.status = Status::Loading { request, purpose };
        self.menu = None;
        cx.notify();
    }

    /// A plan arrived for this view's request.
    pub fn on_plan(
        &mut self,
        history_id: Option<i64>,
        plan: Arc<Plan>,
        findings: Vec<Finding>,
        cx: &mut Context<Self>,
    ) {
        let (offset, purpose, against) = match &self.status {
            Status::Capturing { req, against, .. } => (req.offset, Purpose::Show, *against),
            Status::Loading { purpose, .. } => (None, *purpose, None),
            _ => (None, Purpose::Show, None),
        };
        self.status = Status::Idle;
        self.ticker = None;
        let time = chrono::Local::now().format("%H:%M:%S");
        let kind = match plan.kind {
            PlanKind::Actual => "Actual",
            PlanKind::Estimated => "Estimated",
        };
        // A capture gets its time; a plan loaded from history its entry number.
        let label = match history_id {
            Some(id) if offset.is_none() => format!("{kind} · saved #{id}"),
            _ => format!("{kind} · {time}"),
        };
        let ix = self.add_entry(plan, findings, history_id, offset, label);
        // The baseline of an assistant comparison may have moved when the list was trimmed.
        let against = against.filter(|b| *b < self.entries.len() && *b != ix);
        match (purpose, self.current, against) {
            (_, _, Some(base)) => self.start_compare(base, ix, cx),
            (Purpose::Compare, Some(cur), _) => self.start_compare(ix, cur, cx),
            _ => {
                self.set_current(ix, cx);
                self.fit_pending = true;
            }
        }
        if let (Some(then), Some(session)) = (self.pending.take(), self.session) {
            self.run_after(session, then, cx);
        }
        // A new history entry exists: refresh the saved list next time the menu opens.
        self.saved.clear();
    }

    /// Add a plan to the list (dropping the oldest unused one past twelve); its index.
    fn add_entry(
        &mut self,
        plan: Arc<Plan>,
        findings: Vec<Finding>,
        history_id: Option<i64>,
        offset: Option<usize>,
        label: String,
    ) -> usize {
        let entry = Entry {
            graph: layout::graph(&plan),
            flame: layout::flame(&plan),
            plan,
            findings,
            history_id,
            offset,
            label: label.into(),
            pan: (MARGIN, MARGIN),
        };
        // Keep the list short: drop the oldest plan nobody is comparing against.
        if self.entries.len() >= 12 {
            let keep = [self.current, self.compare.as_ref().map(|c| c.base)];
            if let Some(ix) = (0..self.entries.len()).find(|i| !keep.contains(&Some(*i))) {
                self.remove_entry(ix);
            }
        }
        self.entries.push(entry);
        self.entries.len() - 1
    }

    /// `CREATE INDEX` suggestions from the current plan's findings, one per line.
    fn suggested_indexes(&self) -> String {
        let Some(e) = self.current.and_then(|i| self.entries.get(i)) else {
            return String::new();
        };
        let mut out: Vec<String> = Vec::new();
        for f in &e.findings {
            if let Some(s) = &f.suggestion {
                for line in s.lines() {
                    let l = line.trim();
                    if l.to_ascii_uppercase().starts_with("CREATE INDEX")
                        && !out.iter().any(|o| o == l)
                    {
                        out.push(l.to_owned());
                    }
                }
            }
        }
        out.join("\n")
    }

    /// Open the what-if panel, pre-filled from the findings.
    fn open_what_if(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.suggested_indexes();
        let input = self.what_if_input.get_or_insert_with(|| {
            cx.new(|cx| {
                TextareaState::new(window, cx)
                    .placeholder("CREATE INDEX ON orders (status);")
                    .auto_grow(3, 8)
            })
        });
        if !text.is_empty() {
            input.update(cx, |i, cx| i.set_value(text, window, cx));
        }
        self.open_menu(Menu::WhatIf, cx);
    }

    /// Plan the current statement with the panel's hypothetical indexes.
    fn run_what_if(&mut self, cx: &mut Context<Self>) {
        let (Some(session), Some(input), Some(e)) = (
            self.session,
            self.what_if_input.as_ref(),
            self.current.and_then(|i| self.entries.get(i)),
        ) else {
            return;
        };
        let text = input.read(cx).value().to_string();
        let indexes: Vec<String> = text
            .split([';', '\n'])
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect();
        if indexes.is_empty() {
            return;
        }
        let sql = e.plan.sql.clone();
        self.stop(cx);
        let query = next_id();
        self.core.send(Command::WhatIf {
            session,
            query,
            sql,
            indexes,
        });
        self.status = Status::WhatIf {
            query,
            started: Instant::now(),
        };
        self.menu = None;
        self.tick(cx);
        cx.notify();
    }

    /// The what-if result: the fresh estimate and the hypothetical plan, compared.
    pub fn on_what_if(&mut self, result: Result<Arc<WhatIf>, String>, cx: &mut Context<Self>) {
        self.ticker = None;
        let w = match result {
            Ok(w) => w,
            Err(message) => {
                self.status = Status::Failed { message, req: None };
                cx.notify();
                return;
            }
        };
        self.status = Status::Idle;
        let thresholds = Thresholds::default();
        let time = chrono::Local::now().format("%H:%M:%S");
        let before = Arc::new(w.before.clone());
        let after = Arc::new(w.after.clone());
        let b = self.add_entry(
            before.clone(),
            switchyard_core::plan::analyze(&before, &thresholds),
            None,
            None,
            format!("Estimated · {time}"),
        );
        let names: Vec<String> = w
            .indexes
            .iter()
            .map(|i| {
                let size = i
                    .bytes
                    .map(|b| format!(" ({})", ui::bytes(b as u64)))
                    .unwrap_or_default();
                format!("{}{size}", i.definition)
            })
            .collect();
        let a = self.add_entry(
            after.clone(),
            switchyard_core::plan::analyze(&after, &thresholds),
            None,
            None,
            format!("Hypothetical · {}", names.join(", ")),
        );
        self.start_compare(b, a, cx);
        if !w.uses_hypothetical() {
            cx.emit(PlanViewEvent::Toast(
                "The planner did not use the hypothetical index; the plan is unchanged.".into(),
            ));
        }
    }

    /// The capture or load failed.
    pub fn on_failed(&mut self, error: String, needs_confirmation: bool, cx: &mut Context<Self>) {
        let req = match std::mem::replace(&mut self.status, Status::Idle) {
            Status::Capturing { req, .. } => Some(req),
            _ => None,
        };
        self.ticker = None;
        self.status = match req {
            Some(req) if needs_confirmation => Status::Confirm { req },
            req => Status::Failed {
                message: error,
                req,
            },
        };
        cx.notify();
    }

    /// History search results for the compare menu.
    pub fn on_history(&mut self, entries: Vec<HistoryEntry>, cx: &mut Context<Self>) {
        self.saved_request = None;
        self.saved = entries
            .into_iter()
            .filter(|e| e.has_plan)
            .take(20)
            .collect();
        cx.notify();
    }

    fn remove_entry(&mut self, ix: usize) {
        self.entries.remove(ix);
        let fix = |i: usize| if i > ix { i - 1 } else { i };
        self.current = self.current.map(fix);
        if let Some(c) = &mut self.compare {
            c.base = fix(c.base);
        }
    }

    fn set_current(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.current = Some(ix);
        self.menu = None;
        if self.compare.as_ref().is_some_and(|c| c.base == ix) {
            self.compare = None;
        }
        if let Some(c) = self.compare.as_ref().map(|c| c.base) {
            self.start_compare(c, ix, cx);
            return;
        }
        self.selected = None;
        self.highlight(cx);
        cx.notify();
    }

    fn start_compare(&mut self, base: usize, current: usize, cx: &mut Context<Self>) {
        let (Some(a), Some(b)) = (self.entries.get(base), self.entries.get(current)) else {
            return;
        };
        let cmp = switchyard_core::plan::compare(&a.plan, &b.plan);
        self.current = Some(current);
        self.compare = Some(CompareState {
            base,
            cmp,
            base_selected: None,
        });
        self.refit();
        self.selected = None;
        self.menu = None;
        self.highlight(cx);
        cx.notify();
    }

    /// Pan entry `ix`'s graph (shown in viewport `slot`) so node `id` is in view.
    fn reveal(&mut self, ix: usize, slot: usize, id: u32) {
        let b = self.viewports[slot].get();
        let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
        let z = self.zoom;
        let Some(e) = self.entries.get_mut(ix) else {
            return;
        };
        let Some(nb) = e.graph.get(id) else { return };
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let (x, y) = (e.pan.0 + nb.x * z, e.pan.1 + nb.y * z);
        let (nw, nh) = (NODE_W * z, NODE_H * z);
        if x < 0.0 || y < 0.0 || x + nw > w || y + nh > h {
            e.pan = (
                w / 2.0 - (nb.x * z + nw / 2.0),
                h / 2.0 - (nb.y * z + nh / 2.0),
            );
        }
    }

    /// Select node `id` from outside the graph (hotspots, compare table): also bring it
    /// into view.
    fn select_and_reveal(&mut self, entry: usize, id: u32, cx: &mut Context<Self>) {
        let slot = match &self.compare {
            Some(c) if c.base == entry => 1,
            _ => 0,
        };
        self.reveal(entry, slot, id);
        self.select(entry, Some(id), cx);
    }

    fn select(&mut self, entry: usize, id: Option<u32>, cx: &mut Context<Self>) {
        match &mut self.compare {
            Some(c) if c.base == entry => c.base_selected = id,
            _ => {
                self.selected = id;
                self.highlight(cx);
            }
        }
        cx.notify();
    }

    /// Tell the tab which parts of the statement the selected node reads.
    fn highlight(&self, cx: &mut Context<Self>) {
        let Some(e) = self.current.and_then(|i| self.entries.get(i)) else {
            return;
        };
        let ranges = self
            .selected
            .and_then(|id| e.plan.node(id))
            .map(|n| layout::sql_ranges(&e.plan.sql, n))
            .unwrap_or_default();
        cx.emit(PlanViewEvent::Highlight {
            sql: e.plan.sql.clone(),
            offset: e.offset,
            ranges,
        });
    }

    fn open_menu(&mut self, menu: Menu, cx: &mut Context<Self>) {
        if self.menu == Some(menu) {
            self.menu = None;
        } else {
            self.menu = Some(menu);
            if menu == Menu::Compare && self.saved.is_empty() && self.saved_request.is_none() {
                let request = next_id();
                self.saved_request = Some(request);
                self.core.send(Command::SearchHistory {
                    request,
                    query: String::new(),
                    connection: self.connection.clone(),
                });
            }
        }
        cx.notify();
    }

    fn compare_with_saved(&mut self, history_id: i64, cx: &mut Context<Self>) {
        let Some(cur) = self.current else { return };
        if let Some(ix) = self
            .entries
            .iter()
            .position(|e| e.history_id == Some(history_id))
        {
            if ix != cur {
                self.start_compare(ix, cur, cx);
            }
            return;
        }
        self.load(history_id, Purpose::Compare, cx);
    }

    fn set_zoom(&mut self, z: f32, anchor: Option<(f32, f32)>, cx: &mut Context<Self>) {
        let z = z.clamp(MIN_ZOOM, MAX_ZOOM);
        let ratio = z / self.zoom;
        let panes: Vec<usize> = self
            .current
            .into_iter()
            .chain(self.compare.as_ref().map(|c| c.base))
            .collect();
        for (slot, ix) in panes.into_iter().enumerate() {
            let b = self.viewports[slot].get();
            let (ax, ay) = anchor.unwrap_or((
                f32::from(b.size.width) / 2.0,
                f32::from(b.size.height) / 2.0,
            ));
            if let Some(e) = self.entries.get_mut(ix) {
                e.pan = (ax - (ax - e.pan.0) * ratio, ay - (ay - e.pan.1) * ratio);
            }
        }
        self.zoom = z;
        cx.notify();
    }

    /// Fit again once the panes have been measured in their new layout.
    fn refit(&mut self) {
        for v in &self.viewports {
            v.set(Bounds::default());
        }
        self.fit_pending = true;
    }

    /// Fit the current plan's graph into its pane, but no smaller than 80% so the cards
    /// stay readable (the rest pans).
    /// When comparing, both panes share one zoom that fits both. Returns false while a
    /// pane has not been laid out yet (the fit stays pending).
    fn fit(&mut self, cx: &mut Context<Self>) -> bool {
        let panes: Vec<(usize, usize)> = self
            .current
            .map(|c| (c, 0))
            .into_iter()
            .chain(self.compare.as_ref().map(|c| (c.base, 1)))
            .collect();
        let mut z = 1.0_f32;
        let mut sizes = Vec::new();
        for &(ix, slot) in &panes {
            let b = self.viewports[slot].get();
            let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
            if w <= 0.0 || h <= 0.0 {
                return false;
            }
            if let Some(e) = self.entries.get(ix).filter(|e| e.graph.width > 0.0) {
                z = z
                    .min((w - 2.0 * MARGIN) / e.graph.width)
                    .min((h - 2.0 * MARGIN) / e.graph.height);
            }
            sizes.push((ix, w, h));
        }
        self.fit_pending = false;
        let z = z.clamp(FIT_MIN_ZOOM, 1.0);
        for (ix, w, h) in sizes {
            if let Some(e) = self.entries.get_mut(ix) {
                // Centred when it fits; otherwise the root (left) stays in view, centred
                // vertically on its row.
                let root_y = e.graph.boxes.first().map_or(0.0, |b| b.y);
                let y = if e.graph.height * z + 2.0 * MARGIN <= h {
                    (h - e.graph.height * z) / 2.0
                } else {
                    h / 2.0 - (root_y + NODE_H / 2.0) * z
                };
                e.pan = (((w - e.graph.width * z) / 2.0).max(MARGIN), y);
            }
        }
        self.zoom = z;
        cx.notify();
        true
    }

    fn on_wheel(
        &mut self,
        entry: usize,
        slot: usize,
        ev: &ScrollWheelEvent,
        cx: &mut Context<Self>,
    ) {
        let (dx, dy) = match ev.delta {
            ScrollDelta::Pixels(p) => (f32::from(p.x), f32::from(p.y)),
            ScrollDelta::Lines(l) => (l.x * 40.0, l.y * 40.0),
        };
        if ev.modifiers.secondary() {
            let b = self.viewports[slot].get();
            let anchor = (
                f32::from(ev.position.x - b.origin.x),
                f32::from(ev.position.y - b.origin.y),
            );
            let factor = if dy > 0.0 { 1.1 } else { 1.0 / 1.1 };
            self.set_zoom(self.zoom * factor, Some(anchor), cx);
            return;
        }
        // Shift turns a vertical wheel into horizontal panning.
        let (dx, dy) = if ev.modifiers.shift && dx == 0.0 {
            (dy, 0.0)
        } else {
            (dx, dy)
        };
        if let Some(e) = self.entries.get_mut(entry) {
            e.pan = (e.pan.0 + dx, e.pan.1 + dy);
            cx.notify();
        }
    }

    // ---- rendering ---------------------------------------------------------------------

    /// The hotspot list shows unless the user hid it, or (automatically) the view is
    /// too narrow for it next to the graph.
    fn show_hotspots(&self) -> bool {
        self.hotspots.unwrap_or_else(|| {
            let w = self.width.get();
            w <= 0.0 || w >= 720.0
        })
    }

    fn heat_color(h: Heat, p: &Palette) -> Hsla {
        match h {
            Heat::None => p.bd2,
            Heat::Low => p.stg.opacity(0.55),
            Heat::Mid => p.stg,
            Heat::High => p.prod,
        }
    }

    fn severity_color(s: Severity, p: &Palette) -> Hsla {
        match s {
            Severity::High => p.prod,
            Severity::Medium => p.stg,
            Severity::Low => p.fg3,
        }
    }

    fn render_header(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let entry = self.current.and_then(|i| self.entries.get(i));
        let mut summary: Vec<String> = Vec::new();
        if let Some(e) = entry {
            let plan = &e.plan;
            if let Some(t) = plan.total_time_ms() {
                summary.push(ms(t));
            }
            if let Some(t) = plan.planning_ms {
                summary.push(format!("planning {}", ms(t)));
            }
            if let Some(r) = plan.root.rows() {
                summary.push(format!("{} rows", count(r)));
            }
            if let Some(c) = plan.root.cost {
                summary.push(format!("cost {}", count(c)));
            }
            summary.push(format!("{} nodes", plan.nodes().len()));
        }
        let kind = entry.map(|e| e.plan.kind);
        let graph = self.mode == Mode::Graph;
        div()
            .h(px(34.))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(10.))
            .border_b_1()
            .border_color(p.bd)
            .whitespace_nowrap()
            .overflow_hidden()
            .when(entry.is_some() && self.compare.is_none(), |d| {
                let open = self.show_hotspots();
                let n = entry.map_or(0, |e| e.findings.len());
                d.child(
                    ui::button(
                        "hotspots-toggle",
                        format!("Hotspots {n}"),
                        if open { Kind::Secondary } else { Kind::Ghost },
                        p,
                    )
                    .h(px(22.))
                    .px(px(8.))
                    .text_size(px(11.5))
                    .when(n > 0 && !open, |d| d.text_color(p.stg))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.hotspots = Some(!open);
                        cx.notify();
                    })),
                )
            })
            .child(ui::segmented(
                "plan-mode",
                vec![
                    (
                        "Graph".into(),
                        graph,
                        Box::new(cx.listener(|this, _, _, cx| {
                            this.mode = Mode::Graph;
                            cx.notify();
                        })),
                    ),
                    (
                        "Flame".into(),
                        !graph,
                        Box::new(cx.listener(|this, _, _, cx| {
                            this.mode = Mode::Flame;
                            cx.notify();
                        })),
                    ),
                ],
                20.,
                p,
            ))
            .when_some(kind, |d, k| {
                let (label, color) = match k {
                    PlanKind::Actual => ("ACTUAL", p.dev),
                    PlanKind::Estimated => ("ESTIMATED", p.fg2),
                };
                d.child(
                    div()
                        .px(px(6.))
                        .py(px(1.))
                        .rounded(px(4.))
                        .border_1()
                        .border_color(color.opacity(0.5))
                        .font_family(MONO)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_size(px(9.5))
                        .text_color(color)
                        .child(label),
                )
            })
            .when(self.entries.len() > 1, |d| {
                let label = entry.map(|e| e.label.clone()).unwrap_or_default();
                d.child(
                    ui::button(
                        "plan-pick",
                        format!(
                            "Plan {} of {} · {label} ▾",
                            self.current.map_or(0, |i| i + 1),
                            self.entries.len()
                        ),
                        Kind::Ghost,
                        p,
                    )
                    .h(px(22.))
                    .px(px(6.))
                    .text_size(px(11.5))
                    .on_click(cx.listener(|this, _, _, cx| this.open_menu(Menu::Plans, cx))),
                )
            })
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_family(MONO)
                    .text_size(px(11.))
                    .text_color(p.fg2)
                    .child(summary.join(" · ")),
            )
            .child(div().flex_1())
            .when(graph && entry.is_some(), |d| {
                d.child(
                    ui::button("zoom-out", "−", Kind::Ghost, p)
                        .h(px(22.))
                        .px(px(7.))
                        .on_click(
                            cx.listener(|this, _, _, cx| this.set_zoom(this.zoom / 1.25, None, cx)),
                        ),
                )
                .child(
                    div()
                        .w(px(36.))
                        .flex()
                        .justify_center()
                        .font_family(MONO)
                        .text_size(px(10.5))
                        .text_color(p.fg3)
                        .child(format!("{:.0}%", self.zoom * 100.0)),
                )
                .child(
                    ui::button("zoom-in", "+", Kind::Ghost, p)
                        .h(px(22.))
                        .px(px(7.))
                        .on_click(
                            cx.listener(|this, _, _, cx| this.set_zoom(this.zoom * 1.25, None, cx)),
                        ),
                )
                .child(
                    ui::button("zoom-fit", "Fit", Kind::Ghost, p)
                        .h(px(22.))
                        .px(px(7.))
                        .text_size(px(11.5))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.fit(cx);
                        })),
                )
            })
            .when(
                entry.is_some_and(|e| e.plan.source == PlanSource::Postgres)
                    && self.session.is_some()
                    && self.compare.is_none(),
                |d| {
                    d.child(
                        ui::button("what-if", "What if…", Kind::Ghost, p)
                            .h(px(22.))
                            .px(px(8.))
                            .text_size(px(11.5))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.open_what_if(window, cx)),
                            ),
                    )
                },
            )
            .when(entry.is_some(), |d| {
                let optimize = self.current.and_then(|i| self.entries.get(i)).map(|e| {
                    (
                        e.plan.sql.clone(),
                        e.findings
                            .iter()
                            .map(|f| match &f.suggestion {
                                Some(s) => format!("{} ({}; {})", f.title, f.detail, s),
                                None => format!("{} ({})", f.title, f.detail),
                            })
                            .collect::<Vec<_>>(),
                    )
                });
                let d = d.when_some(optimize, |d, (sql, findings)| {
                    d.child(
                        ui::button("plan-optimize", "Optimize ✦", Kind::Secondary, p)
                            .h(px(22.))
                            .px(px(8.))
                            .text_size(px(11.5))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(PlanViewEvent::Optimize {
                                    sql: sql.clone(),
                                    findings: findings.clone(),
                                })
                            })),
                    )
                });
                if self.compare.is_some() {
                    d.child(
                        ui::button("compare-exit", "Exit compare", Kind::Secondary, p)
                            .h(px(22.))
                            .px(px(8.))
                            .text_size(px(11.5))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.compare = None;
                                this.refit();
                                cx.notify();
                            })),
                    )
                } else {
                    d.child(
                        ui::button("compare", "Compare ▾", Kind::Secondary, p)
                            .h(px(22.))
                            .px(px(8.))
                            .text_size(px(11.5))
                            .on_click(
                                cx.listener(|this, _, _, cx| this.open_menu(Menu::Compare, cx)),
                            ),
                    )
                }
            })
            .into_any_element()
    }

    fn menu_item(
        id: impl Into<gpui_kit::ElementId>,
        title: String,
        sub: String,
        active: bool,
        p: &Palette,
    ) -> gpui_kit::Stateful<Div> {
        div()
            .id(id.into())
            .flex()
            .flex_col()
            .px(px(8.))
            .py(px(4.))
            .rounded(px(4.))
            .hover(|s| s.bg(p.sel))
            .when(active, |d| d.bg(p.hover))
            .child(div().text_size(px(12.)).truncate().child(title))
            .child(
                div()
                    .font_family(MONO)
                    .text_size(px(10.5))
                    .text_color(p.fg3)
                    .truncate()
                    .child(sub),
            )
    }

    fn render_menu(&self, menu: Menu, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let mut body = div()
            .id("plan-menu")
            .absolute()
            .top(px(30.))
            .w(px(380.))
            .max_h(px(360.))
            .overflow_y_scroll()
            .p(px(4.))
            .bg(p.elev)
            .rounded(px(7.))
            .shadow(ui::shadow(p))
            .occlude()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.menu = None;
                cx.notify();
            }));
        let caption = |t: &str| {
            div()
                .px(px(8.))
                .py(px(4.))
                .text_size(px(11.))
                .text_color(p.fg3)
                .child(t.to_owned())
        };
        let summary = |e: &Entry| {
            let mut s = Vec::new();
            if let Some(t) = e.plan.total_time_ms() {
                s.push(ms(t));
            }
            if let Some(c) = e.plan.root.cost {
                s.push(format!("cost {}", count(c)));
            }
            s.push(first_line(&e.plan.sql));
            s.join(" · ")
        };
        match menu {
            Menu::WhatIf => {
                body = body
                    .right(px(10.))
                    .w(px(460.))
                    .child(caption(
                        "Hypothetical indexes (HypoPG): plan this statement as if they existed. \
                         Nothing is created.",
                    ))
                    .when_some(self.what_if_input.clone(), |d, input| {
                        d.child(
                            div()
                                .mx(px(8.))
                                .my(px(4.))
                                .font_family(MONO)
                                .text_size(px(11.5))
                                .child(Textarea::new(&input)),
                        )
                    })
                    .child(caption("One CREATE INDEX per line."))
                    .child(
                        div().flex().justify_end().px(px(8.)).py(px(4.)).child(
                            ui::button("what-if-run", "Plan with these indexes", Kind::Primary, p)
                                .h(px(24.))
                                .on_click(cx.listener(|this, _, _, cx| this.run_what_if(cx))),
                        ),
                    );
            }
            Menu::Plans => {
                body = body.left(px(160.)).child(caption("Plans in this tab"));
                for (i, e) in self.entries.iter().enumerate().rev() {
                    body = body.child(
                        Self::menu_item(
                            ("plan-item", i),
                            format!("Plan {} · {}", i + 1, e.label),
                            summary(e),
                            self.current == Some(i),
                            p,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.set_current(i, cx))),
                    );
                }
            }
            Menu::Compare => {
                body = body.right(px(10.)).child(caption("Compare this plan with"));
                let cur = self.current;
                let mut any = false;
                for (i, e) in self.entries.iter().enumerate().rev() {
                    if Some(i) == cur {
                        continue;
                    }
                    any = true;
                    body = body.child(
                        Self::menu_item(
                            ("cmp-item", i),
                            format!("Plan {} · {}", i + 1, e.label),
                            summary(e),
                            false,
                            p,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(c) = this.current {
                                this.start_compare(i, c, cx);
                            }
                        })),
                    );
                }
                if !any {
                    body = body.child(
                        div()
                            .px(px(8.))
                            .py(px(4.))
                            .text_size(px(11.5))
                            .text_color(p.fg3)
                            .child("No other plan in this tab yet: change the query and explain it again."),
                    );
                }
                body = body
                    .child(div().h(px(1.)).bg(p.bd).my(px(4.)))
                    .child(caption("Saved with history (this connection)"));
                let in_tab: Vec<i64> = self.entries.iter().filter_map(|e| e.history_id).collect();
                let saved: Vec<&HistoryEntry> = self
                    .saved
                    .iter()
                    .filter(|h| Some(h.id) != cur.and_then(|c| self.entries[c].history_id))
                    .collect();
                if self.saved_request.is_some() {
                    body = body.child(div().px(px(8.)).py(px(4.)).child(ui::shimmer(160., p)));
                } else if saved.is_empty() {
                    body = body.child(
                        div()
                            .px(px(8.))
                            .py(px(4.))
                            .text_size(px(11.5))
                            .text_color(p.fg3)
                            .child("No saved plans. Plans are saved with query history when it is on for the connection."),
                    );
                }
                for h in saved {
                    let id = h.id;
                    let when = chrono::DateTime::from_timestamp_millis(h.started_at)
                        .map(|t| {
                            t.with_timezone(&chrono::Local)
                                .format("%b %d %H:%M")
                                .to_string()
                        })
                        .unwrap_or_default();
                    let kind = if h.tags.iter().any(|t| t == "explain-analyze") {
                        "Actual"
                    } else {
                        "Estimated"
                    };
                    body = body.child(
                        Self::menu_item(
                            ("saved-item", id as usize),
                            format!(
                                "{kind} · {when}{}",
                                if in_tab.contains(&id) {
                                    " · in this tab"
                                } else {
                                    ""
                                }
                            ),
                            first_line(&h.sql),
                            false,
                            p,
                        )
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.compare_with_saved(id, cx)),
                        ),
                    );
                }
            }
        }
        deferred(body).with_priority(1).into_any_element()
    }

    fn render_banner(&self, p: &Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        let bar = |bg: Hsla| {
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(12.))
                .py(px(7.))
                .bg(bg)
                .border_b_1()
                .border_color(p.bd)
                .text_size(px(12.))
        };
        match &self.status {
            Status::Idle => None,
            Status::Capturing { req, started, .. } => Some(
                bar(p.panel)
                    .child(ui::shimmer(90., p))
                    .child(div().font_weight(FontWeight::MEDIUM).child(if req.analyze {
                        "Running the statement for an actual plan…"
                    } else {
                        "Capturing the estimated plan…"
                    }))
                    .child(
                        div()
                            .font_family(MONO)
                            .text_color(p.fg3)
                            .child(ui::duration(started.elapsed())),
                    )
                    .when(req.analyze, |d| {
                        d.child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_color(p.fg3)
                                .child("Writes are rolled back."),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        ui::button("plan-stop", "Stop", Kind::Secondary, p)
                            .h(px(22.))
                            .text_color(p.prod)
                            .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                    )
                    .into_any_element(),
            ),
            Status::WhatIf { started, .. } => Some(
                bar(p.panel)
                    .child(ui::shimmer(90., p))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .child("Planning with hypothetical indexes…"),
                    )
                    .child(
                        div()
                            .font_family(MONO)
                            .text_color(p.fg3)
                            .child(ui::duration(started.elapsed())),
                    )
                    .child(div().flex_1())
                    .child(
                        ui::button("what-if-stop", "Stop", Kind::Secondary, p)
                            .h(px(22.))
                            .text_color(p.prod)
                            .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                    )
                    .into_any_element(),
            ),
            Status::Loading { .. } => Some(
                bar(p.panel)
                    .child(ui::shimmer(90., p))
                    .child("Loading the saved plan…")
                    .into_any_element(),
            ),
            Status::Failed { message, req } => {
                let retry = req.clone();
                Some(
                    bar(p.prod_bg)
                        .items_start()
                        .child(
                            div()
                                .flex_none()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(p.prod)
                                .child("Plan failed"),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .font_family(MONO)
                                .text_size(px(11.5))
                                .child(message.clone()),
                        )
                        .when_some(retry, |d, req| {
                            d.child(
                                ui::button("plan-retry", "Retry", Kind::Secondary, p)
                                    .h(px(22.))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.status = Status::Idle;
                                        cx.emit(PlanViewEvent::Rerun(req.clone()));
                                    })),
                            )
                        })
                        .child(
                            ui::button("plan-dismiss", "Dismiss", Kind::Ghost, p)
                                .h(px(22.))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.status = Status::Idle;
                                    cx.notify();
                                })),
                        )
                        .into_any_element(),
                )
            }
            Status::Confirm { req } => {
                let go = req.clone();
                let estimate = req.clone();
                Some(
                    bar(p.prod_bg)
                        .flex_col()
                        .items_start()
                        .gap(px(6.))
                        .py(px(10.))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .child(ui::env_badge_solid(
                                    switchyard_core::store::EnvironmentLabel::Production,
                                    p,
                                ))
                                .child(
                                    div()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child("Run this writing statement on Production for an actual plan?"),
                                ),
                        )
                        .child(
                            div().w_full().text_color(p.fg2).child(
                                "The statement runs inside a transaction that is rolled back, \
                                 but triggers, sequences and locks still take effect while it runs. \
                                 An estimated plan does not run it.",
                            ),
                        )
                        .child(
                            div()
                                .w_full()
                                .px(px(8.))
                                .py(px(6.))
                                .rounded(px(5.))
                                .bg(p.bg)
                                .font_family(MONO)
                                .text_size(px(11.5))
                                .child(first_line(&req.sql)),
                        )
                        .child(
                            div()
                                .flex()
                                .gap(px(8.))
                                .child(
                                    ui::button("plan-confirm", "Run actual plan", Kind::Destructive, p)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.status = Status::Idle;
                                            cx.emit(PlanViewEvent::Rerun(ExplainRequest {
                                                confirmed: true,
                                                ..go.clone()
                                            }));
                                        })),
                                )
                                .child(
                                    ui::button("plan-estimate", "Explain instead", Kind::Secondary, p)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.status = Status::Idle;
                                            cx.emit(PlanViewEvent::Rerun(ExplainRequest {
                                                analyze: false,
                                                ..estimate.clone()
                                            }));
                                        })),
                                )
                                .child(
                                    ui::button("plan-cancel", "Cancel", Kind::Ghost, p).on_click(
                                        cx.listener(|this, _, _, cx| {
                                            this.status = Status::Idle;
                                            cx.notify();
                                        }),
                                    ),
                                ),
                        )
                        .into_any_element(),
                )
            }
        }
    }

    /// The graph (or flame) pane of entry `ix`, in viewport `slot`.
    fn render_pane(
        &self,
        ix: usize,
        slot: usize,
        selected: Option<u32>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(e) = self.entries.get(ix) else {
            return div().into_any_element();
        };
        match self.mode {
            Mode::Graph => self.render_graph(e, ix, slot, selected, p, cx),
            Mode::Flame => self.render_flame(e, ix, slot, selected, p, cx),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_graph(
        &self,
        e: &Entry,
        ix: usize,
        slot: usize,
        selected: Option<u32>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let z = self.zoom;
        let (px0, py0) = e.pan;
        let viewport = self.viewports[slot].clone();
        let vb = viewport.get();
        let (vw, vh) = (f32::from(vb.size.width), f32::from(vb.size.height));
        let known = vw > 0.0 && vh > 0.0;
        let visible = |x: f32, y: f32, w: f32, h: f32| {
            !known || (x + w >= -40.0 && x <= vw + 40.0 && y + h >= -40.0 && y <= vh + 40.0)
        };
        let (w, h) = (NODE_W * z, NODE_H * z);
        // Edges: from each input's left side into its parent's right side.
        let mut edges: Vec<Edge> = Vec::new();
        for b in &e.graph.boxes {
            let Some(parent) = b.parent.and_then(|id| e.graph.get(id)) else {
                continue;
            };
            let from = (px0 + parent.x * z + w, py0 + parent.y * z + h / 2.0);
            let to = (px0 + b.x * z, py0 + b.y * z + h / 2.0);
            let (x0, x1) = (from.0.min(to.0), from.0.max(to.0));
            let (y0, y1) = (from.1.min(to.1), from.1.max(to.1));
            if !visible(x0, y0, x1 - x0, y1 - y0) {
                continue;
            }
            let rows = e.plan.node(b.id).and_then(layout::flow_rows);
            let width = layout::edge_width(rows, e.graph.max_rows) * z.max(0.5);
            let hot = selected == Some(b.id) || selected == b.parent;
            edges.push((from, to, width, if hot { p.acc } else { p.bd2 }));
        }
        let edges_canvas = canvas(
            move |bounds, _, _| viewport.set(bounds),
            move |bounds, _, window, _| {
                let o = bounds.origin;
                for (from, to, width, color) in edges {
                    let mid = (from.0 + to.0) / 2.0;
                    let mut path = PathBuilder::stroke(px(width));
                    path.move_to(o + point(px(from.0), px(from.1)));
                    path.cubic_bezier_to(
                        o + point(px(to.0), px(to.1)),
                        o + point(px(mid), px(from.1)),
                        o + point(px(mid), px(to.1)),
                    );
                    if let Ok(path) = path.build() {
                        window.paint_path(path, color);
                    }
                }
            },
        )
        .absolute()
        .size_full();
        let marks: std::collections::HashMap<u32, Severity> =
            e.findings.iter().fold(Default::default(), |mut m, f| {
                if let Some(id) = f.node_id {
                    let s = m.entry(id).or_insert(f.severity);
                    *s = (*s).max(f.severity);
                }
                m
            });
        let timed = e.plan.total_time_ms().is_some();
        let nodes: Vec<AnyElement> = e
            .graph
            .boxes
            .iter()
            .filter_map(|b| {
                let (x, y) = (px0 + b.x * z, py0 + b.y * z);
                if !visible(x, y, w, h) {
                    return None;
                }
                let n = e.plan.node(b.id)?;
                let share = e.plan.weight(n);
                let heat = Self::heat_color(Heat::of(share), p);
                let is_sel = selected == Some(b.id);
                let id = b.id;
                let metric = if timed {
                    n.self_time_ms().map(ms)
                } else {
                    n.cost.map(|c| format!("cost {}", count(c)))
                };
                Some(
                    div()
                        .id(("pn", slot * 1_000_000 + id as usize))
                        .absolute()
                        .left(px(x))
                        .top(px(y))
                        .w(px(w))
                        .h(px(h))
                        .flex()
                        .overflow_hidden()
                        .rounded(px(6. * z))
                        .border_1()
                        .border_color(if is_sel { p.acc } else { p.bd2 })
                        .bg(if is_sel { p.sel } else { p.elev })
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            if this.drag.as_ref().is_some_and(|d| d.moved) {
                                return;
                            }
                            let pick = (selected != Some(id)).then_some(id);
                            this.select(ix, pick, cx);
                        }))
                        .child(div().w(px(4. * z.max(0.75))).h_full().flex_none().bg(heat))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .justify_center()
                                .px(px(8. * z))
                                .gap(px(1. * z))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(4. * z))
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .truncate()
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_size(px(12. * z))
                                                .child(n.operation.clone()),
                                        )
                                        .when_some(marks.get(&id), |d, s| {
                                            d.child(ui::dot(Self::severity_color(*s, p), 6. * z))
                                        }),
                                )
                                .when(z >= 0.55, |d| {
                                    d.child(
                                        div()
                                            .truncate()
                                            .font_family(MONO)
                                            .text_size(px(10.5 * z))
                                            .text_color(p.fg2)
                                            .child(
                                                n.object.clone().unwrap_or_else(|| "\u{a0}".into()),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .gap(px(6. * z))
                                            .font_family(MONO)
                                            .text_size(px(10.5 * z))
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .truncate()
                                                    .text_color(p.fg3)
                                                    .child(rows_text(n)),
                                            )
                                            .when_some(metric, |d, m| {
                                                d.child(
                                                    div()
                                                        .text_color(if share >= 0.05 {
                                                            heat
                                                        } else {
                                                            p.fg3
                                                        })
                                                        .font_weight(FontWeight::MEDIUM)
                                                        .child(format!(
                                                            "{m} · {:.0}%",
                                                            share * 100.0
                                                        )),
                                                )
                                            }),
                                    )
                                }),
                        )
                        .into_any_element(),
                )
            })
            .collect();
        div()
            .id(("plan-graph", slot))
            .relative()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .bg(p.bg)
            .cursor_grab()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, _, _| {
                    let pan0 = this.entries.get(ix).map_or((0.0, 0.0), |e| e.pan);
                    this.drag = Some(Drag {
                        entry: ix,
                        start: (f32::from(ev.position.x), f32::from(ev.position.y)),
                        pan0,
                        moved: false,
                    });
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                // The drag stays (with `moved`) after the button is released so the
                // node's click, which fires on mouse-up, can tell a drag from a click; the
                // next mouse-down replaces it.
                let Some(d) = &mut this.drag else { return };
                if ev.pressed_button != Some(MouseButton::Left) {
                    return;
                }
                let (dx, dy) = (
                    f32::from(ev.position.x) - d.start.0,
                    f32::from(ev.position.y) - d.start.1,
                );
                if dx.abs() + dy.abs() > 3.0 {
                    d.moved = true;
                }
                let (entry, pan) = (d.entry, (d.pan0.0 + dx, d.pan0.1 + dy));
                if let Some(e) = this.entries.get_mut(entry) {
                    e.pan = pan;
                    cx.notify();
                }
            }))
            .on_scroll_wheel(cx.listener(move |this, ev: &ScrollWheelEvent, _, cx| {
                this.on_wheel(ix, slot, ev, cx)
            }))
            .child(edges_canvas)
            .children(nodes)
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_flame(
        &self,
        e: &Entry,
        ix: usize,
        slot: usize,
        selected: Option<u32>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let depth = e.flame.iter().map(|b| b.depth).max().unwrap_or(0) + 1;
        let timed = e.plan.total_time_ms().is_some();
        let bars: Vec<AnyElement> = e
            .flame
            .iter()
            .filter(|b| b.x1 - b.x0 >= 0.002)
            .filter_map(|b| {
                let n = e.plan.node(b.id)?;
                let share = e.plan.weight(n);
                let heat = Heat::of(share);
                let color = Self::heat_color(heat, p);
                let is_sel = selected == Some(b.id);
                let id = b.id;
                let metric = if timed {
                    n.total_time_ms.map(ms)
                } else {
                    n.cost.map(|c| format!("cost {}", count(c)))
                };
                let label = match (&n.object, metric) {
                    (Some(o), Some(m)) => format!("{} · {o} · {m}", n.operation),
                    (Some(o), None) => format!("{} · {o}", n.operation),
                    (None, Some(m)) => format!("{} · {m}", n.operation),
                    (None, None) => n.operation.clone(),
                };
                Some(
                    div()
                        .id(("fl", slot * 1_000_000 + id as usize))
                        .absolute()
                        .left(relative(b.x0 as f32))
                        .w(relative((b.x1 - b.x0) as f32))
                        .top(px(b.depth as f32 * FLAME_ROW))
                        .h(px(FLAME_ROW))
                        .pr(px(1.))
                        .pb(px(2.))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let pick = (selected != Some(id)).then_some(id);
                            this.select(ix, pick, cx);
                        }))
                        .child(
                            div()
                                .size_full()
                                .flex()
                                .items_center()
                                .px(px(6.))
                                .overflow_hidden()
                                .rounded(px(3.))
                                .bg(if heat == Heat::None {
                                    p.hover
                                } else {
                                    color.opacity(0.35)
                                })
                                .when(is_sel, |d| d.border_1().border_color(p.acc))
                                .hover(|s| s.bg(p.sel))
                                .child(
                                    div()
                                        .truncate()
                                        .font_family(MONO)
                                        .text_size(px(11.))
                                        .child(label),
                                ),
                        )
                        .into_any_element(),
                )
            })
            .collect();
        let viewport = self.viewports[slot].clone();
        div()
            .id(("plan-flame", slot))
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_y_scroll()
            .bg(p.bg)
            .p(px(10.))
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(px(depth as f32 * FLAME_ROW))
                    .child(
                        canvas(move |b, _, _| viewport.set(b), |_, _, _, _| {})
                            .absolute()
                            .size_full(),
                    )
                    .children(bars),
            )
            .into_any_element()
    }

    fn render_hotspots(&self, e: &Entry, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let cards: Vec<AnyElement> = e
            .findings
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let node = f.node_id;
                let active = node.is_some() && node == self.selected;
                let suggestion = f.suggestion.clone();
                div()
                    .id(("hot", i))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .p(px(8.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(if active { p.acc } else { p.bd })
                    .bg(if active { p.sel } else { p.elev })
                    .hover(|s| s.border_color(p.bd2))
                    .when(node.is_some(), |d| d.cursor_pointer())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let (Some(id), Some(cur)) = (node, this.current) {
                            this.select_and_reveal(cur, id, cx);
                        }
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(ui::dot(Self::severity_color(f.severity, p), 7.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(12.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(f.title.clone()),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(p.fg2)
                            .child(f.detail.clone()),
                    )
                    .when_some(suggestion, |d, s| {
                        let copy = s.clone();
                        d.child(
                            div()
                                .mt(px(2.))
                                .flex()
                                .items_start()
                                .gap(px(6.))
                                .p(px(6.))
                                .rounded(px(4.))
                                .bg(p.bg)
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .font_family(MONO)
                                        .text_size(px(11.))
                                        .child(s),
                                )
                                .child(
                                    div()
                                        .id(("hot-copy", i))
                                        .flex_none()
                                        .text_size(px(11.))
                                        .text_color(p.acc)
                                        .cursor_pointer()
                                        .on_click(cx.listener(move |_, _, _, cx| {
                                            cx.write_to_clipboard(
                                                gpui_kit::ClipboardItem::new_string(copy.clone()),
                                            );
                                            cx.emit(PlanViewEvent::Toast("Copied".into()));
                                        }))
                                        .child("Copy"),
                                ),
                        )
                    })
                    .into_any_element()
            })
            .collect();
        let empty = cards.is_empty();
        div()
            .id("hotspots")
            .w(px(280.))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .gap(px(6.))
            .p(px(8.))
            .border_r_1()
            .border_color(p.bd)
            .bg(p.panel)
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(ui::caption("HOTSPOTS", p))
                    .child(
                        div()
                            .font_family(MONO)
                            .text_size(px(10.5))
                            .text_color(p.fg3)
                            .child(e.findings.len().to_string()),
                    ),
            )
            .children(e.plan.warnings.iter().map(|w| {
                div()
                    .flex_none()
                    .text_size(px(11.5))
                    .text_color(p.stg)
                    .child(format!("⚠ {w}"))
            }))
            .children(cards)
            .when(empty, |d| {
                d.child(
                    div()
                        .text_size(px(12.))
                        .text_color(p.fg3)
                        .child("No hotspots: nothing in this plan crosses the finding thresholds."),
                )
            })
            .into_any_element()
    }

    fn render_detail(&self, e: &Entry, id: u32, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let Some(n) = e.plan.node(id) else {
            return div().into_any_element();
        };
        let share = e.plan.weight(n);
        let timed = e.plan.total_time_ms().is_some();
        let mut rows: Vec<(String, String)> = Vec::new();
        if let Some(r) = n.estimated_rows {
            rows.push(("Estimated rows".into(), count(r)));
        }
        if let Some(r) = n.actual_rows {
            let ratio = n.estimate_ratio().map_or(String::new(), |q| {
                if q >= 2.0 {
                    format!("  ({q:.0}× under)")
                } else if q <= 0.5 {
                    format!("  ({:.0}× over)", 1.0 / q)
                } else {
                    String::new()
                }
            });
            rows.push(("Actual rows".into(), format!("{}{ratio}", count(r))));
        }
        if let Some(l) = n.loops {
            rows.push(("Loops".into(), count(l)));
        }
        if let Some(t) = n.self_time_ms() {
            rows.push(("Self time".into(), ms(t)));
        }
        if let Some(t) = n.total_time_ms {
            rows.push(("Total time".into(), ms(t)));
        }
        rows.push((
            if timed {
                "Share of time"
            } else {
                "Share of cost"
            }
            .into(),
            format!("{:.1}%", share * 100.0),
        ));
        if let Some(c) = n.cost {
            rows.push(("Cost".into(), format!("{c:.2}")));
        }
        if let Some(h) = n.io.cache_hits {
            rows.push(("Pages hit".into(), count(h)));
        }
        if let Some(r) = n.io.disk_reads {
            rows.push(("Pages read".into(), count(r)));
        }
        if let Some(t) = n.io.temp_written {
            rows.push(("Temp written".into(), count(t)));
        }
        let findings: Vec<&Finding> = e
            .findings
            .iter()
            .filter(|f| f.node_id == Some(id))
            .collect();
        let section = |t: &str| div().mt(px(8.)).child(ui::caption(t.to_owned(), p));
        let kv = |k: String, v: String| {
            div()
                .flex()
                .gap(px(8.))
                .text_size(px(11.5))
                .child(div().w(px(110.)).flex_none().text_color(p.fg3).child(k))
                .child(div().flex_1().min_w_0().font_family(MONO).child(v))
        };
        let mut col = div()
            .id("plan-detail")
            .w(px(310.))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .gap(px(3.))
            .p(px(10.))
            .border_l_1()
            .border_color(p.bd)
            .bg(p.panel)
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(n.operation.clone()),
                    )
                    .child(
                        div()
                            .id("detail-close")
                            .px(px(4.))
                            .text_color(p.fg3)
                            .cursor_pointer()
                            .hover(|s| s.text_color(p.fg))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(cur) = this.current {
                                    this.select(cur, None, cx);
                                }
                            }))
                            .child("×"),
                    ),
            )
            .when_some(n.object.clone(), |d, o| {
                d.child(
                    div()
                        .font_family(MONO)
                        .text_size(px(11.5))
                        .text_color(p.fg2)
                        .child(o),
                )
            })
            .child(section("METRICS"))
            .children(rows.into_iter().map(|(k, v)| kv(k, v)));
        if !n.predicates.is_empty() {
            col = col.child(section("PREDICATES"));
            for pr in &n.predicates {
                col = col.child(
                    div()
                        .flex()
                        .flex_col()
                        .text_size(px(11.5))
                        .child(div().text_color(p.fg3).child(pr.kind.clone()))
                        .child(
                            div()
                                .p(px(5.))
                                .rounded(px(4.))
                                .bg(p.bg)
                                .font_family(MONO)
                                .child(pr.text.clone()),
                        ),
                );
            }
        }
        if !n.warnings.is_empty() {
            col = col.child(section("WARNINGS"));
            for w in &n.warnings {
                col = col.child(
                    div()
                        .text_size(px(11.5))
                        .text_color(p.stg)
                        .child(format!("⚠ {w}")),
                );
            }
        }
        if !findings.is_empty() {
            col = col.child(section("FINDINGS"));
            for f in findings {
                col = col.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .text_size(px(11.5))
                        .child(ui::dot(Self::severity_color(f.severity, p), 6.))
                        .child(div().flex_1().min_w_0().child(f.title.clone())),
                );
            }
        }
        if !n.details.is_empty() {
            col = col.child(section("DETAILS"));
            for (k, v) in &n.details {
                col = col.child(kv(k.clone(), v.clone()));
            }
        }
        col.into_any_element()
    }

    fn render_compare(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let (Some(c), Some(cur)) = (&self.compare, self.current) else {
            return div().into_any_element();
        };
        let (Some(a), Some(b)) = (self.entries.get(c.base), self.entries.get(cur)) else {
            return div().into_any_element();
        };
        let cmp = &c.cmp;
        let card = |label: &str, pair: &Pair, fmt: fn(f64) -> String, lower_better: bool| {
            let pct = pair.percent();
            let color = match pct {
                Some(v) if v.abs() < 5.0 => p.fg3,
                Some(v) if (v < 0.0) == lower_better => p.dev,
                Some(_) => p.prod,
                None => p.fg3,
            };
            div()
                .flex_1()
                .min_w(px(120.))
                .flex()
                .flex_col()
                .gap(px(2.))
                .px(px(10.))
                .py(px(6.))
                .rounded(px(6.))
                .border_1()
                .border_color(p.bd)
                .bg(p.elev)
                .child(ui::caption(label.to_owned(), p))
                .child(div().font_family(MONO).text_size(px(12.)).child(format!(
                    "{} → {}",
                    pair.a.map_or("–".into(), fmt),
                    pair.b.map_or("–".into(), fmt)
                )))
                .child(
                    div()
                        .font_family(MONO)
                        .text_size(px(11.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(color)
                        .child(pct.map_or("–".into(), |v| format!("{v:+.0}%"))),
                )
        };
        let summary = div()
            .flex_none()
            .flex()
            .gap(px(8.))
            .p(px(8.))
            .border_b_1()
            .border_color(p.bd)
            .child(card("TIME", &cmp.time_ms, ms, true))
            .child(card("PLANNING", &cmp.planning_ms, ms, true))
            .child(card("ROWS", &cmp.rows, count, false))
            .child(card("PAGES (I/O)", &cmp.pages, count, true))
            .child(card("COST", &cmp.cost, count, true));
        let pane_header = |title: String, sub: String| {
            div()
                .h(px(26.))
                .flex_none()
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(10.))
                .border_b_1()
                .border_color(p.bd)
                .bg(p.panel)
                .child(
                    div()
                        .text_size(px(11.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .font_family(MONO)
                        .text_size(px(10.5))
                        .text_color(p.fg3)
                        .child(sub),
                )
        };
        // The graphs keep a usable height; the delta table gives way first.
        let panes = div()
            .flex_1()
            .min_h(px(170.))
            .flex()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .border_r_1()
                    .border_color(p.bd)
                    .child(pane_header(
                        format!("Baseline · Plan {} · {}", c.base + 1, a.label),
                        first_line(&a.plan.sql),
                    ))
                    .child(self.render_pane(c.base, 1, c.base_selected, p, cx)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(pane_header(
                        format!("This plan · Plan {} · {}", cur + 1, b.label),
                        first_line(&b.plan.sql),
                    ))
                    .child(self.render_pane(cur, 0, self.selected, p, cx)),
            );
        let delta_cell = |pair: &Pair, fmt: fn(f64) -> String, lower_better: bool| {
            let color = match pair.delta() {
                Some(d) if d.abs() < f64::EPSILON => p.fg3,
                Some(d) if (d < 0.0) == lower_better => p.dev,
                Some(_) => p.prod,
                None => p.fg3,
            };
            div()
                .w(px(170.))
                .flex_none()
                .flex()
                .justify_end()
                .gap(px(6.))
                .child(div().text_color(p.fg2).child(format!(
                    "{} → {}",
                    pair.a.map_or("–".into(), fmt),
                    pair.b.map_or("–".into(), fmt)
                )))
                .child(
                    div()
                        .w(px(46.))
                        .flex()
                        .justify_end()
                        .text_color(color)
                        .child(match (pair.percent(), pair.a, pair.b) {
                            (Some(v), _, _) => format!("{v:+.0}%"),
                            (None, None, Some(_)) => "new".into(),
                            (None, Some(_), None) => "gone".into(),
                            _ => String::new(),
                        }),
                )
        };
        let rows: Vec<AnyElement> = cmp
            .nodes
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let (ida, idb) = (d.a, d.b);
                let active = (idb.is_some() && idb == self.selected)
                    || (ida.is_some() && ida == c.base_selected);
                div()
                    .id(("delta", i))
                    .h(px(24.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(10.))
                    .font_family(MONO)
                    .text_size(px(11.))
                    .when(active, |d| d.bg(p.sel))
                    .hover(|s| s.bg(p.hover))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let (Some(base), Some(cur)) =
                            (this.compare.as_ref().map(|c| c.base), this.current)
                        else {
                            return;
                        };
                        if let Some(c) = &mut this.compare {
                            c.base_selected = ida;
                        }
                        if let Some(id) = ida {
                            this.reveal(base, 1, id);
                        }
                        if let Some(id) = idb {
                            this.reveal(cur, 0, id);
                        }
                        this.selected = idb;
                        this.highlight(cx);
                        cx.notify();
                    }))
                    .child(div().flex_1().min_w_0().truncate().child(d.label.clone()))
                    .child(delta_cell(&d.self_time_ms, ms, true))
                    .child(delta_cell(&d.rows, count, false))
                    .child(delta_cell(&d.pages, count, true))
                    .into_any_element()
            })
            .collect();
        let head = |t: &str| {
            div()
                .w(px(170.))
                .flex_none()
                .flex()
                .justify_end()
                .child(t.to_owned())
        };
        let table = div()
            .flex_shrink(1.)
            .min_h(px(72.))
            .max_h(px(200.))
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(p.bd)
            .child(
                div()
                    .h(px(24.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(10.))
                    .bg(p.panel)
                    .text_size(px(10.5))
                    .text_color(p.fg3)
                    .child(div().flex_1().child("OPERATOR"))
                    .child(head("SELF TIME"))
                    .child(head("ROWS"))
                    .child(head("PAGES")),
            )
            .child(
                div()
                    .id("delta-rows")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(rows),
            );
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(summary)
            .child(panes)
            .child(table)
            .into_any_element()
    }

    fn render_empty(&self, p: &Palette) -> AnyElement {
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
                    .child("No plan yet"),
            )
            .child(div().text_size(px(12.)).text_color(p.fg3).child(format!(
                "{} explains the statement at the cursor; {} runs it for an actual plan (writes are rolled back).",
                ui::keys("⌘E", "Ctrl+E"),
                ui::keys("⇧⌘E", "Ctrl+Shift+E")
            )))
            .into_any_element()
    }
}

impl Render for PlanView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        if self.fit_pending && self.mode == Mode::Graph && !self.fit(cx) {
            // Not laid out yet: try again once this frame has measured the pane.
            cx.on_next_frame(window, |this, _, cx| {
                if this.fit_pending {
                    cx.notify();
                }
            });
        }
        let show_hotspots = self.show_hotspots();
        let banner = self.render_banner(&p, cx);
        let body: AnyElement = match self
            .current
            .and_then(|i| self.entries.get(i).map(|e| (i, e)))
        {
            None if banner.is_some() => div().flex_1().into_any_element(),
            None => self.render_empty(&p),
            Some(_) if self.compare.is_some() => self.render_compare(&p, cx),
            Some((ix, e)) => {
                let pane = self.render_pane(ix, 0, self.selected, &p, cx);
                let detail = self.selected.map(|id| self.render_detail(e, id, &p, cx));
                let hotspots = show_hotspots.then(|| self.render_hotspots(e, &p, cx));
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .children(hotspots)
                    .child(pane)
                    .children(detail)
                    .into_any_element()
            }
        };
        let menu = self.menu.map(|m| self.render_menu(m, &p, cx));
        let width = self.width.clone();
        div()
            .id("plan-view")
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .font_family(SANS)
            .child(
                canvas(
                    move |b, _, _| width.set(f32::from(b.size.width)),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(
                div()
                    .relative()
                    .flex_none()
                    .child(self.render_header(&p, cx))
                    .children(menu),
            )
            .children(banner)
            .child(body)
    }
}

/// Milliseconds as `0.42 ms`, `125.6 ms` or `1.24 s`.
pub fn ms(v: f64) -> String {
    if v >= 1000.0 {
        format!("{:.2} s", v / 1000.0)
    } else if v >= 10.0 {
        format!("{v:.1} ms")
    } else {
        format!("{v:.2} ms")
    }
}

/// A count with thousands separators (rounded).
pub fn count(v: f64) -> String {
    if v < 0.0 {
        return format!("-{}", count(-v));
    }
    thousands(v.round() as u64)
}

fn rows_text(n: &PlanNode) -> String {
    match (n.actual_rows, n.estimated_rows) {
        (Some(a), Some(e)) => format!("{} of {} est", count(a), count(e)),
        (Some(a), None) => format!("{} rows", count(a)),
        (None, Some(e)) => format!("{} rows est", count(e)),
        (None, None) => String::new(),
    }
}

fn first_line(sql: &str) -> String {
    let s: String = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() > 90 {
        format!("{}…", s.chars().take(90).collect::<String>())
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(ms(0.4231), "0.42 ms");
        assert_eq!(ms(125.64), "125.6 ms");
        assert_eq!(ms(1240.0), "1.24 s");
        assert_eq!(count(1_000_000.4), "1,000,000");
        assert_eq!(count(-12.0), "-12");
        assert_eq!(first_line("select *\n  from   t"), "select * from t");
        let n = PlanNode {
            actual_rows: Some(9990.0),
            estimated_rows: Some(9467.0),
            ..PlanNode::op("Seq Scan")
        };
        assert_eq!(rows_text(&n), "9,990 of 9,467 est");
    }
}
