//! The Load test tab: send the open request many times at once and read the
//! latency percentiles, throughput, status codes, errors and a per-second
//! timeline while it runs ([`switchyard_api::runtime::load`]).
//!
//! The request compiles once, exactly like Send (variables, auth, sign-in,
//! pre-request script), then core's runtime sends it from a worker pool.
//! A test against a Production environment always asks first, whatever the
//! method: a load test is traffic, not a read.

use std::sync::Arc;
use std::time::Duration;

use gpui_kit::{AnyElement, Div, SharedString, div, prelude::*};
use switchyard_api::runtime::{
    LoadProgress, LoadReport, LoadTestConfig, LoadThresholds, MAX_LOAD_CONCURRENCY,
    MAX_LOAD_REQUESTS, run_load_test,
};

use super::view::{
    Col, accent_button, cell, header_cells, heading, icon, mono_field, note_card, outline_chip,
    pill, table, table_row, tinted_cell, verb,
};
use super::*;
use crate::appearance::rpx;
use gpui_kit::component::scroll::ScrollableElement;

/// How often the numbers refresh while a test runs.
const REFRESH: Duration = Duration::from_millis(250);
/// Seconds of timeline drawn (the most recent ones).
const TIMELINE_SECONDS: usize = 120;

/// A test in flight.
struct RunningLoad {
    progress: Arc<LoadProgress>,
    config: LoadTestConfig,
}

/// The Load test tab's inputs and last result.
pub(super) struct LoadTestState {
    requests: Entity<InputState>,
    concurrency: Entity<InputState>,
    duration: Entity<InputState>,
    p95_budget: Entity<InputState>,
    error_budget: Entity<InputState>,
    running: Option<RunningLoad>,
    /// `METHOD url` of the request the shown numbers belong to.
    target: Option<(String, String)>,
    report: Option<LoadReport>,
    error: Option<String>,
    generation: u64,
    _work: Option<gpui_kit::Task<()>>,
    _refresh: Option<gpui_kit::Task<()>>,
}

impl LoadTestState {
    pub(super) fn new(window: &mut Window, cx: &mut Context<WorkbenchPanel>) -> Self {
        let field = |placeholder: &'static str, window: &mut Window, cx: &mut Context<_>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        Self {
            requests: field("100", window, cx),
            concurrency: field("10", window, cx),
            duration: field("none", window, cx),
            p95_budget: field("none", window, cx),
            error_budget: field("none", window, cx),
            running: None,
            target: None,
            report: None,
            error: None,
            generation: 0,
            _work: None,
            _refresh: None,
        }
    }

    pub(super) fn is_running(&self) -> bool {
        self.running.is_some()
    }
}

impl Drop for LoadTestState {
    fn drop(&mut self) {
        // The workers run on core's blocking pool and outlive this view:
        // stop them issuing requests (in-flight ones end on their own).
        if let Some(running) = &self.running {
            running.progress.stop();
        }
    }
}

/// The test the form describes. Blank requests mean 100 unless a duration
/// is set; blank concurrency means 10; blank thresholds are not checked.
pub(super) fn parse_config(
    requests: &str,
    concurrency: &str,
    duration_secs: &str,
    p95_ms: &str,
    error_percent: &str,
) -> Result<LoadTestConfig, String> {
    fn number<T: std::str::FromStr>(value: &str, what: &str) -> Result<Option<T>, String> {
        let value = value.trim();
        if value.is_empty() || value.eq_ignore_ascii_case("none") {
            return Ok(None);
        }
        value
            .parse()
            .map(Some)
            .map_err(|_| format!("{what} must be a number."))
    }
    let duration = number::<u64>(duration_secs, "Duration")?.map(Duration::from_secs);
    let requests = match number::<u32>(requests, "Requests")? {
        Some(requests) => requests,
        None if duration.is_some() => 0,
        None => 100,
    };
    let error_rate_percent = number::<f64>(error_percent, "Error budget")?;
    if error_rate_percent.is_some_and(|value| !(0.0..=100.0).contains(&value)) {
        return Err("Error budget must be between 0 and 100%.".into());
    }
    let config = LoadTestConfig {
        requests,
        concurrency: number::<u32>(concurrency, "Concurrency")?.unwrap_or(10),
        duration,
        thresholds: LoadThresholds {
            p95_ms: number::<u64>(p95_ms, "p95 budget")?,
            error_rate_percent,
        },
    };
    config.validate()?;
    Ok(config)
}

fn format_ms(ms: u64) -> String {
    if ms >= 1_000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else {
        format!("{ms} ms")
    }
}

fn format_bytes(bytes: u64) -> String {
    match bytes {
        0..1_024 => format!("{bytes} B"),
        1_024..1_048_576 => format!("{:.1} KB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
}

impl WorkbenchPanel {
    /// Start a load test of the open request, asking first when the
    /// environment in force is Production.
    pub(super) fn start_load_test_confirmed(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.environment_label_in_force().is_production() {
            self.start_load_test(cx);
            return;
        }
        let message = format!(
            "This sends the open request many times at once to the Production environment “{}”.",
            self.environment_display_name()
        );
        self.confirm_production(
            "Load test Production?",
            message,
            "Load test Production",
            |panel, _, cx| panel.start_load_test(cx),
            window,
            cx,
        );
    }

    /// Compile inputs for the open HTTP request, as Send builds them.
    fn load_send_input(&mut self, cx: &mut Context<Self>) -> Result<StandaloneSendInput, String> {
        let (definition, mut secrets) = self.draft_request(cx)?;
        let Some(data) = self.workspace_data.as_ref() else {
            return Err("Workbench storage is unavailable".into());
        };
        let environment = self.environment_send_context(cx)?;
        let manual_cookies = draft::parse_cookie_pairs(&self.cookies.read(cx).value())?;
        let collection = data.collection(&definition.collection_id).cloned();
        let folders = folder_chain(&data.folders, &definition)
            .into_iter()
            .cloned()
            .collect();
        secrets.merge(environment.secrets);
        Ok(StandaloneSendInput {
            operation_id: RequestId::new().as_str().to_string(),
            definition,
            collection,
            folders,
            environment: environment.environment,
            environment_source: environment.source,
            environment_scope: environment.scope,
            environment_base_url: environment.base_url,
            environment_auth: environment.auth,
            browser_authorization: None,
            secrets,
            workspace: data.workspace.clone(),
            store: data.store.clone(),
            secret_store: self.secret_store.clone(),
            cookie_jar: self.cookie_jar.clone(),
            manual_cookies,
            file_capabilities: self.file_capabilities.clone(),
            transport: self.transport.clone(),
        })
    }

    fn start_load_test(&mut self, cx: &mut Context<Self>) {
        if self.load_test.is_running() {
            return;
        }
        let state = &self.load_test;
        let config = parse_config(
            &state.requests.read(cx).value(),
            &state.concurrency.read(cx).value(),
            &state.duration.read(cx).value(),
            &state.p95_budget.read(cx).value(),
            &state.error_budget.read(cx).value(),
        );
        let config = match config {
            Ok(config) => config,
            Err(error) => {
                self.load_test.error = Some(error);
                cx.notify();
                return;
            }
        };
        if !self.has_request_tab() || self.active_protocol_pane().is_some() {
            self.load_test.error = Some("Open an HTTP request in Compose to load-test it.".into());
            cx.notify();
            return;
        }
        let input = match self.load_send_input(cx) {
            Ok(input) => input,
            Err(error) => {
                self.load_test.error = Some(error);
                cx.notify();
                return;
            }
        };
        let progress = LoadProgress::new();
        let state = &mut self.load_test;
        state.generation = state.generation.wrapping_add(1);
        let generation = state.generation;
        state.target = Some((
            input.definition.method.as_str().to_string(),
            input.definition.url.clone(),
        ));
        state.error = None;
        state.report = None;
        state.running = Some(RunningLoad {
            progress: progress.clone(),
            config: config.clone(),
        });
        let transport = self.transport.clone();
        let files = self.file_capabilities.clone();
        let run_progress = progress.clone();
        let run_config = config.clone();
        state._work = Some(cx.spawn(async move |this, cx| {
            let result = crate::api::compat::blocking(move || {
                let prepared = prepare_standalone_send(input)?;
                if prepared.pre_script_ran {
                    // Compiling opened an operation for the pre-request
                    // script; the test's sends use their own.
                    let _ = transport.cancel(&prepared.operation_id);
                }
                if run_progress.is_cancelled() {
                    return Err("Stopped before the first request.".to_string());
                }
                run_load_test(
                    &prepared.prepared,
                    &files,
                    transport,
                    &run_config,
                    &run_progress,
                )
            })
            .await;
            let _ = this.update(cx, |panel, cx| {
                let state = &mut panel.load_test;
                if state.generation != generation {
                    return;
                }
                state.running = None;
                state._refresh = None;
                match result {
                    Ok(report) => state.report = Some(report),
                    Err(error) => state.error = Some(error),
                }
                cx.notify();
            });
        }));
        state._refresh = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH).await;
                if progress.is_finished() {
                    return;
                }
                let snapshot = progress.clone();
                let snapshot_config = config.clone();
                let report =
                    crate::api::compat::blocking(move || snapshot.report(&snapshot_config)).await;
                let still_running = this
                    .update(cx, |panel, cx| {
                        let state = &mut panel.load_test;
                        if state.generation != generation || state.running.is_none() {
                            return false;
                        }
                        state.report = Some(report);
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !still_running {
                    return;
                }
            }
        }));
        cx.notify();
    }

    pub(super) fn stop_load_test(&mut self, cx: &mut Context<Self>) {
        let Some(running) = &self.load_test.running else {
            return;
        };
        let progress = running.progress.clone();
        progress.stop();
        let transport = self.transport.clone();
        // Cancelling in-flight sends talks to the service: off this thread.
        cx.background_executor()
            .spawn(crate::api::compat::blocking(move || {
                progress.cancel(transport.as_ref())
            }))
            .detach();
        cx.notify();
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

impl WorkbenchPanel {
    pub(super) fn render_load_test(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .flex()
            .flex_1()
            .min_h(rpx(0.))
            .child(self.render_load_form(window, cx))
            .child(self.render_load_results(cx))
            .into_any_element()
    }

    /// The open request the test would send: `METHOD url`, or why there is none.
    fn load_test_candidate(&self, cx: &App) -> Option<(String, String)> {
        if !self.has_request_tab() || self.active_protocol_pane().is_some() {
            return None;
        }
        Some((
            self.current_method(cx),
            self.url.read(cx).value().to_string(),
        ))
    }

    fn render_load_form(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        let state = &self.load_test;
        let value_field = |label: &'static str,
                           selector: &'static str,
                           input: &Entity<InputState>,
                           cx: &mut Context<Self>| {
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(space::SP_2)
                .text_size(text::S12)
                .text_color(palette::text_secondary(cx))
                .child(label)
                .child(
                    mono_field(input, window, cx)
                        .debug_selector(move || selector.into())
                        .w(rpx(120.))
                        .h(rpx(24.))
                        .text_size(text::S11),
                )
        };
        let candidate = self.load_test_candidate(cx);
        let target = match &candidate {
            Some((method, url)) => {
                let tint = self.method_tint_label(method, cx);
                div()
                    .flex()
                    .items_center()
                    .gap(space::SP_2)
                    .p(rpx(10.))
                    .rounded(radius::md())
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.sidebar)
                    .font_family(crate::api::compat::fonts::mono(cx))
                    .text_size(text::S11)
                    .child(verb(method.clone(), tint, 44., cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w(rpx(0.))
                            .truncate()
                            .text_color(palette::text_secondary(cx))
                            .child(if url.trim().is_empty() {
                                "(no URL)".to_string()
                            } else {
                                url.clone()
                            }),
                    )
            }
            None => note_card(
                palette::warm(cx),
                Some("alert"),
                "Open an HTTP request in Compose; the test sends that request.",
                cx,
            ),
        };
        let run_button = if state.is_running() {
            outline_chip("workbench-load-stop".into(), "Stop", cx)
                .child(icon("stop", 12., colors.danger))
                .on_click(cx.listener(|this, _, _, cx| this.stop_load_test(cx)))
                .into_any_element()
        } else {
            accent_button("workbench-load-run".into(), "Run load test", 32., cx)
                .when(candidate.is_none(), |el| el.opacity(0.5))
                .on_click(
                    cx.listener(|this, _, window, cx| this.start_load_test_confirmed(window, cx)),
                )
                .into_any_element()
        };
        div()
            .flex_none()
            .w(rpx(320.))
            .min_h(rpx(0.))
            .border_r_1()
            .border_color(colors.border)
            .overflow_y_scrollbar()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::SP_3)
                    .p(space::SP_4)
                    .child(heading("Target", colors.muted_foreground))
                    .child(target)
                    .child(heading("Load", colors.muted_foreground))
                    .child(value_field(
                        "Requests",
                        "workbench-load-requests",
                        &state.requests,
                        cx,
                    ))
                    .child(value_field(
                        "Concurrency",
                        "workbench-load-concurrency",
                        &state.concurrency,
                        cx,
                    ))
                    .child(value_field(
                        "Duration (s)",
                        "workbench-load-duration",
                        &state.duration,
                        cx,
                    ))
                    .child(heading("Pass when", colors.muted_foreground))
                    .child(value_field(
                        "p95 at most (ms)",
                        "workbench-load-p95",
                        &state.p95_budget,
                        cx,
                    ))
                    .child(value_field(
                        "Errors at most (%)",
                        "workbench-load-errors",
                        &state.error_budget,
                        cx,
                    ))
                    .child(run_button)
                    .when_some(state.error.clone(), |el, error| {
                        el.child(
                            div()
                                .debug_selector(|| "workbench-load-error".into())
                                .child(note_card(colors.danger, Some("alert"), error, cx)),
                        )
                    })
                    .child(
                        div()
                            .text_size(text::S11)
                            .line_height(gpui_kit::relative(1.5))
                            .text_color(palette::text_tertiary(cx))
                            .child(format!(
                                "Up to {MAX_LOAD_REQUESTS} requests, {MAX_LOAD_CONCURRENCY} at once. \
                                 Variables, auth and the pre-request script resolve once; \
                                 post-response scripts do not run. Each request opens its own \
                                 connection, so connect and TLS time count in every request."
                            )),
                    ),
            )
            .into_any_element()
    }

    fn render_load_results(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors;
        let state = &self.load_test;
        let Some(report) = state.report.as_ref() else {
            let (title, body) = if state.is_running() {
                (
                    "Starting…",
                    "Compiling the request and opening the first connections.",
                )
            } else {
                (
                    "No load test yet",
                    "Set the load on the left and run it. Latency percentiles, throughput, \
                     status codes and errors update while it runs.",
                )
            };
            return div()
                .flex()
                .flex_col()
                .flex_1()
                .items_center()
                .justify_center()
                .gap(space::SP_2)
                .p(space::SP_4)
                .child(
                    div()
                        .text_size(text::S13)
                        .font_weight(text::weight::SEMIBOLD)
                        .text_color(palette::text_secondary(cx))
                        .child(title),
                )
                .child(
                    div()
                        .max_w(rpx(420.))
                        .text_size(text::S11)
                        .text_color(palette::text_tertiary(cx))
                        .child(body),
                )
                .into_any_element();
        };
        div()
            .id("workbench-load-results")
            .flex()
            .flex_col()
            .flex_1()
            .min_w(rpx(0.))
            .min_h(rpx(0.))
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::SP_4)
                    .p(space::SP_4)
                    .child(self.render_load_status(report, cx))
                    .child(render_load_tiles(report, cx))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap(space::SP_4)
                            .child(render_histogram(report, cx))
                            .child(render_timeline(report, cx)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap(space::SP_4)
                            .items_start()
                            .child(render_status_table(report, cx))
                            .child(render_phase_table(report, cx))
                            .when(!report.errors.is_empty(), |el| {
                                el.child(render_error_table(report, cx))
                            }),
                    )
                    .child(div().h(rpx(1.)).bg(colors.border)),
            )
            .into_any_element()
    }

    /// The run's state, progress bar and pass / fail verdict.
    fn render_load_status(&self, report: &LoadReport, cx: &App) -> Div {
        let colors = cx.theme().colors;
        let state = &self.load_test;
        let elapsed = format_ms(report.elapsed_ms);
        let line = if state.is_running() {
            if report.planned > 0 {
                format!(
                    "Running · {} of {} · {elapsed}",
                    report.completed, report.planned
                )
            } else {
                let limit = state
                    .running
                    .as_ref()
                    .and_then(|running| running.config.duration)
                    .map(|duration| format!(" of {} s", duration.as_secs()))
                    .unwrap_or_default();
                format!("Running · {} requests · {elapsed}{limit}", report.completed)
            }
        } else if report.cancelled {
            format!("Stopped after {} requests · {elapsed}", report.completed)
        } else {
            format!("Finished {} requests in {elapsed}", report.completed)
        };
        let fraction = if state.is_running() {
            let by_count = (report.planned > 0)
                .then(|| report.completed.min(report.planned) as f32 / report.planned as f32);
            let by_time = state
                .running
                .as_ref()
                .and_then(|running| running.config.duration)
                .map(|duration| {
                    (report.elapsed_ms as f32 / duration.as_millis().max(1) as f32).min(1.0)
                });
            match (by_count, by_time) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            }
        } else {
            None
        };
        let verdict = (!report.thresholds.is_empty() && !state.is_running()).then(|| {
            let passed = report.passed();
            pill(
                if passed { "PASS" } else { "FAIL" },
                if passed {
                    colors.success
                } else {
                    colors.danger
                },
                cx,
            )
            .debug_selector(|| "workbench-load-verdict".into())
        });
        div()
            .flex()
            .flex_col()
            .gap(space::SP_2)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::SP_2)
                    .when_some(state.target.clone(), |el, (method, url)| {
                        el.child(verb(
                            method.clone(),
                            self.method_tint_label(&method, cx),
                            44.,
                            cx,
                        ))
                        .child(
                            div()
                                .min_w(rpx(0.))
                                .max_w(rpx(420.))
                                .truncate()
                                .font_family(crate::api::compat::fonts::mono(cx))
                                .text_size(text::S11)
                                .text_color(palette::text_secondary(cx))
                                .child(url),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        div()
                            .debug_selector(|| "workbench-load-status".into())
                            .text_size(text::S11)
                            .text_color(colors.muted_foreground)
                            .child(line),
                    )
                    .children(verdict),
            )
            .when_some(fraction, |el, fraction| {
                el.child(
                    div()
                        .h(rpx(4.))
                        .w_full()
                        .rounded(radius::full())
                        .bg(colors.muted)
                        .child(
                            div()
                                .h_full()
                                .w(gpui_kit::relative(fraction))
                                .rounded(radius::full())
                                .bg(colors.primary),
                        ),
                )
            })
            .when(!report.thresholds.is_empty(), |el| {
                el.child(div().flex().flex_wrap().gap(space::SP_2).children(
                    report.thresholds.iter().map(|check| {
                        let tint = if state.is_running() {
                            colors.muted_foreground
                        } else if check.passed {
                            colors.success
                        } else {
                            colors.danger
                        };
                        pill(format!("{} · {}", check.label, check.actual), tint, cx)
                    }),
                ))
            })
    }
}

fn tile(label: &'static str, value: String, tint: Option<gpui_kit::Hsla>, cx: &App) -> Div {
    let colors = cx.theme().colors;
    div()
        .flex()
        .flex_col()
        .gap(rpx(2.))
        .min_w(rpx(104.))
        .px(space::SP_3)
        .py(space::SP_2)
        .rounded(radius::md())
        .border_1()
        .border_color(colors.border)
        .bg(colors.muted)
        .child(heading(label, colors.muted_foreground))
        .child(
            div()
                .font_family(crate::api::compat::fonts::mono(cx))
                .text_size(text::S14)
                .font_weight(text::weight::SEMIBOLD)
                .text_color(tint.unwrap_or(colors.foreground))
                .child(value),
        )
}

fn render_load_tiles(report: &LoadReport, cx: &App) -> Div {
    let colors = cx.theme().colors;
    let latency = &report.latency;
    let error_tint = if report.failed == 0 {
        colors.success
    } else {
        colors.danger
    };
    div()
        .debug_selector(|| "workbench-load-tiles".into())
        .flex()
        .flex_wrap()
        .gap(space::SP_2)
        .child(tile("Requests", report.completed.to_string(), None, cx))
        .child(tile(
            "Throughput",
            format!("{:.1}/s", report.requests_per_second),
            None,
            cx,
        ))
        .child(tile(
            "Errors",
            format!("{:.1}%", report.error_rate_percent),
            Some(error_tint),
            cx,
        ))
        .child(tile("p50", format_ms(latency.p50), None, cx))
        .child(tile("p90", format_ms(latency.p90), None, cx))
        .child(tile("p95", format_ms(latency.p95), None, cx))
        .child(tile("p99", format_ms(latency.p99), None, cx))
        .child(tile("Mean", format_ms(latency.mean), None, cx))
        .child(tile("Max", format_ms(latency.max), None, cx))
        .child(tile(
            "Received",
            format_bytes(report.received_bytes),
            None,
            cx,
        ))
}

/// A titled card holding a bar chart.
fn chart_card(title: &'static str, caption: String, bars: Div, cx: &App) -> Div {
    let colors = cx.theme().colors;
    div()
        .flex()
        .flex_col()
        .gap(space::SP_2)
        .flex_1()
        .min_w(rpx(280.))
        .p(space::SP_3)
        .rounded(radius::md())
        .border_1()
        .border_color(colors.border)
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(heading(title, colors.muted_foreground))
                .child(
                    div()
                        .text_size(text::S10)
                        .text_color(palette::text_tertiary(cx))
                        .child(caption),
                ),
        )
        .child(bars)
}

/// Vertical bars for `values` (each `(value, tint)`), scaled to the largest.
fn bars(values: Vec<(u64, gpui_kit::Hsla)>, selector: &'static str) -> Div {
    let peak = values.iter().map(|(v, _)| *v).max().unwrap_or(0).max(1);
    div()
        .debug_selector(move || selector.into())
        .flex()
        .items_end()
        .gap(rpx(2.))
        .h(rpx(96.))
        .children(values.into_iter().map(move |(value, tint)| {
            let fraction = value as f32 / peak as f32;
            div()
                .flex_1()
                .min_w(rpx(2.))
                .h(gpui_kit::relative(fraction.max(if value > 0 {
                    0.02
                } else {
                    0.0
                })))
                .rounded_t(rpx(2.))
                .bg(tint)
        }))
}

fn render_histogram(report: &LoadReport, cx: &App) -> Div {
    let colors = cx.theme().colors;
    let (first, last) = (
        report.histogram.first().map(|b| b.upper_ms),
        report.histogram.last().map(|b| b.upper_ms),
    );
    let caption = match (first, last) {
        (Some(_), Some(last)) => format!("{} → {}", format_ms(report.latency.min), format_ms(last)),
        _ => String::new(),
    };
    let p95 = report.latency.p95;
    let values = report
        .histogram
        .iter()
        .map(|bucket| {
            let tint = if bucket.upper_ms >= p95 && p95 > 0 {
                palette::warm(cx)
            } else {
                colors.primary
            };
            (u64::from(bucket.count), tint)
        })
        .collect();
    chart_card(
        "Latency distribution",
        caption,
        bars(values, "workbench-load-histogram"),
        cx,
    )
}

fn render_timeline(report: &LoadReport, cx: &App) -> Div {
    let colors = cx.theme().colors;
    let points = &report.timeline[report.timeline.len().saturating_sub(TIMELINE_SECONDS)..];
    let values = points
        .iter()
        .map(|point| {
            let tint = if point.failed > 0 {
                colors.danger
            } else {
                colors.success
            };
            (u64::from(point.requests), tint)
        })
        .collect();
    let peak = points.iter().map(|p| p.requests).max().unwrap_or(0);
    chart_card(
        "Requests per second",
        format!("peak {peak}/s · red seconds had errors"),
        bars(values, "workbench-load-timeline"),
        cx,
    )
}

fn render_status_table(report: &LoadReport, cx: &App) -> Div {
    let cols = [Col::Px(90.), Col::Px(80.), Col::Px(70.)];
    let total = report.completed.max(1);
    div()
        .flex()
        .flex_col()
        .gap(space::SP_2)
        .child(heading("Status codes", cx.theme().colors.muted_foreground))
        .child(
            table(cx)
                .debug_selector(|| "workbench-load-statuses".into())
                .child(table_row(
                    &cols,
                    header_cells(&["STATUS", "COUNT", "SHARE"]),
                    true,
                    cx,
                ))
                .children(report.status_counts.iter().map(|(status, count)| {
                    let tint = status
                        .parse::<u16>()
                        .map(|status| super::view::status_tint(status, cx))
                        .unwrap_or(cx.theme().colors.danger);
                    table_row(
                        &cols,
                        vec![
                            tinted_cell(status.clone(), tint),
                            cell(count.to_string()),
                            cell(format!(
                                "{:.1}%",
                                f64::from(*count) * 100.0 / f64::from(total)
                            )),
                        ],
                        false,
                        cx,
                    )
                })),
        )
}

fn render_phase_table(report: &LoadReport, cx: &App) -> Div {
    let cols = [Col::Px(120.), Col::Px(90.)];
    let row = |label: &'static str, value: Option<u64>| {
        table_row(
            &cols,
            vec![
                cell(label),
                cell(value.map(format_ms).unwrap_or_else(|| "—".into())),
            ],
            false,
            cx,
        )
    };
    div()
        .flex()
        .flex_col()
        .gap(space::SP_2)
        .child(heading("Mean phases", cx.theme().colors.muted_foreground))
        .child(
            table(cx)
                .child(table_row(&cols, header_cells(&["PHASE", "MEAN"]), true, cx))
                .child(row("Connect", report.phases.connect_ms))
                .child(row("TLS", report.phases.tls_ms))
                .child(row("First byte", report.phases.first_byte_ms))
                .child(row("Total", Some(report.latency.mean))),
        )
}

fn render_error_table(report: &LoadReport, cx: &App) -> Div {
    let cols = [Col::Flex, Col::Px(70.)];
    div()
        .flex()
        .flex_col()
        .gap(space::SP_2)
        .flex_1()
        .min_w(rpx(280.))
        .child(heading("Errors", cx.theme().colors.danger))
        .child(
            table(cx)
                .debug_selector(|| "workbench-load-errors-table".into())
                .child(table_row(
                    &cols,
                    header_cells(&["MESSAGE", "COUNT"]),
                    true,
                    cx,
                ))
                .children(report.errors.iter().map(|(message, count)| {
                    table_row(
                        &cols,
                        vec![
                            div()
                                .whitespace_normal()
                                .child(SharedString::from(message.clone()))
                                .into_any_element(),
                            cell(count.to_string()),
                        ],
                        false,
                        cx,
                    )
                })),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_fields_mean_a_hundred_requests_ten_at_a_time() {
        let config = parse_config("", "", "", "", "").unwrap();
        assert_eq!(config.requests, 100);
        assert_eq!(config.concurrency, 10);
        assert_eq!(config.duration, None);
        assert_eq!(config.thresholds, LoadThresholds::default());
    }

    #[test]
    fn a_duration_alone_runs_until_it_ends() {
        let config = parse_config("", "4", "30", "250", "1.5").unwrap();
        assert_eq!(config.requests, 0);
        assert_eq!(config.duration, Some(Duration::from_secs(30)));
        assert_eq!(config.thresholds.p95_ms, Some(250));
        assert_eq!(config.thresholds.error_rate_percent, Some(1.5));
    }

    #[test]
    fn bad_values_say_which_field() {
        assert!(
            parse_config("lots", "", "", "", "")
                .unwrap_err()
                .contains("Requests")
        );
        assert!(
            parse_config("", "0", "", "", "")
                .unwrap_err()
                .contains("Concurrency")
        );
        assert!(
            parse_config("", "", "", "", "150")
                .unwrap_err()
                .contains("Error budget")
        );
        assert!(parse_config("", "", "0", "", "").is_err());
    }

    #[test]
    fn durations_and_sizes_read_naturally() {
        assert_eq!(format_ms(42), "42 ms");
        assert_eq!(format_ms(8_002), "8.0 s");
        assert_eq!(format_ms(12_300), "12.3 s");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
    }
}
