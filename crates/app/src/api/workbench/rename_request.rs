//! Metadata-only request renaming, independent of dirty request editors.

use super::*;
use crate::api::compat::dialogs::{self, Dismiss};
use gpui_kit::component::{
    Disableable, WindowExt,
    button::{Button, ButtonVariants},
    input::Input,
};
use gpui_kit::{Subscription, WeakEntity, div};

struct RenameRequest {
    panel: WeakEntity<WorkbenchPanel>,
    workspace: WorkspaceId,
    request: SavedRequest,
    name: Entity<InputState>,
    saving: bool,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl WorkbenchPanel {
    pub(super) fn open_rename_request(
        &mut self,
        id: RequestId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.storage_loading {
            return;
        }
        let Some(request) = self
            .workspace_data
            .as_ref()
            .and_then(|data| data.requests.iter().find(|r| r.id == id))
            .cloned()
        else {
            return;
        };
        let panel = cx.entity();
        let workspace = self.bound_workspace.clone();
        let editor = cx.new(|cx| RenameRequest::new(panel, workspace, request, window, cx));
        let focus = editor.read(cx).name.focus_handle(cx);
        self.focus_handle.focus(window, cx);
        dialogs::open(window, cx, Dismiss::Explicit, move |dialog, window, cx| {
            let saving = editor.read(cx).saving;
            let disabled = editor.read(cx).blocked_reason(cx).is_some();
            let enter = editor.clone();
            let cancel_editor = editor.clone();
            let save = editor.clone();
            dialog
                .title("Rename request")
                .w(dialogs::dialog_width(window, window.rem_size() * 28.))
                .close_button(!saving)
                .keyboard(!saving)
                .on_ok(move |_, window, cx| {
                    enter.update(cx, |editor, cx| editor.save(window, cx));
                    false
                })
                .on_cancel(move |_, _, cx| !cancel_editor.read(cx).saving)
                .child(editor.clone())
                .footer(crate::api::compat::dialogs::footer_row({
                    let cancel = crate::api::compat::dialogs::cancel_button;
                    vec![
                        if saving {
                            Button::new("cancel")
                                .label("Cancel")
                                .disabled(true)
                                .into_any_element()
                        } else {
                            cancel(window, cx)
                        },
                        Button::new("workbench-rename-request-save")
                            .debug_selector(|| "workbench-rename-request-save".into())
                            .primary()
                            .label(if saving { "Renaming…" } else { "Rename" })
                            .disabled(disabled)
                            .on_click({
                                let save = save.clone();
                                move |_, window, cx| {
                                    save.update(cx, |editor, cx| editor.save(window, cx))
                                }
                            })
                            .into_any_element(),
                    ]
                }))
        });
        focus.focus(window, cx);
        cx.notify();
    }

    fn synchronize_request_name(
        &mut self,
        request: &SavedRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.capture_active_request_tab(cx);
        if let Some(saved) = self.workspace_data.as_mut().and_then(|data| {
            data.requests
                .iter_mut()
                .find(|r| r.id == request.id && r.collection_id == request.collection_id)
        }) {
            saved.name = request.name.clone();
        }
        for tab in &mut self.request_tabs {
            if tab.current_request_id.as_ref() == Some(&request.id) {
                if let Some(saved) = &mut tab.current_definition {
                    saved.name = request.name.clone();
                }
                tab.input_values[1] = request.name.clone();
            }
        }
        if self.current_request_id.as_ref() == Some(&request.id) {
            if let Some(saved) = &mut self.current_definition {
                saved.name = request.name.clone();
            }
            if self.request_name.read(cx).value().as_str() != request.name {
                self.pending_editor_hydration_changes += 1;
                set_input(&self.request_name, &request.name, window, cx);
            }
        }
        self.navigation_notice = Some("Request renamed.".into());
        cx.notify();
    }
}

impl RenameRequest {
    fn new(
        panel: Entity<WorkbenchPanel>,
        workspace: WorkspaceId,
        request: SavedRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).default_value(request.name.clone()));
        let subscriptions = vec![
            cx.subscribe(&name, |this, _, event, cx| {
                if matches!(event, InputEvent::Change) {
                    this.error = None;
                    cx.notify();
                }
            }),
            cx.observe(&panel, |editor, panel, cx| {
                if panel.read(cx).bound_workspace != editor.workspace {
                    editor.saving = false;
                    editor.error = Some("The workspace changed. Reopen Rename request.".into());
                }
                cx.notify();
            }),
        ];
        Self {
            panel: panel.downgrade(),
            workspace,
            request,
            name,
            saving: false,
            error: None,
            _subscriptions: subscriptions,
        }
    }

    fn blocked_reason(&self, cx: &App) -> Option<String> {
        if self.saving {
            return Some("Saving request name…".into());
        }
        if self.name.read(cx).value().trim().is_empty() {
            return Some("Enter a request name.".into());
        }
        self.panel
            .read_with(cx, |panel, _| {
                if panel.bound_workspace != self.workspace
                    || current_workspace_id() != self.workspace
                {
                    return Some("The workspace changed. Reopen Rename request.".into());
                }
                if panel.storage_loading {
                    return Some("Wait for the current save to finish.".into());
                }
                if !panel.workspace_data.as_ref().is_some_and(|data| {
                    data.requests.iter().any(|r| {
                        r.id == self.request.id
                            && r.collection_id == self.request.collection_id
                            && r.name == self.request.name
                    })
                }) {
                    return Some("The request changed. Reopen Rename request.".into());
                }
                None
            })
            .unwrap_or_else(|_| Some("The Workbench is no longer open.".into()))
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(error) = self.blocked_reason(cx) {
            self.error = Some(error);
            cx.notify();
            return;
        }
        let name = self.name.read(cx).value().trim().to_owned();
        let request = self.request.clone();
        let workspace = self.workspace.clone();
        let editor = cx.entity().downgrade();
        let started = self
            .panel
            .update(cx, |panel, cx| {
                let Some(data) = panel.workspace_data.as_ref() else {
                    return false;
                };
                let store = data.store.clone();
                panel.storage_generation = panel.storage_generation.wrapping_add(1);
                let generation = panel.storage_generation;
                panel.storage_loading = true;
                panel.storage_error = None;
                panel._storage_work = Some(cx.spawn_in(window, async move |this, cx| {
                    let expected_workspace = workspace.clone();
                    let result = crate::api::compat::blocking(move || {
                        store
                            .rename_request(
                                &workspace,
                                &request.collection_id,
                                &request.id,
                                &request.name,
                                &name,
                            )
                            .map_err(|e| e.to_string())
                    })
                    .await;
                    let _ = this.update_in(cx, |panel, window, cx| {
                        if panel.bound_workspace != expected_workspace
                            || panel.storage_generation != generation
                        {
                            return;
                        }
                        panel.storage_loading = false;
                        panel._storage_work = None;
                        let result = result.and_then(|request| {
                            if current_workspace_id() != expected_workspace
                                || !panel.workspace_data.as_ref().is_some_and(|data| {
                                    data.requests.iter().any(|saved| {
                                        saved.id == request.id
                                            && saved.collection_id == request.collection_id
                                    })
                                })
                            {
                                Err("The workspace or request changed. Reopen Rename request."
                                    .into())
                            } else {
                                Ok(request)
                            }
                        });
                        match result {
                            Ok(request) => {
                                panel.synchronize_request_name(&request, window, cx);
                                window.close_dialog(cx);
                            }
                            Err(error) => {
                                panel.storage_error = Some(error.clone());
                                let _ = editor.update(cx, |editor, cx| {
                                    editor.saving = false;
                                    editor.error = Some(error);
                                    cx.notify();
                                });
                            }
                        }
                        cx.notify();
                    });
                }));
                cx.notify();
                true
            })
            .unwrap_or(false);
        self.saving = started;
        cx.notify();
    }
}

impl Render for RenameRequest {
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
            .child("Request name")
            .child(Input::new(&self.name).disabled(self.saving))
            .when_some(message, |body, message| {
                body.child(div().text_color(cx.theme().danger).child(message))
            })
    }
}
