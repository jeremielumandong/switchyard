//! Edit inherited collection and folder configuration without touching requests.

use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::{
    Disableable, Selectable, Sizable, WindowExt,
    button::{Button, ButtonVariants},
    scroll::ScrollableElement,
};
use gpui_kit::{Task, WeakEntity, div};

use super::*;
use crate::api::compat::dialogs::{self, Dismiss};

#[derive(Clone, PartialEq)]
enum Scope {
    Collection(Collection),
    Folder(Folder),
}

impl Scope {
    fn name(&self) -> &str {
        match self {
            Self::Collection(scope) => &scope.name,
            Self::Folder(scope) => &scope.name,
        }
    }

    fn namespace(&self) -> String {
        match self {
            Self::Collection(scope) => format!("collection.{}", scope.id.as_str()),
            Self::Folder(scope) => format!("folder.{}", scope.id.as_str()),
        }
    }

    fn auth(&self) -> &AuthConfig {
        match self {
            Self::Collection(scope) => &scope.auth,
            Self::Folder(scope) => &scope.auth,
        }
    }

    fn variables(&self) -> &[Variable] {
        match self {
            Self::Collection(scope) => &scope.variables,
            Self::Folder(scope) => &scope.variables,
        }
    }

    fn scripts(&self) -> &Scripts {
        match self {
            Self::Collection(scope) => &scope.scripts,
            Self::Folder(scope) => &scope.scripts,
        }
    }

    fn current(&self, data: &persistence::WorkspaceData) -> Option<Self> {
        match self {
            Self::Collection(scope) => data
                .collections
                .iter()
                .find(|saved| saved.id == scope.id)
                .cloned()
                .map(Self::Collection),
            Self::Folder(scope) => data
                .folders
                .iter()
                .find(|saved| saved.id == scope.id)
                .cloned()
                .map(Self::Folder),
        }
    }

    fn replace(&mut self, variables: Vec<Variable>, auth: AuthConfig, scripts: Scripts) {
        match self {
            Self::Collection(scope) => {
                scope.variables = variables;
                scope.auth = auth;
                scope.scripts = scripts;
            }
            Self::Folder(scope) => {
                scope.variables = variables;
                scope.auth = auth;
                scope.scripts = scripts;
            }
        }
    }

    fn command(self) -> coordinator::StorageCommand {
        match self {
            Self::Collection(scope) => coordinator::StorageCommand::UpsertCollection(scope),
            Self::Folder(scope) => coordinator::StorageCommand::UpsertFolder {
                collection: None,
                folder: scope,
            },
        }
    }
}

struct ScopeEditor {
    panel: WeakEntity<WorkbenchPanel>,
    workspace: WorkspaceId,
    original: Scope,
    variables: Entity<TextareaState>,
    variables_grid: Entity<entries::KvGrid>,
    auth: Entity<TextareaState>,
    auth_form: Entity<entries::AuthForm>,
    auth_mode: draft::AuthMode,
    pre: Entity<TextareaState>,
    post: Entity<TextareaState>,
    section: usize,
    error: Option<String>,
    saving: bool,
    _work: Option<Task<()>>,
}

impl WorkbenchPanel {
    pub(super) fn open_collection_settings(
        &mut self,
        id: CollectionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let scope = self
            .workspace_data
            .as_ref()
            .and_then(|data| data.collections.iter().find(|scope| scope.id == id))
            .cloned()
            .map(Scope::Collection);
        if let Some(scope) = scope {
            self.open_scope_settings(scope, window, cx);
        }
    }

    pub(super) fn open_folder_settings(
        &mut self,
        id: FolderId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let scope = self
            .workspace_data
            .as_ref()
            .and_then(|data| data.folders.iter().find(|scope| scope.id == id))
            .cloned()
            .map(Scope::Folder);
        if let Some(scope) = scope {
            self.open_scope_settings(scope, window, cx);
        }
    }

    fn open_scope_settings(&mut self, scope: Scope, window: &mut Window, cx: &mut Context<Self>) {
        if self.storage_loading {
            return;
        }
        let title = format!(
            "{} settings · {}",
            if matches!(scope, Scope::Collection(_)) {
                "Collection"
            } else {
                "Folder"
            },
            scope.name()
        );
        let panel = cx.entity().downgrade();
        let workspace = self.bound_workspace.clone();
        let editor = cx.new(|cx| ScopeEditor::new(panel, workspace, scope, window, cx));
        self.focus_handle.focus(window, cx);
        dialogs::open(window, cx, Dismiss::Explicit, move |dialog, window, cx| {
            let saving = editor.read(cx).saving;
            let for_save = editor.clone();
            let for_enter = editor.clone();
            let for_cancel = editor.clone();
            dialog
                .title(title.clone())
                .w(dialogs::dialog_width(window, window.rem_size() * 44.))
                .max_h(dialogs::dialog_ceiling(window))
                .close_button(!saving)
                .keyboard(!saving)
                .on_ok(move |_, window, cx| {
                    for_enter.update(cx, |editor, cx| editor.save(window, cx));
                    false
                })
                .on_cancel(move |_, _, cx| !for_cancel.read(cx).saving)
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
                        Button::new("workbench-scope-save")
                            .debug_selector(|| "workbench-scope-save".into())
                            .primary()
                            .label(if saving { "Saving…" } else { "Save settings" })
                            .disabled(saving)
                            .on_click(move |_, window, cx| {
                                for_save.update(cx, |editor, cx| editor.save(window, cx));
                            })
                            .into_any_element(),
                    ]
                }))
        });
    }
}

impl ScopeEditor {
    fn new(
        panel: WeakEntity<WorkbenchPanel>,
        workspace: WorkspaceId,
        original: Scope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let variables = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 10)
                .default_value(format_variables(original.variables()))
        });
        let variables_grid = cx.new(|_| entries::KvGrid::new(variables.clone(), '=', "workbench-scope-vars", "Inherited by requests. Use secret:name for a credential or {{vault.name}} for a named secret.", ("Variable", "Value"), true));
        let auth = cx.new(|cx| {
            TextareaState::new(window, cx).default_value(draft::format_auth(original.auth()))
        });
        let auth_mode = auth_mode(original.auth());
        let auth_form = cx.new(|cx| {
            let mut form = entries::AuthForm::new(auth.clone(), "workbench-scope-auth");
            form.set_mode(auth_mode, true, cx);
            form
        });
        let pre = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(5, 15)
                .default_value(original.scripts().pre_request.clone())
                .placeholder("Pre-request script")
        });
        let post = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(5, 15)
                .default_value(original.scripts().tests.clone())
                .placeholder("pm.test('status', () => pm.expect(pm.response.code).to.equal(200));")
        });
        Self {
            panel,
            workspace,
            original,
            variables,
            variables_grid,
            auth,
            auth_form,
            auth_mode,
            pre,
            post,
            section: 0,
            error: None,
            saving: false,
            _work: None,
        }
    }

    fn edited(&self, cx: &App) -> Result<(Scope, draft::DraftSecrets), String> {
        let (mut variables, mut secrets) = draft::parse_session_variables(
            &self.variables.read(cx).value(),
            &self.original.namespace(),
        )?;
        preserve_variable_metadata(self.original.variables(), &mut variables, &secrets);
        let (mut auth, auth_secrets) = draft::parse_auth(
            self.auth_mode,
            &self.auth.read(cx).value(),
            &self.original.namespace(),
        )?;
        secrets.merge(auth_secrets);
        preserve_saved_secret_refs(self.original.auth(), &mut auth, &secrets);
        let mut edited = self.original.clone();
        edited.replace(
            variables,
            auth,
            Scripts {
                pre_request: self.pre.read(cx).value().to_string(),
                tests: self.post.read(cx).value().to_string(),
            },
        );
        Ok((edited, secrets))
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let (edited, secrets) = match self.edited(cx) {
            Ok(value) => value,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        let prepared = self.panel.update(cx, |panel, cx| {
            if panel.bound_workspace != self.workspace || current_workspace_id() != self.workspace {
                return Err("The workspace changed. Reopen these settings.".to_string());
            }
            if panel.storage_loading || panel.send_state != SendState::Idle {
                return Err("Wait for the current save or request to finish.".to_string());
            }
            let data = panel
                .workspace_data
                .as_ref()
                .ok_or("Workbench storage is unavailable")?;
            if self.original.current(data).as_ref() != Some(&self.original) {
                return Err(
                    "These settings changed since opening. Reopen them to edit the latest version."
                        .into(),
                );
            }
            let store = data.store.clone();
            panel.storage_generation = panel.storage_generation.wrapping_add(1);
            panel.storage_loading = true;
            panel.storage_error = None;
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
        self._work = Some(cx.spawn_in(window, async move |this, cx| {
            let completion_workspace = workspace.clone();
            let result = crate::api::compat::blocking(move || {
                persist_scope(store, workspace, original, edited, secrets, secret_store)
            })
            .await;
            let _ = this.update_in(cx, |editor, window, cx| {
                editor.saving = false;
                let applied = panel.update(cx, |panel, cx| {
                    if panel.bound_workspace != completion_workspace
                        || panel.storage_generation != generation
                    {
                        return Err("The workspace changed. Reopen these settings.".to_string());
                    }
                    panel.storage_loading = false;
                    match result {
                        Ok(data) => {
                            panel.workspace_data = Some(data);
                            panel.storage_error = None;
                        }
                        Err(error) => {
                            panel.storage_error = Some(error.clone());
                            cx.notify();
                            return Err(error);
                        }
                    }
                    cx.notify();
                    Ok(())
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

/// Runs off the frame thread; reading the vault here also preserves login caches.
fn persist_scope(
    store: Arc<switchyard_api::WorkbenchStore>,
    workspace: WorkspaceId,
    original: Scope,
    mut edited: Scope,
    secrets: draft::DraftSecrets,
    secret_store: Arc<dyn switchyard_api::SecretStore>,
) -> Result<persistence::WorkspaceData, String> {
    // Reject stale editors before overwriting any existing credential values.
    let current = persistence::WorkspaceData::hydrate(store.clone(), workspace.clone())?;
    if original.current(&current).as_ref() != Some(&original) {
        return Err(
            "These settings changed since opening. Reopen them to edit the latest version.".into(),
        );
    }
    let mut auth = edited.auth().clone();
    preserve_saved_auth(
        original.auth(),
        &mut auth,
        &secrets,
        &draft::DraftSecrets::with_store(secret_store.clone(), workspace.clone()),
    );
    edited.replace(edited.variables().to_vec(), auth, edited.scripts().clone());
    secrets.persist(secret_store.as_ref(), &workspace)?;
    coordinator::execute(store, workspace, edited.command())
}

/// Masked imported references and descriptions must survive a settings-only edit.
pub(super) fn preserve_variable_metadata(
    original: &[Variable],
    variables: &mut [Variable],
    secrets: &draft::DraftSecrets,
) {
    let mut remaining = original.iter().collect::<Vec<_>>();
    for variable in variables {
        let Some(ix) = remaining.iter().position(|saved| saved.key == variable.key) else {
            continue;
        };
        let saved = remaining.remove(ix);
        variable.id = saved.id.clone();
        variable.description = saved.description.clone();
        if let VariableValue::Secret(reference) = &variable.value
            && !secrets.has_value(reference)
            && switchyard_api::vault::vault_reference_expression(reference).is_none()
            && matches!(
                saved.value,
                VariableValue::Secret(_) | VariableValue::MissingSecret(_)
            )
        {
            variable.value = saved.value.clone();
        }
    }
}

impl Render for ScopeEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let saving = self.saving;
        let content = if saving {
            div().child("Saving settings…").into_any_element()
        } else {
            match self.section {
            0 => self.variables_grid.clone().into_any_element(),
            1 => div().flex().flex_col().gap_3()
                .child(div().flex().flex_wrap().gap_2().children(draft::AuthMode::ALL.into_iter().map(|mode| {
                    Button::new(SharedString::from(format!("workbench-scope-auth-{}", mode.label()))).small().label(mode.label()).selected(mode == self.auth_mode).disabled(saving)
                        .on_click(cx.listener(move |this, _, _, cx| { this.auth_mode = mode; this.auth_form.update(cx, |form, cx| form.set_mode(mode, true, cx)); cx.notify(); }))
                })))
                .child(div().text_color(cx.theme().muted_foreground).child("Requests using Inherit use this authentication. Leave saved secret fields blank to keep them."))
                .child(self.auth_form.clone()).into_any_element(),
            _ => div().flex().flex_col().gap_3()
                .child(div().text_color(cx.theme().muted_foreground).child("Scripts run for requests in this scope, before nested folder and request scripts."))
                .child("Pre-request script").child(Textarea::new(&self.pre).disabled(saving))
                .child("Post-response script").child(Textarea::new(&self.post).disabled(saving)).into_any_element(),
        }
        };
        div()
            .id("workbench-scope-editor")
            .debug_selector(|| "workbench-scope-editor".into())
            .flex()
            .flex_col()
            .gap_3()
            .min_w_0()
            .text_sm()
            .child(
                div().flex().gap_2().children(
                    ["Variables", "Authentication", "Scripts"]
                        .into_iter()
                        .enumerate()
                        .map(|(ix, label)| {
                            Button::new(SharedString::from(format!("workbench-scope-section-{ix}")))
                                .debug_selector(move || format!("workbench-scope-section-{ix}"))
                                .small()
                                .label(label)
                                .selected(self.section == ix)
                                .disabled(saving)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.section = ix;
                                    cx.notify();
                                }))
                        }),
                ),
            )
            .child(
                div()
                    .id("workbench-scope-content")
                    .max_h(window.rem_size() * 25.)
                    .overflow_y_scrollbar()
                    .child(content),
            )
            .when_some(self.error.clone(), |el, error| {
                el.child(div().text_color(cx.theme().danger).child(error))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_api::{MemorySecretStore, RowId, SecretStore, WorkbenchStore};

    fn stored_scope_fixture() -> (Arc<WorkbenchStore>, WorkspaceId, Collection) {
        let path = std::env::temp_dir().join(format!(
            "agentops-scope-editor-{}",
            CollectionId::new().as_str()
        ));
        let store = Arc::new(WorkbenchStore::open(&path).unwrap());
        let workspace = WorkspaceId::new("scope-editor-tests").unwrap();
        let collection = Collection {
            id: CollectionId::new(),
            workspace_id: workspace.clone(),
            name: "API".into(),
            description: "Preserve collection documentation".into(),
            auth: AuthConfig::Inherit,
            variables: Vec::new(),
            scripts: Scripts::default(),
            extensions: serde_json::json!({"imported": true})
                .as_object()
                .unwrap()
                .clone(),
        };
        store.upsert_collection(&collection).unwrap();
        (store, workspace, collection)
    }

    #[test]
    fn saving_collection_and_folder_settings_persists_scripts_and_vaults_credentials() {
        let (store, workspace, collection) = stored_scope_fixture();
        let secret_store = Arc::new(MemorySecretStore::new());
        let folder = Folder {
            id: FolderId::new(),
            collection_id: collection.id.clone(),
            parent_id: None,
            name: "Users".into(),
            auth: AuthConfig::Inherit,
            variables: Vec::new(),
            scripts: Scripts::default(),
            sort_key: 42,
            extensions: collection.extensions.clone(),
        };
        store.upsert_folder(&folder).unwrap();
        for original in [
            Scope::Collection(collection.clone()),
            Scope::Folder(folder.clone()),
        ] {
            let mut edited = original.clone();
            let (variables, mut secrets) = draft::parse_session_variables(
                "secret:token=private-variable\nbase_path=/api",
                &original.namespace(),
            )
            .unwrap();
            let (auth, auth_secrets) = draft::parse_auth(
                draft::AuthMode::Bearer,
                "token=private-auth",
                &original.namespace(),
            )
            .unwrap();
            secrets.merge(auth_secrets);
            let scripts = Scripts {
                pre_request: "pm.variables.set('x', 'y');".into(),
                tests: "pm.test('ok', () => pm.expect(true).to.be.true);".into(),
            };
            edited.replace(variables, auth, scripts.clone());
            let data = persist_scope(
                store.clone(),
                workspace.clone(),
                original,
                edited.clone(),
                secrets,
                secret_store.clone(),
            )
            .unwrap();
            let saved = edited.current(&data).unwrap();
            assert!(saved == edited);
            assert_eq!(saved.scripts(), &scripts);
            let VariableValue::Secret(reference) = &saved.variables()[0].value else {
                panic!("must be vaulted")
            };
            assert_eq!(
                secret_store
                    .get_secret(&workspace, reference)
                    .unwrap()
                    .expose_secret(),
                "private-variable"
            );
            let AuthConfig::Bearer { token, .. } = saved.auth() else {
                panic!("must keep bearer auth")
            };
            assert_eq!(
                secret_store
                    .get_secret(&workspace, token)
                    .unwrap()
                    .expose_secret(),
                "private-auth"
            );
            let serialized = match &saved {
                Scope::Collection(value) => serde_json::to_string(value),
                Scope::Folder(value) => serde_json::to_string(value),
            }
            .unwrap();
            assert!(!serialized.contains("private-variable"));
            assert!(!serialized.contains("private-auth"));
        }
    }

    #[test]
    fn stale_scope_save_rejects_before_writing_credentials() {
        let (store, workspace, collection) = stored_scope_fixture();
        let original = Scope::Collection(collection.clone());
        let mut edited = original.clone();
        let (variables, secrets) =
            draft::parse_session_variables("secret:token=replacement", &original.namespace())
                .unwrap();
        let VariableValue::Secret(reference) = variables[0].value.clone() else {
            panic!("must be vaulted")
        };
        edited.replace(variables, AuthConfig::None, Scripts::default());
        let mut newer = collection;
        newer.name = "Renamed elsewhere".into();
        store.upsert_collection(&newer).unwrap();
        let secret_store = Arc::new(MemorySecretStore::new());
        let result = persist_scope(
            store,
            workspace.clone(),
            original,
            edited,
            secrets,
            secret_store.clone(),
        );
        assert!(result.err().unwrap().contains("changed since opening"));
        assert!(secret_store.get_secret(&workspace, &reference).is_err());
    }

    #[test]
    fn scope_variables_preserve_imported_secrets_and_metadata_but_accept_replacement() {
        let saved = Variable {
            id: RowId::new(),
            key: "token".into(),
            value: VariableValue::Secret(SecretRef::new("imported.token").unwrap()),
            enabled: true,
            description: "Imported credential".into(),
        };
        let (mut rows, secrets) =
            draft::parse_session_variables("secret:token=", "folder.test").unwrap();
        preserve_variable_metadata(std::slice::from_ref(&saved), &mut rows, &secrets);
        assert_eq!(rows, vec![saved.clone()]);
        let (mut rows, secrets) =
            draft::parse_session_variables("secret:token=replacement", "folder.test").unwrap();
        preserve_variable_metadata(std::slice::from_ref(&saved), &mut rows, &secrets);
        assert_eq!(rows[0].id, saved.id);
        assert_ne!(rows[0].value, saved.value);
        let VariableValue::Secret(reference) = &rows[0].value else {
            panic!("credential must remain vaulted")
        };
        assert_eq!(secrets.resolve(reference).unwrap(), "replacement");
        let (mut rows, secrets) =
            draft::parse_session_variables("secret:token={{vault.shared}}", "folder.test").unwrap();
        preserve_variable_metadata(&[saved], &mut rows, &secrets);
        let VariableValue::Secret(reference) = &rows[0].value else {
            panic!("named credential must remain vaulted")
        };
        assert_eq!(
            switchyard_api::vault::vault_reference_expression(reference).as_deref(),
            Some("{{vault.shared}}")
        );
    }
}
