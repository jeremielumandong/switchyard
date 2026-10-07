//! Workspace globals use the same key/value editor and vault rules as other scopes.
use super::*;
use crate::api::compat::dialogs::{self, Dismiss};
use gpui_kit::component::input::TextareaState;
use gpui_kit::component::{
    Disableable, WindowExt,
    button::{Button, ButtonVariants},
    scroll::ScrollableElement,
};
use gpui_kit::{Task, WeakEntity, div};

struct GlobalsEditor {
    panel: WeakEntity<WorkbenchPanel>,
    workspace: WorkspaceId,
    original: Vec<Variable>,
    variables: Entity<TextareaState>,
    grid: Entity<entries::KvGrid>,
    loading: bool,
    saving: bool,
    error: Option<String>,
    work: Option<Task<()>>,
}

impl WorkbenchPanel {
    pub(super) fn open_globals(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(data) = &self.workspace_data else {
            return;
        };
        let store = data.store.clone();
        let workspace = self.bound_workspace.clone();
        let panel = cx.entity().downgrade();
        let editor = cx.new(|cx| {
            let variables = cx.new(|cx| TextareaState::new(window, cx).auto_grow(3, 12));
            let grid = cx.new(|_| entries::KvGrid::new(variables.clone(), '=', "workbench-globals", "Available in every collection. Environment, collection, data and local values override globals. Use secret:name for credentials.", ("Variable", "Value"), true));
            GlobalsEditor { panel, workspace: workspace.clone(), original: Vec::new(), variables, grid, loading: true, saving: false, error: None, work: None }
        });
        editor.update(cx, |editor, cx| {
            editor.work = Some(cx.spawn_in(window, async move |this, cx| {
                let result = crate::api::compat::blocking(move || {
                    store
                        .global_variables(&workspace)
                        .map_err(|e| e.to_string())
                })
                .await;
                let _ = this.update_in(cx, |editor, window, cx| {
                    match result {
                        Ok(variables) => {
                            editor.variables.update(cx, |input, cx| {
                                input.set_value(format_variables(&variables), window, cx)
                            });
                            editor.original = variables;
                            editor.loading = false;
                        }
                        Err(error) => editor.error = Some(error),
                    }
                    cx.notify();
                });
            }));
        });
        self.focus_handle.focus(window, cx);
        dialogs::open(window, cx, Dismiss::Explicit, move |dialog, window, cx| {
            let saving = editor.read(cx).saving;
            let loading = editor.read(cx).loading;
            let enter = editor.clone();
            let save = editor.clone();
            let cancel_editor = editor.clone();
            dialog
                .title("Workspace globals")
                .w(dialogs::dialog_width(window, window.rem_size() * 44.))
                .max_h(dialogs::dialog_ceiling(window))
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
                    let save = save.clone();
                    vec![
                        if saving {
                            Button::new("cancel")
                                .label("Cancel")
                                .disabled(true)
                                .into_any_element()
                        } else {
                            cancel(window, cx)
                        },
                        Button::new("workbench-globals-save")
                            .debug_selector(|| "workbench-globals-save".into())
                            .primary()
                            .label(if saving { "Saving?" } else { "Save globals" })
                            .disabled(saving || loading)
                            .on_click(move |_, window, cx| {
                                save.update(cx, |editor, cx| editor.save(window, cx));
                            })
                            .into_any_element(),
                    ]
                }))
        });
    }
}

impl GlobalsEditor {
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading || self.saving {
            return;
        }
        let (mut variables, secrets) =
            match draft::parse_session_variables(&self.variables.read(cx).value(), "globals") {
                Ok(parsed) => parsed,
                Err(error) => {
                    self.error = Some(error);
                    cx.notify();
                    return;
                }
            };
        super::scope_editor::preserve_variable_metadata(&self.original, &mut variables, &secrets);
        let prepared = self.panel.update(cx, |panel, cx| {
            if panel.bound_workspace != self.workspace || current_workspace_id() != self.workspace {
                return Err("The workspace changed. Reopen globals.".to_string());
            }
            if panel.storage_loading
                || panel.send_state != SendState::Idle
                || panel.active_run.is_some()
            {
                return Err("Wait for the current request, run or save to finish.".to_string());
            }
            let store = panel
                .workspace_data
                .as_ref()
                .ok_or("Workbench storage is unavailable")?
                .store
                .clone();
            panel.storage_generation = panel.storage_generation.wrapping_add(1);
            panel.storage_loading = true;
            cx.notify();
            Ok((store, panel.secret_store.clone(), panel.storage_generation))
        });
        let (store, secret_store, generation) = match prepared {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
            Err(_) => {
                self.error = Some("The Workbench is no longer open.".into());
                cx.notify();
                return;
            }
        };
        self.saving = true;
        self.error = None;
        let workspace = self.workspace.clone();
        let original = self.original.clone();
        let panel = self.panel.clone();
        self.work = Some(cx.spawn_in(window, async move |this, cx| {
            let completion_workspace = workspace.clone();
            let result = crate::api::compat::blocking(move || {
                persist_globals(
                    store.as_ref(),
                    &workspace,
                    &original,
                    &variables,
                    &secrets,
                    secret_store.as_ref(),
                )
            })
            .await;
            let _ = this.update_in(cx, |editor, window, cx| {
                editor.saving = false;
                let applied = panel.update(cx, |panel, cx| {
                    if panel.bound_workspace != completion_workspace
                        || panel.storage_generation != generation
                    {
                        return Err("The workspace changed. Reopen globals.".to_string());
                    }
                    panel.storage_loading = false;
                    if result.is_ok() {
                        panel.navigation_notice = Some("Workspace globals saved.".into());
                    }
                    cx.notify();
                    result
                });
                match applied {
                    Ok(Ok(())) => window.close_dialog(cx),
                    Ok(Err(error)) => editor.error = Some(error),
                    Err(_) => editor.error = Some("The Workbench is no longer open.".into()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }
}

fn persist_globals(
    store: &switchyard_api::WorkbenchStore,
    workspace: &WorkspaceId,
    original: &[Variable],
    variables: &[Variable],
    secrets: &draft::DraftSecrets,
    secret_store: &dyn switchyard_api::SecretStore,
) -> Result<(), String> {
    if store
        .global_variables(workspace)
        .map_err(|e| e.to_string())?
        != original
    {
        return Err("Globals changed since opening. Reopen them to edit the latest values.".into());
    }
    secrets.persist(secret_store, workspace)?;
    store
        .save_global_variables(workspace, variables)
        .map_err(|e| e.to_string())
}

impl Render for GlobalsEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_3()
            .min_h_0()
            .overflow_y_scrollbar()
            .when(self.loading, |el| el.child("Loading globals?"))
            .when(!self.loading && !self.saving, |el| {
                el.child(self.grid.clone())
            })
            .when(self.saving, |el| el.child("Saving globals?"))
            .when_some(self.error.clone(), |el, error| {
                el.child(div().text_sm().text_color(cx.theme().danger).child(error))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_api::{MemorySecretStore, SecretStore, WorkbenchStore};

    #[test]
    fn globals_vault_roundtrip_and_stale_save_preserve_saved_credentials() {
        let path =
            std::env::temp_dir().join(format!("agentops-globals-editor-{}", CollectionId::new()));
        let store = WorkbenchStore::open(&path).unwrap();
        let workspace = WorkspaceId::new("globals-editor").unwrap();
        let vault = MemorySecretStore::new();
        let (variables, secrets) =
            draft::parse_session_variables("secret:token=first-secret\nbase_path=/api", "globals")
                .unwrap();
        persist_globals(&store, &workspace, &[], &variables, &secrets, &vault).unwrap();
        let saved = store.global_variables(&workspace).unwrap();
        assert!(
            !serde_json::to_string(&saved)
                .unwrap()
                .contains("first-secret")
        );
        let VariableValue::Secret(reference) = &saved[0].value else {
            panic!("must be vaulted")
        };
        assert_eq!(
            vault
                .get_secret(&workspace, reference)
                .unwrap()
                .expose_secret(),
            "first-secret"
        );
        let (replacement, secrets) =
            draft::parse_session_variables("secret:token=second-secret", "globals").unwrap();
        assert!(persist_globals(&store, &workspace, &[], &replacement, &secrets, &vault).is_err());
        assert_eq!(
            vault
                .get_secret(&workspace, reference)
                .unwrap()
                .expose_secret(),
            "first-secret"
        );
        let (mut masked, secrets) =
            draft::parse_session_variables(&format_variables(&saved), "globals").unwrap();
        super::super::scope_editor::preserve_variable_metadata(&saved, &mut masked, &secrets);
        persist_globals(&store, &workspace, &saved, &masked, &secrets, &vault).unwrap();
        assert_eq!(store.global_variables(&workspace).unwrap(), saved);
    }
}
