//! Environment labels in the API workbench: the Envs tab picker, the colour
//! of the environment in force, and the confirmation asked before a request
//! that is not GET / HEAD / OPTIONS is sent to Production.

use super::view::{chip, heading};
use super::*;
use crate::api::compat::dialogs::{self, Dismiss};
use gpui_kit::{Div, Hsla, div, px};
use switchyard_api::{EnvironmentLabel, send_needs_confirmation};
use switchyard_core::store::EnvironmentLabel as DbLabel;

/// The database connection label with the same meaning, for its colours.
fn db_label(label: EnvironmentLabel) -> DbLabel {
    match label {
        EnvironmentLabel::Production => DbLabel::Production,
        EnvironmentLabel::Staging => DbLabel::Staging,
        EnvironmentLabel::Development => DbLabel::Development,
        EnvironmentLabel::Local => DbLabel::Local,
    }
}

/// The accent of `label` (red, amber, green), or `None` for Local.
pub(super) fn label_color(label: EnvironmentLabel, cx: &App) -> Option<Hsla> {
    (!label.is_local()).then(|| crate::theme::palette(cx).env(db_label(label)))
}

/// The short badge (`PROD`, `STG`, `DEV`, `LOCAL`) database connections use.
pub(super) fn badge(label: EnvironmentLabel) -> &'static str {
    db_label(label).badge()
}

/// Whether a run of requests with these methods needs one confirmation.
fn run_needs_confirmation<'a>(
    methods: impl IntoIterator<Item = &'a str>,
    label: EnvironmentLabel,
) -> bool {
    methods
        .into_iter()
        .any(|method| send_needs_confirmation(method, label))
}

impl WorkbenchPanel {
    /// The label in force. Like the base URL, the Envs editor's value is in
    /// force (it mirrors the active environment); a saved Production label
    /// still counts until the change is saved.
    pub(super) fn environment_label_in_force(&self) -> EnvironmentLabel {
        if self
            .active_environment()
            .is_some_and(|environment| environment.label.is_production())
        {
            EnvironmentLabel::Production
        } else {
            self.environment_label
        }
    }

    /// The colour of the environment in force, `None` when it is Local.
    pub(super) fn environment_label_color(&self, cx: &App) -> Option<Hsla> {
        label_color(self.environment_label_in_force(), cx)
    }

    /// The Envs tab's `LABEL` row: Production / Staging / Development /
    /// Local. Saved with the rest of the form.
    pub(super) fn render_env_label(&self, cx: &mut Context<Self>) -> Div {
        let colors = cx.theme().colors;
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(heading("Label", colors.muted_foreground))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(4.))
                    .children(EnvironmentLabel::ALL.into_iter().map(|label| {
                        let selected = self.environment_label == label;
                        let mut button = chip(
                            format!("workbench-env-label-{}", label.as_str()),
                            label.name(),
                            selected,
                            cx,
                        );
                        if let Some(color) = label_color(label, cx) {
                            button = button.text_color(color);
                            if selected {
                                button = button
                                    .bg(crate::theme::palette(cx).env_bg(db_label(label)))
                                    .border_1()
                                    .border_color(color);
                            }
                        }
                        button.on_click(cx.listener(move |this, _, _, cx| {
                            this.environment_label = label;
                            cx.notify();
                        }))
                    })),
            )
            .child(
                div()
                    .text_size(text::S11)
                    .text_color(colors.muted_foreground)
                    .child(
                        "Production is shown in red and asks before sending anything but GET, HEAD or OPTIONS",
                    ),
            )
    }

    /// Send, asking first when the method is not safe and the environment
    /// in force is Production.
    pub(super) fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let method = self.current_method(cx);
        let label = self.environment_label_in_force();
        if !send_needs_confirmation(&method, label) {
            self.send_now(window, cx);
            return;
        }
        let url = self.url.read(cx).value().trim().to_string();
        let base = self.environment_base_url_value(cx);
        let target = if url.starts_with('/') && !base.is_empty() {
            format!("{}{url}", base.trim_end_matches('/'))
        } else {
            url
        };
        let message = format!(
            "{method} {target}\n\nThis sends to the Production environment “{}”.",
            self.environment_display_name()
        );
        self.confirm_production(
            "Send to Production?",
            message,
            "Send to Production",
            |panel, window, cx| panel.send_now(window, cx),
            window,
            cx,
        );
    }

    /// Run the collection, asking once first when the run targets
    /// Production and contains a request that is not GET / HEAD / OPTIONS.
    pub(super) fn run_collection_confirmed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let label = self.environment_label_in_force();
        let methods = self.run_methods(cx);
        if !run_needs_confirmation(methods.iter().map(String::as_str), label) {
            self.run_collection(cx);
            return;
        }
        let mut unsafe_methods: Vec<&str> = methods
            .iter()
            .map(String::as_str)
            .filter(|method| send_needs_confirmation(method, label))
            .collect();
        unsafe_methods.sort_unstable();
        unsafe_methods.dedup();
        let message = format!(
            "This run sends {} of {} requests with {} to the Production environment “{}”.",
            methods
                .iter()
                .filter(|method| send_needs_confirmation(method, label))
                .count(),
            methods.len(),
            unsafe_methods.join(", "),
            self.environment_display_name()
        );
        self.confirm_production(
            "Run against Production?",
            message,
            "Run against Production",
            |panel, _, cx| panel.run_collection(cx),
            window,
            cx,
        );
    }

    /// The methods `run_collection` would send, the open draft included the
    /// way it includes it.
    fn run_methods(&self, cx: &App) -> Vec<String> {
        let (Some(data), Some(collection_id)) = (
            self.workspace_data.as_ref(),
            self.current_collection_id.as_ref(),
        ) else {
            return Vec::new();
        };
        let folder = self.runner_folder_id.as_ref().filter(|folder| {
            data.folders.iter().any(|candidate| {
                &candidate.id == *folder && &candidate.collection_id == collection_id
            })
        });
        let selected = self
            .runner_request_selection_active
            .then(|| self.runner_request_ids.iter().cloned().collect::<Vec<_>>());
        let requests = select_run_requests(
            &data.requests,
            &data.folders,
            collection_id,
            folder,
            selected.as_deref(),
        );
        let draft = self.current_request_id.as_ref();
        let draft_method = self.current_method(cx);
        let mut methods: Vec<String> = requests
            .iter()
            .map(|request| {
                if Some(&request.id) == draft {
                    draft_method.clone()
                } else {
                    request.method.as_str().to_string()
                }
            })
            .collect();
        if requests.is_empty() && !self.runner_request_selection_active && folder.is_none() {
            methods.push(draft_method);
        }
        methods
    }

    fn confirm_production(
        &mut self,
        title: &'static str,
        message: String,
        confirm: &'static str,
        then: fn(&mut WorkbenchPanel, &mut Window, &mut Context<WorkbenchPanel>),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let panel = cx.entity().downgrade();
        let red = label_color(EnvironmentLabel::Production, cx)
            .unwrap_or_else(|| cx.theme().colors.danger);
        self.focus_handle.focus(window, cx);
        dialogs::open(window, cx, Dismiss::Explicit, move |dialog, window, _| {
            let panel = panel.clone();
            dialog
                .title(title)
                .w(dialogs::dialog_width(window, window.rem_size() * 30.))
                .child(
                    div()
                        .id("workbench-production-confirm")
                        .debug_selector(|| "workbench-production-confirm".into())
                        .flex()
                        .flex_col()
                        .gap_2()
                        .pl_3()
                        .border_l_2()
                        .border_color(red)
                        .whitespace_normal()
                        .children(
                            message
                                .split("\n\n")
                                .map(|line| div().child(line.to_string())),
                        ),
                )
                .footer(dialogs::confirm_footer(confirm))
                .on_ok(move |_, window, cx| {
                    let _ = panel.update(cx, |panel, cx| then(panel, window, cx));
                    true
                })
        });
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_confirms_once_when_any_request_writes_to_production() {
        let reads = ["GET", "HEAD", "OPTIONS"];
        let mixed = ["GET", "POST", "DELETE"];
        assert!(!run_needs_confirmation(reads, EnvironmentLabel::Production));
        assert!(run_needs_confirmation(mixed, EnvironmentLabel::Production));
        for label in [
            EnvironmentLabel::Staging,
            EnvironmentLabel::Development,
            EnvironmentLabel::Local,
        ] {
            assert!(!run_needs_confirmation(mixed, label));
        }
        assert!(!run_needs_confirmation([], EnvironmentLabel::Production));
    }

    #[test]
    fn labels_map_to_the_database_colours() {
        for label in EnvironmentLabel::ALL {
            assert_eq!(db_label(label).name(), label.name());
        }
    }
}
