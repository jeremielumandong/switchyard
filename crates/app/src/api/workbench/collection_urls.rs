//! Preview and save literal URL replacements within a collection.

use gpui_kit::component::{
    Disableable, WindowExt,
    button::{Button, ButtonVariants},
    input::Input,
    scroll::Scrollbar,
};
use gpui_kit::{ListAlignment, ListState, Subscription, WeakEntity, div, list};
use switchyard_api::RequestUrlUpdate;
use switchyard_api::runtime::preview_request_url_updates;

use super::*;
use crate::api::compat::dialogs::{self, Dismiss};

pub(super) struct UrlReplacement {
    panel: WeakEntity<WorkbenchPanel>,
    workspace: WorkspaceId,
    collection_id: CollectionId,
    collection_name: String,
    find: Entity<InputState>,
    replacement: Entity<InputState>,
    updates: Vec<RequestUrlUpdate>,
    preview: ListState,
    error: Option<String>,
    saving_generation: Option<u64>,
    _subscriptions: Vec<Subscription>,
}

impl WorkbenchPanel {
    pub(super) fn open_url_replacement(
        &mut self,
        collection_id: CollectionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.storage_loading || self.url_replacement.is_some() {
            return;
        }
        let Some(collection) = self
            .workspace_data
            .as_ref()
            .and_then(|data| data.collection(&collection_id))
        else {
            return;
        };
        let collection_name = collection.name.clone();
        self.capture_active_request_tab(cx);
        let panel = cx.entity();
        let workspace = self.bound_workspace.clone();
        let editor = cx.new(|cx| {
            UrlReplacement::new(panel, workspace, collection_id, collection_name, window, cx)
        });
        self.url_replacement = Some(editor.clone());
        let owner = cx.entity().downgrade();
        let focus = editor.read(cx).find.focus_handle(cx);
        // The collection menu is about to disappear; restore focus to its rail.
        self.focus_handle.focus(window, cx);
        dialogs::open(window, cx, Dismiss::Explicit, move |dialog, window, cx| {
            let saving = editor.read(cx).saving_generation.is_some();
            let disabled = !editor.read(cx).is_ready(cx);
            let for_save = editor.clone();
            let for_enter = editor.clone();
            let for_cancel = editor.clone();
            let for_close = owner.clone();
            dialog
                .title("Replace request URLs")
                .w(dialogs::dialog_width(window, window.rem_size() * 44.))
                .max_h(dialogs::dialog_ceiling(window))
                .close_button(!saving)
                .keyboard(!saving)
                .on_ok(move |_, _, cx| {
                    for_enter.update(cx, |editor, cx| editor.save(cx));
                    false
                })
                .on_cancel(move |_, _, cx| for_cancel.read(cx).saving_generation.is_none())
                .on_close(move |_, _, cx| {
                    let _ = for_close.update(cx, |panel, cx| {
                        panel.url_replacement = None;
                        cx.notify();
                    });
                })
                .child(editor.clone())
                .footer(crate::api::compat::dialogs::footer_row({
                    let cancel = crate::api::compat::dialogs::cancel_button;
                    let for_save = for_save.clone();
                    vec![
                        if saving {
                            Button::new("cancel")
                                .label("Cancel")
                                .disabled(true)
                                .into_any_element()
                        } else {
                            cancel(window, cx)
                        },
                        Button::new("workbench-url-replacement-save")
                            .debug_selector(|| "workbench-url-replacement-save".into())
                            .primary()
                            .label(if saving {
                                "Saving URLs…"
                            } else {
                                "Save URLs"
                            })
                            .disabled(disabled)
                            .on_click(move |_, _, cx| {
                                for_save.update(cx, |editor, cx| editor.save(cx));
                            })
                            .into_any_element(),
                    ]
                }))
        });
        focus.focus(window, cx);
        cx.notify();
    }

    /// Update saved baselines and URLs together, preserving any newer editor text.
    fn synchronize_replaced_urls(
        &mut self,
        updates: &[RequestUrlUpdate],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.capture_active_request_tab(cx);
        for tab in &mut self.request_tabs {
            let Some(update) = updates
                .iter()
                .find(|update| tab.current_request_id.as_ref() == Some(&update.request_id))
            else {
                continue;
            };
            if let Some(definition) = &mut tab.current_definition {
                definition.url = update.url.clone();
            }
            if tab.input_values[2] == update.original_url {
                tab.input_values[2] = update.url.clone();
            }
        }
        if let Some(update) = updates
            .iter()
            .find(|update| self.current_request_id.as_ref() == Some(&update.request_id))
        {
            if let Some(definition) = &mut self.current_definition {
                definition.url = update.url.clone();
            }
            if self.url.read(cx).value().as_ref() == update.original_url {
                self.pending_editor_hydration_changes += 1;
                set_input(&self.url, &update.url, window, cx);
            }
        }
        self.navigation_notice = Some(format!("Saved {} request URLs.", updates.len()));
        cx.notify();
    }
}

impl UrlReplacement {
    fn new(
        panel: Entity<WorkbenchPanel>,
        workspace: WorkspaceId,
        collection_id: CollectionId,
        collection_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let find = cx.new(|cx| {
            InputState::new(window, cx).placeholder("{{basepath}} or https://old.example.com")
        });
        let replacement = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Leave blank to remove the matching text")
        });
        let mut subscriptions = Vec::new();
        for input in [&find, &replacement] {
            subscriptions.push(cx.subscribe(input, |this, _, event, cx| {
                if matches!(event, InputEvent::Change) && this.saving_generation.is_none() {
                    this.refresh_preview(cx);
                }
            }));
        }
        subscriptions.push(cx.observe_in(&panel, window, |this, panel, window, cx| {
            this.storage_changed(panel, window, cx);
        }));
        Self {
            panel: panel.downgrade(),
            workspace,
            collection_id,
            collection_name,
            find,
            replacement,
            updates: Vec::new(),
            preview: ListState::new(0, ListAlignment::Top, gpui_kit::px(0.)),
            error: None,
            saving_generation: None,
            _subscriptions: subscriptions,
        }
    }

    fn refresh_preview(&mut self, cx: &mut Context<Self>) {
        let find = self.find.read(cx).value();
        let replacement = self.replacement.read(cx).value();
        self.error = None;
        self.updates.clear();
        if !find.is_empty() {
            let result = self.panel.read_with(cx, |panel, _| {
                let data = panel
                    .workspace_data
                    .as_ref()
                    .ok_or("Workbench storage is unavailable")?;
                preview_request_url_updates(
                    &data.requests,
                    &self.collection_id,
                    &find,
                    &replacement,
                )
            });
            match result {
                Ok(Ok(updates)) => self.updates = updates,
                Ok(Err(error)) => self.error = Some(error),
                Err(_) => self.error = Some("The Workbench is no longer open.".into()),
            }
        }
        self.preview.reset(self.updates.len());
        cx.notify();
    }

    fn blocked_reason(&self, cx: &App) -> Option<String> {
        self.panel
            .read_with(cx, |panel, _| {
                if panel.bound_workspace != self.workspace
                    || current_workspace_id() != self.workspace
                {
                    return Some("Reopen Replace request URLs in the current project.".into());
                }
                if panel.storage_loading {
                    return Some("Wait for the current save to finish.".into());
                }
                if panel.send_state != SendState::Idle {
                    return Some(
                        "Wait for the current request or collection run to finish.".into(),
                    );
                }
                for (ix, tab) in panel.request_tabs.iter().enumerate() {
                    let (request_id, dirty) = if ix == panel.active_request_tab {
                        (panel.current_request_id.as_ref(), panel.dirty)
                    } else {
                        (tab.current_request_id.as_ref(), tab.dirty)
                    };
                    if dirty
                        && self
                            .updates
                            .iter()
                            .any(|update| request_id == Some(&update.request_id))
                    {
                        return Some(format!(
                            "Save or discard changes in “{}” before replacing its URL.",
                            tab.name()
                        ));
                    }
                }
                None
            })
            .unwrap_or_else(|_| Some("The Workbench is no longer open.".into()))
    }

    fn is_ready(&self, cx: &App) -> bool {
        self.saving_generation.is_none()
            && !self.updates.is_empty()
            && self.blocked_reason(cx).is_none()
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        if !self.is_ready(cx) {
            return;
        }
        let command = coordinator::StorageCommand::UpdateRequestUrls {
            collection_id: self.collection_id.clone(),
            updates: self.updates.clone(),
        };
        let result = self.panel.update(cx, |panel, cx| {
            panel
                .run_storage_command(command, cx)
                .then_some(panel.storage_generation)
        });
        match result {
            Ok(Some(generation)) => self.saving_generation = Some(generation),
            _ => self.error = Some("Request URLs could not be saved. Try again.".into()),
        }
        cx.notify();
    }

    fn storage_changed(
        &mut self,
        panel: Entity<WorkbenchPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(generation) = self.saving_generation else {
            cx.notify();
            return;
        };
        let owner = panel.read(cx);
        if owner.bound_workspace != self.workspace || owner.storage_generation != generation {
            self.saving_generation = None;
            self.error =
                Some("The project changed. Reopen Replace request URLs to continue.".into());
        } else if !owner.storage_loading {
            self.saving_generation = None;
            if let Some(error) = &owner.storage_error {
                self.error = Some(error.clone());
            } else {
                panel.update(cx, |panel, cx| {
                    panel.synchronize_replaced_urls(&self.updates, window, cx);
                    panel.url_replacement = None;
                });
                window.close_dialog(cx);
            }
        }
        cx.notify();
    }
}

impl Render for UrlReplacement {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let saving = self.saving_generation.is_some();
        let message = self.error.clone().or_else(|| self.blocked_reason(cx));
        let editor = cx.entity().downgrade();
        div()
            .id("workbench-url-replacement")
            .debug_selector(|| "workbench-url-replacement".into())
            .flex().flex_col().min_w_0().gap_3().text_sm()
            .child(div().child(format!("Collection: {} · includes all folders", self.collection_name)))
            .child(div().flex().flex_col().gap_1()
                .debug_selector(|| "workbench-url-replacement-find".into())
                .child("Find")
                .child(Input::new(&self.find).disabled(saving)))
            .child(div().flex().flex_col().gap_1()
                .debug_selector(|| "workbench-url-replacement-with".into())
                .child("Replace with")
                .child(Input::new(&self.replacement).disabled(saving))
                .child(div().text_color(cx.theme().muted_foreground)
                    .child("Leave blank to remove matching text. Relative URLs use the active environment’s base URL.")))
            .child(div().child(if self.find.read(cx).value().is_empty() {
                "Enter the exact text to find in saved request URLs.".into()
            } else if self.updates.is_empty() {
                "No request URLs will change.".into()
            } else {
                format!("{} request URLs will change. Matching is case-sensitive.", self.updates.len())
            }))
            .when(!self.updates.is_empty(), |body| body.child(
                div().relative().h_64().min_h_0()
                    .child(list(self.preview.clone(), move |ix, _, cx| {
                        let Some(editor) = editor.upgrade() else { return div().into_any_element(); };
                        let state = editor.read(cx);
                        let Some(update) = state.updates.get(ix) else { return div().into_any_element(); };
                        let name = state.panel.read_with(cx, |panel, _| {
                            panel.workspace_data.as_ref().and_then(|data| data.requests.iter()
                                .find(|request| request.id == update.request_id))
                                .map(|request| request.name.clone()).unwrap_or_else(|| "Request".into())
                        }).unwrap_or_else(|_| "Request".into());
                        div().id(SharedString::from(format!("url-replacement-{}", update.request_id)))
                            .flex().flex_col().min_w_0().gap_1().py_2().pr_4()
                            .border_b_1().border_color(cx.theme().border)
                            .child(div().font_weight(gpui_kit::FontWeight::MEDIUM).child(name))
                            .child(div().font_family(crate::api::compat::fonts::mono(cx)).text_color(cx.theme().muted_foreground)
                                .child(format!("Before: {}", update.original_url)))
                            .child(div().font_family(crate::api::compat::fonts::mono(cx)).child(format!("After: {}", update.url)))
                            .into_any_element()
                    }).size_full())
                    .child(Scrollbar::vertical(&self.preview))
            ))
            .when_some(message, |body, message| body.child(
                div().text_color(cx.theme().danger).child(message)
            ))
    }
}
