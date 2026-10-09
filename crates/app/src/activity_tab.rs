//! Activity monitor tab (DBX-5b): the sessions and running queries of one connection's
//! server, refreshed every 2, 5 or 10 s (or paused), with Cancel query and Terminate
//! session.
//!
//! The tab owns a session of its own (opened by the first [`Command::Activity`]), so the
//! list keeps refreshing while the user's query tabs are busy. Every action asks first;
//! on a Production connection the user also types the session id or `KILL`. The core
//! re-checks Production, refuses the monitor's own session and writes every action to
//! history. None of this is reachable from the MCP server or `swy`.

use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, ClipboardItem, Context, Entity, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div, px, relative,
    uniform_list,
};
use switchyard_core::db::Engine;
use switchyard_core::db::activity::{
    self, Activity, ActivityAction, ActivitySession, SessionTarget,
};
use switchyard_core::store::{DbConnection, ProfileId};
use switchyard_core::{Command, RequestId, RuntimeHandle, SessionId};

use crate::app_state::next_id;
use crate::appearance::{rpx, ts};
use crate::plan_view::ms;
use crate::theme::{MONO, Palette, SANS, palette};
use crate::ui::{self, Kind};
use crate::workspace::{Tab, Workspace};

/// Row height of the session list.
const ROW_H: f32 = 26.;

/// Refresh intervals offered, seconds.
const INTERVALS: [u64; 3] = [2, 5, 10];

/// Columns after the id: header and width. SQL takes the rest.
const COLUMNS: [(&str, f32); 9] = [
    ("USER", 110.),
    ("DATABASE", 110.),
    ("CLIENT", 120.),
    ("STATE", 90.),
    ("WAIT", 140.),
    ("STARTED", 74.),
    ("DURATION", 80.),
    ("BLOCKED BY", 80.),
    ("SQL", 0.),
];

/// An action waiting for the user's confirmation.
struct Pending {
    action: ActivityAction,
    target: SessionTarget,
    /// Who it hits, for the prompt (`1234 · app@shop`).
    who: String,
}

/// The second, typed confirmation a Production connection needs: the session id or
/// `KILL` (case-sensitive), surrounding spaces ignored.
pub fn typed_confirmation_ok(typed: &str, target: &SessionTarget) -> bool {
    let t = typed.trim();
    t == "KILL" || t == target.label()
}

/// An activity monitor tab.
pub struct ActivityTab {
    core: RuntimeHandle,
    /// The connection.
    pub connection: DbConnection,
    session: SessionId,
    request: Option<RequestId>,
    action_request: Option<RequestId>,
    data: Option<Arc<Activity>>,
    error: Option<String>,
    /// Outcome of the last action.
    status: Option<(bool, String)>,
    paused: bool,
    interval: u64,
    /// Id of the selected row.
    selected: Option<String>,
    pending: Option<Pending>,
    confirm_input: Entity<InputState>,
    sql_editor: Entity<EditorState>,
    /// SQL shown in the editor (to update it only when it changes).
    shown_sql: String,
    _ticker: Task<()>,
    _sub: Subscription,
}

impl ActivityTab {
    /// A tab on `connection` that opens its own session and lists right away.
    pub fn new(
        core: RuntimeHandle,
        connection: DbConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let confirm_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Type the session id or KILL"));
        let sub = cx.subscribe_in(
            &confirm_input,
            window,
            |this, _, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { .. } => this.confirm(window, cx),
                InputEvent::Change => cx.notify(),
                _ => {}
            },
        );
        let sql_editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql")
                .line_number(false)
                .indent_guides(false)
                .soft_wrap(true)
        });
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                let Ok(secs) = this.update(cx, |t, _| t.interval) else {
                    return;
                };
                cx.background_executor()
                    .timer(Duration::from_secs(secs))
                    .await;
                let alive = this.update(cx, |t, cx| {
                    if !t.paused && t.request.is_none() {
                        t.refresh(cx);
                    }
                });
                if alive.is_err() {
                    return;
                }
            }
        });
        let mut t = Self {
            core,
            connection,
            session: next_id(),
            request: None,
            action_request: None,
            data: None,
            error: None,
            status: None,
            paused: false,
            interval: INTERVALS[0],
            selected: None,
            pending: None,
            confirm_input,
            sql_editor,
            shown_sql: String::new(),
            _ticker: ticker,
            _sub: sub,
        };
        t.send_refresh();
        t
    }

    fn send_refresh(&mut self) {
        let request = next_id();
        self.request = Some(request);
        self.core.send(Command::Activity {
            session: self.session,
            connection: self.connection.id.clone(),
            request,
        });
    }

    /// List the sessions again.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.send_refresh();
        cx.notify();
    }

    /// Whether `request` is this tab's listing.
    pub fn owns(&self, request: RequestId) -> bool {
        self.request == Some(request)
    }

    /// Whether `request` is this tab's action.
    pub fn owns_action(&self, request: RequestId) -> bool {
        self.action_request == Some(request)
    }

    /// Close the monitor's session (the tab is closing).
    pub fn shutdown(&mut self) {
        self.core.send(Command::CloseSession {
            session: self.session,
        });
    }

    /// The list arrived.
    pub fn on_result(
        &mut self,
        result: Result<Arc<Activity>, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.request = None;
        match result {
            Ok(a) => {
                self.error = None;
                self.data = Some(a);
                self.sync_editor(window, cx);
            }
            Err(e) => self.error = Some(e),
        }
        cx.notify();
    }

    /// A cancel or kill finished.
    pub fn on_action_result(&mut self, result: Result<String, String>, cx: &mut Context<Self>) {
        self.action_request = None;
        self.status = Some(match result {
            Ok(m) => (true, m),
            Err(e) => (false, e),
        });
        if self.request.is_none() {
            self.send_refresh();
        }
        cx.notify();
    }

    fn selected_row(&self) -> Option<&ActivitySession> {
        let id = self.selected.as_ref()?;
        self.data.as_ref()?.sessions.iter().find(|s| &s.id == id)
    }

    /// Show the selected row's full SQL in the read-only editor.
    fn sync_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sql = self
            .selected_row()
            .and_then(|s| s.sql.clone())
            .unwrap_or_default();
        if sql != self.shown_sql {
            self.shown_sql = sql.clone();
            self.sql_editor
                .update(cx, |e, cx| e.set_value(sql, window, cx));
        }
    }

    fn select(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(id);
        self.sync_editor(window, cx);
        cx.notify();
    }

    /// Ask for confirmation of `action` on the selected row.
    fn ask(&mut self, action: ActivityAction, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.selected_row() else {
            return;
        };
        if row.is_self {
            self.status = Some((false, "This is the monitor's own session".into()));
            cx.notify();
            return;
        }
        let Some(target) = row.target.clone() else {
            self.status = Some((false, format!("Session id {} is not valid", row.id)));
            cx.notify();
            return;
        };
        let who = match (&row.user, &row.database) {
            (Some(u), Some(d)) => format!("{} · {u}@{d}", target.label()),
            (Some(u), None) => format!("{} · {u}", target.label()),
            _ => target.label(),
        };
        self.pending = Some(Pending {
            action,
            target,
            who,
        });
        self.confirm_input
            .update(cx, |i, cx| i.set_value("", window, cx));
        if self.connection.environment.is_production() {
            self.confirm_input.update(cx, |i, cx| i.focus(window, cx));
        }
        cx.notify();
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.pending.as_ref() else {
            return;
        };
        let production = self.connection.environment.is_production();
        if production && !typed_confirmation_ok(&self.confirm_input.read(cx).value(), &p.target) {
            return;
        }
        let Some(p) = self.pending.take() else {
            return;
        };
        let request = next_id();
        self.action_request = Some(request);
        self.status = None;
        self.core.send(Command::SessionAction {
            session: self.session,
            request,
            action: p.action,
            target: p.target,
            confirmed: production,
        });
        self.confirm_input
            .update(cx, |i, cx| i.set_value("", window, cx));
        cx.notify();
    }

    fn render_hints(&self, a: &Activity, p: &Palette, cx: &mut Context<Self>) -> Vec<AnyElement> {
        a.hints
            .iter()
            .enumerate()
            .map(|(i, h)| {
                div()
                    .flex_none()
                    .flex()
                    .items_start()
                    .gap(rpx(8.))
                    .mx(rpx(12.))
                    .mt(rpx(8.))
                    .px(rpx(10.))
                    .py(rpx(6.))
                    .rounded(px(6.))
                    .bg(p.stg.opacity(0.08))
                    .border_1()
                    .border_color(p.stg.opacity(0.35))
                    .text_size(ts::BODY)
                    .child(div().flex_1().min_w_0().child(h.message.clone()))
                    .when_some(h.fix.clone(), |d, fix| {
                        d.child(
                            div()
                                .flex_none()
                                .px(rpx(6.))
                                .rounded(px(4.))
                                .bg(p.bg)
                                .font_family(MONO)
                                .text_size(ts::SMALL)
                                .child(fix.clone()),
                        )
                        .child(
                            ui::button(("act-hint-copy", i), "Copy", Kind::Ghost, p)
                                .h(rpx(20.))
                                .text_size(ts::SMALL)
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(fix.clone()));
                                })),
                        )
                    })
                    .into_any_element()
            })
            .collect()
    }

    fn render_confirm(&self, p: &Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        let pending = self.pending.as_ref()?;
        let production = self.connection.environment.is_production();
        let ok = !production
            || typed_confirmation_ok(&self.confirm_input.read(cx).value(), &pending.target);
        let (verb, consequence) = match pending.action {
            ActivityAction::CancelQuery => ("Cancel the running query of", "The session stays."),
            ActivityAction::Terminate => (
                "Terminate session",
                "Its open transaction is rolled back and the client is disconnected.",
            ),
        };
        Some(
            div()
                .flex_none()
                .flex()
                .flex_col()
                .gap(rpx(6.))
                .mx(rpx(12.))
                .mt(rpx(8.))
                .px(rpx(10.))
                .py(rpx(8.))
                .rounded(px(6.))
                .bg(p.prod.opacity(0.08))
                .border_1()
                .border_color(p.prod.opacity(0.5))
                .text_size(ts::BODY)
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(format!("{verb} {}?", pending.who)),
                )
                .child(div().text_color(p.fg2).child(consequence))
                .when(production, |d| {
                    d.child(
                        div().text_color(p.prod).child(
                            "Production connection: type the session id or KILL to confirm.",
                        ),
                    )
                    .child(
                        div()
                            .w(rpx(260.))
                            .child(Input::new(&self.confirm_input).text_size(ts::BODY)),
                    )
                })
                .child(
                    div()
                        .flex()
                        .gap(rpx(8.))
                        .child(
                            ui::button("act-confirm-no", "Keep it", Kind::Secondary, p).on_click(
                                cx.listener(|t, _, _, cx| {
                                    t.pending = None;
                                    cx.notify();
                                }),
                            ),
                        )
                        .child(
                            ui::button(
                                "act-confirm-yes",
                                pending.action.label(),
                                if ok {
                                    Kind::Destructive
                                } else {
                                    Kind::Secondary
                                },
                                p,
                            )
                            .when(!ok, |b| b.opacity(0.5))
                            .on_click(cx.listener(|t, _, window, cx| t.confirm(window, cx))),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_list(&self, a: &Arc<Activity>, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let cell = |w: f32| {
            let d = div().min_w_0().px(rpx(5.)).truncate();
            if w == 0. {
                d.flex_1().min_w(rpx(160.))
            } else {
                d.w(rpx(w)).flex_none()
            }
        };
        let header = div()
            .flex_none()
            .flex()
            .h(rpx(ROW_H))
            .items_center()
            .px(rpx(8.))
            .border_b_1()
            .border_color(p.bd)
            .text_size(ts::CAPTION_PLUS)
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(p.fg3)
            .child(cell(90.).child("ID"))
            .children(COLUMNS.iter().map(|(h, w)| cell(*w).child(*h)))
            .child(div().w(rpx(44.)).flex_none());
        if a.sessions.is_empty() {
            return div()
                .flex_1()
                .flex()
                .flex_col()
                .child(header)
                .child(
                    div()
                        .p(rpx(16.))
                        .text_size(ts::BODY)
                        .text_color(p.fg3)
                        .child(match a.engine {
                            Engine::Snowflake => "No running queries.",
                            _ => "No sessions visible.",
                        }),
                )
                .into_any_element();
        }
        let data = a.clone();
        let selected = self.selected.clone();
        let p2 = *p;
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(header)
            .child(
                uniform_list(
                    "activity-rows",
                    data.sessions.len(),
                    cx.processor(move |_this, range: std::ops::Range<usize>, _w, cx| {
                        let p = &p2;
                        range
                            .map(|r| {
                                let s = &data.sessions[r];
                                let id = s.id.clone();
                                let tsv = s.to_tsv();
                                let opt = |v: &Option<String>| v.clone().unwrap_or_default();
                                let started = s
                                    .started_ms
                                    .and_then(chrono::DateTime::from_timestamp_millis)
                                    .map(|d| {
                                        d.with_timezone(&chrono::Local)
                                            .format("%H:%M:%S")
                                            .to_string()
                                    })
                                    .unwrap_or_default();
                                let cells: [String; 9] = [
                                    opt(&s.user),
                                    opt(&s.database),
                                    opt(&s.client),
                                    opt(&s.state),
                                    opt(&s.wait),
                                    started,
                                    s.duration_ms.map(|d| ms(d as f64)).unwrap_or_default(),
                                    opt(&s.blocked_by),
                                    s.sql_preview(),
                                ];
                                let is_sel = selected.as_deref() == Some(s.id.as_str());
                                div()
                                    .id(("act-row", r))
                                    .h(rpx(ROW_H))
                                    .flex()
                                    .items_center()
                                    .px(rpx(8.))
                                    .border_b_1()
                                    .border_color(p.line)
                                    .text_size(ts::BODY)
                                    .text_color(if s.running { p.fg } else { p.fg2 })
                                    .when(is_sel, |d| d.bg(p.sel))
                                    .when(!is_sel, |d| d.hover(|st| st.bg(p.hover)))
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.select(id.clone(), window, cx)
                                    }))
                                    .child(
                                        cell(90.)
                                            .flex()
                                            .gap(rpx(4.))
                                            .font_family(MONO)
                                            .child(div().min_w_0().truncate().child(s.id.clone()))
                                            .when(s.is_self, |d| {
                                                d.child(
                                                    div()
                                                        .flex_none()
                                                        .text_size(ts::TINY)
                                                        .text_color(p.acc)
                                                        .child("ME"),
                                                )
                                            }),
                                    )
                                    .children(
                                        cells.into_iter().zip(COLUMNS.iter()).enumerate().map(
                                            |(c, (text, (_, w)))| {
                                                cell(*w)
                                                    .when(c == 8 || c == 6, |d| d.font_family(MONO))
                                                    .when(c == 7 && !text.is_empty(), |d| {
                                                        d.text_color(p.prod)
                                                    })
                                                    .child(text)
                                            },
                                        ),
                                    )
                                    .child(
                                        div()
                                            .id(("act-copy", r))
                                            .w(rpx(44.))
                                            .flex_none()
                                            .text_size(ts::SMALL)
                                            .text_color(p.fg3)
                                            .hover(|st| st.text_color(p.acc))
                                            .on_click(move |_, _, cx| {
                                                cx.stop_propagation();
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    tsv.clone(),
                                                ));
                                            })
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

    fn render_detail(&self, p: &Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        let row = self.selected_row()?;
        let a = self.data.as_ref()?;
        let busy = self.action_request.is_some() || self.pending.is_some();
        let can = |allowed: bool| allowed && !row.is_self && row.target.is_some() && !busy;
        let tsv = row.to_tsv();
        let sql = row.sql.clone().unwrap_or_default();
        let action_btn = |id: &'static str, action: ActivityAction, allowed: bool| {
            let enabled = can(allowed);
            ui::button(
                id,
                action.label(),
                if enabled {
                    Kind::Destructive
                } else {
                    Kind::Secondary
                },
                p,
            )
            .when(!enabled, |b| b.opacity(0.45))
            .when(enabled, |b| {
                b.on_click(cx.listener(move |t, _, window, cx| t.ask(action, window, cx)))
            })
        };
        Some(
            div()
                .flex_none()
                .h(rpx(200.))
                .flex()
                .flex_col()
                .border_t_1()
                .border_color(p.bd)
                .child(
                    div()
                        .h(rpx(34.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(rpx(8.))
                        .px(rpx(12.))
                        .border_b_1()
                        .border_color(p.bd)
                        .text_size(ts::BODY)
                        .child(div().flex_1().min_w_0().truncate().font_family(MONO).child(
                            format!(
                                    "{}{}{}",
                                    row.id,
                                    row.program
                                        .as_deref()
                                        .map(|p| format!(" · {p}"))
                                        .unwrap_or_default(),
                                    if row.is_self { " · this monitor" } else { "" }
                                ),
                        ))
                        .child(
                            ui::button("act-copy-row", "Copy row", Kind::Ghost, p).on_click(
                                cx.listener(move |_, _, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(tsv.clone()));
                                }),
                            ),
                        )
                        .child(
                            ui::button("act-copy-sql", "Copy SQL", Kind::Ghost, p).on_click(
                                cx.listener(move |_, _, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(sql.clone()));
                                }),
                            ),
                        )
                        .when(a.can_cancel, |d| {
                            d.child(action_btn(
                                "act-cancel",
                                ActivityAction::CancelQuery,
                                row.running,
                            ))
                        })
                        .when(a.can_terminate, |d| {
                            d.child(action_btn("act-kill", ActivityAction::Terminate, true))
                        }),
                )
                .child(
                    div().flex_1().min_h_0().child(
                        Editor::new(&self.sql_editor)
                            // Read-only, not disabled: the text stays selectable.
                            .readonly(true)
                            .bordered(false)
                            .appearance(false)
                            .h(relative(1.))
                            .font_family(MONO)
                            .text_size(ts::BODY),
                    ),
                )
                .into_any_element(),
        )
    }
}

impl Render for ActivityTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let interval_btn = |secs: u64| -> (SharedString, bool, ui::OnClick) {
            let this = cx.entity().downgrade();
            (
                format!("{secs} s").into(),
                self.interval == secs,
                Box::new(move |_, _, cx| {
                    let _ = this.update(cx, |t, cx| {
                        t.interval = secs;
                        cx.notify();
                    });
                }),
            )
        };
        let count = self.data.as_ref().map_or(0, |a| a.sessions.len());
        let header = div()
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
                    .child(format!("Activity · {}", self.connection.name)),
            )
            .when(self.connection.environment.is_production(), |d| {
                d.child(
                    div()
                        .px(rpx(5.))
                        .rounded(px(3.))
                        .border_1()
                        .border_color(p.prod)
                        .text_size(ts::TINY_PLUS)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.prod)
                        .child("PRODUCTION"),
                )
            })
            .child(
                div()
                    .font_family(MONO)
                    .text_size(ts::SMALL)
                    .text_color(p.fg3)
                    .child(format!("{count} sessions")),
            )
            .child(div().flex_1())
            .child(ui::segmented(
                "activity-interval",
                INTERVALS.iter().map(|s| interval_btn(*s)).collect(),
                20.,
                &p,
            ))
            .child(
                ui::button(
                    "activity-pause",
                    if self.paused { "Resume" } else { "Pause" },
                    Kind::Secondary,
                    &p,
                )
                .h(rpx(24.))
                .on_click(cx.listener(|t, _, _, cx| {
                    t.paused = !t.paused;
                    cx.notify();
                })),
            )
            .child(if self.request.is_some() && self.data.is_none() {
                ui::shimmer(70., &p)
            } else {
                ui::button("activity-refresh", "Refresh", Kind::Secondary, &p)
                    .h(rpx(24.))
                    .when(self.request.is_some(), |b| b.opacity(0.6))
                    .on_click(cx.listener(|t, _, _, cx| {
                        if t.request.is_none() {
                            t.refresh(cx);
                        }
                    }))
                    .into_any_element()
            });
        let status = self.status.as_ref().map(|(ok, m)| {
            div()
                .flex_none()
                .mx(rpx(12.))
                .mt(rpx(8.))
                .text_size(ts::BODY)
                .text_color(if *ok { p.dev } else { p.prod })
                .child(m.clone())
        });
        let error = self.error.as_ref().map(|e| {
            div()
                .flex_none()
                .mx(rpx(12.))
                .mt(rpx(8.))
                .text_size(ts::BODY)
                .text_color(p.prod)
                .child(e.clone())
        });
        let data = self.data.clone();
        let hints = data
            .as_ref()
            .map(|a| self.render_hints(a, &p, cx))
            .unwrap_or_default();
        let confirm = self.render_confirm(&p, cx);
        let list = match &data {
            Some(a) => self.render_list(a, &p, cx),
            None => div()
                .p(rpx(16.))
                .when(self.error.is_none(), |d| d.child(ui::shimmer(240., &p)))
                .into_any_element(),
        };
        let detail = self.render_detail(&p, cx);
        div()
            .id("activity-tab")
            .size_full()
            .flex()
            .flex_col()
            .font_family(SANS)
            .bg(p.bg)
            .text_color(p.fg)
            .child(header)
            .children(hints)
            .children(error)
            .children(status)
            .children(confirm)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .mt(rpx(6.))
                    .flex()
                    .flex_col()
                    .child(list),
            )
            .children(detail)
    }
}

impl Workspace {
    /// Open (or focus) the activity monitor of `connection`.
    pub(crate) fn open_activity(
        &mut self,
        connection: DbConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !activity::supported(connection.engine) {
            return self.toast(
                format!(
                    "The activity monitor is not available for {}",
                    connection.engine.display_name()
                ),
                cx,
            );
        }
        if let Some(ix) = self.tabs.iter().position(
            |t| matches!(t, Tab::Activity(a) if a.read(cx).connection.id == connection.id),
        ) {
            self.activate(ix, cx);
            return;
        }
        let core = self.core.clone();
        let tab = cx.new(|cx| ActivityTab::new(core, connection, window, cx));
        self.tabs.push(Tab::Activity(tab));
        self.activate(self.tabs.len() - 1, cx);
    }

    /// The connection menu's "Activity monitor".
    pub(crate) fn open_activity_for(
        &mut self,
        id: &ProfileId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(conn) = self.profiles.db(id).cloned() {
            self.open_activity(conn, window, cx);
        }
    }

    /// The palette's "Activity Monitor": the active query tab's connection, else the
    /// schema tree's.
    pub(crate) fn open_activity_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let conn = self
            .active_sql()
            .and_then(|t| t.read(cx).connection.clone())
            .or_else(|| {
                let id = self.explorer.scope_conn()?;
                self.profiles.db(&id).cloned()
            });
        match conn {
            Some(c) => self.open_activity(c, window, cx),
            None => self.toast("Open a connection first", cx),
        }
    }

    /// Route [`switchyard_core::Event::Activity`] to its tab.
    pub(crate) fn on_activity(
        &mut self,
        request: RequestId,
        result: Result<Arc<Activity>, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tab = self.tabs.iter().find_map(|t| match t {
            Tab::Activity(a) if a.read(cx).owns(request) => Some(a.clone()),
            _ => None,
        });
        if let Some(tab) = tab {
            tab.update(cx, |t, cx| t.on_result(result, window, cx));
        }
    }

    /// Route [`switchyard_core::Event::SessionAction`] to its tab.
    pub(crate) fn on_session_action(
        &mut self,
        request: RequestId,
        result: Result<String, String>,
        cx: &mut Context<Self>,
    ) {
        let tab = self.tabs.iter().find_map(|t| match t {
            Tab::Activity(a) if a.read(cx).owns_action(request) => Some(a.clone()),
            _ => None,
        });
        if let Some(tab) = tab {
            tab.update(cx, |t, cx| t.on_action_result(result, cx));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_needs_the_id_or_kill() {
        let t = SessionTarget::Backend { id: 4242 };
        assert!(typed_confirmation_ok("4242", &t));
        assert!(typed_confirmation_ok(" KILL ", &t));
        assert!(!typed_confirmation_ok("kill", &t));
        assert!(!typed_confirmation_ok("", &t));
        assert!(!typed_confirmation_ok("424", &t));
        let o = SessionTarget::Oracle { sid: 12, serial: 3 };
        assert!(typed_confirmation_ok("12,3", &o));
        assert!(!typed_confirmation_ok("12", &o));
    }
}
