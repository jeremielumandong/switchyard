//! Durable, workspace-scoped Workbench state and the blocking persistence
//! commands that mutate it.
//!
//! The desktop app runs these on its background executor; the sidecar calls
//! them directly. SQLite and vault access lives only here, so neither can be
//! reintroduced on a frame thread by accident.

use std::path::Path;
use std::sync::Arc;

use crate::{
    AuthConfig, Collection, CollectionId, CollectionRun, CookieJar, Environment, EnvironmentId,
    Example, ExampleId, Exchange, ExchangeId, Folder, FolderId, ImportResult, ImportSelection,
    RequestId, RequestUrlUpdate, SavedRequest, Scripts, SecretStore, WorkbenchStore,
    WorkspaceEntry, WorkspaceId,
};

use super::secrets::DraftSecrets;

/// The workspace a project maps to: the project path when there is one,
/// else the current directory, else `"default"` — the desktop app's rule.
pub fn workspace_id_for(project: Option<&str>) -> WorkspaceId {
    project
        .and_then(|project| WorkspaceId::new(project).ok())
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .and_then(|path| WorkspaceId::new(path.to_string_lossy()).ok())
        })
        .unwrap_or_else(WorkspaceId::default_workspace)
}

/// The named workspaces and the one to open first (the last one opened).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkspaceList {
    pub workspaces: Vec<WorkspaceEntry>,
    pub last_opened: Option<WorkspaceId>,
}

/// List the named workspaces in `app_data`'s store.
pub fn list_workspaces(app_data: &Path) -> Result<WorkspaceList, String> {
    let store = WorkbenchStore::open(app_data).map_err(|error| error.to_string())?;
    Ok(WorkspaceList {
        workspaces: store.list_workspaces().map_err(|error| error.to_string())?,
        last_opened: store
            .last_opened_workspace()
            .map_err(|error| error.to_string())?,
    })
}

/// Create the next empty workspace, shown in the Workbench as a project:
/// `Project N`, numbered past every name of that form already taken.
pub fn create_workspace(app_data: &Path) -> Result<WorkspaceEntry, String> {
    let store = WorkbenchStore::open(app_data).map_err(|error| error.to_string())?;
    let existing = store.list_workspaces().map_err(|error| error.to_string())?;
    let next = existing
        .iter()
        .filter_map(|workspace| workspace.name.strip_prefix("Project "))
        .filter_map(|number| number.parse::<u32>().ok())
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    store
        .create_workspace(&format!("Project {next}"))
        .map_err(|error| error.to_string())
}

/// Rename `workspace` in `app_data`'s store.
pub fn rename_workspace(
    app_data: &Path,
    workspace: &WorkspaceId,
    name: &str,
) -> Result<WorkspaceEntry, String> {
    WorkbenchStore::open(app_data)
        .and_then(|store| store.rename_workspace(workspace, name))
        .map_err(|error| error.to_string())
}

/// Remember `workspace` as the one to reopen on the next launch.
pub fn mark_workspace_opened(app_data: &Path, workspace: &WorkspaceId) -> Result<(), String> {
    WorkbenchStore::open(app_data)
        .and_then(|store| store.mark_workspace_opened(workspace))
        .map_err(|error| error.to_string())
}

/// Preview literal, case-sensitive replacement in every matching request
/// URL in the collection, including its folders. An empty replacement removes
/// the text; authentication URLs and other request fields are not searched.
pub fn preview_request_url_updates(
    requests: &[SavedRequest],
    collection_id: &CollectionId,
    find: &str,
    replacement: &str,
) -> Result<Vec<RequestUrlUpdate>, String> {
    if find.is_empty() {
        return Err("Enter text to find in request URLs.".into());
    }
    requests
        .iter()
        .filter(|request| request.collection_id == *collection_id)
        .filter_map(|request| {
            let url = request.url.replace(find, replacement);
            (url != request.url).then(|| {
                let url = crate::persistence_safety::persistence_safe_request_url(&url)?;
                Ok(RequestUrlUpdate {
                    request_id: request.id.clone(),
                    original_url: request.url.clone(),
                    url,
                })
            })
        })
        .collect()
}

/// One workspace's durable Workbench state, hydrated from the store.
pub struct WorkspaceData {
    pub store: Arc<WorkbenchStore>,
    pub workspace: WorkspaceId,
    pub collections: Vec<Collection>,
    pub folders: Vec<Folder>,
    pub requests: Vec<SavedRequest>,
    pub environments: Vec<Environment>,
    pub examples: Vec<Example>,
    pub history: Vec<Exchange>,
    pub runs: Vec<CollectionRun>,
    pub cookies: CookieJar,
}

impl WorkspaceData {
    pub fn open(app_data: &Path, workspace: WorkspaceId) -> Result<Self, String> {
        let store = Arc::new(WorkbenchStore::open(app_data).map_err(|error| error.to_string())?);
        Self::hydrate(store, workspace)
    }

    pub fn hydrate(store: Arc<WorkbenchStore>, workspace: WorkspaceId) -> Result<Self, String> {
        let snapshot = store
            .hydrate_workspace(&workspace, 1_000, 100)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            store,
            workspace,
            collections: snapshot.collections,
            folders: snapshot.folders,
            requests: snapshot.requests,
            environments: snapshot.environments,
            examples: snapshot.examples,
            history: snapshot.history,
            runs: snapshot.runs,
            cookies: snapshot.cookies,
        })
    }

    pub fn ensure_collection(&mut self) -> Result<CollectionId, String> {
        if let Some(collection) = self.collections.first() {
            return Ok(collection.id.clone());
        }
        let collection = Collection {
            id: CollectionId::new(),
            workspace_id: self.workspace.clone(),
            name: "My API".into(),
            description: String::new(),
            auth: AuthConfig::None,
            variables: Vec::new(),
            scripts: Scripts::default(),
            extensions: Default::default(),
        };
        self.store
            .upsert_collection(&collection)
            .map_err(|error| error.to_string())?;
        let id = collection.id.clone();
        self.collections.push(collection);
        Ok(id)
    }

    fn refresh(&mut self) -> Result<(), String> {
        let refreshed = Self::hydrate(self.store.clone(), self.workspace.clone())?;
        *self = refreshed;
        Ok(())
    }

    pub fn save_collection(&mut self, collection: Collection) -> Result<(), String> {
        self.store
            .upsert_collection(&collection)
            .map_err(|error| error.to_string())?;
        self.refresh()
    }

    pub fn delete_collection(&mut self, id: &CollectionId) -> Result<(), String> {
        self.store
            .delete_collection(&self.workspace, id)
            .map_err(|error| error.to_string())?;
        self.refresh()
    }

    pub fn save_folder(&mut self, folder: Folder) -> Result<(), String> {
        self.store
            .upsert_folder(&folder)
            .map_err(|error| error.to_string())?;
        self.refresh()
    }

    pub fn delete_folder(
        &mut self,
        collection: &CollectionId,
        id: &FolderId,
    ) -> Result<(), String> {
        self.store
            .delete_folder(collection, id)
            .map_err(|error| error.to_string())?;
        self.refresh()
    }

    pub fn delete_request(
        &mut self,
        collection: &CollectionId,
        id: &RequestId,
    ) -> Result<(), String> {
        self.store
            .delete_request(collection, id)
            .map_err(|error| error.to_string())?;
        self.refresh()
    }

    pub fn delete_environment(&mut self, id: &EnvironmentId) -> Result<(), String> {
        self.store
            .delete_environment(&self.workspace, id)
            .map_err(|error| error.to_string())?;
        self.refresh()
    }

    pub fn delete_example(&mut self, request: &RequestId, id: &ExampleId) -> Result<(), String> {
        self.store
            .delete_example(request, id)
            .map_err(|error| error.to_string())?;
        self.refresh()
    }

    pub fn save_request(&mut self, request: SavedRequest) -> Result<(), String> {
        self.store
            .upsert_request(&request)
            .map_err(|error| error.to_string())?;
        if let Some(existing) = self
            .requests
            .iter_mut()
            .find(|existing| existing.id == request.id)
        {
            *existing = request;
        } else {
            self.requests.push(request);
        }
        Ok(())
    }

    pub fn save_environment(&mut self, environment: Environment) -> Result<(), String> {
        self.store
            .upsert_environment(&environment)
            .map_err(|error| error.to_string())?;
        if environment.active {
            for existing in &mut self.environments {
                existing.active = false;
            }
        }
        if let Some(existing) = self
            .environments
            .iter_mut()
            .find(|existing| existing.id == environment.id)
        {
            *existing = environment;
        } else {
            self.environments.push(environment);
        }
        Ok(())
    }

    pub fn activate_environment(&mut self, id: Option<&EnvironmentId>) -> Result<(), String> {
        self.store
            .set_active_environment(&self.workspace, id)
            .map_err(|error| error.to_string())?;
        for environment in &mut self.environments {
            environment.active = id.is_some_and(|id| environment.id == *id);
        }
        Ok(())
    }

    pub fn record_exchange(
        &mut self,
        exchange: Exchange,
        redactions: &[String],
    ) -> Result<(), String> {
        self.store
            .record_exchange(&exchange, redactions)
            .map_err(|error| error.to_string())?;
        self.history
            .insert(0, crate::redact_exchange(&exchange, redactions));
        self.history.truncate(1_000);
        Ok(())
    }

    pub fn save_run(&mut self, run: CollectionRun, redactions: &[String]) -> Result<(), String> {
        self.store
            .upsert_run(&run, redactions)
            .map_err(|error| error.to_string())?;
        self.apply_run_snapshot(run);
        Ok(())
    }

    pub fn save_cookie_jar(&mut self, jar: CookieJar) -> Result<(), String> {
        self.store
            .save_cookie_jar(&jar)
            .map_err(|error| error.to_string())?;
        self.cookies = jar;
        Ok(())
    }

    pub fn apply_run_snapshot(&mut self, run: CollectionRun) {
        if let Some(existing) = self.runs.iter_mut().find(|existing| existing.id == run.id) {
            *existing = run;
        } else {
            self.runs.insert(0, run);
        }
    }

    pub fn save_example(&mut self, example: Example, redactions: &[String]) -> Result<(), String> {
        self.store
            .upsert_example(&example, redactions)
            .map_err(|error| error.to_string())?;
        let example = crate::redact_example(&example, redactions);
        if let Some(existing) = self
            .examples
            .iter_mut()
            .find(|existing| existing.id == example.id)
        {
            *existing = example;
        } else {
            self.examples.push(example);
        }
        Ok(())
    }

    pub fn commit_import(
        &mut self,
        imported: &ImportResult,
        selection: &ImportSelection,
    ) -> Result<(), String> {
        self.store
            .commit_import_selection(&self.workspace, imported, selection)
            .map_err(|error| error.to_string())?;
        self.refresh()
    }

    pub fn active_environment(&self) -> Option<&Environment> {
        self.environments
            .iter()
            .find(|environment| environment.active)
    }

    pub fn collection(&self, id: &CollectionId) -> Option<&Collection> {
        self.collections
            .iter()
            .find(|collection| collection.id == *id)
    }
}

#[allow(clippy::large_enum_variant)]
pub enum StorageCommand {
    UpsertCollection(Collection),
    DeleteCollection(CollectionId),
    UpsertFolder {
        collection: Option<Collection>,
        folder: Folder,
    },
    DeleteFolder {
        collection_id: CollectionId,
        folder_id: FolderId,
    },
    DeleteRequest {
        collection_id: CollectionId,
        request_id: RequestId,
    },
    UpdateRequestUrls {
        collection_id: CollectionId,
        updates: Vec<RequestUrlUpdate>,
    },
    UpsertRequest {
        request: SavedRequest,
        secrets: DraftSecrets,
        secret_store: Arc<dyn SecretStore>,
    },
    UpsertExample {
        example: Example,
        redactions: Vec<String>,
    },
    DeleteExample {
        request_id: RequestId,
        example_id: ExampleId,
    },
    UpsertEnvironment {
        environment: Environment,
        secrets: DraftSecrets,
        secret_store: Arc<dyn SecretStore>,
    },
    DeleteEnvironment(EnvironmentId),
    ActivateEnvironment(Option<EnvironmentId>),
    DeleteExchange(ExchangeId),
    SaveExchangeAsRequest {
        exchange_id: ExchangeId,
        collection_id: CollectionId,
        folder_id: Option<FolderId>,
        name: String,
    },
    PruneHistory(usize),
}

#[allow(clippy::large_enum_variant)]
pub enum TerminalCommand {
    RecordExchange {
        exchange: Exchange,
        redactions: Vec<String>,
    },
    UpsertRun {
        run: CollectionRun,
        redactions: Vec<String>,
    },
}

pub fn persist_terminal(
    store: Arc<WorkbenchStore>,
    commands: Vec<TerminalCommand>,
) -> Result<(), String> {
    for command in commands {
        match command {
            TerminalCommand::RecordExchange {
                exchange,
                redactions,
            } => store
                .record_exchange(&exchange, &redactions)
                .map_err(|error| error.to_string())?,
            TerminalCommand::UpsertRun { run, redactions } => store
                .upsert_run(&run, &redactions)
                .map_err(|error| error.to_string())?,
        }
    }
    Ok(())
}

pub fn execute(
    store: Arc<WorkbenchStore>,
    workspace: WorkspaceId,
    command: StorageCommand,
) -> Result<WorkspaceData, String> {
    match command {
        StorageCommand::UpsertCollection(collection) => store
            .upsert_collection(&collection)
            .map_err(|error| error.to_string())?,
        StorageCommand::DeleteCollection(collection_id) => {
            store
                .delete_collection(&workspace, &collection_id)
                .map_err(|error| error.to_string())?;
        }
        StorageCommand::UpsertFolder { collection, folder } => {
            if let Some(collection) = collection {
                store
                    .upsert_collection(&collection)
                    .map_err(|error| error.to_string())?;
            }
            store
                .upsert_folder(&folder)
                .map_err(|error| error.to_string())?;
        }
        StorageCommand::DeleteFolder {
            collection_id,
            folder_id,
        } => {
            store
                .delete_folder(&collection_id, &folder_id)
                .map_err(|error| error.to_string())?;
        }
        StorageCommand::DeleteRequest {
            collection_id,
            request_id,
        } => {
            store
                .delete_request(&collection_id, &request_id)
                .map_err(|error| error.to_string())?;
        }
        StorageCommand::UpsertRequest {
            request,
            secrets,
            secret_store,
        } => {
            secrets.persist(secret_store.as_ref(), &workspace)?;
            store
                .upsert_request(&request)
                .map_err(|error| error.to_string())?;
        }
        StorageCommand::UpdateRequestUrls {
            collection_id,
            updates,
        } => store
            .update_request_urls(&workspace, &collection_id, &updates)
            .map_err(|error| error.to_string())?,
        StorageCommand::UpsertExample {
            example,
            redactions,
        } => store
            .upsert_example(&example, &redactions)
            .map_err(|error| error.to_string())?,
        StorageCommand::DeleteExample {
            request_id,
            example_id,
        } => {
            store
                .delete_example(&request_id, &example_id)
                .map_err(|error| error.to_string())?;
        }
        StorageCommand::UpsertEnvironment {
            environment,
            secrets,
            secret_store,
        } => {
            secrets.persist(secret_store.as_ref(), &workspace)?;
            store
                .upsert_environment(&environment)
                .map_err(|error| error.to_string())?;
        }
        StorageCommand::DeleteEnvironment(environment_id) => {
            store
                .delete_environment(&workspace, &environment_id)
                .map_err(|error| error.to_string())?;
        }
        StorageCommand::ActivateEnvironment(environment_id) => store
            .set_active_environment(&workspace, environment_id.as_ref())
            .map_err(|error| error.to_string())?,
        StorageCommand::DeleteExchange(exchange_id) => {
            store
                .delete_exchange(&workspace, &exchange_id)
                .map_err(|error| error.to_string())?;
        }
        StorageCommand::SaveExchangeAsRequest {
            exchange_id,
            collection_id,
            folder_id,
            name,
        } => {
            store
                .save_exchange_as_request(
                    &workspace,
                    &exchange_id,
                    &collection_id,
                    folder_id.as_ref(),
                    &name,
                )
                .map_err(|error| error.to_string())?;
        }
        StorageCommand::PruneHistory(keep) => {
            store
                .prune_history(&workspace, keep)
                .map_err(|error| error.to_string())?;
        }
    }
    WorkspaceData::hydrate(store, workspace)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Body, EnvironmentId, ExampleId, ExchangeId, HttpMethod, RequestId, RequestSettings,
        ResponseSnapshot, RunId, RunStatus, import,
    };
    use base64::Engine as _;

    fn url_preview_request(collection_id: &CollectionId, url: &str) -> SavedRequest {
        serde_json::from_value(serde_json::json!({
            "id": RequestId::new(),
            "collection_id": collection_id,
            "folder_id": null,
            "name": "URL replacement",
            "method": "GET",
            "url": url
        }))
        .unwrap()
    }

    #[test]
    fn request_url_preview_replaces_literal_matches_only_in_the_selected_collection() {
        let collection = CollectionId::new();
        let first = url_preview_request(&collection, "{{basepath}}/items");
        let mut nested = url_preview_request(&collection, "{{basepath}}/echo/{{basepath}}");
        nested.folder_id = Some(FolderId::new());
        let unmatched = url_preview_request(&collection, "{{BASEPATH}}/items");
        let foreign = url_preview_request(&CollectionId::new(), "{{basepath}}/items");
        let requests = [first.clone(), nested.clone(), unmatched, foreign];
        let preview =
            preview_request_url_updates(&requests, &collection, "{{basepath}}", "").unwrap();
        assert_eq!(
            preview,
            vec![
                RequestUrlUpdate {
                    request_id: first.id,
                    original_url: first.url,
                    url: "/items".into()
                },
                RequestUrlUpdate {
                    request_id: nested.id,
                    original_url: nested.url,
                    url: "/echo/".into()
                },
            ]
        );
        assert!(
            preview_request_url_updates(&requests, &collection, "missing", "value")
                .unwrap()
                .is_empty()
        );
        assert!(preview_request_url_updates(&requests, &collection, "", "value").is_err());
        assert!(
            preview_request_url_updates(&requests, &collection, "{{basepath}}", "{{basepath}}")
                .unwrap()
                .is_empty()
        );
        let empty = [url_preview_request(&collection, "{{basepath}}")];
        assert!(
            preview_request_url_updates(&empty, &collection, "{{basepath}}", "")
                .unwrap_err()
                .contains("cannot be empty")
        );
    }

    #[test]
    fn request_url_storage_command_rehydrates_and_compiles_against_environment_base_url() {
        let path = scratch("url-replacement");
        let workspace = WorkspaceId::new("/project/url-replacement").unwrap();
        let mut data = WorkspaceData::open(&path, workspace.clone()).unwrap();
        let collection_id = data.ensure_collection().unwrap();
        data.save_request(url_preview_request(&collection_id, "{{basepath}}/items"))
            .unwrap();
        let updates =
            preview_request_url_updates(&data.requests, &collection_id, "{{basepath}}", "")
                .unwrap();
        let refreshed = execute(
            data.store.clone(),
            workspace,
            StorageCommand::UpdateRequestUrls {
                collection_id,
                updates,
            },
        )
        .unwrap();
        assert_eq!(refreshed.requests[0].url, "/items");
        let secrets = DraftSecrets::default();
        let (compiled, _) = crate::compile_request(
            &refreshed.requests[0],
            None,
            &crate::CompileContext {
                global: &[],
                environment: &[],
                data: &[],
                local: &[],
                secrets: &secrets,
                environment_base_url: Some("https://api.example.test/v1"),
                environment_auth: None,
            },
        )
        .unwrap();
        assert_eq!(compiled.url, "https://api.example.test/v1/items");
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "agentops-core-runtime-workspace-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    #[test]
    fn save_then_reopen_hydrates_the_project_request() {
        let path = scratch("reopen");
        let workspace = WorkspaceId::new("/project/one").unwrap();
        let request_id = RequestId::new();
        {
            let mut data = WorkspaceData::open(&path, workspace.clone()).unwrap();
            let collection_id = data.ensure_collection().unwrap();
            data.save_request(SavedRequest {
                id: request_id.clone(),
                collection_id,
                folder_id: None,
                name: "Durable request".into(),
                method: HttpMethod::new("REPORT").unwrap(),
                url: "https://example.test".into(),
                params: Vec::new(),
                headers: Vec::new(),
                auth: AuthConfig::None,
                body: Body::None,
                variables: Vec::new(),
                scripts: Scripts::default(),
                settings: RequestSettings::default(),
                extensions: Default::default(),
                sort_key: 0,
            })
            .unwrap();
            data.save_example(
                Example {
                    id: ExampleId::new(),
                    request_id: request_id.clone(),
                    name: "Durable example".into(),
                    request: None,
                    response: ResponseSnapshot {
                        status: 200,
                        reason: "OK".into(),
                        headers: Vec::new(),
                        body_base64: "e30=".into(),
                        duration_ms: 1,
                        truncated: false,
                        ..ResponseSnapshot::default()
                    },
                    extensions: Default::default(),
                    sort_key: 0,
                },
                &[],
            )
            .unwrap();
        }
        let reopened = WorkspaceData::open(&path, workspace).unwrap();
        assert_eq!(reopened.requests[0].id, request_id);
        assert_eq!(reopened.requests[0].method.as_str(), "REPORT");
        assert_eq!(reopened.examples[0].name, "Durable example");
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn environment_history_run_and_import_selection_survive_hydration() {
        let path = scratch("complete-hydration");
        let workspace = WorkspaceId::new("/project/hydration").unwrap();
        let mut data = WorkspaceData::open(&path, workspace.clone()).unwrap();
        let collection_id = data.ensure_collection().unwrap();
        let first_environment = Environment {
            id: EnvironmentId::new(),
            workspace_id: workspace.clone(),
            name: "Staging".into(),
            base_url: String::new(),
            auth: Default::default(),
            variables: Vec::new(),
            active: true,
            extensions: Default::default(),
        };
        data.save_environment(first_environment.clone()).unwrap();
        let second_environment = Environment {
            id: EnvironmentId::new(),
            workspace_id: workspace.clone(),
            name: "Production".into(),
            base_url: String::new(),
            auth: Default::default(),
            variables: Vec::new(),
            active: true,
            extensions: Default::default(),
        };
        data.save_environment(second_environment).unwrap();
        data.activate_environment(Some(&first_environment.id))
            .unwrap();

        data.record_exchange(
            Exchange {
                console: Vec::new(),
                test_results: Vec::new(),
                id: ExchangeId::new(),
                workspace_id: workspace.clone(),
                request_id: None,
                request: crate::RedactedRequestSnapshot {
                    method: "GET".into(),
                    url: "https://example.test".into(),
                    headers: Vec::new(),
                    body: String::new(),
                    ..Default::default()
                },
                response: Some(ResponseSnapshot {
                    status: 206,
                    reason: "Partial Content".into(),
                    headers: vec![("X-Echo".into(), "top-secret".into())],
                    body_base64: "dG9rZW49dG9wLXNlY3JldA==".into(),
                    duration_ms: 7,
                    truncated: true,
                    ..ResponseSnapshot::default()
                }),
                error: None,
                started_at: 1,
                completed_at: 2,
            },
            &["top-secret".into()],
        )
        .unwrap();
        data.save_run(
            CollectionRun {
                id: RunId::new(),
                workspace_id: workspace.clone(),
                collection_id,
                environment_id: Some(first_environment.id.clone()),
                iteration_count: 1,
                stop_on_error: false,
                selected_folder_id: None,
                selected_request_ids: Vec::new(),
                delay_ms: 0,
                keep_variable_values: true,
                item_results: Vec::new(),
                status: RunStatus::Completed,
                started_at: 1,
                completed_at: Some(2),
            },
            &[],
        )
        .unwrap();

        let imported = import(
            &workspace,
            b"curl --request GET https://imported.example.test/items",
        )
        .unwrap();
        let mut selection = ImportSelection::all(&imported);
        selection.request_ids.clear();
        data.commit_import(&imported, &selection).unwrap();

        let reopened = WorkspaceData::open(&path, workspace).unwrap();
        assert_eq!(
            reopened.active_environment().map(|value| &value.id),
            Some(&first_environment.id)
        );
        assert!(reopened.history[0].response.as_ref().unwrap().truncated);
        let redacted = reopened.history[0].response.as_ref().unwrap();
        assert!(!redacted.headers[0].1.contains("top-secret"));
        let body = base64::engine::general_purpose::STANDARD
            .decode(&redacted.body_base64)
            .unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("top-secret"));
        assert_eq!(reopened.runs[0].status, RunStatus::Completed);
        assert!(
            !reopened
                .requests
                .iter()
                .any(|request| request.url.contains("imported.example.test"))
        );
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn created_workspaces_are_numbered_and_the_last_opened_one_comes_back() {
        let path = scratch("workspace-list");
        assert_eq!(list_workspaces(&path).unwrap(), WorkspaceList::default());

        let first = create_workspace(&path).unwrap();
        let second = create_workspace(&path).unwrap();
        assert_eq!(first.name, "Project 1");
        assert_eq!(second.name, "Project 2");

        mark_workspace_opened(&path, &first.id).unwrap();
        let listed = list_workspaces(&path).unwrap();
        assert_eq!(listed.workspaces, vec![first.clone(), second]);
        assert_eq!(listed.last_opened, Some(first.id.clone()));

        let data = WorkspaceData::open(&path, first.id).unwrap();
        assert!(data.collections.is_empty() && data.requests.is_empty());
    }

    #[test]
    fn workspace_id_prefers_the_project_then_the_current_directory() {
        assert_eq!(
            workspace_id_for(Some("/tmp/project")).as_str(),
            "/tmp/project"
        );
        let fallback = workspace_id_for(None);
        match std::env::current_dir().ok() {
            Some(cwd) => assert_eq!(fallback, WorkspaceId::new(cwd.to_string_lossy()).unwrap()),
            None => assert_eq!(fallback.as_str(), "default"),
        }
    }
}
