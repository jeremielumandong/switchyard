//! Unsaved files: the Save / Discard / Cancel dialog shown when editor tabs with unsaved
//! changes are closed, alone, in a group, or with the window.

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::{
    Context, Entity, EntityId, IntoElement as _, ParentElement as _, Styled as _, Window, div,
};

use crate::api::compat::dialogs::{self, Dismiss};
use crate::editor_tab::EditorTab;
use crate::workspace::{Tab, Workspace};

/// What happens once the unsaved files are saved or discarded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AfterUnsaved {
    /// Close those tabs.
    CloseTabs,
    /// Close those tabs, then the window.
    CloseWindow,
}

/// Saves the workspace waits on before closing tabs or the window.
#[derive(Default)]
pub struct UnsavedState {
    /// Editor tabs to close once their save lands.
    close_after_save: Vec<EntityId>,
    /// Close the window once those saves land.
    close_window_after_save: bool,
    /// The user chose to close the window; don't ask again.
    window_close_confirmed: bool,
}

/// The dialog's message: what is unsaved and what can't be saved.
pub fn message(titles: &[String], unsavable: usize) -> String {
    let mut text = match titles {
        [one] => format!("“{one}” has unsaved changes."),
        many => format!(
            "{} files have unsaved changes: {}.",
            many.len(),
            many.iter()
                .map(|t| format!("“{t}”"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    if unsavable > 0 {
        text.push_str(if titles.len() == 1 {
            " It can't be saved right now (still loading, saving, or changed on the server)."
        } else {
            " Some can't be saved right now (still loading, saving, or changed on the server)."
        });
    }
    text
}

impl Workspace {
    /// Ask whether to save or discard `editors` (all with unsaved changes), then do `then`.
    /// Save is offered only when every one of them can be saved now.
    pub(crate) fn confirm_unsaved(
        &mut self,
        editors: Vec<Entity<EditorTab>>,
        then: AfterUnsaved,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if editors.is_empty() {
            return;
        }
        let titles: Vec<String> = editors.iter().map(|e| e.read(cx).title.clone()).collect();
        let unsavable = editors.iter().filter(|e| !e.read(cx).can_save()).count();
        let can_save = unsavable == 0;
        let text = message(&titles, unsavable);
        let ids: Vec<EntityId> = editors.iter().map(|e| e.entity_id()).collect();
        let ws = cx.entity().downgrade();
        let title = if then == AfterUnsaved::CloseWindow {
            "Close the window?"
        } else {
            "Unsaved changes"
        };
        dialogs::open(window, cx, Dismiss::Explicit, move |dialog, window, cx| {
            let mut buttons = vec![dialogs::cancel_button(window, cx)];
            let (discard_ws, discard_ids) = (ws.clone(), ids.clone());
            buttons.push(
                Button::new("unsaved-discard")
                    .danger()
                    .label(if ids.len() == 1 {
                        "Discard"
                    } else {
                        "Discard all"
                    })
                    .on_click(move |_, window, cx| {
                        window.close_dialog(cx);
                        let ids = discard_ids.clone();
                        let _ = discard_ws
                            .update(cx, |w, cx| w.finish_unsaved(ids, false, then, window, cx));
                    })
                    .into_any_element(),
            );
            if can_save {
                let (save_ws, save_ids) = (ws.clone(), ids.clone());
                buttons.push(
                    Button::new("unsaved-save")
                        .primary()
                        .label(if ids.len() == 1 { "Save" } else { "Save all" })
                        .on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            let ids = save_ids.clone();
                            let _ = save_ws
                                .update(cx, |w, cx| w.finish_unsaved(ids, true, then, window, cx));
                        })
                        .into_any_element(),
                );
            }
            dialog
                .title(title)
                .w(dialogs::dialog_width(window, window.rem_size() * 30.))
                .child(div().whitespace_normal().child(text.clone()))
                .footer(dialogs::footer_row(buttons))
        });
        cx.notify();
    }

    fn editor_tab(&self, id: EntityId) -> Option<(usize, Entity<EditorTab>)> {
        self.tabs.iter().enumerate().find_map(|(i, t)| match t {
            Tab::Editor(e) if e.entity_id() == id => Some((i, e.clone())),
            _ => None,
        })
    }

    /// The dialog's answer: save (closing each tab once its save lands) or discard.
    fn finish_unsaved(
        &mut self,
        ids: Vec<EntityId>,
        save: bool,
        then: AfterUnsaved,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if save {
            for id in ids {
                if let Some((_, e)) = self.editor_tab(id) {
                    e.update(cx, |e, cx| e.save_file(cx));
                    self.unsaved.close_after_save.push(id);
                }
            }
            self.unsaved.close_window_after_save |= then == AfterUnsaved::CloseWindow;
            self.finish_saved_closes(window, cx);
            return;
        }
        if then == AfterUnsaved::CloseWindow {
            self.unsaved.window_close_confirmed = true;
            window.remove_window();
            return;
        }
        for id in ids {
            if let Some((ix, _)) = self.editor_tab(id) {
                self.close_tab_now(ix, cx);
            }
        }
        cx.notify();
    }

    /// After a file save: close the tabs (and the window) that were waiting for it. A
    /// failed save keeps its tab open, with the editor's error banner.
    pub(crate) fn finish_saved_closes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.unsaved.close_after_save.is_empty() {
            return;
        }
        let mut failed = Vec::new();
        let mut i = 0;
        while i < self.unsaved.close_after_save.len() {
            let id = self.unsaved.close_after_save[i];
            let Some((ix, e)) = self.editor_tab(id) else {
                self.unsaved.close_after_save.remove(i);
                continue;
            };
            match e.read(cx).save_outcome() {
                None => i += 1,
                Some(true) => {
                    self.unsaved.close_after_save.remove(i);
                    self.close_tab_now(ix, cx);
                }
                Some(false) => {
                    self.unsaved.close_after_save.remove(i);
                    failed.push(e.read(cx).title.clone());
                }
            }
        }
        if !failed.is_empty() {
            self.unsaved.close_window_after_save = false;
            self.toast(
                format!("Could not save {} · the tab stays open", failed.join(", ")),
                cx,
            );
        } else if self.unsaved.close_after_save.is_empty() && self.unsaved.close_window_after_save {
            self.unsaved.close_window_after_save = false;
            self.unsaved.window_close_confirmed = true;
            window.remove_window();
        }
        cx.notify();
    }

    /// The window is about to close: `true` lets it, `false` keeps it open while the
    /// unsaved-changes dialog asks.
    pub(crate) fn request_close_window(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.unsaved.window_close_confirmed {
            return true;
        }
        let dirty: Vec<Entity<EditorTab>> = self
            .tabs
            .iter()
            .filter_map(|t| match t {
                Tab::Editor(e) if e.read(cx).dirty => Some(e.clone()),
                _ => None,
            })
            .collect();
        if dirty.is_empty() {
            return true;
        }
        if !window.has_active_dialog(cx) {
            self.confirm_unsaved(dirty, AfterUnsaved::CloseWindow, window, cx);
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_names_the_files() {
        assert_eq!(
            message(&["a.sql · web".into()], 0),
            "“a.sql · web” has unsaved changes."
        );
        let m = message(&["a".into(), "b".into()], 1);
        assert!(m.starts_with("2 files have unsaved changes: “a”, “b”."));
        assert!(m.contains("Some can't be saved"));
    }
}
