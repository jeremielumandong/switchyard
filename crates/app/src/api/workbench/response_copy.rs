//! Copying out of the response panel: plain-text renderings of the
//! Headers, Console, Trace and Tests tabs, the toolbar's "Copy response" /
//! "Copy as cURL", and the in-place "Copied" confirmation every copy control
//! shows for [`COPIED_FOR`] instead of a toast.
//!
//! cURL comes from the redacted request snapshot that produced the response
//! (`response_request`) through the same generator as the Snippet tab, so the
//! clipboard never carries a secret the Snippet tab would not show.

use std::time::Duration;

use gpui_kit::component::Disableable;
use gpui_kit::component::button::Button;
use gpui_kit::{Div, Stateful, Task, div};

use super::view::{icon, outline_chip};
use super::*;
use crate::appearance::rpx;

/// How long a copy control reads "Copied" after a click.
pub(super) const COPIED_FOR: Duration = Duration::from_millis(1500);

/// Which copy control last succeeded; cleared after [`COPIED_FOR`].
#[derive(Default)]
pub(super) struct CopyFeedback {
    key: Option<SharedString>,
    generation: u64,
    _reset: Option<Task<()>>,
}

impl CopyFeedback {
    fn is(&self, key: &str) -> bool {
        self.key.as_deref() == Some(key)
    }
}

/// One header as it is written on the wire: `name: value`.
pub(super) fn header_line(name: &str, value: &str) -> String {
    format!("{name}: {value}")
}

/// All headers, one `name: value` per line, in response order.
pub(super) fn headers_text(headers: &[(String, String)]) -> String {
    headers
        .iter()
        .map(|(name, value)| header_line(name, value))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The Console tab's lines: entries after the "Clear view" mark that match
/// `filter` (lower-case), then the execution error when no entry already
/// carries it and it was not cleared.
pub(super) fn console_lines(
    entries: &[switchyard_api::ConsoleEntry],
    cleared: usize,
    filter: &str,
    error: Option<&str>,
    cleared_error: Option<&str>,
) -> Vec<String> {
    let mut lines: Vec<String> = entries
        .iter()
        .skip(cleared)
        .filter(|entry| {
            format!("{} {} {}", entry.phase, entry.level, entry.message)
                .to_lowercase()
                .contains(filter)
        })
        .map(|entry| format!("[{}] {}: {}", entry.phase, entry.level, entry.message))
        .collect();
    if let Some(error) = error.filter(|error| !error.is_empty()) {
        let already_logged = entries.iter().any(|entry| entry.message.contains(error));
        let line = format!("[execution] error: {error}");
        if !already_logged && cleared_error != Some(error) && line.to_lowercase().contains(filter) {
            lines.push(line);
        }
    }
    lines
}

/// One Trace row: offset from send, a short tag, the event, and how long
/// it took when that is known.
pub(super) struct TraceRow {
    pub offset_ms: u64,
    pub tag: &'static str,
    pub text: String,
    pub duration: Option<u64>,
}

/// Trace rows built from the real exchange: connection timings, each
/// redirect, `done`, and the error line — no synthetic events.
pub(super) fn trace_rows(
    response: &transport::Response,
    request: Option<&RedactedRequestSnapshot>,
    error: Option<&str>,
) -> Vec<TraceRow> {
    let timings = &response.timings;
    let mut rows = Vec::new();
    let target = request
        .map(|request| format!("{} {}", request.method, request.url))
        .unwrap_or_else(|| response.final_url.clone());
    rows.push(TraceRow {
        offset_ms: 0,
        tag: "send",
        text: target,
        duration: None,
    });
    // What the request authenticated with — the redacted snapshot keeps a
    // bearer JWT's public claims, which is how two tokens are told apart.
    if let Some((_, value)) = request.and_then(|request| {
        request
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("Authorization"))
    }) {
        rows.push(TraceRow {
            offset_ms: 0,
            tag: "auth",
            text: value.clone(),
            duration: None,
        });
    }
    let mut offset = 0;
    for (tag, value) in [
        ("dns", timings.dns_ms),
        ("conn", timings.connect_ms),
        ("tls", timings.tls_ms),
    ] {
        if let Some(ms) = value {
            rows.push(TraceRow {
                offset_ms: offset,
                tag,
                text: format!("{tag} resolved"),
                duration: Some(ms),
            });
            offset += ms;
        }
    }
    if let Some(ms) = timings.first_byte_ms {
        rows.push(TraceRow {
            offset_ms: ms,
            tag: "ttfb",
            text: format!("first byte · HTTP {}", response.http_version),
            duration: None,
        });
    }
    for redirect in &response.redirects {
        rows.push(TraceRow {
            offset_ms: timings.first_byte_ms.unwrap_or(0),
            tag: "redir",
            text: format!(
                "{} {} → {}{}",
                redirect.status,
                redirect.from,
                redirect.to,
                if redirect.cross_origin {
                    " · cross-origin credentials/body stripped"
                } else {
                    ""
                }
            ),
            duration: None,
        });
    }
    if let Some(ms) = timings.download_ms {
        rows.push(TraceRow {
            offset_ms: response.duration_ms.saturating_sub(ms),
            tag: "body",
            text: format!("{} received", pretty::human_size(response.received_bytes)),
            duration: Some(ms),
        });
    }
    rows.push(TraceRow {
        offset_ms: response.duration_ms,
        tag: if response.truncated { "warn" } else { "done" },
        text: format!(
            "{} {} · {}{}",
            response.status,
            response.reason,
            response.final_url,
            if response.truncated {
                " · response truncated at limit"
            } else {
                ""
            }
        ),
        duration: Some(response.duration_ms),
    });
    if let Some(error) = error {
        rows.push(TraceRow {
            offset_ms: response.duration_ms,
            tag: "error",
            text: error.to_string(),
            duration: None,
        });
    }
    rows
}

/// `   12 ms  dns    dns resolved  (3 ms)` — columns line up across rows.
pub(super) fn trace_line(row: &TraceRow) -> String {
    let mut line = format!("{:>6} ms  {:<6} {}", row.offset_ms, row.tag, row.text);
    if let Some(ms) = row.duration {
        line.push_str(&format!("  ({ms} ms)"));
    }
    line
}

/// Every Trace row, one per line.
pub(super) fn trace_text(rows: &[TraceRow]) -> String {
    rows.iter().map(trace_line).collect::<Vec<_>>().join("\n")
}

/// `PASS name`, `SKIP name`, or `FAIL name` plus an indented detail line.
pub(super) fn test_line(result: &switchyard_api::TestResult) -> String {
    let verdict = if result.skipped {
        "SKIP"
    } else if result.passed {
        "PASS"
    } else {
        "FAIL"
    };
    match result.error.as_deref().filter(|error| !error.is_empty()) {
        Some(error) => format!("{verdict} {}\n     {error}", result.name),
        None => format!("{verdict} {}", result.name),
    }
}

/// Every test result, then the same `passed · skipped · failed` summary as
/// the response readout.
pub(super) fn tests_text(results: &[switchyard_api::TestResult]) -> String {
    let passed = results
        .iter()
        .filter(|result| result.passed && !result.skipped)
        .count();
    let skipped = results.iter().filter(|result| result.skipped).count();
    let failed = results.len() - passed - skipped;
    let mut lines: Vec<String> = results.iter().map(test_line).collect();
    lines.push(format!(
        "{passed} passed · {skipped} skipped · {failed} failed"
    ));
    lines.join("\n")
}

/// The request that produced the shown response, as the Snippet tab's
/// cURL: built from the redacted snapshot, never the prepared request.
pub(super) fn curl_text(request: &RedactedRequestSnapshot) -> String {
    switchyard_api::generate_snippet(SnippetLanguage::Curl, request)
}

impl WorkbenchPanel {
    /// Put `text` on the clipboard and flip the `key` control to "Copied"
    /// for [`COPIED_FOR`].
    pub(super) fn copy_with_feedback(
        &mut self,
        key: SharedString,
        text: String,
        cx: &mut Context<Self>,
    ) {
        if text.is_empty() {
            return;
        }
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(text));
        let feedback = &mut self.ux.copy_feedback;
        feedback.generation = feedback.generation.wrapping_add(1);
        let generation = feedback.generation;
        feedback.key = Some(key);
        feedback._reset = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPIED_FOR).await;
            let _ = this.update(cx, |panel, cx| {
                if panel.ux.copy_feedback.generation == generation {
                    panel.ux.copy_feedback.key = None;
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    /// An outline "Copy …" chip that reads "Copied" right after a click.
    /// `text` runs on click, so large bodies are not cloned every frame.
    pub(super) fn copy_chip(
        &self,
        key: &'static str,
        label: &'static str,
        enabled: bool,
        text: impl Fn(&Self, &App) -> String + 'static,
        cx: &mut Context<Self>,
    ) -> Button {
        let copied = self.ux.copy_feedback.is(key);
        outline_chip(key.into(), if copied { "Copied" } else { label }, cx)
            .child(icon(
                if copied { "check" } else { "copy" },
                11.,
                palette::text_secondary(cx),
            ))
            .disabled(!enabled)
            .on_click(cx.listener(move |this, _, _, cx| {
                let payload = text(this, cx);
                this.copy_with_feedback(key.into(), payload, cx);
            }))
    }

    /// A small copy icon for one row (a header, a log line, a trace event);
    /// it turns into a check mark right after a click.
    pub(super) fn copy_row_icon(
        &self,
        key: String,
        text: String,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let copied = self.ux.copy_feedback.is(&key);
        let selector = key.clone();
        let key = SharedString::from(key);
        div()
            .id(key.clone())
            .debug_selector(move || selector.clone())
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(rpx(20.))
            .rounded(radius::sm())
            .cursor_pointer()
            .hover(|el| el.bg(cx.theme().colors.muted))
            .child(icon(
                if copied { "check" } else { "copy" },
                11.,
                if copied {
                    cx.theme().colors.success
                } else {
                    palette::text_tertiary(cx)
                },
            ))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.copy_with_feedback(key.clone(), text.clone(), cx);
            }))
    }

    /// The raw response body, as received (not the formatted view).
    pub(super) fn response_raw_text(&self) -> String {
        self.response_body
            .as_ref()
            .map(|body| body.raw.to_string())
            .or_else(|| self.response.as_ref().map(|response| response.body.clone()))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_api::{ConsoleEntry, TestResult};

    fn entry(phase: &str, level: &str, message: &str) -> ConsoleEntry {
        ConsoleEntry {
            phase: phase.into(),
            level: level.into(),
            message: message.into(),
        }
    }

    #[test]
    fn headers_render_as_wire_lines() {
        let headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("X-Trace".to_string(), "a: b".to_string()),
        ];
        assert_eq!(
            header_line("Content-Type", "application/json"),
            "Content-Type: application/json"
        );
        assert_eq!(
            headers_text(&headers),
            "Content-Type: application/json\nX-Trace: a: b"
        );
        assert_eq!(headers_text(&[]), "");
    }

    #[test]
    fn console_lines_respect_clear_filter_and_error() {
        let entries = vec![
            entry("pre-request", "log", "old"),
            entry("test", "log", "hello"),
            entry("test", "warn", "careful"),
        ];
        assert_eq!(
            console_lines(&entries, 1, "", None, None),
            vec!["[test] log: hello", "[test] warn: careful"]
        );
        assert_eq!(
            console_lines(&entries, 0, "warn", None, None),
            vec!["[test] warn: careful"]
        );
        assert_eq!(
            console_lines(&entries, 3, "", Some("boom"), None),
            vec!["[execution] error: boom"]
        );
        // Already logged by a script, or cleared: no duplicate error line.
        assert!(console_lines(&entries, 3, "", Some("hello"), None).is_empty());
        assert!(console_lines(&entries, 3, "", Some("boom"), Some("boom")).is_empty());
    }

    #[test]
    fn trace_text_lines_up_columns() {
        let rows = vec![
            TraceRow {
                offset_ms: 0,
                tag: "send",
                text: "GET https://example.test/".into(),
                duration: None,
            },
            TraceRow {
                offset_ms: 12,
                tag: "done",
                text: "200 OK · https://example.test/".into(),
                duration: Some(12),
            },
        ];
        assert_eq!(
            trace_text(&rows),
            "     0 ms  send   GET https://example.test/\n    12 ms  done   200 OK · https://example.test/  (12 ms)"
        );
    }

    #[test]
    fn trace_rows_follow_the_exchange() {
        let response = transport::Response {
            console: Vec::new(),
            status: 200,
            reason: "OK".into(),
            headers: Vec::new(),
            set_cookies: Vec::new(),
            cookie_mutations: Vec::new(),
            body: String::new(),
            body_base64: String::new(),
            binary: false,
            final_url: "https://example.test/".into(),
            http_version: "1.1".into(),
            received_bytes: 0,
            stored_bytes: 0,
            full_body_sha256: None,
            timings: switchyard_api::ResponseTimings {
                dns_ms: Some(3),
                connect_ms: Some(4),
                ..Default::default()
            },
            cookies: Vec::new(),
            duration_ms: 20,
            truncated: false,
            redirects: Vec::new(),
            test_results: Vec::new(),
        };
        let text = trace_text(&trace_rows(&response, None, Some("script failed")));
        let tags: Vec<&str> = text
            .lines()
            .map(|line| line.split_whitespace().nth(2).unwrap_or_default())
            .collect();
        assert_eq!(tags, ["send", "dns", "conn", "done", "error"]);
        assert!(text.contains("     3 ms  conn   conn resolved  (4 ms)"));
        assert!(text.ends_with("error  script failed"));
    }

    #[test]
    fn tests_text_lists_verdicts_and_summary() {
        let results = vec![
            TestResult {
                name: "status is 200".into(),
                passed: true,
                skipped: false,
                error: None,
            },
            TestResult {
                name: "has id".into(),
                passed: false,
                skipped: false,
                error: Some("expected id".into()),
            },
            TestResult {
                name: "later".into(),
                passed: false,
                skipped: true,
                error: None,
            },
        ];
        assert_eq!(
            tests_text(&results),
            "PASS status is 200\nFAIL has id\n     expected id\nSKIP later\n1 passed · 1 skipped · 1 failed"
        );
    }
}
