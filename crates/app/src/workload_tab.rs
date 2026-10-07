//! Workload tab: how a database is used. Statements by total time (pg_stat_statements or
//! Query Store), tables by full scans, indexes by use, and the indexes SQL Server reports
//! missing. Anything unavailable shows as a hint with the statement that fixes it.

use std::sync::Arc;

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, ClipboardItem, Context, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    div, px,
};
use switchyard_core::plan::access::{Hint, Workload};
use switchyard_core::{Command, RequestId, RuntimeHandle, SessionId};

use crate::app_state::next_id;
use crate::plan_view::{count, ms};
use crate::theme::{MONO, Palette, SANS, palette};
use crate::ui::{self, Kind};

/// Which list is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    Statements,
    Tables,
    Indexes,
    Missing,
}

/// The workload of one connection, read through an open session.
pub struct WorkloadTab {
    core: RuntimeHandle,
    session: SessionId,
    /// Connection name, for the tab title.
    pub name: String,
    request: Option<RequestId>,
    data: Option<Arc<Workload>>,
    error: Option<String>,
    section: Section,
}

impl WorkloadTab {
    /// A tab that loads the workload of `session` right away.
    pub fn new(core: RuntimeHandle, session: SessionId, name: String) -> Self {
        let mut t = Self {
            core,
            session,
            name,
            request: None,
            data: None,
            error: None,
            section: Section::Statements,
        };
        t.refresh();
        t
    }

    /// Read the statistics again.
    pub fn refresh(&mut self) {
        let request = next_id();
        self.request = Some(request);
        self.error = None;
        self.core.send(Command::Workload {
            session: self.session,
            request,
        });
    }

    /// Whether `request` is this tab's.
    pub fn owns(&self, request: RequestId) -> bool {
        self.request == Some(request)
    }

    /// The statistics arrived.
    pub fn on_result(&mut self, result: Result<Arc<Workload>, String>, cx: &mut Context<Self>) {
        self.request = None;
        match result {
            Ok(w) => {
                // Open on the first list that has something.
                if self.data.is_none() {
                    self.section = if !w.statements.is_empty() {
                        Section::Statements
                    } else if !w.missing_indexes.is_empty() {
                        Section::Missing
                    } else {
                        Section::Tables
                    };
                }
                self.data = Some(w);
            }
            Err(e) => self.error = Some(e),
        }
        cx.notify();
    }

    fn copy_button(
        id: impl Into<gpui_kit::ElementId>,
        text: String,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        ui::button(id, "Copy", Kind::Ghost, p)
            .h(px(20.))
            .px(px(6.))
            .text_size(px(11.))
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
            }))
            .into_any_element()
    }

    fn render_hint(&self, i: usize, h: &Hint, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(4.))
            .px(px(10.))
            .py(px(8.))
            .rounded(px(6.))
            .bg(p.stg.opacity(0.08))
            .border_1()
            .border_color(p.stg.opacity(0.35))
            .child(
                div()
                    .min_w_0()
                    .text_size(px(12.))
                    .text_color(p.fg)
                    .child(h.message.clone()),
            )
            .when_some(h.fix.clone(), |d, fix| {
                d.child(
                    div()
                        .flex()
                        .items_start()
                        .gap(px(8.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .px(px(8.))
                                .py(px(5.))
                                .rounded(px(4.))
                                .bg(p.bg)
                                .font_family(MONO)
                                .text_size(px(11.5))
                                .child(fix.clone()),
                        )
                        .child(Self::copy_button(("hint-copy", i), fix, p, cx)),
                )
            })
            .into_any_element()
    }

    /// A header row and data rows of fixed-width columns (the first column flexes).
    fn table(
        headers: &[(&'static str, f32)],
        rows: Vec<Vec<AnyElement>>,
        p: &Palette,
    ) -> AnyElement {
        let cell = |w: f32, first: bool| {
            let d = div().min_w_0().px(px(6.)).truncate();
            if first {
                d.flex_1()
            } else {
                d.w(px(w)).flex_none()
            }
        };
        let mut list = div().flex().flex_col().child(
            div()
                .flex()
                .h(px(26.))
                .items_center()
                .border_b_1()
                .border_color(p.bd)
                .text_size(px(10.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.fg3)
                .children(
                    headers
                        .iter()
                        .enumerate()
                        .map(|(i, (h, w))| cell(*w, i == 0).child(*h)),
                ),
        );
        for row in rows {
            list = list.child(
                div()
                    .flex()
                    .min_h(px(26.))
                    .items_center()
                    .border_b_1()
                    .border_color(p.bd.opacity(0.5))
                    .text_size(px(12.))
                    .children(
                        row.into_iter()
                            .zip(headers.iter())
                            .enumerate()
                            .map(|(i, (el, (_, w)))| cell(*w, i == 0).child(el)),
                    ),
            );
        }
        list.into_any_element()
    }

    fn num(v: Option<f64>, p: &Palette) -> AnyElement {
        div()
            .font_family(MONO)
            .text_size(px(11.5))
            .text_color(if v.is_some() { p.fg } else { p.fg3 })
            .child(v.map_or("–".to_owned(), count))
            .into_any_element()
    }

    fn size(v: Option<f64>, p: &Palette) -> AnyElement {
        div()
            .font_family(MONO)
            .text_size(px(11.5))
            .text_color(p.fg2)
            .child(v.map_or("–".to_owned(), |b| ui::bytes(b as u64)))
            .into_any_element()
    }

    fn tag(text: &'static str, color: gpui_kit::Hsla) -> AnyElement {
        div()
            .flex_none()
            .px(px(5.))
            .rounded(px(3.))
            .border_1()
            .border_color(color.opacity(0.5))
            .text_size(px(9.5))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(color)
            .child(text)
            .into_any_element()
    }

    fn render_section(&self, w: &Workload, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let empty = |t: &'static str| {
            div()
                .p(px(16.))
                .text_size(px(12.))
                .text_color(p.fg3)
                .child(t)
                .into_any_element()
        };
        match self.section {
            Section::Statements => {
                if w.statements.is_empty() {
                    return empty("No statement statistics (see the hints above).");
                }
                let rows = w
                    .statements
                    .iter()
                    .enumerate()
                    .map(|(i, q)| {
                        vec![
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .min_w_0()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .font_family(MONO)
                                        .text_size(px(11.5))
                                        .child(
                                            q.query
                                                .split_whitespace()
                                                .collect::<Vec<_>>()
                                                .join(" "),
                                        ),
                                )
                                .child(Self::copy_button(("q-copy", i), q.query.clone(), p, cx))
                                .into_any_element(),
                            Self::num(Some(q.calls), p),
                            div()
                                .font_family(MONO)
                                .text_size(px(11.5))
                                .child(ms(q.total_ms))
                                .into_any_element(),
                            div()
                                .font_family(MONO)
                                .text_size(px(11.5))
                                .child(ms(q.mean_ms))
                                .into_any_element(),
                            Self::num(q.rows, p),
                            Self::num(q.pages, p),
                        ]
                    })
                    .collect();
                Self::table(
                    &[
                        ("STATEMENT", 0.),
                        ("CALLS", 80.),
                        ("TOTAL", 90.),
                        ("MEAN", 90.),
                        ("ROWS", 90.),
                        ("PAGES", 90.),
                    ],
                    rows,
                    p,
                )
            }
            Section::Tables => {
                if w.tables.is_empty() {
                    return empty("No tables.");
                }
                let rows = w
                    .tables
                    .iter()
                    .map(|t| {
                        vec![
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .min_w_0()
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .child(format!("{}.{}", t.schema, t.name)),
                                )
                                .when(t.mostly_sequential(), |d| {
                                    d.child(Self::tag("MOSTLY FULL SCANS", p.stg))
                                })
                                .into_any_element(),
                            Self::num(t.seq_scans, p),
                            Self::num(t.seq_rows_read, p),
                            Self::num(t.index_scans, p),
                            Self::num(t.rows, p),
                            Self::num(t.dead_rows, p),
                            Self::size(t.bytes, p),
                        ]
                    })
                    .collect();
                Self::table(
                    &[
                        ("TABLE", 0.),
                        ("FULL SCANS", 90.),
                        ("ROWS SCANNED", 110.),
                        ("INDEX SCANS", 95.),
                        ("ROWS", 90.),
                        ("DEAD", 80.),
                        ("SIZE", 80.),
                    ],
                    rows,
                    p,
                )
            }
            Section::Indexes => {
                if w.indexes.is_empty() {
                    return empty("No indexes.");
                }
                let rows = w
                    .indexes
                    .iter()
                    .map(|i| {
                        vec![
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .min_w_0()
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .child(format!("{}.{}", i.table, i.name)),
                                )
                                .when(i.primary, |d| d.child(Self::tag("PK", p.fg3)))
                                .when(i.unique && !i.primary, |d| {
                                    d.child(Self::tag("UNIQUE", p.fg3))
                                })
                                .when(i.unused(), |d| d.child(Self::tag("UNUSED", p.prod)))
                                .into_any_element(),
                            Self::num(i.scans, p),
                            Self::num(i.writes, p),
                            Self::size(i.bytes, p),
                        ]
                    })
                    .collect();
                Self::table(
                    &[
                        ("INDEX", 0.),
                        ("READS", 90.),
                        ("WRITES", 90.),
                        ("SIZE", 80.),
                    ],
                    rows,
                    p,
                )
            }
            Section::Missing => {
                if w.missing_indexes.is_empty() {
                    return empty("The engine reports no missing indexes.");
                }
                let rows = w
                    .missing_indexes
                    .iter()
                    .enumerate()
                    .map(|(n, m)| {
                        let stmt = m.create_statement();
                        vec![
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .min_w_0()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .font_family(MONO)
                                        .text_size(px(11.5))
                                        .child(stmt.clone()),
                                )
                                .child(Self::copy_button(("mi-copy", n), stmt, p, cx))
                                .into_any_element(),
                            div()
                                .font_family(MONO)
                                .text_size(px(11.5))
                                .child(m.impact.map_or("–".into(), |v| format!("{v:.0}%")))
                                .into_any_element(),
                        ]
                    })
                    .collect();
                Self::table(&[("SUGGESTED INDEX", 0.), ("IMPACT", 80.)], rows, p)
            }
        }
    }
}

impl Render for WorkloadTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let since = self.data.as_ref().and_then(|w| w.since_ms).and_then(|t| {
            chrono::DateTime::from_timestamp_millis(t).map(|d| {
                d.with_timezone(&chrono::Local)
                    .format("since %b %d %H:%M")
                    .to_string()
            })
        });
        let section_btn = |s: Section, label: String| -> (SharedString, bool, ui::OnClick) {
            let on = self.section == s;
            let this = cx.entity().downgrade();
            (
                label.into(),
                on,
                Box::new(move |_, _, cx| {
                    let _ = this.update(cx, |t, cx| {
                        t.section = s;
                        cx.notify();
                    });
                }),
            )
        };
        let counts = self.data.as_ref().map(|w| {
            (
                w.statements.len(),
                w.tables.len(),
                w.indexes.len(),
                w.missing_indexes.len(),
            )
        });
        let (nq, nt, ni, nm) = counts.unwrap_or_default();
        let header = div()
            .flex_none()
            .h(px(38.))
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(12.))
            .border_b_1()
            .border_color(p.bd)
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(px(13.))
                    .child(format!("Workload · {}", self.name)),
            )
            .children(since.map(|s| {
                div()
                    .font_family(MONO)
                    .text_size(px(11.))
                    .text_color(p.fg3)
                    .child(s)
            }))
            .child(ui::segmented(
                "workload-section",
                vec![
                    section_btn(Section::Statements, format!("Statements {nq}")),
                    section_btn(Section::Tables, format!("Tables {nt}")),
                    section_btn(Section::Indexes, format!("Indexes {ni}")),
                    section_btn(Section::Missing, format!("Missing indexes {nm}")),
                ],
                22.,
                &p,
            ))
            .child(div().flex_1())
            .child(if self.request.is_some() {
                ui::shimmer(80., &p)
            } else {
                ui::button("workload-refresh", "Refresh", Kind::Secondary, &p)
                    .h(px(24.))
                    .on_click(cx.listener(|t, _, _, cx| {
                        t.refresh();
                        cx.notify();
                    }))
                    .into_any_element()
            });
        let body = match (&self.data, &self.error) {
            (_, Some(e)) => div()
                .p(px(16.))
                .text_size(px(12.5))
                .text_color(p.prod)
                .child(e.clone())
                .into_any_element(),
            (None, None) => div()
                .p(px(16.))
                .child(ui::shimmer(240., &p))
                .into_any_element(),
            (Some(w), None) => {
                let w = w.clone();
                let hints: Vec<AnyElement> = w
                    .hints
                    .iter()
                    .enumerate()
                    .map(|(i, h)| self.render_hint(i, h, &p, cx))
                    .collect();
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .p(px(12.))
                    .children(hints)
                    .child(self.render_section(&w, &p, cx))
                    .into_any_element()
            }
        };
        div()
            .id("workload-tab")
            .size_full()
            .flex()
            .flex_col()
            .font_family(SANS)
            .bg(p.bg)
            .text_color(p.fg)
            .child(header)
            .child(
                div()
                    .id("workload-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(body),
            )
    }
}
