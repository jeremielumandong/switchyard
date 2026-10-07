//! Bounded response search and explicit capture into the current request draft.

use gpui_kit::component::{
    Disableable,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    dialog::DialogButtonProps,
    input::Input,
    scroll::ScrollableElement,
};
use gpui_kit::{Subscription, WeakEntity, div};

use super::*;
use crate::api::compat::dialogs::{self, Dismiss};

const MAX_INSPECT_BYTES: usize = 1024 * 1024;
const MAX_CAPTURE_BYTES: usize = 64 * 1024;
const MAX_MATCHES: usize = 12;

struct Match {
    line: usize,
    column: usize,
    excerpt: String,
}

fn find_matches(body: &str, needle: &str) -> (usize, Vec<Match>) {
    if needle.is_empty() {
        return (0, Vec::new());
    }
    let mut total = 0;
    let mut matches = Vec::new();
    for (line_ix, line) in body.lines().enumerate() {
        for (offset, _) in line.match_indices(needle) {
            total += 1;
            if matches.len() < MAX_MATCHES {
                let column = line[..offset].chars().count();
                let start = column.saturating_sub(40);
                let excerpt: String = line.chars().skip(start).take(160).collect();
                matches.push(Match {
                    line: line_ix + 1,
                    column: column + 1,
                    excerpt: format!(
                        "{}{}{}",
                        if start > 0 { "…" } else { "" },
                        excerpt,
                        if line.chars().count() > start + 160 {
                            "…"
                        } else {
                            ""
                        }
                    ),
                });
            }
        }
    }
    (total, matches)
}

fn captured_value(json: &serde_json::Value, pointer: &str) -> Result<String, String> {
    if !pointer.is_empty() && !pointer.starts_with('/') {
        return Err(
            "Use a JSON Pointer such as /data/id; leave it empty for the whole body.".into(),
        );
    }
    // serde_json resolves RFC 6901 escapes, including ~1 for / and ~0 for ~.
    let value = json
        .pointer(pointer)
        .ok_or("No value exists at this JSON Pointer.")?;
    let value = match value {
        serde_json::Value::String(value) => value.clone(),
        value => serde_json::to_string(value).map_err(|error| error.to_string())?,
    };
    if value.len() > MAX_CAPTURE_BYTES {
        return Err(
            "Choose a smaller value; request-variable capture is limited to 64 KiB.".into(),
        );
    }
    if value.contains("<redacted>") {
        return Err("This value was redacted. Capture it from a new live response instead.".into());
    }
    if value.contains(['\r', '\n']) || value.trim() != value {
        return Err("This string contains line breaks or outer whitespace that Vars cannot preserve. Encode it in a script or choose another value.".into());
    }
    Ok(value)
}

fn set_variable(source: &str, name: &str, value: &str, secret: bool) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty()
        || !name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    {
        return Err("Variable names must start with a letter or underscore and contain only letters, digits, dots, hyphens, or underscores.".into());
    }
    let replacement = format!("{}{name}={value}", if secret { "secret:" } else { "" });
    let mut found = false;
    let mut lines = Vec::new();
    for line in source.lines() {
        let rows = entries::KvGrid::parse(line, '=');
        if rows
            .first()
            .is_some_and(|(_, key, _)| key.strip_prefix("secret:").unwrap_or(key) == name)
        {
            if !found {
                lines.push(replacement.clone());
                found = true;
            }
        } else {
            lines.push(line.to_string());
        }
    }
    if !found {
        lines.push(replacement);
    }
    Ok(lines.join("\n"))
}

struct ResponseInspector {
    panel: WeakEntity<WorkbenchPanel>,
    workspace: WorkspaceId,
    tab_id: u64,
    response_generation: u64,
    body: String,
    json: Result<serde_json::Value, String>,
    unavailable: Option<String>,
    query: Entity<InputState>,
    pointer: Entity<InputState>,
    name: Entity<InputState>,
    secret: bool,
    reveal: bool,
    matches: Vec<Match>,
    total: Option<usize>,
    captured: Result<String, String>,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl WorkbenchPanel {
    pub(super) fn open_response_inspector(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(response) = self.response.clone() else {
            return;
        };
        let Some(tab) = self.request_tabs.get(self.active_request_tab) else {
            return;
        };
        let tab_id = tab.id;
        let workspace = self.bound_workspace.clone();
        let response_generation = self.response_body_generation;
        let panel = cx.entity().downgrade();
        let inspector = cx.new(|cx| {
            ResponseInspector::new(
                panel,
                workspace,
                tab_id,
                response_generation,
                &response,
                window,
                cx,
            )
        });
        self.focus_handle.focus(window, cx);
        dialogs::open(window, cx, Dismiss::Explicit, move |dialog, window, cx| {
            let save = inspector.clone();
            let button = inspector.clone();
            let disabled =
                inspector.read(cx).unavailable.is_some() || inspector.read(cx).captured.is_err();
            dialog
                .title("Inspect response body")
                .w(dialogs::dialog_width(window, window.rem_size() * 42.))
                .max_h(dialogs::dialog_ceiling(window))
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("Set request variable")
                        .cancel_text("Close"),
                )
                .child(inspector.clone())
                .on_ok(move |_, window, cx| {
                    save.update(cx, |this, cx| {
                        if this.query.focus_handle(cx).is_focused(window) {
                            this.find(cx);
                            false
                        } else {
                            this.capture(window, cx)
                        }
                    })
                })
                .footer(crate::api::compat::dialogs::footer_row({
                    let cancel = crate::api::compat::dialogs::cancel_button;
                    let button = button.clone();
                    vec![
                        cancel(window, cx),
                        Button::new("capture-response-variable")
                            .primary()
                            .label("Set request variable")
                            .disabled(disabled)
                            .on_click(move |_, window, cx| {
                                if button.update(cx, |this, cx| this.capture(window, cx)) {
                                    gpui_kit::component::WindowExt::close_dialog(window, cx);
                                }
                            })
                            .into_any_element(),
                    ]
                }))
        });
    }
}

impl ResponseInspector {
    #[allow(clippy::too_many_arguments)]
    fn new(
        panel: WeakEntity<WorkbenchPanel>,
        workspace: WorkspaceId,
        tab_id: u64,
        response_generation: u64,
        response: &transport::Response,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let unavailable = if response.binary {
            Some("Binary responses cannot be searched or captured as text.".into())
        } else if response.body.len() > MAX_INSPECT_BYTES {
            Some("Inspect body supports text responses up to 1 MiB. Use Copy output from Raw for this larger response.".into())
        } else {
            None
        };
        // Explicit size bound keeps one-time parsing and user-triggered searches
        // off the render path without copying a large response into the dialog.
        let body = if unavailable.is_none() {
            response.body.clone()
        } else {
            String::new()
        };
        let json = serde_json::from_str(&body)
            .map_err(|_| "The response is not valid JSON. Text search is still available.".into());
        let query =
            cx.new(|cx| InputState::new(window, cx).placeholder("Find text (case-sensitive)"));
        let pointer = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("/data/id")
                .default_value("/data/id")
        });
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Variable name")
                .default_value("response_value")
        });
        let subscriptions = vec![
            cx.subscribe(&pointer, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.preview(cx);
                }
            }),
            cx.subscribe(&query, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.total = None;
                    this.matches.clear();
                    cx.notify();
                }
            }),
        ];
        let captured = json
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|json| captured_value(json, "/data/id"));
        Self {
            panel,
            workspace,
            tab_id,
            response_generation,
            body,
            json,
            unavailable,
            query,
            pointer,
            name,
            secret: true,
            reveal: false,
            matches: Vec::new(),
            total: None,
            captured,
            error: None,
            _subscriptions: subscriptions,
        }
    }

    fn preview(&mut self, cx: &mut Context<Self>) {
        self.captured = self
            .json
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|json| captured_value(json, &self.pointer.read(cx).value()));
        self.error = None;
        cx.notify();
    }

    fn find(&mut self, cx: &mut Context<Self>) {
        let (total, matches) = find_matches(&self.body, &self.query.read(cx).value());
        self.total = Some(total);
        self.matches = matches;
        cx.notify();
    }

    fn capture(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if let Some(error) = &self.unavailable {
            self.error = Some(error.clone());
            cx.notify();
            return false;
        }
        self.preview(cx);
        let value = match &self.captured {
            Ok(value) => value.clone(),
            Err(error) => {
                self.error = Some(error.clone());
                return false;
            }
        };
        let name = self.name.read(cx).value().to_string();
        let secret = self.secret;
        let workspace = self.workspace.clone();
        let tab_id = self.tab_id;
        let generation = self.response_generation;
        let result = self.panel.update(cx, |panel, cx| {
            if panel.bound_workspace != workspace
                || panel.request_tabs.get(panel.active_request_tab).map(|tab| tab.id) != Some(tab_id)
                || panel.response_body_generation != generation
            {
                return Err("The request or response changed. Close this inspector and reopen it for the current response.".to_string());
            }
            let source = set_variable(&panel.variables.read(cx).value(), &name, &value, secret)?;
            set_input(&panel.variables, &source, window, cx);
            panel.dirty = true;
            panel.composer_tab = ComposerTab::Vars;
            cx.notify();
            Ok(())
        }).unwrap_or_else(|_| Err("The request is no longer open.".into()));
        match result {
            Ok(()) => true,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                false
            }
        }
    }
}

impl Render for ResponseInspector {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let unavailable = self.unavailable.is_some();
        let preview = match &self.captured {
            Ok(value) if self.secret => format!("Value hidden · {} bytes", value.len()),
            Ok(value) => {
                value.chars().take(240).collect::<String>()
                    + if value.chars().count() > 240 {
                        "…"
                    } else {
                        ""
                    }
            }
            Err(error) => error.clone(),
        };
        div()
            .id("response-inspector-body")
            .flex()
            .flex_col()
            .gap_3()
            .max_h(window.rem_size() * 30.)
            .overflow_y_scrollbar()
            .when_some(self.unavailable.clone(), |el, error| {
                el.child(div().text_color(cx.theme().warning).child(error))
            })
            .child("Find in this response")
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(Input::new(&self.query).flex_1())
                    .child(
                        Button::new("find-response-text")
                            .outline()
                            .label("Find")
                            .disabled(unavailable)
                            .on_click(cx.listener(|this, _, _, cx| this.find(cx))),
                    ),
            )
            .child(
                Checkbox::new("reveal-response-excerpts")
                    .label("Show matching excerpts")
                    .checked(self.reveal)
                    .on_click(cx.listener(|this, checked, _, cx| {
                        this.reveal = *checked;
                        cx.notify();
                    })),
            )
            .when_some(self.total, |el, total| {
                el.child(format!("{total} matches · showing up to {MAX_MATCHES}"))
            })
            .children(self.matches.iter().map(|item| {
                div().text_sm().child(format!(
                    "Line {}, column {}{}",
                    item.line,
                    item.column,
                    if self.reveal {
                        format!(": {}", item.excerpt)
                    } else {
                        " · excerpt hidden".into()
                    }
                ))
            }))
            .child("Capture a JSON value")
            .child("JSON Pointer · use /data/id or leave empty for the whole response")
            .child(Input::new(&self.pointer))
            .child("Request variable name · replaces an existing variable with this name")
            .child(Input::new(&self.name))
            .child(
                Checkbox::new("capture-response-secret")
                    .label("Store as secret and hide the value")
                    .checked(self.secret)
                    .on_click(cx.listener(|this, checked, _, cx| {
                        this.secret = *checked;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(preview),
            )
            .child(
                "The variable is added to this request's Vars draft. Save the request to keep it.",
            )
            .when_some(self.error.clone(), |el, error| {
                el.child(div().text_color(cx.theme().danger).child(error))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_capture_preserves_json_types_and_rejects_lossy_strings() {
        let value = serde_json::json!({"data": [{"id": 7}], "a/b": {"~": "token"}, "null": null, "space": " keep ", "lines": "a\nb"});
        assert_eq!(captured_value(&value, "/data/0/id").unwrap(), "7");
        assert_eq!(captured_value(&value, "/a~1b/~0").unwrap(), "token");
        assert_eq!(captured_value(&value, "/null").unwrap(), "null");
        assert!(captured_value(&value, "/missing").is_err());
        assert!(captured_value(&value, "data").is_err());
        assert!(captured_value(&value, "/space").is_err());
        assert!(captured_value(&value, "/lines").is_err());
        assert!(captured_value(&serde_json::json!("<redacted>"), "").is_err());
        assert!(captured_value(&serde_json::json!("x".repeat(MAX_CAPTURE_BYTES + 1)), "").is_err());
    }

    #[test]
    fn capture_replaces_matching_rows_without_touching_other_variables() {
        let source = "# secret:token=old\nother=keep\ntoken=duplicate";
        assert_eq!(
            set_variable(source, "token", "new", true).unwrap(),
            "secret:token=new\nother=keep"
        );
        assert_eq!(
            set_variable("other=keep", "id", "7", false).unwrap(),
            "other=keep\nid=7"
        );
        assert!(set_variable("", "secret:token", "new", true).is_err());
        assert!(set_variable("", "a\nb", "new", true).is_err());
    }

    #[test]
    fn search_is_unicode_safe_and_bounds_result_rows() {
        let (total, matches) = find_matches("éé token\nTOKEN token", "token");
        assert_eq!(total, 2);
        assert_eq!((matches[0].line, matches[0].column), (1, 4));
        assert_eq!((matches[1].line, matches[1].column), (2, 7));
        let (total, matches) = find_matches(&"x ".repeat(100), "x");
        assert_eq!(total, 100);
        assert_eq!(matches.len(), MAX_MATCHES);
        assert_eq!(find_matches("text", "").0, 0);
    }
}
