//! The header menu's "Rename workspace…": a name dialog that renames the
//! open workspace in the store, then in the panel's workspace list.

use super::*;
use crate::api::compat::dialogs::{self, Dismiss};
use gpui_kit::component::{
    Disableable, WindowExt,
    button::{Button, ButtonVariants},
    input::Input,
};
use gpui_kit::{Subscription, WeakEntity, div};
use switchyard_api::runtime::workspace::rename_workspace;

struct RenameWorkspace {
    panel: WeakEntity<WorkbenchPanel>,
    workspace: WorkspaceId,
    name: Entity<InputState>,
    saving: bool,
    error: Option<String>,
    _work: Option<gpui_kit::Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl WorkbenchPanel {
    /// Open the rename dialog for the bound workspace.
    pub(super) fn open_rename_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.bound_workspace.clone();
        if !self
            .workspace_entries()
            .iter()
            .any(|entry| entry.id == workspace)
        {
            return;
        }
        let current = self.workspace_display_name();
        let panel = cx.entity().downgrade();
        let editor = cx.new(|cx| RenameWorkspace::new(panel, workspace, current, window, cx));
        let focus = editor.read(cx).name.focus_handle(cx);
        dialogs::open(window, cx, Dismiss::Explicit, move |dialog, window, cx| {
            let saving = editor.read(cx).saving;
            let disabled = editor.read(cx).blocked_reason(cx).is_some();
            let enter = editor.clone();
            let cancel_editor = editor.clone();
            let save = editor.clone();
            dialog
                .title("Rename workspace")
                .w(dialogs::dialog_width(window, window.rem_size() * 28.))
                .close_button(!saving)
                .keyboard(!saving)
                .on_ok(move |_, window, cx| {
                    enter.update(cx, |editor, cx| editor.save(window, cx));
                    false
                })
                .on_cancel(move |_, _, cx| !cancel_editor.read(cx).saving)
                .child(editor.clone())
                .footer(dialogs::footer_row(vec![
                    if saving {
                        Button::new("cancel")
                            .label("Cancel")
                            .disabled(true)
                            .into_any_element()
                    } else {
                        dialogs::cancel_button(window, cx)
                    },
                    Button::new("workbench-rename-workspace-save")
                        .debug_selector(|| "workbench-rename-workspace-save".into())
                        .primary()
                        .label(if saving { "Renaming…" } else { "Rename" })
                        .disabled(disabled)
                        .on_click(move |_, window, cx| {
                            save.update(cx, |editor, cx| editor.save(window, cx))
                        })
                        .into_any_element(),
                ]))
        });
        focus.focus(window, cx);
        cx.notify();
    }

    /// Apply a stored rename to the workspace list the header chip reads.
    fn apply_workspace_rename(
        &mut self,
        entry: switchyard_api::WorkspaceEntry,
        cx: &mut Context<Self>,
    ) {
        if let Some(saved) = self
            .workspaces
            .as_mut()
            .and_then(|list| list.iter_mut().find(|saved| saved.id == entry.id))
        {
            saved.name = entry.name;
        }
        self.navigation_notice = Some("Workspace renamed.".into());
        cx.notify();
    }
}

impl RenameWorkspace {
    fn new(
        panel: WeakEntity<WorkbenchPanel>,
        workspace: WorkspaceId,
        current: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).default_value(current));
        let subscriptions = vec![cx.subscribe(&name, |this, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                this.error = None;
                cx.notify();
            }
        })];
        Self {
            panel,
            workspace,
            name,
            saving: false,
            error: None,
            _work: None,
            _subscriptions: subscriptions,
        }
    }

    fn blocked_reason(&self, cx: &App) -> Option<String> {
        if self.saving {
            return Some("Saving workspace name…".into());
        }
        if self.name.read(cx).value().trim().is_empty() {
            return Some("Enter a workspace name.".into());
        }
        None
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(error) = self.blocked_reason(cx) {
            self.error = Some(error);
            cx.notify();
            return;
        }
        let name = self.name.read(cx).value().trim().to_owned();
        let workspace = self.workspace.clone();
        let panel = self.panel.clone();
        let data_dir = workbench_data_dir();
        self.saving = true;
        self._work = Some(cx.spawn_in(window, async move |this, cx| {
            let result = crate::api::compat::blocking(move || {
                data_dir
                    .ok_or_else(|| "Cannot resolve the Workbench data directory.".to_string())
                    .and_then(|path| rename_workspace(&path, &workspace, &name))
            })
            .await;
            let _ = this.update_in(cx, |editor, window, cx| {
                editor.saving = false;
                editor._work = None;
                match result {
                    Ok(entry) => {
                        let _ =
                            panel.update(cx, |panel, cx| panel.apply_workspace_rename(entry, cx));
                        window.close_dialog(cx);
                    }
                    Err(error) => editor.error = Some(error),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }
}

impl Render for RenameWorkspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let message = self.error.clone().or_else(|| {
            if self.saving {
                None
            } else {
                self.blocked_reason(cx)
            }
        });
        div()
            .flex()
            .flex_col()
            .gap_2()
            .text_sm()
            .child("Workspace name")
            .child(Input::new(&self.name).disabled(self.saving))
            .when_some(message, |body, message| {
                body.child(div().text_color(cx.theme().danger).child(message))
            })
    }
}
