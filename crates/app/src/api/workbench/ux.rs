//! Request controls and read-only execution inspection.
use std::collections::{BTreeMap, BTreeSet};

use gpui_kit::component::input::TextareaState;
use gpui_kit::component::{
    Disableable, Sizable,
    button::{Button, ButtonVariants},
    dialog::DialogButtonProps,
    input::Input,
    resizable::ResizableState,
};
use gpui_kit::{Div, Subscription, div};

use super::*;
use crate::api::compat::dialogs::{self, Dismiss};

pub(super) struct WorkbenchUx {
    pub request_tabs_scroll: gpui_kit::ScrollHandle,
    pub revealed_request_tab: std::cell::Cell<Option<u64>>,
    pub creation: request_creation::CreationState,
    /// A first-run quick action waiting for its new project to finish opening.
    pub pending_start: Option<empty_state::StartAction>,
    pub request_settings: BTreeMap<u64, switchyard_api::RequestSettings>,
    pub body_editor: Entity<body_editor::BodyEditor>,
    pub rail_split: Entity<ResizableState>,
    pub response_split: Entity<ResizableState>,
    pub layout: layout::LayoutPreferences,
    pub response_view: response_controls::ResponseView,
    pub filter: Entity<InputState>,
    pub console_cleared: usize,
    pub console_cleared_error: Option<String>,
    _filter_subscription: Subscription,
}

impl WorkbenchUx {
    pub fn new(
        body: Entity<TextareaState>,
        window: &mut Window,
        cx: &mut Context<WorkbenchPanel>,
    ) -> Self {
        let filter =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter console messages"));
        let subscription = cx.subscribe(&filter, |_, _, _: &InputEvent, cx| cx.notify());
        Self {
            request_tabs_scroll: Default::default(),
            revealed_request_tab: Default::default(),
            creation: Default::default(),
            pending_start: None,
            request_settings: BTreeMap::new(),
            body_editor: cx.new(|cx| body_editor::BodyEditor::new(body, window, cx)),
            rail_split: cx.new(|_| ResizableState::default()),
            response_split: cx.new(|_| ResizableState::default()),
            layout: layout::LayoutPreferences::load(),
            response_view: response_controls::ResponseView::new(window, cx),
            filter,
            console_cleared: 0,
            console_cleared_error: None,
            _filter_subscription: subscription,
        }
    }
}

pub(super) const ASSERTION_SNIPPETS: &[(&str, &str)] = &[
    (
        "Status is 200",
        "pm.test('Status is 200', () => { pm.response.to.have.status(200); });",
    ),
    (
        "Response time below 500 ms",
        "pm.test('Response time below 500 ms', () => { pm.expect(pm.response.responseTime).to.be.below(500); });",
    ),
    (
        "Content-Type header exists",
        "pm.test('Content-Type exists', () => { pm.response.to.have.header('Content-Type'); });",
    ),
    (
        "JSON property equals a value",
        "pm.test('JSON property', () => { pm.expect(pm.response.json()).to.have.property('id', 1); });",
    ),
    (
        "JSON object includes values",
        "pm.test('JSON values', () => { pm.expect(pm.response.json()).to.include({success: true}); });",
    ),
    (
        "Validate JSON schema",
        "pm.test('Response schema', () => { pm.response.to.have.jsonSchema({type: 'object', required: ['id'], properties: {id: {type: 'integer'}}}); });",
    ),
];

impl WorkbenchPanel {
    pub(super) fn render_script_snippets(&self, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_wrap()
            .gap_2()
            .children(ASSERTION_SNIPPETS.iter().map(|(label, source)| {
                Button::new(gpui_kit::SharedString::from(format!(
                    "workbench-assertion-{label}"
                )))
                .small()
                .outline()
                .label(*label)
                .on_click(cx.listener(move |this, _, window, cx| {
                    let existing = this.assertions.read(cx).value();
                    let text = if existing.trim().is_empty() {
                        source.to_string()
                    } else {
                        format!("{existing}\n\n{source}")
                    };
                    set_input(&this.assertions, &text, window, cx);
                    this.dirty = true;
                    this.assertions.focus_handle(cx).focus(window, cx);
                    cx.notify();
                }))
            }))
    }

    pub(super) fn render_console(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let filter = self.ux.filter.read(cx).value().to_lowercase();
        let mut lines: Vec<String> = self
            .response
            .as_ref()
            .into_iter()
            .flat_map(|response| response.console.iter())
            .skip(self.ux.console_cleared)
            .filter(|entry| {
                format!("{} {} {}", entry.phase, entry.level, entry.message)
                    .to_lowercase()
                    .contains(&filter)
            })
            .map(|entry| format!("[{}] {}: {}", entry.phase, entry.level, entry.message))
            .collect();
        if let Some(error) = self.error.as_ref().filter(|error| !error.is_empty()) {
            let already_logged = self.response.as_ref().is_some_and(|response| {
                response
                    .console
                    .iter()
                    .any(|entry| entry.message.contains(error))
            });
            let line = format!("[execution] error: {error}");
            if !already_logged
                && self.ux.console_cleared_error.as_ref() != Some(error)
                && line.to_lowercase().contains(&filter)
            {
                lines.push(line);
            }
        }
        let copy = lines.join("\n");
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(Input::new(&self.ux.filter).flex_1())
                    .child(
                        Button::new("workbench-console-copy")
                            .debug_selector(|| "workbench-console-copy".into())
                            .small()
                            .outline()
                            .label("Copy logs")
                            .disabled(copy.is_empty())
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
                                    copy.clone(),
                                ));
                            }),
                    )
                    .child(
                        Button::new("workbench-console-clear")
                            .debug_selector(|| "workbench-console-clear".into())
                            .small()
                            .ghost()
                            .label("Clear view")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.ux.console_cleared =
                                    this.response.as_ref().map_or(0, |r| r.console.len());
                                this.ux.console_cleared_error = this.error.clone();
                                cx.notify();
                            })),
                    ),
            )
            .when(lines.is_empty(), |el| {
                el.child("No matching console messages. Use console.log in a script, then send.")
            })
            .children(lines.into_iter().map(|line| {
                div()
                    .font_family(crate::api::compat::fonts::mono(cx))
                    .text_sm()
                    .child(line)
            }))
            .into_any_element()
    }

    pub(super) fn render_request_settings(&self, cx: &mut Context<Self>) -> Div {
        let settings = self.effective_request_settings();
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(format!(
                "Timeout: {} ms · Redirects: {} · Maximum hops: {}",
                settings.timeout_ms,
                if settings.follow_redirects {
                    "follow"
                } else {
                    "do not follow"
                },
                settings.max_redirects
            ))
            .child(
                Button::new("workbench-edit-settings")
                    .outline()
                    .label("Edit request settings")
                    .on_click(
                        cx.listener(|this, _, window, cx| this.edit_request_settings(window, cx)),
                    ),
            )
    }

    fn edit_request_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let settings = self.effective_request_settings();
        let timeout =
            cx.new(|cx| InputState::new(window, cx).default_value(settings.timeout_ms.to_string()));
        let redirects = cx.new(|cx| {
            InputState::new(window, cx).default_value(settings.max_redirects.to_string())
        });
        let follow = cx.new(|_| settings.follow_redirects);
        let error = cx.new(|_| String::new());
        let panel = cx.entity().downgrade();
        let Some(tab_id) = self.active_request_tab_id() else {
            return;
        };
        let workspace = self.bound_workspace.clone();
        let timeout_save = timeout.clone();
        let redirects_save = redirects.clone();
        let follow_save = follow.clone();
        let error_save = error.clone();
        self.focus_handle.focus(window, cx);
        dialogs::open(window, cx, Dismiss::Explicit, move |dialog, window, cx| {
            let follow_value = *follow.read(cx);
            let follow = follow.clone();
            let panel = panel.clone();
            let workspace = workspace.clone();
            let timeout_save = timeout_save.clone();
            let redirects_save = redirects_save.clone();
            let follow_save = follow_save.clone();
            let error_save = error_save.clone();
            dialog.title("Request settings")
                .footer(crate::api::compat::dialogs::confirm_footer("Apply settings"))
                .button_props(DialogButtonProps::default().ok_text("Apply settings"))
                .w(dialogs::dialog_width(window, window.rem_size() * 30.))
                .max_h(dialogs::dialog_ceiling(window))
                .child(div().id("workbench-request-settings-dialog").debug_selector(|| "workbench-request-settings-dialog".into()).flex().flex_col().gap_3()
                .child("Timeout (milliseconds, 1–120000)").child(Input::new(&timeout))
                .child("Maximum redirect hops (0–10)").child(Input::new(&redirects))
                .child(Button::new("follow-redirects").outline()
                    .label(if follow_value { "Follow redirects: on" } else { "Follow redirects: off" })
                    .on_click(move |_, _, cx| follow.update(cx, |value, cx| { *value = !*value; cx.notify(); })))
                .child(div().text_color(cx.theme().danger).child(error.read(cx).clone())))
                .on_ok(move |_, _, cx| {
                    let parsed = timeout_save.read(cx).value().trim().parse::<u64>().ok().filter(|n| (1..=120_000).contains(n))
                        .zip(redirects_save.read(cx).value().trim().parse::<u8>().ok().filter(|n| *n <= 10));
                    let Some((timeout_ms, max_redirects)) = parsed else {
                        error_save.update(cx, |value, cx| { *value = "Enter a valid timeout and redirect limit.".into(); cx.notify(); });
                        return false;
                    };
                    let follow_redirects = *follow_save.read(cx);
                    panel.update(cx, |panel, cx| {
                        if panel.bound_workspace != workspace || current_workspace_id() != workspace
                            || panel.request_tabs.get(panel.active_request_tab).is_none_or(|tab| tab.id != tab_id) {
                            error_save.update(cx, |value, cx| { *value = "The request changed. Close this dialog and reopen its settings.".into(); cx.notify(); });
                            return false;
                        }
                        panel.apply_request_settings(timeout_ms, max_redirects, follow_redirects, cx);
                        true
                    }).unwrap_or(true)
                })
        });
    }

    fn effective_request_settings(&self) -> switchyard_api::RequestSettings {
        let mut settings = self
            .request_tabs
            .get(self.active_request_tab)
            .and_then(|tab| self.ux.request_settings.get(&tab.id))
            .cloned()
            .or_else(|| {
                self.current_definition
                    .as_ref()
                    .map(|request| request.settings.clone())
            })
            .unwrap_or_default();
        settings.allow_private_network = self.allow_private_network;
        settings
    }

    fn apply_request_settings(
        &mut self,
        timeout_ms: u64,
        max_redirects: u8,
        follow_redirects: bool,
        cx: &mut Context<Self>,
    ) {
        let mut settings = self.effective_request_settings();
        if (
            settings.timeout_ms,
            settings.max_redirects,
            settings.follow_redirects,
        ) == (timeout_ms, max_redirects, follow_redirects)
        {
            return;
        }
        settings.timeout_ms = timeout_ms;
        settings.max_redirects = max_redirects;
        settings.follow_redirects = follow_redirects;
        let Some(tab_id) = self.active_request_tab_id() else {
            return;
        };
        self.ux.request_settings.insert(tab_id, settings);
        self.dirty = true;
        cx.notify();
    }

    pub(super) fn render_variable_inspector(&self, cx: &App) -> Div {
        let mut scopes: Vec<(String, Vec<Variable>)> = Vec::new();
        if let Some(data) = &self.workspace_data {
            if let Some(collection) = self
                .current_collection_id
                .as_ref()
                .and_then(|id| data.collection(id))
            {
                scopes.push((
                    format!("Collection: {}", collection.name),
                    collection.variables.clone(),
                ));
            }
            let mut folder_id = self.selected_folder();
            let mut seen = BTreeSet::new();
            let mut folders = Vec::new();
            while let Some(id) = folder_id {
                if !seen.insert(id.clone()) {
                    break;
                }
                let Some(folder) = data.folders.iter().find(|f| f.id == id) else {
                    break;
                };
                folders.push(folder);
                folder_id = folder.parent_id.clone();
            }
            for folder in folders.into_iter().rev() {
                scopes.push((format!("Folder: {}", folder.name), folder.variables.clone()));
            }
        }
        let mut values = BTreeMap::<String, (String, String, Vec<String>)>::new();
        let base = self.environment_base_url_value(cx);
        if !base.is_empty() {
            for key in ["base_url", "baseUrl"] {
                insert_variable(
                    &mut values,
                    key.into(),
                    base.clone(),
                    "Environment base URL".into(),
                );
            }
        }
        for (scope, variables) in scopes {
            for variable in variables.into_iter().filter(|v| v.enabled) {
                let value = match variable.value {
                    VariableValue::Plain(value) if !sensitive_name(&variable.key) => value,
                    _ => "••••".into(),
                };
                insert_variable(&mut values, variable.key, value, scope.clone());
            }
        }
        // Sending reads these live environment fields, including unsaved edits.
        for (source, label) in [
            (&self.environment_variables, "Environment draft"),
            (&self.variables, "Request"),
        ] {
            for (enabled, name, value) in entries::KvGrid::parse(&source.read(cx).value(), '=') {
                if !enabled {
                    continue;
                }
                let secret = name.starts_with("secret:") || sensitive_name(&name);
                let name = name.strip_prefix("secret:").unwrap_or(&name).to_string();
                insert_variable(
                    &mut values,
                    name,
                    if secret { "••••".into() } else { value },
                    label.into(),
                );
            }
        }
        let references = scan_references(&[
            self.url.read(cx).value().as_ref(),
            self.params.read(cx).value().as_ref(),
            self.headers.read(cx).value().as_ref(),
            self.body.read(cx).value().as_ref(),
            self.variables.read(cx).value().as_ref(),
            self.environment_variables.read(cx).value().as_ref(),
        ]);
        let names: BTreeSet<_> = values.keys().cloned().chain(references.names).collect();
        let more = names.len() > 256;
        let rows: Vec<_> = names
            .iter()
            .take(256)
            .map(|name| {
                let value = resolve_preview(name, &values, &mut BTreeSet::new());
                let detail = match values.get(name) {
                    Some((_, source, shadowed)) if shadowed.is_empty() => source.clone(),
                    Some((_, source, shadowed)) => {
                        format!("{source} · overrides {}", shadowed.join(", "))
                    }
                    None if name.starts_with("vault.") => {
                        "Named vault reference · availability checked on send".into()
                    }
                    None if is_runtime_variable(name) => {
                        "Runtime expression · validated on send".into()
                    }
                    None => "Referenced in the draft · not defined in these scopes".into(),
                };
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .py_1()
                    .child(format!("{} = {value}", bounded_text(name, 256)))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(detail),
                    )
            })
            .collect::<Vec<_>>();
        div().flex().flex_col().gap_2().border_t_1().border_color(cx.theme().border).pt_3()
            .child("Variable draft preview")
            .child(div().text_xs().text_color(cx.theme().muted_foreground)
                .child("Draft preview before scripts and runner data. Secrets stay masked; runtime expressions resolve when sent."))
            .when(rows.is_empty(), |el| el.child("No enabled variables in these scopes."))
            .when(references.malformed > 0, |el| el.child(div().text_color(cx.theme().danger)
                .child(format!("{} malformed template expression(s). Check unmatched or empty braces.", references.malformed))))
            .when(references.truncated || more, |el| el.child("Preview limit reached; only part of this draft is shown."))
            .children(rows)
    }
}

type PreviewVariables = BTreeMap<String, (String, String, Vec<String>)>;

fn sensitive_name(name: &str) -> bool {
    // Match the persistence policy's credential names without resolving a vault.
    let name: String = name
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect();
    name == "sid"
        || name == "auth"
        || name.ends_with("auth")
        || [
            "authorization",
            "bearer",
            "credential",
            "password",
            "passwd",
            "secret",
            "token",
            "apikey",
            "accesskey",
            "privatekey",
            "cookie",
            "sessionid",
        ]
        .iter()
        .any(|marker| name.contains(marker))
}

fn insert_variable(values: &mut PreviewVariables, name: String, value: String, source: String) {
    let mut shadowed = Vec::new();
    if let Some((_, old_source, old_shadowed)) = values.remove(&name) {
        shadowed.extend(old_shadowed);
        shadowed.push(old_source);
    }
    values.insert(name, (value, source, shadowed));
}

fn resolve_preview(name: &str, values: &PreviewVariables, stack: &mut BTreeSet<String>) -> String {
    let mut output = String::new();
    let mut visits = 256;
    expand_preview(name, values, stack, &mut visits, &mut output);
    output
}

fn push_preview(output: &mut String, text: &str) {
    let remaining = 8192usize.saturating_sub(output.len());
    output.push_str(&text[..text.floor_char_boundary(remaining.min(text.len()))]);
}

fn expand_preview(
    name: &str,
    values: &PreviewVariables,
    stack: &mut BTreeSet<String>,
    visits: &mut usize,
    output: &mut String,
) {
    if output.len() >= 8192 {
        return;
    }
    if *visits == 0 {
        push_preview(output, "[preview limit]");
        return;
    }
    *visits -= 1;
    if name.starts_with("vault.") {
        push_preview(output, "••••");
        return;
    }
    let Some((value, _, _)) = values.get(name) else {
        if is_runtime_variable(name) {
            push_preview(output, "generated on send");
        } else {
            push_preview(output, &format!("[missing: {}]", bounded_text(name, 256)));
        }
        return;
    };
    if stack.len() >= 32 || !stack.insert(name.into()) {
        push_preview(output, "[variable cycle]");
        return;
    }
    let mut rest = value.as_str();
    while !rest.is_empty() && output.len() < 8192 && *visits > 0 {
        let Some(start) = rest.find("{{") else {
            push_preview(output, rest);
            break;
        };
        push_preview(output, &rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            push_preview(output, "[malformed template]");
            break;
        };
        let key = after[..end].trim();
        if key.is_empty() || key.contains("{{") {
            push_preview(output, "[malformed template]");
        } else {
            expand_preview(key, values, stack, visits, output);
        }
        rest = &after[end + 2..];
    }
    stack.remove(name);
}

fn bounded_text(text: &str, max: usize) -> &str {
    &text[..text.floor_char_boundary(max.min(text.len()))]
}

fn is_runtime_variable(name: &str) -> bool {
    matches!(
        name,
        "$isoTimestamp"
            | "$timestamp"
            | "$timestampMs"
            | "$date"
            | "$time"
            | "$uuid"
            | "$guid"
            | "$randomInt"
    )
}

#[derive(Default)]
struct ReferenceScan {
    names: BTreeSet<String>,
    malformed: usize,
    truncated: bool,
}

fn scan_references(sources: &[&str]) -> ReferenceScan {
    let mut result = ReferenceScan::default();
    let mut remaining = 65_536usize;
    for source in sources {
        let mut rest = bounded_text(source, remaining);
        remaining -= rest.len();
        result.truncated |= rest.len() != source.len();
        while !rest.is_empty() {
            if result.names.len() >= 256 {
                result.truncated = true;
                break;
            }
            let start = rest.find("{{");
            let close = rest.find("}}");
            if let Some(close) = close
                && start.is_none_or(|start| close < start)
            {
                result.malformed += 1;
                rest = &rest[close + 2..];
                continue;
            }
            let Some(start) = start else {
                break;
            };
            let after = &rest[start + 2..];
            let Some(end) = after.find("}}") else {
                result.malformed += 1;
                break;
            };
            let name = after[..end].trim();
            if name.is_empty() || name.contains("{{") {
                result.malformed += 1;
            } else if name.len() > 256 {
                result.truncated = true;
            } else {
                result.names.insert(name.to_string());
            }
            rest = &after[end + 2..];
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variable_preview_is_bounded_and_user_values_override_runtime_names() {
        let mut values = BTreeMap::new();
        assert_eq!(
            resolve_preview("$guid", &values, &mut BTreeSet::new()),
            "generated on send"
        );
        assert!(resolve_preview("$unsupported", &values, &mut BTreeSet::new()).contains("missing"));
        insert_variable(
            &mut values,
            "$uuid".into(),
            "fixed".into(),
            "Request".into(),
        );
        insert_variable(
            &mut values,
            "huge".into(),
            "é".repeat(10_000),
            "Request".into(),
        );
        insert_variable(
            &mut values,
            "suffix".into(),
            format!("{{{{$uuid}}}}{}", "界".repeat(10_000)),
            "Request".into(),
        );
        assert_eq!(
            resolve_preview("$uuid", &values, &mut BTreeSet::new()),
            "fixed"
        );
        for name in ["huge", "suffix"] {
            assert!(resolve_preview(name, &values, &mut BTreeSet::new()).len() <= 8192);
        }
        insert_variable(
            &mut values,
            "broken".into(),
            "before {{missing".into(),
            "Request".into(),
        );
        assert!(resolve_preview("broken", &values, &mut BTreeSet::new()).contains("malformed"));
        for name in [
            "X-Access-Key",
            "session_id",
            "sid",
            "bearer",
            "customAuth",
            "password",
        ] {
            assert!(sensitive_name(name));
        }
    }

    #[test]
    fn variable_references_include_missing_names_and_bound_malformed_input() {
        let references = scan_references(&[
            "https://{{host}}/{{missing_path}}",
            "header={{vault.key}}",
            "{{}} {{unterminated",
        ]);
        assert_eq!(
            references.names,
            BTreeSet::from(["host".into(), "missing_path".into(), "vault.key".into()])
        );
        assert_eq!(references.malformed, 2);
        let huge = format!("{}{{{{too_late}}}}", "x".repeat(70_000));
        let references = scan_references(&[&huge]);
        assert!(references.truncated);
        assert!(!references.names.contains("too_late"));
    }
    #[test]
    fn variable_preview_reports_precedence_cycles_and_masks_vault_values() {
        let mut values = BTreeMap::new();
        insert_variable(
            &mut values,
            "host".into(),
            "old".into(),
            "Collection".into(),
        );
        insert_variable(
            &mut values,
            "host".into(),
            "new".into(),
            "Environment".into(),
        );
        insert_variable(
            &mut values,
            "url".into(),
            "https://{{host}}/{{vault.key}}".into(),
            "Request".into(),
        );
        assert_eq!(
            resolve_preview("url", &values, &mut BTreeSet::new()),
            "https://new/••••"
        );
        assert_eq!(values["host"].2, vec!["Collection"]);
        insert_variable(
            &mut values,
            "cycle".into(),
            "{{cycle}}".into(),
            "Request".into(),
        );
        assert!(resolve_preview("cycle", &values, &mut BTreeSet::new()).contains("cycle"));
        assert!(resolve_preview("absent", &values, &mut BTreeSet::new()).contains("missing"));
    }
}
