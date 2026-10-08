use super::{
    Body, Collection, CollectionId, CollectionRun, CookieJar, Environment, EnvironmentId, Example,
    ExampleId, Exchange, Folder, FolderId, ImportResult, ImportSelection, RequestId,
    RequestUrlUpdate, SavedRequest, WorkspaceId, persistence_safe_collection,
    persistence_safe_environment, persistence_safe_folder, persistence_safe_saved_request,
    redact_collection_run, redact_example, redact_exchange,
};
use base64::Engine as _;
use rusqlite::{Connection, OptionalExtension as _, Transaction, params};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

const DATABASE_FILE: &str = "workbench.db";
const SCHEMA_VERSION: i32 = 7;
const MMAP_SIZE_BYTES: i64 = 128 * 1024 * 1024;
pub const DEFAULT_HISTORY_BODY_BYTES: usize = 256 * 1024;
pub const DEFAULT_HISTORY_ENTRIES: usize = 1_000;
pub const DEFAULT_RUN_ENTRIES: usize = 100;
pub const DEFAULT_RUN_ITEM_RESULTS: usize = 10_000;
pub const DEFAULT_RUN_RESPONSE_BODY_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistoryBodyPolicy {
    pub max_body_bytes: usize,
    pub store_binary: bool,
    pub store_sensitive: bool,
    pub max_entries: usize,
}

impl Default for HistoryBodyPolicy {
    fn default() -> Self {
        Self {
            max_body_bytes: DEFAULT_HISTORY_BODY_BYTES,
            store_binary: false,
            store_sensitive: false,
            max_entries: DEFAULT_HISTORY_ENTRIES,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunStoragePolicy {
    pub max_runs: usize,
    pub max_item_results: usize,
    pub max_response_body_bytes: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HistoryQuery {
    pub text: String,
    pub status: Option<u16>,
    pub limit: usize,
}

impl Default for RunStoragePolicy {
    fn default() -> Self {
        Self {
            max_runs: DEFAULT_RUN_ENTRIES,
            max_item_results: DEFAULT_RUN_ITEM_RESULTS,
            max_response_body_bytes: DEFAULT_RUN_RESPONSE_BODY_BYTES,
        }
    }
}

pub type StoreResult<T> = Result<T, StoreError>;

#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceSnapshot {
    pub collections: Vec<Collection>,
    pub folders: Vec<Folder>,
    pub requests: Vec<SavedRequest>,
    pub environments: Vec<Environment>,
    pub examples: Vec<Example>,
    pub history: Vec<Exchange>,
    pub runs: Vec<CollectionRun>,
    pub cookies: CookieJar,
}

/// A named Workbench workspace: the scope collections, environments,
/// history and runs belong to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceEntry {
    pub id: WorkspaceId,
    pub name: String,
}

/// The request tabs a workspace had open, for restoring them when it is
/// opened again: saved request ids in tab order and the active one. Unsaved
/// drafts are not part of it, so it never holds request contents.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TabSession {
    /// Saved requests open as tabs, left to right.
    #[serde(default)]
    pub requests: Vec<RequestId>,
    /// The active tab's request, when the active tab was a saved request.
    #[serde(default)]
    pub active: Option<RequestId>,
}

#[derive(Debug)]
pub enum StoreError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    Json(serde_json::Error),
    InvalidInput(String),
    UnsupportedSchema(i32),
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "workbench database: {error}"),
            Self::Io(error) => write!(formatter, "workbench storage: {error}"),
            Self::Json(error) => write!(formatter, "workbench data: {error}"),
            Self::InvalidInput(message) => formatter.write_str(message),
            Self::UnsupportedSchema(version) => {
                write!(
                    formatter,
                    "workbench database schema {version} is newer than supported"
                )
            }
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sqlite(value)
    }
}

impl From<std::io::Error> for StoreError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

pub struct WorkbenchStore {
    connection: Mutex<Connection>,
    database_path: PathBuf,
}

impl WorkbenchStore {
    /// The connection, recovering it if a panicking writer poisoned the lock (SQLite's
    /// own transactions keep the data consistent).
    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn global_variables(&self, workspace: &WorkspaceId) -> StoreResult<Vec<super::Variable>> {
        Ok(query_one_json(
            &self.conn(),
            "SELECT metadata_json FROM workbench_globals WHERE workspace_id = ?1",
            params![workspace.as_str()],
        )?
        .unwrap_or_default())
    }

    pub fn save_global_variables(
        &self,
        workspace: &WorkspaceId,
        variables: &[super::Variable],
    ) -> StoreResult<()> {
        let mut variables = variables.to_vec();
        super::persistence_safety::sanitize_variables(&mut variables);
        self.conn().execute("INSERT INTO workbench_globals(workspace_id, metadata_json) VALUES (?1, ?2) ON CONFLICT(workspace_id) DO UPDATE SET metadata_json = excluded.metadata_json", params![workspace.as_str(), serde_json::to_string(&variables)?])?;
        Ok(())
    }

    /// The tab session remembered for `workspace`; empty when none was saved.
    pub fn tab_session(&self, workspace: &WorkspaceId) -> StoreResult<TabSession> {
        Ok(query_one_json(
            &self.conn(),
            "SELECT session_json FROM workbench_tab_sessions WHERE workspace_id = ?1",
            params![workspace.as_str()],
        )?
        .unwrap_or_default())
    }

    /// Remember `session` as the open tabs of `workspace`, replacing the last one.
    pub fn save_tab_session(
        &self,
        workspace: &WorkspaceId,
        session: &TabSession,
    ) -> StoreResult<()> {
        self.conn().execute(
            "INSERT INTO workbench_tab_sessions(workspace_id, session_json, updated_at)
             VALUES (?1, ?2, unixepoch() * 1000)
             ON CONFLICT(workspace_id) DO UPDATE SET
               session_json=excluded.session_json,
               updated_at=excluded.updated_at",
            params![workspace.as_str(), serde_json::to_string(session)?],
        )?;
        Ok(())
    }

    pub fn open(app_data: impl AsRef<Path>) -> StoreResult<Self> {
        fs::create_dir_all(app_data.as_ref())?;
        Self::open_database(app_data.as_ref().join(DATABASE_FILE))
    }

    pub fn open_database(database_path: impl AsRef<Path>) -> StoreResult<Self> {
        let database_path = database_path.as_ref().to_path_buf();
        if let Some(parent) = database_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut connection = Connection::open(&database_path)?;
        let journal = crate::sqlite_journal::resolve_journal_mode(&database_path);
        let mmap = crate::sqlite_journal::mmap_size_for(&database_path, MMAP_SIZE_BYTES);
        connection.execute_batch(&format!(
            "PRAGMA journal_mode={journal};
             PRAGMA synchronous=NORMAL;
             PRAGMA busy_timeout=10000;
             PRAGMA foreign_keys=ON;
             PRAGMA mmap_size={mmap};"
        ))?;
        migrate(&mut connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
            database_path,
        })
    }

    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    pub fn schema_version(&self) -> StoreResult<i32> {
        Ok(self
            .conn()
            .query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }

    /// Every named workspace, oldest first.
    pub fn list_workspaces(&self) -> StoreResult<Vec<WorkspaceEntry>> {
        let connection = self.conn();
        let mut statement = connection
            .prepare("SELECT id, name FROM workbench_workspaces ORDER BY created_at, rowid")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut workspaces = Vec::new();
        for row in rows {
            let (id, name) = row?;
            let id = WorkspaceId::new(id).map_err(StoreError::InvalidInput)?;
            workspaces.push(WorkspaceEntry { id, name });
        }
        Ok(workspaces)
    }

    /// The workspace opened most recently, if any.
    pub fn last_opened_workspace(&self) -> StoreResult<Option<WorkspaceId>> {
        let id: Option<String> = self
            .conn()
            .query_row(
                "SELECT id FROM workbench_workspaces
                 ORDER BY opened_order DESC, rowid DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        id.map(|id| WorkspaceId::new(id).map_err(StoreError::InvalidInput))
            .transpose()
    }

    /// Create an empty workspace named `name` with a fresh id.
    pub fn create_workspace(&self, name: &str) -> StoreResult<WorkspaceEntry> {
        validate_name("workspace", name)?;
        let id = WorkspaceId::new(format!("workspace-{}", uuid::Uuid::new_v4()))
            .map_err(StoreError::InvalidInput)?;
        let name = name.trim().to_owned();
        self.conn().execute(
            "INSERT INTO workbench_workspaces(id, name, created_at, opened_order)
             SELECT ?1, ?2, unixepoch() * 1000, coalesce(max(opened_order), 0) + 1
             FROM workbench_workspaces",
            params![id.as_str(), name],
        )?;
        Ok(WorkspaceEntry { id, name })
    }

    /// Rename `workspace` to `name` (trimmed).
    pub fn rename_workspace(
        &self,
        workspace: &WorkspaceId,
        name: &str,
    ) -> StoreResult<WorkspaceEntry> {
        validate_name("workspace", name)?;
        let name = name.trim().to_owned();
        let changed = self.conn().execute(
            "UPDATE workbench_workspaces SET name=?2 WHERE id=?1",
            params![workspace.as_str(), name],
        )?;
        if changed == 0 {
            return Err(StoreError::InvalidInput(format!(
                "workspace {} does not exist",
                workspace.as_str()
            )));
        }
        Ok(WorkspaceEntry {
            id: workspace.clone(),
            name,
        })
    }

    /// Record that `workspace` was opened, so the next launch reopens it.
    pub fn mark_workspace_opened(&self, workspace: &WorkspaceId) -> StoreResult<()> {
        self.conn().execute(
            "UPDATE workbench_workspaces
             SET opened_order=(SELECT coalesce(max(opened_order), 0) + 1 FROM workbench_workspaces)
             WHERE id=?1",
            [workspace.as_str()],
        )?;
        Ok(())
    }

    pub fn upsert_collection(&self, collection: &Collection) -> StoreResult<()> {
        validate_name("collection", &collection.name)?;
        let safe = persistence_safe_collection(collection);
        let definition = serde_json::to_string(&safe)?;
        let connection = self.conn();
        reject_scope_move(
            &connection,
            "workbench_collections",
            collection.id.as_str(),
            "workspace_id",
            collection.workspace_id.as_str(),
            "collection",
        )?;
        connection.execute(
            "INSERT INTO workbench_collections(id, workspace_id, name, definition_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, unixepoch() * 1000)
             ON CONFLICT(id) DO UPDATE SET
               name=excluded.name,
               definition_json=excluded.definition_json,
               updated_at=excluded.updated_at",
            params![
                safe.id.as_str(),
                safe.workspace_id.as_str(),
                safe.name,
                definition
            ],
        )?;
        Ok(())
    }

    pub fn list_collections(&self, workspace: &WorkspaceId) -> StoreResult<Vec<Collection>> {
        query_json(
            &self.conn(),
            "SELECT definition_json FROM workbench_collections
             WHERE workspace_id=?1 ORDER BY lower(name), id",
            [workspace.as_str()],
        )
    }

    pub fn collection(
        &self,
        workspace: &WorkspaceId,
        id: &CollectionId,
    ) -> StoreResult<Option<Collection>> {
        query_one_json(
            &self.conn(),
            "SELECT definition_json FROM workbench_collections WHERE workspace_id=?1 AND id=?2",
            params![workspace.as_str(), id.as_str()],
        )
    }

    pub fn delete_collection(
        &self,
        workspace: &WorkspaceId,
        id: &CollectionId,
    ) -> StoreResult<bool> {
        Ok(self.conn().execute(
            "DELETE FROM workbench_collections WHERE workspace_id=?1 AND id=?2",
            params![workspace.as_str(), id.as_str()],
        )? != 0)
    }

    pub fn upsert_folder(&self, folder: &Folder) -> StoreResult<()> {
        validate_name("folder", &folder.name)?;
        let safe = persistence_safe_folder(folder);
        let definition = serde_json::to_string(&safe)?;
        let connection = self.conn();
        require_collection(&connection, &folder.collection_id)?;
        reject_scope_move(
            &connection,
            "workbench_folders",
            folder.id.as_str(),
            "collection_id",
            folder.collection_id.as_str(),
            "folder",
        )?;
        validate_folder_parent(&connection, &safe)?;
        connection.execute(
            "INSERT INTO workbench_folders(id, collection_id, parent_id, name, sort_key, definition_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET parent_id=excluded.parent_id,
               name=excluded.name, sort_key=excluded.sort_key,
               definition_json=excluded.definition_json",
            params![
                safe.id.as_str(), safe.collection_id.as_str(),
                safe.parent_id.as_ref().map(FolderId::as_str), safe.name,
                safe.sort_key, definition
            ],
        )?;
        Ok(())
    }

    pub fn list_folders(&self, collection: &CollectionId) -> StoreResult<Vec<Folder>> {
        query_json(
            &self.conn(),
            "SELECT definition_json FROM workbench_folders
             WHERE collection_id=?1 ORDER BY sort_key, lower(name), id",
            [collection.as_str()],
        )
    }

    pub fn delete_folder(&self, collection: &CollectionId, id: &FolderId) -> StoreResult<bool> {
        let mut connection = self.conn();
        let transaction = connection.transaction()?;
        let mut detached: Vec<SavedRequest> = query_json(
            &transaction,
            "WITH RECURSIVE descendants(id) AS (
                 SELECT id FROM workbench_folders WHERE collection_id=?1 AND id=?2
                 UNION ALL
                 SELECT child.id FROM workbench_folders child
                 JOIN descendants parent ON child.parent_id=parent.id
                 WHERE child.collection_id=?1
             )
             SELECT request.definition_json FROM workbench_requests request
             JOIN descendants folder ON request.folder_id=folder.id
             WHERE request.collection_id=?1",
            params![collection.as_str(), id.as_str()],
        )?;
        let deleted = transaction.execute(
            "DELETE FROM workbench_folders WHERE collection_id=?1 AND id=?2",
            params![collection.as_str(), id.as_str()],
        )? != 0;
        if deleted {
            // SQLite updates the relational folder_id through ON DELETE SET NULL,
            // but definitions are hydrated from JSON. Keep both representations
            // in the same transaction, including requests in cascaded children.
            for request in &mut detached {
                request.folder_id = None;
                transaction.execute(
                    "UPDATE workbench_requests SET definition_json=?1, updated_at=unixepoch() * 1000
                     WHERE collection_id=?2 AND id=?3",
                    params![
                        serde_json::to_string(
                            &persistence_safe_saved_request(request)
                                .map_err(StoreError::InvalidInput)?,
                        )?,
                        collection.as_str(),
                        request.id.as_str()
                    ],
                )?;
            }
        }
        transaction.commit()?;
        Ok(deleted)
    }

    pub fn upsert_request(&self, request: &SavedRequest) -> StoreResult<()> {
        validate_name("request", &request.name)?;
        super::persistence_safety::validate_saved_login_auth(&request.auth)
            .map_err(StoreError::InvalidInput)?;
        let safe = persistence_safe_saved_request(request).map_err(StoreError::InvalidInput)?;
        let definition = serde_json::to_string(&safe)?;
        let connection = self.conn();
        require_collection(&connection, &request.collection_id)?;
        reject_scope_move(
            &connection,
            "workbench_requests",
            request.id.as_str(),
            "collection_id",
            request.collection_id.as_str(),
            "request",
        )?;
        validate_request_folder(&connection, &safe)?;
        connection.execute(
            "INSERT INTO workbench_requests(id, collection_id, folder_id, name, method, url, sort_key, definition_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, unixepoch() * 1000)
             ON CONFLICT(id) DO UPDATE SET folder_id=excluded.folder_id,
               name=excluded.name, method=excluded.method,
               url=excluded.url, sort_key=excluded.sort_key,
               definition_json=excluded.definition_json, updated_at=excluded.updated_at",
            params![
                safe.id.as_str(), safe.collection_id.as_str(),
                safe.folder_id.as_ref().map(FolderId::as_str), safe.name,
                safe.method.as_str(), safe.url, safe.sort_key, definition
            ],
        )?;
        Ok(())
    }

    /// Move only a request's location, retaining its identity and latest saved
    /// definition. Ordinary upserts still reject implicit ownership changes.
    pub fn move_request(
        &self,
        workspace: &WorkspaceId,
        source: &CollectionId,
        id: &RequestId,
        destination: &CollectionId,
        folder: Option<&FolderId>,
    ) -> StoreResult<SavedRequest> {
        let mut connection = self.conn();
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if require_collection(&transaction, source)? != *workspace
            || require_collection(&transaction, destination)? != *workspace
        {
            return Err(StoreError::InvalidInput(
                "requests can only move between collections in the same workspace".into(),
            ));
        }
        let json: String = transaction
            .query_row(
                "SELECT definition_json FROM workbench_requests WHERE collection_id=?1 AND id=?2",
                params![source.as_str(), id.as_str()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| {
                StoreError::InvalidInput(
                    "request no longer belongs to the source collection".into(),
                )
            })?;
        let mut request: SavedRequest = serde_json::from_str(&json)?;
        if request.id != *id || request.collection_id != *source {
            return Err(StoreError::InvalidInput(
                "saved request ownership is inconsistent".into(),
            ));
        }
        request.collection_id = destination.clone();
        request.folder_id = folder.cloned();
        validate_request_folder(&transaction, &request)?;
        let last: Option<i64> = transaction.query_row(
            "SELECT MAX(sort_key) FROM workbench_requests WHERE collection_id=?1 AND folder_id IS ?2 AND id<>?3",
            params![destination.as_str(), folder.map(FolderId::as_str), id.as_str()],
            |row| row.get(0),
        )?;
        request.sort_key = last.map_or(0, |last| last.saturating_add(1));
        // Preserve unknown fields as well as credentials, scripts and body.
        let mut definition: serde_json::Value = serde_json::from_str(&json)?;
        definition["collection_id"] = serde_json::to_value(&request.collection_id)?;
        definition["folder_id"] = serde_json::to_value(&request.folder_id)?;
        definition["sort_key"] = serde_json::to_value(request.sort_key)?;
        transaction.execute(
            "UPDATE workbench_requests SET collection_id=?1, folder_id=?2, sort_key=?3, definition_json=?4, updated_at=unixepoch() * 1000 WHERE collection_id=?5 AND id=?6",
            params![destination.as_str(), folder.map(FolderId::as_str), request.sort_key,
                serde_json::to_string(&definition)?, source.as_str(), id.as_str()],
        )?;
        transaction.commit()?;
        Ok(request)
    }

    /// Rename only metadata, retaining the latest saved definition and unknown fields.
    pub fn rename_request(
        &self,
        workspace: &WorkspaceId,
        collection: &CollectionId,
        id: &RequestId,
        expected_name: &str,
        name: &str,
    ) -> StoreResult<SavedRequest> {
        let name = name.trim();
        validate_name("request", name)?;
        let mut connection = self.conn();
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if require_collection(&transaction, collection)? != *workspace {
            return Err(StoreError::InvalidInput(
                "collection belongs to another workspace".into(),
            ));
        }
        let json: String = transaction
            .query_row(
                "SELECT definition_json FROM workbench_requests WHERE collection_id=?1 AND id=?2",
                params![collection.as_str(), id.as_str()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| {
                StoreError::InvalidInput("request no longer belongs to this collection".into())
            })?;
        let mut request: SavedRequest = serde_json::from_str(&json)?;
        if request.id != *id
            || request.collection_id != *collection
            || request.name != expected_name
        {
            return Err(StoreError::InvalidInput(
                "request changed; reopen Rename request".into(),
            ));
        }
        request.name = name.to_owned();
        let mut definition: serde_json::Value = serde_json::from_str(&json)?;
        definition["name"] = serde_json::Value::String(name.to_owned());
        transaction.execute(
            "UPDATE workbench_requests SET name=?1, definition_json=?2, updated_at=unixepoch() * 1000 WHERE collection_id=?3 AND id=?4",
            params![name, serde_json::to_string(&definition)?, collection.as_str(), id.as_str()],
        )?;
        transaction.commit()?;
        Ok(request)
    }

    pub fn request(
        &self,
        collection: &CollectionId,
        id: &RequestId,
    ) -> StoreResult<Option<SavedRequest>> {
        let json: Option<String> = self
            .conn()
            .query_row(
                "SELECT definition_json FROM workbench_requests WHERE collection_id=?1 AND id=?2",
                params![collection.as_str(), id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        json.map(|value| serde_json::from_str(&value).map_err(StoreError::from))
            .transpose()
    }

    /// Apply only the previewed URLs to the latest saved definitions. Any
    /// stale URL, missing request, or invalid target rolls back the batch.
    pub fn update_request_urls(
        &self,
        workspace: &WorkspaceId,
        collection: &CollectionId,
        updates: &[RequestUrlUpdate],
    ) -> StoreResult<()> {
        let mut connection = self.conn();
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if require_collection(&transaction, collection)? != *workspace {
            return Err(StoreError::InvalidInput(
                "collection belongs to another workspace".into(),
            ));
        }
        let mut seen = BTreeSet::new();
        for update in updates {
            if !seen.insert(&update.request_id) {
                return Err(StoreError::InvalidInput(
                    "request URL update contains a duplicate request".into(),
                ));
            }
            let json: Option<String> = transaction.query_row(
                "SELECT definition_json FROM workbench_requests WHERE collection_id=?1 AND id=?2",
                params![collection.as_str(), update.request_id.as_str()],
                |row| row.get(0),
            ).optional()?;
            let json = json.ok_or_else(|| StoreError::InvalidInput(
                "a selected request no longer belongs to this collection; refresh the URL preview".into(),
            ))?;
            let request: SavedRequest = serde_json::from_str(&json)?;
            if request.id != update.request_id || request.collection_id != *collection {
                return Err(StoreError::InvalidInput(
                    "saved request ownership is inconsistent".into(),
                ));
            }
            if request.url != update.original_url {
                return Err(StoreError::InvalidInput(format!(
                    "request {:?} changed since the URL preview; reload the collection and preview again",
                    request.name,
                )));
            }
            let url = super::persistence_safety::persistence_safe_request_url(&update.url)
                .map_err(StoreError::InvalidInput)?;
            // Keep the latest auth, body, scripts and any unknown serialized
            // fields. A preview carries no replacement request definition.
            let mut definition: serde_json::Value = serde_json::from_str(&json)?;
            definition["url"] = serde_json::Value::String(url.clone());
            transaction.execute(
                "UPDATE workbench_requests SET url=?1, definition_json=?2, updated_at=unixepoch() * 1000
                 WHERE collection_id=?3 AND id=?4",
                params![url, serde_json::to_string(&definition)?, collection.as_str(), update.request_id.as_str()],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn list_requests(&self, collection: &CollectionId) -> StoreResult<Vec<SavedRequest>> {
        query_json(
            &self.conn(),
            "SELECT definition_json FROM workbench_requests
             WHERE collection_id=?1 ORDER BY sort_key, lower(name), id",
            [collection.as_str()],
        )
    }

    pub fn delete_request(&self, collection: &CollectionId, id: &RequestId) -> StoreResult<bool> {
        Ok(self.conn().execute(
            "DELETE FROM workbench_requests WHERE collection_id=?1 AND id=?2",
            params![collection.as_str(), id.as_str()],
        )? != 0)
    }

    pub fn upsert_environment(&self, environment: &Environment) -> StoreResult<()> {
        validate_name("environment", &environment.name)?;
        super::persistence_safety::validate_saved_login_auth(&environment.auth)
            .map_err(StoreError::InvalidInput)?;
        let safe = persistence_safe_environment(environment);
        let definition = serde_json::to_string(&safe)?;
        let mut connection = self.conn();
        reject_scope_move(
            &connection,
            "workbench_environments",
            environment.id.as_str(),
            "workspace_id",
            environment.workspace_id.as_str(),
            "environment",
        )?;
        let mut existing: Vec<Environment> = query_json(
            &connection,
            "SELECT definition_json FROM workbench_environments WHERE workspace_id=?1",
            [environment.workspace_id.as_str()],
        )?;
        let transaction = connection.transaction()?;
        if safe.active {
            for value in &mut existing {
                if value.id != environment.id && value.active {
                    value.active = false;
                    transaction.execute(
                        "UPDATE workbench_environments SET active=0, definition_json=?1, updated_at=unixepoch() * 1000 WHERE id=?2",
                        params![serde_json::to_string(&persistence_safe_environment(value))?, value.id.as_str()],
                    )?;
                }
            }
        }
        transaction.execute(
            "INSERT INTO workbench_environments(id, workspace_id, name, active, label, definition_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, unixepoch() * 1000)
             ON CONFLICT(id) DO UPDATE SET name=excluded.name, active=excluded.active,
               label=excluded.label, definition_json=excluded.definition_json,
               updated_at=excluded.updated_at",
            params![
                safe.id.as_str(), safe.workspace_id.as_str(), safe.name,
                i64::from(safe.active), safe.label.as_str(), definition
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn list_environments(&self, workspace: &WorkspaceId) -> StoreResult<Vec<Environment>> {
        query_json(
            &self.conn(),
            "SELECT definition_json FROM workbench_environments
             WHERE workspace_id=?1 ORDER BY active DESC, lower(name), id",
            [workspace.as_str()],
        )
    }

    pub fn set_active_environment(
        &self,
        workspace: &WorkspaceId,
        environment: Option<&EnvironmentId>,
    ) -> StoreResult<()> {
        let mut connection = self.conn();
        let mut environments: Vec<Environment> = query_json(
            &connection,
            "SELECT definition_json FROM workbench_environments WHERE workspace_id=?1",
            [workspace.as_str()],
        )?;
        if environment.is_some_and(|id| !environments.iter().any(|value| &value.id == id)) {
            return Err(StoreError::InvalidInput(
                "active environment does not belong to this workspace".into(),
            ));
        }
        let transaction = connection.transaction()?;
        for value in &mut environments {
            if value.active {
                value.active = false;
                transaction.execute(
                    "UPDATE workbench_environments SET active=0, definition_json=?1, updated_at=unixepoch() * 1000 WHERE id=?2",
                    params![serde_json::to_string(&persistence_safe_environment(value))?, value.id.as_str()],
                )?;
            }
        }
        if let Some(environment) = environment {
            let value = environments
                .iter_mut()
                .find(|value| &value.id == environment)
                .ok_or_else(|| {
                    StoreError::InvalidInput("environment does not belong to this workspace".into())
                })?;
            value.active = true;
            transaction.execute(
                "UPDATE workbench_environments SET active=1, definition_json=?1, updated_at=unixepoch() * 1000 WHERE id=?2",
                params![serde_json::to_string(&persistence_safe_environment(value))?, value.id.as_str()],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn delete_environment(
        &self,
        workspace: &WorkspaceId,
        id: &EnvironmentId,
    ) -> StoreResult<bool> {
        Ok(self.conn().execute(
            "DELETE FROM workbench_environments WHERE workspace_id=?1 AND id=?2",
            params![workspace.as_str(), id.as_str()],
        )? != 0)
    }

    /// Persists cookie scope and vault references for one workspace. Cookie
    /// values remain exclusively in the configured secret store.
    pub fn save_cookie_jar(&self, jar: &CookieJar) -> StoreResult<()> {
        let metadata = serde_json::to_string(jar)?;
        self.conn().execute(
            "INSERT INTO workbench_cookie_jars(workspace_id, metadata_json, updated_at)
             VALUES (?1, ?2, unixepoch() * 1000)
             ON CONFLICT(workspace_id) DO UPDATE SET
               metadata_json=excluded.metadata_json,
               updated_at=excluded.updated_at",
            params![jar.workspace_id.as_str(), metadata],
        )?;
        Ok(())
    }

    pub fn cookie_jar(&self, workspace: &WorkspaceId) -> StoreResult<CookieJar> {
        query_one_json(
            &self.conn(),
            "SELECT metadata_json FROM workbench_cookie_jars WHERE workspace_id=?1",
            [workspace.as_str()],
        )
        .map(|jar| jar.unwrap_or_else(|| CookieJar::new(workspace.clone())))
    }

    pub fn record_exchange(&self, exchange: &Exchange, redactions: &[String]) -> StoreResult<()> {
        self.record_exchange_with_policy(exchange, redactions, HistoryBodyPolicy::default())
    }

    pub fn record_exchange_with_policy(
        &self,
        exchange: &Exchange,
        redactions: &[String],
        policy: HistoryBodyPolicy,
    ) -> StoreResult<()> {
        reject_snapshot_url_userinfo(&exchange.request)?;
        let mut connection = self.conn();
        let transaction = connection.transaction()?;
        reject_scope_move(
            &transaction,
            "workbench_history",
            exchange.id.as_str(),
            "workspace_id",
            exchange.workspace_id.as_str(),
            "exchange",
        )?;
        if let Some(request) = &exchange.request_id {
            let request_workspace = require_request_workspace(&transaction, request)?;
            if request_workspace != exchange.workspace_id {
                return Err(StoreError::InvalidInput(
                    "exchange request belongs to another workspace".into(),
                ));
            }
        }
        let mut safe = redact_exchange(exchange, redactions);
        apply_history_body_policy(&mut safe, policy);
        let existing: Option<String> = transaction
            .query_row(
                "SELECT snapshot_json FROM workbench_history WHERE id=?1",
                [exchange.id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            let existing: Exchange = serde_json::from_str(&existing)?;
            if existing.request_id != safe.request_id
                || existing.request != safe.request
                || existing.started_at != safe.started_at
            {
                return Err(StoreError::InvalidInput(
                    "exchange request snapshot is immutable after submission".into(),
                ));
            }
        }
        let snapshot = serde_json::to_string(&safe)?;
        transaction.execute(
            "INSERT INTO workbench_history(id, workspace_id, request_id, started_at, completed_at, snapshot_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET completed_at=excluded.completed_at,
               snapshot_json=excluded.snapshot_json",
            params![
                exchange.id.as_str(), exchange.workspace_id.as_str(),
                exchange.request_id.as_ref().map(RequestId::as_str),
                exchange.started_at, exchange.completed_at, snapshot
            ],
        )?;
        let keep = i64::try_from(policy.max_entries.min(10_000)).unwrap_or(10_000);
        transaction.execute(
            "DELETE FROM workbench_history WHERE workspace_id=?1 AND id NOT IN (
               SELECT id FROM workbench_history WHERE workspace_id=?1
               ORDER BY completed_at DESC, id DESC LIMIT ?2
             )",
            params![exchange.workspace_id.as_str(), keep],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn history(&self, workspace: &WorkspaceId, limit: usize) -> StoreResult<Vec<Exchange>> {
        let limit = i64::try_from(limit.min(10_000)).unwrap_or(10_000);
        let connection = self.conn();
        let mut statement = connection.prepare(
            "SELECT snapshot_json FROM workbench_history
             WHERE workspace_id=?1 ORDER BY completed_at DESC, id DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![workspace.as_str(), limit], |row| {
            row.get::<_, String>(0)
        })?;
        decode_rows(rows)
    }

    pub fn query_history(
        &self,
        workspace: &WorkspaceId,
        query: &HistoryQuery,
    ) -> StoreResult<Vec<Exchange>> {
        let needle = query.text.trim().to_ascii_lowercase();
        let limit = if query.limit == 0 {
            DEFAULT_HISTORY_ENTRIES
        } else {
            query.limit.min(10_000)
        };
        Ok(self
            .history(workspace, 10_000)?
            .into_iter()
            .filter(|exchange| {
                query.status.is_none_or(|status| {
                    exchange.response.as_ref().map(|response| response.status) == Some(status)
                }) && (needle.is_empty()
                    || exchange
                        .request
                        .method
                        .to_ascii_lowercase()
                        .contains(&needle)
                    || exchange.request.url.to_ascii_lowercase().contains(&needle)
                    || exchange
                        .request
                        .replay
                        .as_ref()
                        .is_some_and(|replay| replay.name.to_ascii_lowercase().contains(&needle))
                    || exchange
                        .error
                        .as_ref()
                        .is_some_and(|error| error.to_ascii_lowercase().contains(&needle)))
            })
            .take(limit)
            .collect())
    }

    pub fn delete_exchange(
        &self,
        workspace: &WorkspaceId,
        id: &super::ExchangeId,
    ) -> StoreResult<bool> {
        Ok(self.conn().execute(
            "DELETE FROM workbench_history WHERE workspace_id=?1 AND id=?2",
            params![workspace.as_str(), id.as_str()],
        )? != 0)
    }

    pub fn save_exchange_as_request(
        &self,
        workspace: &WorkspaceId,
        exchange_id: &super::ExchangeId,
        collection_id: &CollectionId,
        folder_id: Option<&FolderId>,
        name: &str,
    ) -> StoreResult<SavedRequest> {
        validate_name("request", name)?;
        let mut connection = self.conn();
        let transaction = connection.transaction()?;
        if require_collection(&transaction, collection_id)? != *workspace {
            return Err(StoreError::InvalidInput(
                "target collection belongs to another workspace".into(),
            ));
        }
        let snapshot: String = transaction
            .query_row(
                "SELECT snapshot_json FROM workbench_history WHERE workspace_id=?1 AND id=?2",
                params![workspace.as_str(), exchange_id.as_str()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::InvalidInput("history exchange does not exist".into()))?;
        let exchange: Exchange = serde_json::from_str(&snapshot)?;
        let replay = exchange.request.replay.ok_or_else(|| {
            StoreError::InvalidInput("history exchange has no replayable request snapshot".into())
        })?;
        let sort_key: i64 = transaction.query_row(
            "SELECT COALESCE(MAX(sort_key), -1) + 1 FROM workbench_requests WHERE collection_id=?1",
            [collection_id.as_str()],
            |row| row.get(0),
        )?;
        let request = persistence_safe_saved_request(&SavedRequest {
            id: RequestId::new(),
            collection_id: collection_id.clone(),
            folder_id: folder_id.cloned(),
            name: name.trim().into(),
            method: replay.method,
            url: replay.url,
            params: replay.params,
            headers: replay.headers,
            auth: replay.auth,
            body: replay.body,
            variables: replay.variables,
            scripts: replay.scripts,
            settings: replay.settings,
            extensions: Default::default(),
            sort_key,
        })
        .map_err(StoreError::InvalidInput)?;
        validate_request_folder(&transaction, &request)?;
        insert_request(&transaction, &request)?;
        transaction.commit()?;
        Ok(request)
    }

    pub fn prune_history(&self, workspace: &WorkspaceId, keep: usize) -> StoreResult<usize> {
        let keep = i64::try_from(keep.min(10_000)).unwrap_or(10_000);
        Ok(self.conn().execute(
            "DELETE FROM workbench_history WHERE workspace_id=?1 AND id NOT IN (
               SELECT id FROM workbench_history WHERE workspace_id=?1
               ORDER BY completed_at DESC, id DESC LIMIT ?2
             )",
            params![workspace.as_str(), keep],
        )?)
    }

    pub fn upsert_example(&self, example: &Example, redactions: &[String]) -> StoreResult<()> {
        validate_name("example", &example.name)?;
        if let Some(request) = &example.request {
            reject_snapshot_url_userinfo(request)?;
        }
        let safe = redact_example(example, redactions);
        let definition = serde_json::to_string(&safe)?;
        let connection = self.conn();
        require_request_workspace(&connection, &example.request_id)?;
        reject_scope_move(
            &connection,
            "workbench_examples",
            example.id.as_str(),
            "request_id",
            example.request_id.as_str(),
            "example",
        )?;
        connection.execute(
            "INSERT INTO workbench_examples(id, request_id, name, sort_key, definition_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, unixepoch() * 1000)
             ON CONFLICT(id) DO UPDATE SET request_id=excluded.request_id,
               name=excluded.name, sort_key=excluded.sort_key,
               definition_json=excluded.definition_json, updated_at=excluded.updated_at",
            params![
                example.id.as_str(), example.request_id.as_str(), example.name,
                example.sort_key, definition
            ],
        )?;
        Ok(())
    }

    pub fn list_examples(&self, request: &RequestId) -> StoreResult<Vec<Example>> {
        query_json(
            &self.conn(),
            "SELECT definition_json FROM workbench_examples
             WHERE request_id=?1 ORDER BY sort_key, lower(name), id",
            [request.as_str()],
        )
    }

    pub fn delete_example(&self, request: &RequestId, id: &ExampleId) -> StoreResult<bool> {
        Ok(self.conn().execute(
            "DELETE FROM workbench_examples WHERE request_id=?1 AND id=?2",
            params![request.as_str(), id.as_str()],
        )? != 0)
    }

    pub fn upsert_run(&self, run: &CollectionRun, redactions: &[String]) -> StoreResult<()> {
        self.upsert_run_with_policy(run, redactions, RunStoragePolicy::default())
    }

    pub fn upsert_run_with_policy(
        &self,
        run: &CollectionRun,
        redactions: &[String],
        policy: RunStoragePolicy,
    ) -> StoreResult<()> {
        if run.item_results.len() > DEFAULT_RUN_ITEM_RESULTS {
            return Err(StoreError::InvalidInput(format!(
                "run contains more than {DEFAULT_RUN_ITEM_RESULTS} item results"
            )));
        }
        let mut connection = self.conn();
        let collection_workspace = require_collection(&connection, &run.collection_id)?;
        if collection_workspace != run.workspace_id {
            return Err(StoreError::InvalidInput(
                "run collection belongs to another workspace".into(),
            ));
        }
        if let Some(environment) = &run.environment_id {
            let workspace: Option<String> = connection
                .query_row(
                    "SELECT workspace_id FROM workbench_environments WHERE id=?1",
                    [environment.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            if workspace.as_deref() != Some(run.workspace_id.as_str()) {
                return Err(StoreError::InvalidInput(
                    "run environment belongs to another workspace".into(),
                ));
            }
        }
        if run.delay_ms > 60_000 {
            return Err(StoreError::InvalidInput(
                "run delay must not exceed 60000 ms".into(),
            ));
        }
        if let Some(folder) = &run.selected_folder_id {
            let collection: Option<String> = connection
                .query_row(
                    "SELECT collection_id FROM workbench_folders WHERE id=?1",
                    [folder.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            if collection.as_deref() != Some(run.collection_id.as_str()) {
                return Err(StoreError::InvalidInput(
                    "run folder scope belongs to another collection or does not exist".into(),
                ));
            }
        }
        let mut selected_requests = BTreeSet::new();
        for request in &run.selected_request_ids {
            if !selected_requests.insert(request) {
                return Err(StoreError::InvalidInput(
                    "run request selection contains duplicate ids".into(),
                ));
            }
            let (collection, folder): (String, Option<String>) = connection
                .query_row(
                    "SELECT collection_id, folder_id FROM workbench_requests WHERE id=?1",
                    [request.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?
                .ok_or_else(|| {
                    StoreError::InvalidInput("selected run request does not exist".into())
                })?;
            if collection != run.collection_id.as_str() {
                return Err(StoreError::InvalidInput(
                    "selected run request belongs to another collection".into(),
                ));
            }
            if let Some(root) = &run.selected_folder_id {
                let Some(folder) = folder else {
                    return Err(StoreError::InvalidInput(
                        "selected run request is outside the folder scope".into(),
                    ));
                };
                if !folder_descends_from(&connection, &folder, root)? {
                    return Err(StoreError::InvalidInput(
                        "selected run request is outside the folder scope".into(),
                    ));
                }
            }
        }
        let mut validated_requests = BTreeSet::new();
        for item in &run.item_results {
            if !validated_requests.insert(&item.request_id) {
                continue;
            }
            let collection: Option<String> = connection
                .query_row(
                    "SELECT collection_id FROM workbench_requests WHERE id=?1",
                    [item.request_id.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            match collection.as_deref() {
                Some(collection) if collection == run.collection_id.as_str() => {}
                Some(_) => {
                    return Err(StoreError::InvalidInput(
                        "run item request belongs to another collection".into(),
                    ));
                }
                None => {
                    return Err(StoreError::InvalidInput(
                        "run item request does not exist".into(),
                    ));
                }
            }
        }
        reject_scope_move(
            &connection,
            "workbench_runs",
            run.id.as_str(),
            "workspace_id",
            run.workspace_id.as_str(),
            "run",
        )?;
        let existing_collection: Option<String> = connection
            .query_row(
                "SELECT collection_id FROM workbench_runs WHERE id=?1",
                [run.id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if existing_collection.is_some_and(|collection| collection != run.collection_id.as_str()) {
            return Err(StoreError::InvalidInput(
                "run id already belongs to another collection".into(),
            ));
        }
        let mut safe = redact_collection_run(run, redactions);
        safe.item_results
            .truncate(policy.max_item_results.min(DEFAULT_RUN_ITEM_RESULTS));
        let body_policy = HistoryBodyPolicy {
            max_body_bytes: policy
                .max_response_body_bytes
                .min(DEFAULT_HISTORY_BODY_BYTES),
            store_binary: false,
            store_sensitive: false,
            max_entries: 0,
        };
        for item in &mut safe.item_results {
            if let Some(response) = &mut item.response {
                apply_response_body_policy(response, body_policy);
            }
        }
        let definition = serde_json::to_string(&safe)?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO workbench_runs(id, workspace_id, collection_id, started_at, definition_json)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET definition_json=excluded.definition_json",
            params![
                run.id.as_str(), run.workspace_id.as_str(), run.collection_id.as_str(),
                run.started_at, definition
            ],
        )?;
        let keep = i64::try_from(policy.max_runs.min(1_000)).unwrap_or(1_000);
        transaction.execute(
            "DELETE FROM workbench_runs WHERE workspace_id=?1 AND id NOT IN (
               SELECT id FROM workbench_runs WHERE workspace_id=?1
               ORDER BY started_at DESC, id DESC LIMIT ?2
             )",
            params![run.workspace_id.as_str(), keep],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn runs(&self, workspace: &WorkspaceId, limit: usize) -> StoreResult<Vec<CollectionRun>> {
        let limit = i64::try_from(limit.min(1_000)).unwrap_or(1_000);
        let connection = self.conn();
        let mut statement = connection.prepare(
            "SELECT definition_json FROM workbench_runs
             WHERE workspace_id=?1 ORDER BY started_at DESC, id DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![workspace.as_str(), limit], |row| row.get(0))?;
        decode_rows(rows)
    }

    pub fn hydrate_workspace(
        &self,
        workspace: &WorkspaceId,
        history_limit: usize,
        run_limit: usize,
    ) -> StoreResult<WorkspaceSnapshot> {
        let history_limit = i64::try_from(history_limit.min(10_000)).unwrap_or(10_000);
        let run_limit = i64::try_from(run_limit.min(1_000)).unwrap_or(1_000);
        let mut connection = self.conn();
        let transaction = connection.transaction()?;
        let collections = query_json(
            &transaction,
            "SELECT definition_json FROM workbench_collections
             WHERE workspace_id=?1 ORDER BY lower(name), id",
            [workspace.as_str()],
        )?;
        let folders = query_json(
            &transaction,
            "SELECT f.definition_json FROM workbench_folders f
             JOIN workbench_collections c ON c.id=f.collection_id
             WHERE c.workspace_id=?1 ORDER BY f.sort_key, lower(f.name), f.id",
            [workspace.as_str()],
        )?;
        let requests = query_json(
            &transaction,
            "SELECT r.definition_json FROM workbench_requests r
             JOIN workbench_collections c ON c.id=r.collection_id
             WHERE c.workspace_id=?1 ORDER BY r.sort_key, lower(r.name), r.id",
            [workspace.as_str()],
        )?;
        let examples = query_json(
            &transaction,
            "SELECT e.definition_json FROM workbench_examples e
             JOIN workbench_requests r ON r.id=e.request_id
             JOIN workbench_collections c ON c.id=r.collection_id
             WHERE c.workspace_id=?1 ORDER BY e.sort_key, lower(e.name), e.id",
            [workspace.as_str()],
        )?;
        let environments = query_json(
            &transaction,
            "SELECT definition_json FROM workbench_environments
             WHERE workspace_id=?1 ORDER BY active DESC, lower(name), id",
            [workspace.as_str()],
        )?;
        let history = query_json(
            &transaction,
            "SELECT snapshot_json FROM workbench_history
             WHERE workspace_id=?1 ORDER BY completed_at DESC, id DESC LIMIT ?2",
            params![workspace.as_str(), history_limit],
        )?;
        let runs = query_json(
            &transaction,
            "SELECT definition_json FROM workbench_runs
             WHERE workspace_id=?1 ORDER BY started_at DESC, id DESC LIMIT ?2",
            params![workspace.as_str(), run_limit],
        )?;
        let cookies = query_one_json(
            &transaction,
            "SELECT metadata_json FROM workbench_cookie_jars WHERE workspace_id=?1",
            [workspace.as_str()],
        )?
        .unwrap_or_else(|| CookieJar::new(workspace.clone()));
        let snapshot = WorkspaceSnapshot {
            collections,
            folders,
            requests,
            environments,
            examples,
            history,
            runs,
            cookies,
        };
        transaction.commit()?;
        Ok(snapshot)
    }

    pub fn commit_import(
        &self,
        workspace: &WorkspaceId,
        imported: &ImportResult,
    ) -> StoreResult<()> {
        self.commit_import_selection(workspace, imported, &ImportSelection::all(imported))
    }

    pub fn commit_import_selection(
        &self,
        workspace: &WorkspaceId,
        imported: &ImportResult,
        selection: &ImportSelection,
    ) -> StoreResult<()> {
        let safe_import = super::import_export::sanitize_import_result(imported)
            .map_err(StoreError::InvalidInput)?;
        validate_import_graph(workspace, &safe_import)?;
        validate_import_selection(&safe_import, selection)?;
        let selected_folders = selected_folders_in_parent_order(&safe_import, selection)?;
        let mut connection = self.conn();
        let transaction = connection.transaction()?;
        if selection.include_collection {
            insert_collection(&transaction, &safe_import.collection)?;
        }
        for folder in selected_folders {
            insert_folder(&transaction, folder)?;
        }
        for request in safe_import
            .requests
            .iter()
            .filter(|value| selection.request_ids.contains(&value.id))
        {
            insert_request(&transaction, request)?;
        }
        let selected_environments: Vec<&Environment> = safe_import
            .environments
            .iter()
            .filter(|value| selection.environment_ids.contains(&value.id))
            .collect();
        if selected_environments
            .iter()
            .any(|environment| environment.active)
        {
            let mut existing: Vec<Environment> = query_json(
                &transaction,
                "SELECT definition_json FROM workbench_environments WHERE workspace_id=?1 AND active=1",
                [workspace.as_str()],
            )?;
            for environment in &mut existing {
                environment.active = false;
                transaction.execute(
                    "UPDATE workbench_environments SET active=0, definition_json=?1, updated_at=unixepoch() * 1000 WHERE id=?2",
                    params![serde_json::to_string(&persistence_safe_environment(environment))?, environment.id.as_str()],
                )?;
            }
        }
        // A workspace with no active environment adopts the first imported
        // one, so imported relative URLs are sendable right away. An existing
        // active environment is never displaced by an inactive import.
        let workspace_has_active: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM workbench_environments WHERE workspace_id=?1 AND active=1)",
            [workspace.as_str()],
            |row| row.get(0),
        )?;
        let mut adopted = workspace_has_active
            || selected_environments
                .iter()
                .any(|environment| environment.active);
        for environment in selected_environments {
            if adopted {
                insert_environment(&transaction, environment)?;
            } else {
                let mut environment = environment.clone();
                environment.active = true;
                adopted = true;
                insert_environment(&transaction, &environment)?;
            }
        }
        for example in safe_import
            .examples
            .iter()
            .filter(|value| selection.example_ids.contains(&value.id))
        {
            insert_example(&transaction, example)?;
        }
        transaction.commit()?;
        Ok(())
    }
}

fn apply_history_body_policy(exchange: &mut Exchange, policy: HistoryBodyPolicy) {
    apply_request_body_policy(&mut exchange.request, policy);
    let Some(response) = &mut exchange.response else {
        return;
    };
    apply_response_body_policy(response, policy);
}

fn apply_response_body_policy(response: &mut super::ResponseSnapshot, policy: HistoryBodyPolicy) {
    let Ok(mut bytes) = base64::engine::general_purpose::STANDARD.decode(&response.body_base64)
    else {
        response.body_base64.clear();
        response.stored_bytes = 0;
        response.body_omitted_reason = Some("invalid_base64".into());
        return;
    };
    if response.received_bytes == 0 {
        response.received_bytes = bytes.len() as u64;
    }
    let supplied_digest_is_valid = response
        .full_body_sha256
        .as_deref()
        .is_some_and(valid_sha256);
    if !supplied_digest_is_valid {
        response.full_body_sha256 = None;
        if !bytes.is_empty() && !response.truncated && response.received_bytes == bytes.len() as u64
        {
            response.full_body_sha256 = Some(hex::encode(Sha256::digest(&bytes)));
        }
    }
    let content_type = response
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.as_str());
    let binary = std::str::from_utf8(&bytes).is_err()
        || content_type.is_some_and(|value| {
            let media_type = value.split(';').next().unwrap_or(value).trim();
            media_type.starts_with("image/")
                || media_type.starts_with("audio/")
                || media_type.starts_with("video/")
                || media_type.starts_with("font/")
                || media_type.eq_ignore_ascii_case("application/octet-stream")
        });
    let omitted = if response.sensitive && !policy.store_sensitive {
        Some("sensitive")
    } else if binary && !policy.store_binary {
        Some("binary")
    } else {
        None
    };
    if let Some(reason) = omitted {
        response.body_base64.clear();
        response.stored_bytes = 0;
        response.body_omitted_reason = Some(reason.into());
        return;
    }
    if bytes.len() > policy.max_body_bytes {
        bytes.truncate(policy.max_body_bytes);
        response.truncated = true;
        response.body_omitted_reason = Some("size_limit".into());
    }
    response.stored_bytes = bytes.len() as u64;
    response.body_base64 = base64::engine::general_purpose::STANDARD.encode(bytes);
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn apply_request_body_policy(
    request: &mut super::RedactedRequestSnapshot,
    policy: HistoryBodyPolicy,
) {
    if request.body_bytes == 0 {
        request.body_bytes = u64::try_from(request.body.len()).unwrap_or(u64::MAX);
    }
    let omitted = if request.body_sensitive && !policy.store_sensitive {
        Some("sensitive")
    } else if request.body_binary && !policy.store_binary {
        Some("binary")
    } else {
        None
    };
    if let Some(reason) = omitted {
        request.body.clear();
        if let Some(replay) = &mut request.replay {
            replay.body = Body::None;
        }
        request.body_omitted_reason = Some(reason.into());
        return;
    }
    if request.body.len() > policy.max_body_bytes {
        truncate_utf8(&mut request.body, policy.max_body_bytes);
        // A partially retained typed body would look executable while no
        // longer representing the submitted request. Keep the bounded wire
        // preview and make the replay limitation explicit instead.
        if let Some(replay) = &mut request.replay {
            replay.body = Body::None;
        }
        request.body_truncated = true;
        request.body_omitted_reason = Some("size_limit".into());
    }
}

fn truncate_utf8(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
}

fn reject_url_userinfo(value: &str) -> StoreResult<()> {
    if super::persistence_safety::url_has_userinfo(value) {
        return Err(StoreError::InvalidInput(
            "exchange request URL must not include user information".into(),
        ));
    }
    Ok(())
}

fn reject_snapshot_url_userinfo(value: &super::RedactedRequestSnapshot) -> StoreResult<()> {
    reject_url_userinfo(&value.url)?;
    if let Some(replay) = &value.replay {
        reject_url_userinfo(&replay.url)?;
    }
    Ok(())
}

fn query_json<T, P>(connection: &Connection, sql: &str, params: P) -> StoreResult<Vec<T>>
where
    T: serde::de::DeserializeOwned,
    P: rusqlite::Params,
{
    let mut statement = connection.prepare(sql)?;
    let rows = statement.query_map(params, |row| row.get::<_, String>(0))?;
    decode_rows(rows)
}

fn query_one_json<T, P>(connection: &Connection, sql: &str, params: P) -> StoreResult<Option<T>>
where
    T: serde::de::DeserializeOwned,
    P: rusqlite::Params,
{
    let json: Option<String> = connection
        .query_row(sql, params, |row| row.get(0))
        .optional()?;
    json.map(|value| serde_json::from_str(&value).map_err(StoreError::from))
        .transpose()
}

fn decode_rows<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<String>>,
) -> StoreResult<Vec<T>>
where
    T: serde::de::DeserializeOwned,
{
    let mut values = Vec::new();
    for row in rows {
        values.push(serde_json::from_str(&row?)?);
    }
    Ok(values)
}

fn validate_name(kind: &str, name: &str) -> StoreResult<()> {
    if name.trim().is_empty() {
        return Err(StoreError::InvalidInput(format!(
            "{kind} name cannot be empty"
        )));
    }
    Ok(())
}

fn reject_scope_move(
    connection: &Connection,
    table: &str,
    id: &str,
    scope_column: &str,
    expected_scope: &str,
    kind: &str,
) -> StoreResult<()> {
    debug_assert!(matches!(
        (table, scope_column),
        ("workbench_collections", "workspace_id")
            | ("workbench_folders", "collection_id")
            | ("workbench_requests", "collection_id")
            | ("workbench_environments", "workspace_id")
            | ("workbench_examples", "request_id")
            | ("workbench_history", "workspace_id")
            | ("workbench_runs", "workspace_id")
    ));
    let existing: Option<String> = connection
        .query_row(
            &format!("SELECT {scope_column} FROM {table} WHERE id=?1"),
            [id],
            |row| row.get(0),
        )
        .optional()?;
    if existing.is_some_and(|scope| scope != expected_scope) {
        return Err(StoreError::InvalidInput(format!(
            "{kind} id already belongs to another {scope_column}"
        )));
    }
    Ok(())
}

fn require_collection(connection: &Connection, id: &CollectionId) -> StoreResult<WorkspaceId> {
    let workspace: Option<String> = connection
        .query_row(
            "SELECT workspace_id FROM workbench_collections WHERE id=?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    workspace
        .ok_or_else(|| StoreError::InvalidInput("collection does not exist".into()))
        .and_then(|workspace| WorkspaceId::new(workspace).map_err(StoreError::InvalidInput))
}

fn require_request_workspace(connection: &Connection, id: &RequestId) -> StoreResult<WorkspaceId> {
    let workspace: Option<String> = connection
        .query_row(
            "SELECT c.workspace_id FROM workbench_requests r
             JOIN workbench_collections c ON c.id=r.collection_id WHERE r.id=?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    workspace
        .ok_or_else(|| StoreError::InvalidInput("request does not exist".into()))
        .and_then(|workspace| WorkspaceId::new(workspace).map_err(StoreError::InvalidInput))
}

fn validate_folder_parent(connection: &Connection, folder: &Folder) -> StoreResult<()> {
    let Some(parent_id) = &folder.parent_id else {
        return Ok(());
    };
    if parent_id == &folder.id {
        return Err(StoreError::InvalidInput(
            "folder cannot be its own parent".into(),
        ));
    }
    let mut cursor = Some(parent_id.clone());
    let mut visited = BTreeSet::new();
    while let Some(id) = cursor {
        if id == folder.id || !visited.insert(id.clone()) {
            return Err(StoreError::InvalidInput(
                "folder hierarchy would contain a cycle".into(),
            ));
        }
        let parent: Option<(String, Option<String>)> = connection
            .query_row(
                "SELECT collection_id, parent_id FROM workbench_folders WHERE id=?1",
                [id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((collection_id, next)) = parent else {
            return Err(StoreError::InvalidInput(
                "folder parent does not exist".into(),
            ));
        };
        if collection_id != folder.collection_id.as_str() {
            return Err(StoreError::InvalidInput(
                "folder parent belongs to another collection".into(),
            ));
        }
        cursor = next
            .map(FolderId::parse)
            .transpose()
            .map_err(StoreError::InvalidInput)?;
    }
    Ok(())
}

fn validate_request_folder(connection: &Connection, request: &SavedRequest) -> StoreResult<()> {
    let Some(folder) = &request.folder_id else {
        return Ok(());
    };
    let collection: Option<String> = connection
        .query_row(
            "SELECT collection_id FROM workbench_folders WHERE id=?1",
            [folder.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    match collection {
        Some(collection) if collection == request.collection_id.as_str() => Ok(()),
        Some(_) => Err(StoreError::InvalidInput(
            "request folder belongs to another collection".into(),
        )),
        None => Err(StoreError::InvalidInput(
            "request folder does not exist".into(),
        )),
    }
}

fn folder_descends_from(
    connection: &Connection,
    folder: &str,
    root: &FolderId,
) -> StoreResult<bool> {
    let mut current = Some(folder.to_string());
    let mut visited = BTreeSet::new();
    while let Some(id) = current {
        if id == root.as_str() {
            return Ok(true);
        }
        if !visited.insert(id.clone()) {
            return Err(StoreError::InvalidInput(
                "run folder scope contains a cycle".into(),
            ));
        }
        current = connection
            .query_row(
                "SELECT parent_id FROM workbench_folders WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
    }
    Ok(false)
}

fn insert_collection(transaction: &Transaction<'_>, value: &Collection) -> StoreResult<()> {
    validate_name("collection", &value.name)?;
    let safe = persistence_safe_collection(value);
    transaction.execute(
        "INSERT INTO workbench_collections(id, workspace_id, name, definition_json, updated_at)
         VALUES (?1, ?2, ?3, ?4, unixepoch() * 1000)",
        params![
            safe.id.as_str(),
            safe.workspace_id.as_str(),
            safe.name,
            serde_json::to_string(&safe)?
        ],
    )?;
    Ok(())
}

fn insert_folder(transaction: &Transaction<'_>, value: &Folder) -> StoreResult<()> {
    validate_name("folder", &value.name)?;
    let safe = persistence_safe_folder(value);
    transaction.execute(
        "INSERT INTO workbench_folders(id, collection_id, parent_id, name, sort_key, definition_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![safe.id.as_str(), safe.collection_id.as_str(), safe.parent_id.as_ref().map(FolderId::as_str), safe.name, safe.sort_key, serde_json::to_string(&safe)?],
    )?;
    Ok(())
}

fn insert_request(transaction: &Transaction<'_>, value: &SavedRequest) -> StoreResult<()> {
    validate_name("request", &value.name)?;
    let safe = persistence_safe_saved_request(value).map_err(StoreError::InvalidInput)?;
    transaction.execute(
        "INSERT INTO workbench_requests(id, collection_id, folder_id, name, method, url, sort_key, definition_json, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, unixepoch() * 1000)",
        params![safe.id.as_str(), safe.collection_id.as_str(), safe.folder_id.as_ref().map(FolderId::as_str), safe.name, safe.method.as_str(), safe.url, safe.sort_key, serde_json::to_string(&safe)?],
    )?;
    Ok(())
}

fn insert_environment(transaction: &Transaction<'_>, value: &Environment) -> StoreResult<()> {
    validate_name("environment", &value.name)?;
    let safe = persistence_safe_environment(value);
    transaction.execute(
        "INSERT INTO workbench_environments(id, workspace_id, name, active, label, definition_json, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, unixepoch() * 1000)",
        params![safe.id.as_str(), safe.workspace_id.as_str(), safe.name, i64::from(safe.active), safe.label.as_str(), serde_json::to_string(&safe)?],
    )?;
    Ok(())
}

fn insert_example(transaction: &Transaction<'_>, value: &Example) -> StoreResult<()> {
    validate_name("example", &value.name)?;
    if let Some(request) = &value.request {
        reject_snapshot_url_userinfo(request)?;
    }
    let safe = redact_example(value, &[]);
    transaction.execute(
        "INSERT INTO workbench_examples(id, request_id, name, sort_key, definition_json, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, unixepoch() * 1000)",
        params![
            value.id.as_str(), value.request_id.as_str(), value.name, value.sort_key,
            serde_json::to_string(&safe)?
        ],
    )?;
    Ok(())
}

pub fn validate_import_graph(workspace: &WorkspaceId, imported: &ImportResult) -> StoreResult<()> {
    if imported.collection.workspace_id != *workspace {
        return Err(StoreError::InvalidInput(
            "imported collection belongs to another workspace".into(),
        ));
    }
    validate_name("collection", &imported.collection.name)?;
    let folder_by_id: BTreeMap<&FolderId, &Folder> = imported
        .folders
        .iter()
        .map(|value| (&value.id, value))
        .collect();
    let request_by_id: BTreeMap<&RequestId, &SavedRequest> = imported
        .requests
        .iter()
        .map(|value| (&value.id, value))
        .collect();
    let environment_ids: BTreeSet<&EnvironmentId> = imported
        .environments
        .iter()
        .map(|value| &value.id)
        .collect();
    let example_ids: BTreeSet<&ExampleId> =
        imported.examples.iter().map(|value| &value.id).collect();
    if folder_by_id.len() != imported.folders.len()
        || request_by_id.len() != imported.requests.len()
        || environment_ids.len() != imported.environments.len()
        || example_ids.len() != imported.examples.len()
    {
        return Err(StoreError::InvalidInput(
            "import graph contains duplicate ids".into(),
        ));
    }
    for folder in &imported.folders {
        validate_name("folder", &folder.name)?;
        if folder.collection_id != imported.collection.id {
            return Err(StoreError::InvalidInput(
                "imported folder belongs to another collection".into(),
            ));
        }
        let mut cursor = folder.parent_id.as_ref();
        let mut visited = BTreeSet::new();
        while let Some(parent_id) = cursor {
            if !visited.insert(parent_id) {
                return Err(StoreError::InvalidInput(
                    "imported folder hierarchy contains a cycle".into(),
                ));
            }
            let parent = folder_by_id.get(parent_id).ok_or_else(|| {
                StoreError::InvalidInput("imported folder references a missing parent".into())
            })?;
            if parent.collection_id != imported.collection.id {
                return Err(StoreError::InvalidInput(
                    "imported folder parent belongs to another collection".into(),
                ));
            }
            cursor = parent.parent_id.as_ref();
        }
    }
    for request in &imported.requests {
        validate_name("request", &request.name)?;
        if request.collection_id != imported.collection.id {
            return Err(StoreError::InvalidInput(
                "imported request belongs to another collection".into(),
            ));
        }
        if request
            .folder_id
            .as_ref()
            .is_some_and(|folder| !folder_by_id.contains_key(folder))
        {
            return Err(StoreError::InvalidInput(
                "imported request references a missing folder".into(),
            ));
        }
    }
    if imported
        .environments
        .iter()
        .filter(|value| value.active)
        .count()
        > 1
    {
        return Err(StoreError::InvalidInput(
            "import contains more than one active environment".into(),
        ));
    }
    for environment in &imported.environments {
        validate_name("environment", &environment.name)?;
        if environment.workspace_id != *workspace {
            return Err(StoreError::InvalidInput(
                "imported environment belongs to another workspace".into(),
            ));
        }
    }
    for example in &imported.examples {
        validate_name("example", &example.name)?;
        if !request_by_id.contains_key(&example.request_id) {
            return Err(StoreError::InvalidInput(
                "imported example references a missing request".into(),
            ));
        }
    }
    Ok(())
}

fn validate_import_selection(
    imported: &ImportResult,
    selection: &ImportSelection,
) -> StoreResult<()> {
    let folder_ids: BTreeSet<&FolderId> = imported.folders.iter().map(|value| &value.id).collect();
    let request_ids: BTreeSet<&RequestId> =
        imported.requests.iter().map(|value| &value.id).collect();
    let environment_ids: BTreeSet<&EnvironmentId> = imported
        .environments
        .iter()
        .map(|value| &value.id)
        .collect();
    let example_ids: BTreeSet<&ExampleId> =
        imported.examples.iter().map(|value| &value.id).collect();
    if selection
        .folder_ids
        .iter()
        .any(|id| !folder_ids.contains(id))
        || selection
            .request_ids
            .iter()
            .any(|id| !request_ids.contains(id))
        || selection
            .environment_ids
            .iter()
            .any(|id| !environment_ids.contains(id))
        || selection
            .example_ids
            .iter()
            .any(|id| !example_ids.contains(id))
    {
        return Err(StoreError::InvalidInput(
            "import selection contains an unknown id".into(),
        ));
    }
    if !selection.include_collection
        && (!selection.folder_ids.is_empty()
            || !selection.request_ids.is_empty()
            || !selection.example_ids.is_empty())
    {
        return Err(StoreError::InvalidInput(
            "folders, requests, and examples require the imported collection".into(),
        ));
    }
    for folder in imported
        .folders
        .iter()
        .filter(|folder| selection.folder_ids.contains(&folder.id))
    {
        if folder
            .parent_id
            .as_ref()
            .is_some_and(|parent| !selection.folder_ids.contains(parent))
        {
            return Err(StoreError::InvalidInput(
                "selected folder requires its parent folder".into(),
            ));
        }
    }
    for request in imported
        .requests
        .iter()
        .filter(|request| selection.request_ids.contains(&request.id))
    {
        if request
            .folder_id
            .as_ref()
            .is_some_and(|folder| !selection.folder_ids.contains(folder))
        {
            return Err(StoreError::InvalidInput(
                "selected request requires its folder".into(),
            ));
        }
    }
    for example in imported
        .examples
        .iter()
        .filter(|example| selection.example_ids.contains(&example.id))
    {
        if !selection.request_ids.contains(&example.request_id) {
            return Err(StoreError::InvalidInput(
                "selected example requires its request".into(),
            ));
        }
    }
    Ok(())
}

fn selected_folders_in_parent_order<'a>(
    imported: &'a ImportResult,
    selection: &ImportSelection,
) -> StoreResult<Vec<&'a Folder>> {
    let mut remaining: Vec<&Folder> = imported
        .folders
        .iter()
        .filter(|folder| selection.folder_ids.contains(&folder.id))
        .collect();
    let mut inserted = BTreeSet::new();
    let mut ordered = Vec::with_capacity(remaining.len());
    while !remaining.is_empty() {
        let Some(position) = remaining.iter().position(|folder| {
            folder
                .parent_id
                .as_ref()
                .is_none_or(|parent| inserted.contains(parent))
        }) else {
            return Err(StoreError::InvalidInput(
                "selected folder hierarchy cannot be ordered".into(),
            ));
        };
        let folder = remaining.remove(position);
        inserted.insert(folder.id.clone());
        ordered.push(folder);
    }
    Ok(ordered)
}

fn migrate(connection: &mut Connection) -> StoreResult<()> {
    let mut current: i32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if current > SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema(current));
    }
    if current < 1 {
        let transaction = connection.transaction()?;
        transaction.execute_batch(
            "CREATE TABLE workbench_schema_migrations (
             version INTEGER PRIMARY KEY,
             applied_at INTEGER NOT NULL
         );
         CREATE TABLE workbench_collections (
             id TEXT PRIMARY KEY,
             workspace_id TEXT NOT NULL,
             name TEXT NOT NULL,
             definition_json TEXT NOT NULL,
             updated_at INTEGER NOT NULL
         );
         CREATE INDEX workbench_collections_workspace
             ON workbench_collections(workspace_id, name);
         CREATE TABLE workbench_folders (
             id TEXT PRIMARY KEY,
             collection_id TEXT NOT NULL REFERENCES workbench_collections(id) ON DELETE CASCADE,
             parent_id TEXT REFERENCES workbench_folders(id) ON DELETE CASCADE,
             name TEXT NOT NULL,
             sort_key INTEGER NOT NULL,
             definition_json TEXT NOT NULL
         );
         CREATE INDEX workbench_folders_collection
             ON workbench_folders(collection_id, parent_id, sort_key);
         CREATE TABLE workbench_requests (
             id TEXT PRIMARY KEY,
             collection_id TEXT NOT NULL REFERENCES workbench_collections(id) ON DELETE CASCADE,
             folder_id TEXT REFERENCES workbench_folders(id) ON DELETE SET NULL,
             name TEXT NOT NULL,
             method TEXT NOT NULL,
             url TEXT NOT NULL,
             sort_key INTEGER NOT NULL,
             definition_json TEXT NOT NULL,
             updated_at INTEGER NOT NULL
         );
         CREATE INDEX workbench_requests_collection
             ON workbench_requests(collection_id, folder_id, sort_key);
         CREATE TABLE workbench_environments (
             id TEXT PRIMARY KEY,
             workspace_id TEXT NOT NULL,
             name TEXT NOT NULL,
             active INTEGER NOT NULL DEFAULT 0 CHECK(active IN (0, 1)),
             definition_json TEXT NOT NULL,
             updated_at INTEGER NOT NULL
         );
         CREATE UNIQUE INDEX workbench_one_active_environment
             ON workbench_environments(workspace_id) WHERE active=1;
         CREATE TABLE workbench_history (
             id TEXT PRIMARY KEY,
             workspace_id TEXT NOT NULL,
             request_id TEXT,
             started_at INTEGER NOT NULL,
             completed_at INTEGER NOT NULL,
             snapshot_json TEXT NOT NULL
         );
         CREATE INDEX workbench_history_workspace
             ON workbench_history(workspace_id, completed_at DESC);
         CREATE TABLE workbench_runs (
             id TEXT PRIMARY KEY,
             workspace_id TEXT NOT NULL,
             collection_id TEXT NOT NULL,
             started_at INTEGER NOT NULL,
             definition_json TEXT NOT NULL
         );
         CREATE INDEX workbench_runs_workspace
             ON workbench_runs(workspace_id, started_at DESC);",
        )?;
        transaction.execute(
            "INSERT INTO workbench_schema_migrations(version, applied_at)
             VALUES (1, unixepoch() * 1000)",
            [],
        )?;
        transaction.pragma_update(None, "user_version", 1)?;
        transaction.commit()?;
        current = 1;
    }
    if current < 2 {
        let transaction = connection.transaction()?;
        transaction.execute_batch(
            "CREATE TABLE workbench_examples (
                 id TEXT PRIMARY KEY,
                 request_id TEXT NOT NULL REFERENCES workbench_requests(id) ON DELETE CASCADE,
                 name TEXT NOT NULL,
                 sort_key INTEGER NOT NULL,
                 definition_json TEXT NOT NULL,
                 updated_at INTEGER NOT NULL
             );
             CREATE INDEX workbench_examples_request
                 ON workbench_examples(request_id, sort_key, name);",
        )?;
        transaction.execute(
            "INSERT INTO workbench_schema_migrations(version, applied_at)
             VALUES (2, unixepoch() * 1000)",
            [],
        )?;
        transaction.pragma_update(None, "user_version", 2)?;
        transaction.commit()?;
        current = 2;
    }
    if current < 3 {
        let transaction = connection.transaction()?;
        transaction.execute_batch(
            "CREATE TABLE workbench_cookie_jars (
                 workspace_id TEXT PRIMARY KEY,
                 metadata_json TEXT NOT NULL,
                 updated_at INTEGER NOT NULL
             );",
        )?;
        transaction.execute(
            "INSERT INTO workbench_schema_migrations(version, applied_at)
             VALUES (3, unixepoch() * 1000)",
            [],
        )?;
        transaction.pragma_update(None, "user_version", 3)?;
        transaction.commit()?;
    }
    if current < 4 {
        let transaction = connection.transaction()?;
        transaction.execute_batch("CREATE TABLE workbench_globals (workspace_id TEXT PRIMARY KEY, metadata_json TEXT NOT NULL);")?;
        transaction.execute("INSERT INTO workbench_schema_migrations(version, applied_at) VALUES (4, unixepoch() * 1000)", [])?;
        transaction.pragma_update(None, "user_version", 4)?;
        transaction.commit()?;
    }
    if current < 5 {
        let transaction = connection.transaction()?;
        transaction.execute_batch(
            "CREATE TABLE workbench_workspaces (
                 id TEXT PRIMARY KEY,
                 name TEXT NOT NULL,
                 created_at INTEGER NOT NULL,
                 -- Rises on every open; the highest is reopened at launch.
                 opened_order INTEGER NOT NULL
             );",
        )?;
        // Data saved before workspaces had names lives under an implicit
        // scope (the project path); list each one so it stays reachable.
        let existing = {
            let mut statement = transaction.prepare(
                "SELECT workspace_id FROM workbench_collections
                 UNION SELECT workspace_id FROM workbench_environments
                 UNION SELECT workspace_id FROM workbench_history
                 UNION SELECT workspace_id FROM workbench_runs
                 UNION SELECT workspace_id FROM workbench_globals
                 ORDER BY 1",
            )?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for id in existing {
            transaction.execute(
                "INSERT INTO workbench_workspaces(id, name, created_at, opened_order)
                 VALUES (?1, ?2, unixepoch() * 1000, 0)",
                params![id, implicit_workspace_name(&id)],
            )?;
        }
        transaction.execute(
            "INSERT INTO workbench_schema_migrations(version, applied_at)
             VALUES (5, unixepoch() * 1000)",
            [],
        )?;
        transaction.pragma_update(None, "user_version", 5)?;
        transaction.commit()?;
    }
    if current < 6 {
        let transaction = connection.transaction()?;
        // The label (Production / Staging / Development / Local) lives in
        // definition_json like every other field; the column mirrors it so
        // it can be read without decoding the definition.
        transaction.execute_batch(
            "ALTER TABLE workbench_environments
                 ADD COLUMN label TEXT NOT NULL DEFAULT 'local'
                 CHECK(label IN ('production', 'staging', 'development', 'local'));
             UPDATE workbench_environments
                 SET label=json_extract(definition_json, '$.label')
                 WHERE json_extract(definition_json, '$.label')
                     IN ('production', 'staging', 'development');",
        )?;
        transaction.execute(
            "INSERT INTO workbench_schema_migrations(version, applied_at)
             VALUES (6, unixepoch() * 1000)",
            [],
        )?;
        transaction.pragma_update(None, "user_version", 6)?;
        transaction.commit()?;
    }
    if current < 7 {
        let transaction = connection.transaction()?;
        // Saved request ids only (never draft contents), keyed by workspace.
        transaction.execute_batch(
            "CREATE TABLE workbench_tab_sessions (
                 workspace_id TEXT PRIMARY KEY,
                 session_json TEXT NOT NULL,
                 updated_at INTEGER NOT NULL
             );",
        )?;
        transaction.execute(
            "INSERT INTO workbench_schema_migrations(version, applied_at)
             VALUES (7, unixepoch() * 1000)",
            [],
        )?;
        transaction.pragma_update(None, "user_version", 7)?;
        transaction.commit()?;
    }
    Ok(())
}

/// A readable name for a pre-workspace scope: the last segment of its
/// project path (`/home/me/shop` → `shop`), else the scope itself.
fn implicit_workspace_name(id: &str) -> String {
    id.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|segment| !segment.is_empty())
        .unwrap_or(id)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AuthConfig, Body, EnvironmentLabel, Example, ExampleId, HttpMethod, ImportFormat,
        ImportSelection, MemorySecretStore, RequestSettings, ResponseSnapshot, Scripts, SecretRef,
        Variable, VariableValue,
    };
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "agentops-workbench-{name}-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn collection(workspace: &WorkspaceId, name: &str) -> Collection {
        Collection {
            id: CollectionId::new(),
            workspace_id: workspace.clone(),
            name: name.into(),
            description: String::new(),
            auth: AuthConfig::None,
            variables: Vec::new(),
            scripts: Scripts::default(),
            extensions: Default::default(),
        }
    }

    fn request(collection: &Collection, name: &str) -> SavedRequest {
        SavedRequest {
            id: RequestId::new(),
            collection_id: collection.id.clone(),
            folder_id: None,
            name: name.into(),
            method: HttpMethod::get(),
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
        }
    }

    #[test]
    fn request_rename_preserves_blank_url_and_latest_definition_and_rejects_stale_names() {
        let scratch = Scratch::new("request-rename");
        let store = WorkbenchStore::open(&scratch.0).unwrap();
        let workspace = WorkspaceId::new("project").unwrap();
        let collection = collection(&workspace, "Requests");
        store.upsert_collection(&collection).unwrap();
        let mut saved = request(&collection, "Original");
        saved.url.clear();
        saved.scripts.pre_request = "console.log('retained')".into();
        store.upsert_request(&saved).unwrap();
        let renamed = store
            .rename_request(
                &workspace,
                &collection.id,
                &saved.id,
                "Original",
                "  Renamed  ",
            )
            .unwrap();
        let mut expected = saved.clone();
        expected.name = "Renamed".into();
        assert_eq!(
            serde_json::to_value(&renamed).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        assert!(
            store
                .rename_request(&workspace, &collection.id, &saved.id, "Original", "Stale")
                .is_err()
        );
        assert!(
            store
                .rename_request(&workspace, &collection.id, &saved.id, "Renamed", "  ")
                .is_err()
        );
        assert!(
            store
                .rename_request(
                    &WorkspaceId::new("other").unwrap(),
                    &collection.id,
                    &saved.id,
                    "Renamed",
                    "Wrong scope"
                )
                .is_err()
        );
        assert_eq!(
            store
                .request(&collection.id, &saved.id)
                .unwrap()
                .unwrap()
                .name,
            "Renamed"
        );
    }

    #[test]
    fn request_moves_preserve_identity_and_credentials_across_collections_and_folders() {
        let scratch = Scratch::new("request-move");
        let store = WorkbenchStore::open(&scratch.0).unwrap();
        let workspace = WorkspaceId::new("project").unwrap();
        let source = collection(&workspace, "Source");
        let destination = collection(&workspace, "Destination");
        store.upsert_collection(&source).unwrap();
        store.upsert_collection(&destination).unwrap();
        let folder = Folder {
            id: FolderId::new(),
            collection_id: destination.id.clone(),
            parent_id: None,
            name: "Sessions".into(),
            auth: AuthConfig::None,
            variables: Vec::new(),
            scripts: Scripts::default(),
            sort_key: 0,
            extensions: Default::default(),
        };
        store.upsert_folder(&folder).unwrap();
        let mut saved = request(&source, "Login");
        saved.auth = AuthConfig::Basic {
            username: "operator".into(),
            password: super::super::SecretRef::new("vault-password").unwrap(),
        };
        saved.scripts.pre_request = "pm.variables.set('kept', 'yes')".into();
        store.upsert_request(&saved).unwrap();
        let moved = store
            .move_request(
                &workspace,
                &source.id,
                &saved.id,
                &destination.id,
                Some(&folder.id),
            )
            .unwrap();
        assert!(store.request(&source.id, &saved.id).unwrap().is_none());
        let mut expected = saved.clone();
        expected.collection_id = destination.id.clone();
        expected.folder_id = Some(folder.id.clone());
        assert_eq!(moved, expected);
        assert_eq!(
            store.request(&destination.id, &saved.id).unwrap(),
            Some(expected)
        );
        let root = store
            .move_request(
                &workspace,
                &destination.id,
                &saved.id,
                &destination.id,
                None,
            )
            .unwrap();
        assert_eq!(root.id, saved.id);
        assert!(root.folder_id.is_none());
        assert_eq!(root.auth, saved.auth);
        assert_eq!(root.scripts, saved.scripts);
    }

    #[test]
    fn request_moves_reject_foreign_workspaces_folders_and_stale_sources_without_changes() {
        let scratch = Scratch::new("request-move-invalid");
        let store = WorkbenchStore::open(&scratch.0).unwrap();
        let workspace = WorkspaceId::new("project").unwrap();
        let source = collection(&workspace, "Source");
        let destination = collection(&workspace, "Destination");
        let foreign = collection(&WorkspaceId::new("other").unwrap(), "Other");
        for collection in [&source, &destination, &foreign] {
            store.upsert_collection(collection).unwrap();
        }
        let saved = request(&source, "Kept");
        store.upsert_request(&saved).unwrap();
        assert!(
            store
                .move_request(&workspace, &source.id, &saved.id, &foreign.id, None)
                .is_err()
        );
        assert!(
            store
                .move_request(
                    &workspace,
                    &source.id,
                    &saved.id,
                    &destination.id,
                    Some(&FolderId::new())
                )
                .is_err()
        );
        assert!(
            store
                .move_request(&workspace, &destination.id, &saved.id, &source.id, None)
                .is_err()
        );
        assert_eq!(store.request(&source.id, &saved.id).unwrap(), Some(saved));
        assert!(store.list_requests(&destination.id).unwrap().is_empty());
        assert!(store.list_requests(&foreign.id).unwrap().is_empty());
    }

    #[test]
    fn request_url_batch_rolls_back_stale_or_invalid_updates_and_preserves_latest_fields() {
        let scratch = Scratch::new("request-url-batch");
        let store = WorkbenchStore::open(&scratch.0).unwrap();
        let workspace = WorkspaceId::new("project").unwrap();
        let collection = collection(&workspace, "API");
        store.upsert_collection(&collection).unwrap();
        let mut first = request(&collection, "First");
        first.url = "{{basepath}}/first".into();
        first.body = Body::Raw {
            media_type: super::super::RawBodyKind::Text,
            text: "{{basepath}}/body-must-stay".into(),
        };
        first.auth = serde_json::from_value(serde_json::json!({
            "kind": "login", "url": "{{basepath}}/auth", "token_path": "access_token"
        }))
        .unwrap();
        let mut second = request(&collection, "Second");
        second.url = "{{basepath}}/second".into();
        store.upsert_request(&first).unwrap();
        store.upsert_request(&second).unwrap();
        let mut updates = vec![
            RequestUrlUpdate {
                request_id: first.id.clone(),
                original_url: first.url.clone(),
                url: "/first".into(),
            },
            RequestUrlUpdate {
                request_id: second.id.clone(),
                original_url: second.url.clone(),
                url: "/second".into(),
            },
        ];
        for invalid in ["", "https://user:password@example.test/second"] {
            updates[1].url = invalid.into();
            assert!(
                store
                    .update_request_urls(&workspace, &collection.id, &updates)
                    .is_err()
            );
            assert_eq!(
                store.request(&collection.id, &first.id).unwrap().unwrap(),
                first
            );
        }
        updates[1].url = "/second".into();
        second.url = "/concurrent-edit".into();
        store.upsert_request(&second).unwrap();
        let error = store
            .update_request_urls(&workspace, &collection.id, &updates)
            .unwrap_err();
        assert!(error.to_string().contains("changed since the URL preview"));
        assert_eq!(
            store.request(&collection.id, &first.id).unwrap().unwrap(),
            first
        );
        assert_eq!(
            store.request(&collection.id, &second.id).unwrap().unwrap(),
            second
        );

        // Non-URL edits after preview must survive applying the URL batch.
        first.name = "Renamed after preview".into();
        first.scripts.tests = "check the latest response".into();
        store.upsert_request(&first).unwrap();
        let mut serialized = serde_json::to_value(&first).unwrap();
        serialized["future_field"] = serde_json::json!({"keep":"{{basepath}}"});
        store
            .conn()
            .execute(
                "UPDATE workbench_requests SET definition_json=?1 WHERE id=?2",
                params![serialized.to_string(), first.id.as_str()],
            )
            .unwrap();
        updates[1].original_url = second.url.clone();
        store
            .update_request_urls(&workspace, &collection.id, &updates)
            .unwrap();
        first.url = "/first".into();
        second.url = "/second".into();
        assert_eq!(
            store.request(&collection.id, &first.id).unwrap().unwrap(),
            first
        );
        assert_eq!(
            store.request(&collection.id, &second.id).unwrap().unwrap(),
            second
        );
        let (url, json): (String, String) = store
            .conn()
            .query_row(
                "SELECT url, definition_json FROM workbench_requests WHERE id=?1",
                [first.id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(url, "/first");
        serialized["url"] = serde_json::json!("/first");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).unwrap(),
            serialized
        );
    }

    #[test]
    fn request_url_batch_rejects_foreign_missing_and_duplicate_requests() {
        let scratch = Scratch::new("request-url-scope");
        let store = WorkbenchStore::open(&scratch.0).unwrap();
        let workspace = WorkspaceId::new("project").unwrap();
        let primary = collection(&workspace, "Primary");
        let other = collection(&workspace, "Other");
        store.upsert_collection(&primary).unwrap();
        store.upsert_collection(&other).unwrap();
        let first = request(&primary, "First");
        let second = request(&other, "Second");
        store.upsert_request(&first).unwrap();
        store.upsert_request(&second).unwrap();
        let update = RequestUrlUpdate {
            request_id: first.id.clone(),
            original_url: first.url.clone(),
            url: "/updated".into(),
        };
        assert!(
            store
                .update_request_urls(
                    &WorkspaceId::new("foreign").unwrap(),
                    &primary.id,
                    std::slice::from_ref(&update)
                )
                .is_err()
        );
        for request_id in [second.id.clone(), RequestId::new(), first.id.clone()] {
            let invalid = RequestUrlUpdate {
                request_id,
                ..update.clone()
            };
            assert!(
                store
                    .update_request_urls(&workspace, &primary.id, &[update.clone(), invalid])
                    .is_err()
            );
            assert_eq!(
                store.request(&primary.id, &first.id).unwrap().unwrap(),
                first
            );
            assert_eq!(
                store.request(&other.id, &second.id).unwrap().unwrap(),
                second
            );
        }
    }

    #[test]
    fn reopens_and_isolates_workspaces() {
        let scratch = Scratch::new("isolation");
        let first = WorkspaceId::new("/projects/first").unwrap();
        let second = WorkspaceId::new("/projects/second").unwrap();
        let collection = collection(&first, "First API");
        let saved_request = request(&collection, "List");
        {
            let store = WorkbenchStore::open(&scratch.0).unwrap();
            store.upsert_collection(&collection).unwrap();
            store.upsert_request(&saved_request).unwrap();
            assert!(store.list_collections(&second).unwrap().is_empty());
        }
        let reopened = WorkbenchStore::open(&scratch.0).unwrap();
        assert_eq!(
            reopened.list_collections(&first).unwrap(),
            vec![collection.clone()]
        );
        assert_eq!(
            reopened.list_requests(&collection.id).unwrap(),
            vec![saved_request]
        );
    }

    #[test]
    fn request_upsert_applies_the_canonical_persistence_sanitizer() {
        let scratch = Scratch::new("request-persistence-sanitizer");
        let workspace = WorkspaceId::new("project").unwrap();
        let collection = collection(&workspace, "API");
        let mut request = request(&collection, "Unsafe");
        request.headers.push(super::super::KeyValueRow::enabled(
            "Authorization",
            "Bearer literal-database-secret",
        ));
        request.url = "{{base_url}}/items?api_key=literal-query-database-secret".into();
        let store = WorkbenchStore::open(&scratch.0).unwrap();
        store.upsert_collection(&collection).unwrap();
        store.upsert_request(&request).unwrap();

        let stored = store.request(&collection.id, &request.id).unwrap().unwrap();
        assert_eq!(stored.headers[0].value, "<redacted>");
        assert!(!stored.headers[0].enabled);
        assert!(!stored.url.contains("literal-query-database-secret"));
        let raw: String = store
            .conn()
            .query_row(
                "SELECT definition_json FROM workbench_requests WHERE id=?1",
                [request.id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!raw.contains("literal-database-secret"));
        assert!(!raw.contains("literal-query-database-secret"));

        let mut unsafe_authority = request.clone();
        unsafe_authority.id = RequestId::new();
        unsafe_authority.url = "https://user:literal-password@{{host}}/items".into();
        assert!(matches!(
            store.upsert_request(&unsafe_authority),
            Err(StoreError::InvalidInput(message)) if message.contains("user information")
        ));
        let unsafe_rows: i64 = store.conn()
            .query_row(
                "SELECT count(*) FROM workbench_requests WHERE definition_json LIKE '%literal-password%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(unsafe_rows, 0);

        let exchange = Exchange {
            console: Vec::new(),
            test_results: Vec::new(),
            id: super::super::ExchangeId::new(),
            workspace_id: workspace,
            request_id: Some(request.id.clone()),
            request: super::super::RedactedRequestSnapshot {
                method: "GET".into(),
                url: "{{base_url}}?access_token=literal-history-query-secret".into(),
                replay: Some(super::super::ReplayRequestSnapshot {
                    name: "Replay".into(),
                    method: HttpMethod::get(),
                    url: "{{base_url}}?api_key=literal-replay-query-secret".into(),
                    params: Vec::new(),
                    headers: vec![super::super::KeyValueRow::enabled(
                        "Authorization",
                        "Bearer literal-replay-header-secret",
                    )],
                    auth: AuthConfig::None,
                    body: Body::None,
                    variables: Vec::new(),
                    scripts: Scripts::default(),
                    settings: RequestSettings::default(),
                }),
                ..Default::default()
            },
            response: None,
            error: None,
            started_at: 1,
            completed_at: 2,
        };
        store.record_exchange(&exchange, &[]).unwrap();
        let raw_history: String = store
            .conn()
            .query_row(
                "SELECT snapshot_json FROM workbench_history WHERE id=?1",
                [exchange.id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        for secret in [
            "literal-history-query-secret",
            "literal-replay-query-secret",
            "literal-replay-header-secret",
        ] {
            assert!(!raw_history.contains(secret));
        }
    }

    #[test]
    fn collection_folder_and_environment_literals_never_enter_raw_sqlite_json() {
        let scratch = Scratch::new("container-persistence-sanitizer");
        let workspace = WorkspaceId::new("project").unwrap();
        let mut collection = collection(&workspace, "API");
        collection.variables.push(Variable {
            id: Default::default(),
            key: "access_token".into(),
            value: VariableValue::Plain("collection-plain-secret".into()),
            enabled: true,
            description: String::new(),
        });
        collection.extensions.insert(
            "x-auth".into(),
            serde_json::json!({"value":"collection-extension-secret"}),
        );
        let folder = Folder {
            id: super::super::FolderId::new(),
            collection_id: collection.id.clone(),
            parent_id: None,
            name: "Folder".into(),
            auth: AuthConfig::Unsupported {
                name: "custom".into(),
                raw: serde_json::json!({"nested":{"value":"folder-auth-secret"}}),
            },
            variables: vec![Variable {
                id: Default::default(),
                key: "client_secret".into(),
                value: VariableValue::Plain("folder-plain-secret".into()),
                enabled: true,
                description: String::new(),
            }],
            scripts: Scripts::default(),
            sort_key: 0,
            extensions: Default::default(),
        };
        let environment = Environment {
            label: Default::default(),
            id: EnvironmentId::new(),
            workspace_id: workspace,
            name: "Environment".into(),
            base_url: String::new(),
            auth: Default::default(),
            variables: vec![Variable {
                id: Default::default(),
                key: "api_key".into(),
                value: VariableValue::Plain("environment-plain-secret".into()),
                enabled: true,
                description: String::new(),
            }],
            active: true,
            extensions: serde_json::from_value(serde_json::json!({
                "x-auth": {"value":"environment-extension-secret"}
            }))
            .unwrap(),
        };
        let store = WorkbenchStore::open(&scratch.0).unwrap();
        store.upsert_collection(&collection).unwrap();
        store.upsert_folder(&folder).unwrap();
        store.upsert_environment(&environment).unwrap();

        let connection = store.conn();
        let raw = [
            ("workbench_collections", collection.id.as_str()),
            ("workbench_folders", folder.id.as_str()),
            ("workbench_environments", environment.id.as_str()),
        ]
        .into_iter()
        .map(|(table, id)| {
            connection
                .query_row(
                    &format!("SELECT definition_json FROM {table} WHERE id=?1"),
                    [id],
                    |row| row.get::<_, String>(0),
                )
                .unwrap()
        })
        .collect::<Vec<_>>()
        .join("\n");
        for secret in [
            "collection-plain-secret",
            "collection-extension-secret",
            "folder-auth-secret",
            "folder-plain-secret",
            "environment-plain-secret",
            "environment-extension-secret",
        ] {
            assert!(
                !raw.contains(secret),
                "raw SQLite retained {secret:?}: {raw}"
            );
        }
    }

    #[test]
    fn history_query_save_as_and_delete_are_workspace_scoped() {
        let scratch = Scratch::new("history-actions");
        let workspace = WorkspaceId::new("project").unwrap();
        let foreign = WorkspaceId::new("foreign").unwrap();
        let collection = collection(&workspace, "API");
        let request = request(&collection, "Original");
        let exchange = Exchange {
            console: Vec::new(),
            test_results: Vec::new(),
            id: super::super::ExchangeId::new(),
            workspace_id: workspace.clone(),
            request_id: Some(request.id.clone()),
            request: super::super::RedactedRequestSnapshot {
                method: "GET".into(),
                url: "https://example.test/searchable".into(),
                replay: Some(super::super::ReplayRequestSnapshot {
                    name: "Searchable original".into(),
                    method: HttpMethod::get(),
                    url: "https://example.test/searchable".into(),
                    params: Vec::new(),
                    headers: Vec::new(),
                    auth: AuthConfig::None,
                    body: Body::None,
                    variables: Vec::new(),
                    scripts: Scripts::default(),
                    settings: RequestSettings::default(),
                }),
                ..Default::default()
            },
            response: Some(ResponseSnapshot {
                status: 204,
                ..Default::default()
            }),
            error: None,
            started_at: 1,
            completed_at: 2,
        };
        let store = WorkbenchStore::open(&scratch.0).unwrap();
        store.upsert_collection(&collection).unwrap();
        store.upsert_request(&request).unwrap();
        store.record_exchange(&exchange, &[]).unwrap();

        let matches = store
            .query_history(
                &workspace,
                &HistoryQuery {
                    text: "searchable".into(),
                    status: Some(204),
                    limit: 10,
                },
            )
            .unwrap();
        assert_eq!(matches.len(), 1);
        let saved = store
            .save_exchange_as_request(
                &workspace,
                &exchange.id,
                &collection.id,
                None,
                "Saved replay",
            )
            .unwrap();
        assert_eq!(saved.url, "https://example.test/searchable");
        assert!(!store.delete_exchange(&foreign, &exchange.id).unwrap());
        assert!(store.delete_exchange(&workspace, &exchange.id).unwrap());
        assert!(store.history(&workspace, 10).unwrap().is_empty());
    }

    #[test]
    fn cookie_metadata_survives_reopen_without_persisting_values() {
        let scratch = Scratch::new("cookie-reopen");
        let workspace = WorkspaceId::new("project").unwrap();
        let vault = MemorySecretStore::default();
        let mut jar = CookieJar::new(workspace.clone());
        jar.set_cookie(
            &vault,
            "https://api.example.test/v1/items",
            "session=actual-cookie-value; Path=/v1; Secure; HttpOnly; SameSite=Lax",
            1_700_000_000,
        )
        .unwrap();

        let metadata = serde_json::to_string(&jar).unwrap();
        assert!(!metadata.contains("actual-cookie-value"));
        let expected = jar.clone();
        WorkbenchStore::open(&scratch.0)
            .unwrap()
            .save_cookie_jar(&jar)
            .unwrap();

        let reopened = WorkbenchStore::open(&scratch.0).unwrap();
        assert_eq!(reopened.cookie_jar(&workspace).unwrap(), expected);
        assert_eq!(
            reopened
                .hydrate_workspace(&workspace, 10, 10)
                .unwrap()
                .cookies,
            expected
        );
    }

    #[test]
    fn history_omits_binary_and_sensitive_bodies_without_explicit_opt_in() {
        let scratch = Scratch::new("history-body-policy");
        let workspace = WorkspaceId::new("project").unwrap();
        let store = WorkbenchStore::open(&scratch.0).unwrap();
        for (index, (content_type, sensitive, reason)) in [
            ("application/octet-stream", false, "binary"),
            ("application/json", true, "sensitive"),
        ]
        .into_iter()
        .enumerate()
        {
            let exchange = Exchange {
                console: Vec::new(),
                test_results: Vec::new(),
                id: super::super::ExchangeId::new(),
                workspace_id: workspace.clone(),
                request_id: None,
                request: super::super::RedactedRequestSnapshot {
                    method: "GET".into(),
                    url: "https://example.test".into(),
                    headers: Vec::new(),
                    body: String::new(),
                    ..Default::default()
                },
                response: Some(ResponseSnapshot {
                    status: 200,
                    headers: vec![("Content-Type".into(), content_type.into())],
                    body_base64: base64::engine::general_purpose::STANDARD.encode("private-body"),
                    received_bytes: 12,
                    sensitive,
                    ..ResponseSnapshot::default()
                }),
                error: None,
                started_at: 1,
                completed_at: 2 + index as i64,
            };
            store.record_exchange(&exchange, &[]).unwrap();
            let stored = store.history(&workspace, 1).unwrap().remove(0);
            let response = stored.response.unwrap();
            assert!(response.body_base64.is_empty());
            assert_eq!(response.stored_bytes, 0);
            assert_eq!(response.body_omitted_reason.as_deref(), Some(reason));
            assert_eq!(
                response.full_body_sha256.as_deref(),
                Some("76393f3aa50cb3280496239f697e0b4f29644a27d43fe8e60284323212cadc2e")
            );
        }
    }

    #[test]
    fn history_caps_text_bodies_and_records_byte_counts() {
        let scratch = Scratch::new("history-body-limit");
        let workspace = WorkspaceId::new("project").unwrap();
        let store = WorkbenchStore::open(&scratch.0).unwrap();
        let exchange = Exchange {
            console: Vec::new(),
            test_results: Vec::new(),
            id: super::super::ExchangeId::new(),
            workspace_id: workspace.clone(),
            request_id: None,
            request: super::super::RedactedRequestSnapshot {
                method: "GET".into(),
                url: "https://example.test".into(),
                headers: Vec::new(),
                body: String::new(),
                ..Default::default()
            },
            response: Some(ResponseSnapshot {
                status: 200,
                headers: vec![("Content-Type".into(), "text/plain".into())],
                body_base64: base64::engine::general_purpose::STANDARD.encode("1234567890"),
                ..ResponseSnapshot::default()
            }),
            error: None,
            started_at: 1,
            completed_at: 2,
        };
        store
            .record_exchange_with_policy(
                &exchange,
                &[],
                HistoryBodyPolicy {
                    max_body_bytes: 4,
                    ..HistoryBodyPolicy::default()
                },
            )
            .unwrap();

        let stored = store.history(&workspace, 1).unwrap().remove(0);
        let response = stored.response.unwrap();
        assert_eq!(response.received_bytes, 10);
        assert_eq!(response.stored_bytes, 4);
        assert!(response.truncated);
        assert_eq!(response.body_omitted_reason.as_deref(), Some("size_limit"));
        assert_eq!(
            response.full_body_sha256.as_deref(),
            Some("c775e7b757ede630cd0aa1113bd102661ab38829ca52a6422ab782862f268646")
        );
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(response.body_base64)
                .unwrap(),
            b"1234"
        );
    }

    #[test]
    fn history_applies_the_same_body_ceiling_to_request_snapshots() {
        let scratch = Scratch::new("request-history-body-limit");
        let workspace = WorkspaceId::new("project").unwrap();
        let store = WorkbenchStore::open(&scratch.0).unwrap();
        let exchange = Exchange {
            console: Vec::new(),
            test_results: Vec::new(),
            id: super::super::ExchangeId::new(),
            workspace_id: workspace.clone(),
            request_id: None,
            request: super::super::RedactedRequestSnapshot {
                method: "POST".into(),
                url: "https://example.test/items".into(),
                headers: vec![("Content-Type".into(), "text/plain".into())],
                body: "αβγδε".into(),
                replay: Some(super::super::ReplayRequestSnapshot {
                    name: "Submitted".into(),
                    method: HttpMethod::new("POST").unwrap(),
                    url: "https://example.test/items".into(),
                    params: Vec::new(),
                    headers: Vec::new(),
                    auth: AuthConfig::None,
                    body: Body::Raw {
                        media_type: super::super::RawBodyKind::Text,
                        text: "αβγδε".into(),
                    },
                    variables: Vec::new(),
                    scripts: Scripts::default(),
                    settings: RequestSettings::default(),
                }),
                ..Default::default()
            },
            response: None,
            error: None,
            started_at: 1,
            completed_at: 2,
        };

        store
            .record_exchange_with_policy(
                &exchange,
                &[],
                HistoryBodyPolicy {
                    max_body_bytes: 5,
                    ..HistoryBodyPolicy::default()
                },
            )
            .unwrap();

        let stored = store.history(&workspace, 1).unwrap().remove(0);
        assert_eq!(stored.request.body, "αβ");
        assert_eq!(stored.request.body_bytes, 10);
        assert!(stored.request.body_truncated);
        assert_eq!(
            stored.request.body_omitted_reason.as_deref(),
            Some("size_limit")
        );
        assert!(matches!(
            stored.request.replay.map(|snapshot| snapshot.body),
            Some(Body::None)
        ));
    }

    #[test]
    fn exchange_insertion_prunes_history_in_the_same_operation() {
        let scratch = Scratch::new("history-insertion-prune");
        let workspace = WorkspaceId::new("project").unwrap();
        let store = WorkbenchStore::open(&scratch.0).unwrap();
        let mut inserted = Vec::new();
        for completed_at in 1..=3 {
            let exchange = Exchange {
                console: Vec::new(),
                test_results: Vec::new(),
                id: super::super::ExchangeId::new(),
                workspace_id: workspace.clone(),
                request_id: None,
                request: super::super::RedactedRequestSnapshot {
                    method: "GET".into(),
                    url: format!("https://example.test/{completed_at}"),
                    ..Default::default()
                },
                response: None,
                error: None,
                started_at: completed_at,
                completed_at,
            };
            inserted.push(exchange.clone());
            store
                .record_exchange_with_policy(
                    &exchange,
                    &[],
                    HistoryBodyPolicy {
                        max_entries: 2,
                        ..HistoryBodyPolicy::default()
                    },
                )
                .unwrap();
        }

        assert_eq!(
            store.history(&workspace, 10).unwrap(),
            vec![inserted[2].clone(), inserted[1].clone()]
        );
    }

    #[test]
    fn history_rejects_a_forged_snapshot_with_url_userinfo() {
        let scratch = Scratch::new("history-url-userinfo");
        let workspace = WorkspaceId::new("project").unwrap();
        let store = WorkbenchStore::open(&scratch.0).unwrap();
        let exchange = Exchange {
            console: Vec::new(),
            test_results: Vec::new(),
            id: super::super::ExchangeId::new(),
            workspace_id: workspace.clone(),
            request_id: None,
            request: super::super::RedactedRequestSnapshot {
                method: "GET".into(),
                url: "https://user:literal-password@{{host}}/items".into(),
                ..Default::default()
            },
            response: None,
            error: None,
            started_at: 1,
            completed_at: 2,
        };

        assert!(matches!(
            store.record_exchange(&exchange, &[]),
            Err(StoreError::InvalidInput(message)) if message.contains("user information")
        ));

        let mut replay_exchange = exchange.clone();
        replay_exchange.id = super::super::ExchangeId::new();
        replay_exchange.request.url = "https://example.test/items".into();
        replay_exchange.request.replay = Some(super::super::ReplayRequestSnapshot {
            name: "Forged replay".into(),
            method: HttpMethod::get(),
            url: "https://user:literal-password@{{host}}/items".into(),
            params: Vec::new(),
            headers: Vec::new(),
            auth: AuthConfig::None,
            body: Body::None,
            variables: Vec::new(),
            scripts: Scripts::default(),
            settings: RequestSettings::default(),
        });
        assert!(matches!(
            store.record_exchange(&replay_exchange, &[]),
            Err(StoreError::InvalidInput(message)) if message.contains("user information")
        ));
        assert!(store.history(&workspace, 10).unwrap().is_empty());
        let persisted: i64 = store.conn()
            .query_row(
                "SELECT count(*) FROM workbench_history WHERE snapshot_json LIKE '%literal-password%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(persisted, 0);
    }

    #[test]
    fn secret_references_persist_but_secret_values_do_not() {
        let scratch = Scratch::new("secret-scan");
        let workspace = WorkspaceId::new("secret-project").unwrap();
        let mut collection = collection(&workspace, "Secret API");
        collection.variables.push(Variable {
            id: super::super::RowId::new(),
            key: "token".into(),
            value: VariableValue::Secret(SecretRef::new("vault-ref-1").unwrap()),
            enabled: true,
            description: String::new(),
        });
        let store = WorkbenchStore::open(&scratch.0).unwrap();
        store.upsert_collection(&collection).unwrap();
        let request = request(&collection, "Secret request");
        store.upsert_request(&request).unwrap();
        let example = Example {
            id: ExampleId::new(),
            request_id: request.id.clone(),
            name: "Sanitized".into(),
            request: Some(super::super::RedactedRequestSnapshot {
                method: "GET".into(),
                url: "https://example.test".into(),
                headers: vec![("Authorization".into(), "actual-password".into())],
                body: String::new(),
                ..Default::default()
            }),
            response: ResponseSnapshot {
                status: 200,
                reason: "OK".into(),
                headers: vec![("X-Echo".into(), "actual-password".into())],
                body_base64: base64::engine::general_purpose::STANDARD
                    .encode(b"prefix\0actual-password\xffsuffix"),
                duration_ms: 0,
                truncated: false,
                ..ResponseSnapshot::default()
            },
            extensions: Default::default(),
            sort_key: 0,
        };
        store
            .upsert_example(&example, &["actual-password".into()])
            .unwrap();
        let exchange = Exchange {
            console: Vec::new(),
            test_results: Vec::new(),
            id: super::super::ExchangeId::new(),
            workspace_id: workspace.clone(),
            request_id: Some(request.id.clone()),
            request: example.request.clone().unwrap(),
            response: Some(example.response.clone()),
            error: Some("server echoed actual-password".into()),
            started_at: 1,
            completed_at: 2,
        };
        store
            .record_exchange(&exchange, &["actual-password".into()])
            .unwrap();
        let stored_example = &store.list_examples(&request.id).unwrap()[0];
        assert_eq!(
            stored_example.request.as_ref().unwrap().headers[0].1,
            "<redacted>"
        );
        assert_eq!(stored_example.response.headers[0].1, "<redacted>");
        let example_body = base64::engine::general_purpose::STANDARD
            .decode(&stored_example.response.body_base64)
            .unwrap();
        assert!(
            !example_body
                .windows(b"actual-password".len())
                .any(|window| window == b"actual-password")
        );
        assert!(
            example_body
                .windows(b"<redacted>".len())
                .any(|window| window == b"<redacted>")
        );
        let stored_exchange = &store.history(&workspace, 1).unwrap()[0];
        assert_eq!(
            stored_exchange.response.as_ref().unwrap().headers[0].1,
            "<redacted>"
        );
        assert_eq!(
            stored_exchange.error.as_deref(),
            Some("server echoed <redacted>")
        );
        let exchange_response = stored_exchange.response.as_ref().unwrap();
        assert!(exchange_response.body_base64.is_empty());
        assert_eq!(
            exchange_response.body_omitted_reason.as_deref(),
            Some("binary")
        );
        let database = store.database_path().to_path_buf();
        store
            .conn()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        drop(store);
        let bytes = fs::read(database).unwrap();
        assert!(bytes.windows(11).any(|window| window == b"vault-ref-1"));
        assert!(!bytes.windows(15).any(|window| window == b"actual-password"));
    }

    #[test]
    fn deleting_collection_cascades_requests() {
        let scratch = Scratch::new("cascade");
        let workspace = WorkspaceId::new("project").unwrap();
        let collection = collection(&workspace, "API");
        let request = request(&collection, "List");
        let store = WorkbenchStore::open(&scratch.0).unwrap();
        store.upsert_collection(&collection).unwrap();
        store.upsert_request(&request).unwrap();
        assert!(store.delete_collection(&workspace, &collection.id).unwrap());
        assert!(store.list_requests(&collection.id).unwrap().is_empty());
    }

    #[test]
    fn deleting_folder_clears_serialized_folder_ids_for_descendant_requests() {
        let scratch = Scratch::new("folder-delete");
        let workspace = WorkspaceId::new("project").unwrap();
        let collection = collection(&workspace, "API");
        let root = Folder {
            id: FolderId::new(),
            collection_id: collection.id.clone(),
            parent_id: None,
            name: "Root".into(),
            auth: AuthConfig::None,
            variables: Vec::new(),
            scripts: Scripts::default(),
            sort_key: 0,
            extensions: Default::default(),
        };
        let child = Folder {
            id: FolderId::new(),
            collection_id: collection.id.clone(),
            parent_id: Some(root.id.clone()),
            name: "Child".into(),
            auth: AuthConfig::None,
            variables: Vec::new(),
            scripts: Scripts::default(),
            sort_key: 0,
            extensions: Default::default(),
        };
        let mut root_request = request(&collection, "Root request");
        root_request.folder_id = Some(root.id.clone());
        let mut child_request = request(&collection, "Child request");
        child_request.folder_id = Some(child.id.clone());
        let store = WorkbenchStore::open(&scratch.0).unwrap();
        store.upsert_collection(&collection).unwrap();
        store.upsert_folder(&root).unwrap();
        store.upsert_folder(&child).unwrap();
        store.upsert_request(&root_request).unwrap();
        store.upsert_request(&child_request).unwrap();

        assert!(store.delete_folder(&collection.id, &root.id).unwrap());
        drop(store);

        let reopened = WorkbenchStore::open(&scratch.0).unwrap();
        let hydrated = reopened.hydrate_workspace(&workspace, 0, 0).unwrap();
        assert!(hydrated.folders.is_empty());
        let requests = hydrated.requests;
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|request| request.folder_id.is_none()));
    }

    #[test]
    fn workspaces_are_created_listed_and_reopened() {
        let scratch = Scratch::new("workspaces");
        let store = WorkbenchStore::open_database(scratch.0.join(DATABASE_FILE)).unwrap();
        assert!(store.list_workspaces().unwrap().is_empty());
        assert_eq!(store.last_opened_workspace().unwrap(), None);
        assert!(matches!(
            store.create_workspace("  "),
            Err(StoreError::InvalidInput(_))
        ));

        let first = store.create_workspace("Workspace 1").unwrap();
        let second = store.create_workspace(" Payments ").unwrap();
        assert_eq!(second.name, "Payments");
        assert_ne!(first.id, second.id);
        assert_eq!(
            store.list_workspaces().unwrap(),
            vec![first.clone(), second.clone()]
        );
        assert_eq!(
            store.last_opened_workspace().unwrap(),
            Some(second.id.clone())
        );

        store.mark_workspace_opened(&first.id).unwrap();
        assert_eq!(
            store.last_opened_workspace().unwrap(),
            Some(first.id.clone())
        );
        assert!(store.list_collections(&first.id).unwrap().is_empty());

        let renamed = store.rename_workspace(&first.id, " Billing ").unwrap();
        assert_eq!(renamed.name, "Billing");
        assert_eq!(store.list_workspaces().unwrap(), vec![renamed, second]);
        assert!(matches!(
            store.rename_workspace(&first.id, " "),
            Err(StoreError::InvalidInput(_))
        ));
        let missing = WorkspaceId::new("workspace-missing").unwrap();
        assert!(matches!(
            store.rename_workspace(&missing, "Ghost"),
            Err(StoreError::InvalidInput(_))
        ));
    }

    #[test]
    fn implicit_workspace_names_use_the_last_path_segment() {
        assert_eq!(implicit_workspace_name("/home/me/shop"), "shop");
        assert_eq!(implicit_workspace_name("/home/me/shop/"), "shop");
        assert_eq!(implicit_workspace_name("default"), "default");
        assert_eq!(implicit_workspace_name("/"), "/");
    }

    #[test]
    fn newer_schema_fails_closed() {
        let scratch = Scratch::new("schema");
        let path = scratch.0.join(DATABASE_FILE);
        let connection = Connection::open(&path).unwrap();
        connection.pragma_update(None, "user_version", 99).unwrap();
        drop(connection);
        assert!(matches!(
            WorkbenchStore::open_database(path),
            Err(StoreError::UnsupportedSchema(99))
        ));
    }

    #[test]
    fn version_one_database_migrates_examples_without_recreating_existing_tables() {
        let scratch = Scratch::new("v1-migration");
        let path = scratch.0.join(DATABASE_FILE);
        let workspace = WorkspaceId::new("migration-project").unwrap();
        let collection = collection(&workspace, "Existing API");
        let request = request(&collection, "Existing request");
        let store = WorkbenchStore::open_database(&path).unwrap();
        store.upsert_collection(&collection).unwrap();
        store.upsert_request(&request).unwrap();
        drop(store);
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "DROP TABLE workbench_examples;
                 DROP TABLE workbench_cookie_jars;
                 DROP TABLE workbench_globals;
                 DROP TABLE workbench_workspaces;
                 DROP TABLE workbench_tab_sessions;
                 ALTER TABLE workbench_environments DROP COLUMN label;
                 DELETE FROM workbench_schema_migrations WHERE version IN (2, 3, 4, 5, 6, 7);
                 PRAGMA user_version=1;",
            )
            .unwrap();
        drop(connection);

        let migrated = WorkbenchStore::open_database(&path).unwrap();
        assert_eq!(migrated.schema_version().unwrap(), 7);
        assert_eq!(
            migrated.list_workspaces().unwrap(),
            vec![WorkspaceEntry {
                id: workspace.clone(),
                name: "migration-project".into(),
            }]
        );
        let table: String = migrated
            .conn()
            .query_row(
                "SELECT name FROM sqlite_master WHERE type='table' AND name='workbench_examples'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table, "workbench_examples");
        assert_eq!(
            migrated.list_collections(&workspace).unwrap(),
            vec![collection]
        );
        assert_eq!(
            migrated.list_requests(&request.collection_id).unwrap(),
            vec![request]
        );
    }

    fn labelled_environment(
        workspace: &WorkspaceId,
        name: &str,
        label: EnvironmentLabel,
    ) -> Environment {
        Environment {
            id: EnvironmentId::new(),
            workspace_id: workspace.clone(),
            name: name.into(),
            label,
            base_url: String::new(),
            auth: Default::default(),
            variables: Vec::new(),
            active: false,
            extensions: Default::default(),
        }
    }

    fn stored_labels(store: &WorkbenchStore) -> Vec<(String, String)> {
        let connection = store.conn();
        let mut statement = connection
            .prepare("SELECT name, label FROM workbench_environments ORDER BY name")
            .unwrap();
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn environment_labels_round_trip() {
        let scratch = Scratch::new("env-label");
        let workspace = WorkspaceId::new("project").unwrap();
        let store = WorkbenchStore::open_database(scratch.0.join(DATABASE_FILE)).unwrap();
        let mut production = labelled_environment(&workspace, "Prod", EnvironmentLabel::Production);
        let plain = labelled_environment(&workspace, "Sandbox", EnvironmentLabel::Local);
        store.upsert_environment(&production).unwrap();
        store.upsert_environment(&plain).unwrap();
        let mut listed = store.list_environments(&workspace).unwrap();
        listed.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(listed, vec![production.clone(), plain.clone()]);
        assert_eq!(
            stored_labels(&store),
            vec![
                ("Prod".into(), "production".into()),
                ("Sandbox".into(), "local".into())
            ]
        );

        production.label = EnvironmentLabel::Staging;
        store.upsert_environment(&production).unwrap();
        store
            .set_active_environment(&workspace, Some(&production.id))
            .unwrap();
        let reloaded = store
            .list_environments(&workspace)
            .unwrap()
            .into_iter()
            .find(|value| value.id == production.id)
            .unwrap();
        assert_eq!(reloaded.label, EnvironmentLabel::Staging);
        assert!(reloaded.active);
        assert_eq!(stored_labels(&store)[0].1, "staging");
    }

    #[test]
    fn version_five_database_migrates_environment_labels() {
        let scratch = Scratch::new("v5-migration");
        let path = scratch.0.join(DATABASE_FILE);
        let workspace = WorkspaceId::new("project").unwrap();
        let production = labelled_environment(&workspace, "Prod", EnvironmentLabel::Production);
        let plain = labelled_environment(&workspace, "Sandbox", EnvironmentLabel::Local);
        let store = WorkbenchStore::open_database(&path).unwrap();
        store.upsert_environment(&production).unwrap();
        store.upsert_environment(&plain).unwrap();
        drop(store);
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "ALTER TABLE workbench_environments DROP COLUMN label;
                 DROP TABLE workbench_tab_sessions;
                 DELETE FROM workbench_schema_migrations WHERE version IN (6, 7);
                 PRAGMA user_version=5;",
            )
            .unwrap();
        drop(connection);

        let migrated = WorkbenchStore::open_database(&path).unwrap();
        assert_eq!(migrated.schema_version().unwrap(), 7);
        assert_eq!(
            stored_labels(&migrated),
            vec![
                ("Prod".into(), "production".into()),
                ("Sandbox".into(), "local".into())
            ]
        );
        let mut listed = migrated.list_environments(&workspace).unwrap();
        listed.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(listed, vec![production, plain]);
    }

    #[test]
    fn tab_sessions_round_trip_per_workspace() {
        let scratch = Scratch::new("tab-session");
        let path = scratch.0.join(DATABASE_FILE);
        let first = WorkspaceId::new("first").unwrap();
        let second = WorkspaceId::new("second").unwrap();
        let store = WorkbenchStore::open_database(&path).unwrap();
        assert_eq!(store.tab_session(&first).unwrap(), TabSession::default());

        let (a, b) = (RequestId::new(), RequestId::new());
        let session = TabSession {
            requests: vec![b.clone(), a.clone()],
            active: Some(a.clone()),
        };
        store.save_tab_session(&first, &session).unwrap();
        assert_eq!(store.tab_session(&first).unwrap(), session);
        assert_eq!(store.tab_session(&second).unwrap(), TabSession::default());

        let replaced = TabSession {
            requests: vec![a],
            active: None,
        };
        store.save_tab_session(&first, &replaced).unwrap();
        drop(store);
        let reopened = WorkbenchStore::open_database(&path).unwrap();
        assert_eq!(reopened.tab_session(&first).unwrap(), replaced);
    }

    #[test]
    fn version_six_database_migrates_to_tab_sessions() {
        let scratch = Scratch::new("v6-migration");
        let path = scratch.0.join(DATABASE_FILE);
        let workspace = WorkspaceId::new("project").unwrap();
        let collection = collection(&workspace, "API");
        let request = request(&collection, "List");
        let store = WorkbenchStore::open_database(&path).unwrap();
        store.upsert_collection(&collection).unwrap();
        store.upsert_request(&request).unwrap();
        drop(store);
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "DROP TABLE workbench_tab_sessions;
                 DELETE FROM workbench_schema_migrations WHERE version=7;
                 PRAGMA user_version=6;",
            )
            .unwrap();
        drop(connection);

        let migrated = WorkbenchStore::open_database(&path).unwrap();
        assert_eq!(migrated.schema_version().unwrap(), 7);
        assert_eq!(
            migrated.tab_session(&workspace).unwrap(),
            TabSession::default()
        );
        let session = TabSession {
            requests: vec![request.id.clone()],
            active: Some(request.id.clone()),
        };
        migrated.save_tab_session(&workspace, &session).unwrap();
        assert_eq!(migrated.tab_session(&workspace).unwrap(), session);
        assert_eq!(
            migrated.list_requests(&request.collection_id).unwrap(),
            vec![request]
        );
    }

    #[test]
    fn examples_environments_and_history_survive_workspace_hydration() {
        let scratch = Scratch::new("hydrate");
        let workspace = WorkspaceId::new("project").unwrap();
        let collection = collection(&workspace, "API");
        let request = request(&collection, "List");
        let first_environment = Environment {
            label: Default::default(),
            id: EnvironmentId::new(),
            workspace_id: workspace.clone(),
            name: "First".into(),
            base_url: String::new(),
            auth: Default::default(),
            variables: Vec::new(),
            active: true,
            extensions: Default::default(),
        };
        let second_environment = Environment {
            label: Default::default(),
            id: EnvironmentId::new(),
            workspace_id: workspace.clone(),
            name: "Second".into(),
            base_url: String::new(),
            auth: Default::default(),
            variables: Vec::new(),
            active: true,
            extensions: Default::default(),
        };
        let example = Example {
            id: ExampleId::new(),
            request_id: request.id.clone(),
            name: "OK".into(),
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
        };
        let exchange = Exchange {
            console: Vec::new(),
            test_results: Vec::new(),
            id: super::super::ExchangeId::new(),
            workspace_id: workspace.clone(),
            request_id: Some(request.id.clone()),
            request: super::super::RedactedRequestSnapshot {
                method: "GET".into(),
                url: request.url.clone(),
                headers: Vec::new(),
                body: String::new(),
                ..Default::default()
            },
            response: Some(ResponseSnapshot {
                status: 200,
                reason: "OK".into(),
                headers: Vec::new(),
                body_base64: String::new(),
                duration_ms: 2,
                truncated: false,
                ..ResponseSnapshot::default()
            }),
            error: None,
            started_at: 10,
            completed_at: 12,
        };
        let run = CollectionRun {
            id: super::super::RunId::new(),
            workspace_id: workspace.clone(),
            collection_id: collection.id.clone(),
            environment_id: None,
            iteration_count: 1,
            stop_on_error: false,
            selected_folder_id: None,
            selected_request_ids: Vec::new(),
            delay_ms: 0,
            keep_variable_values: true,
            item_results: Vec::new(),
            status: super::super::RunStatus::Completed,
            started_at: 10,
            completed_at: Some(12),
        };
        let store = WorkbenchStore::open(&scratch.0).unwrap();
        store.upsert_collection(&collection).unwrap();
        store.upsert_request(&request).unwrap();
        store.upsert_example(&example, &[]).unwrap();
        store.record_exchange(&exchange, &[]).unwrap();
        store.upsert_run(&run, &[]).unwrap();
        store.upsert_environment(&first_environment).unwrap();
        store.upsert_environment(&second_environment).unwrap();
        store
            .set_active_environment(&workspace, Some(&first_environment.id))
            .unwrap();
        drop(store);

        let reopened = WorkbenchStore::open(&scratch.0).unwrap();
        let snapshot = reopened.hydrate_workspace(&workspace, 100, 100).unwrap();
        assert_eq!(snapshot.examples, vec![example]);
        assert_eq!(snapshot.requests, vec![request]);
        assert_eq!(snapshot.history, vec![exchange]);
        assert_eq!(snapshot.runs, vec![run]);
        assert_eq!(
            snapshot
                .environments
                .iter()
                .filter(|value| value.active)
                .count(),
            1
        );
        assert!(
            snapshot
                .environments
                .iter()
                .any(|value| value.id == first_environment.id && value.active)
        );
    }

    #[test]
    fn import_rejects_cross_collection_edges_before_writing_any_rows() {
        let scratch = Scratch::new("malicious-import");
        let workspace = WorkspaceId::new("target").unwrap();
        let other_workspace = WorkspaceId::new("other").unwrap();
        let victim = collection(&other_workspace, "Victim");
        let imported_collection = collection(&workspace, "Imported");
        let malicious = SavedRequest {
            collection_id: victim.id.clone(),
            ..request(&imported_collection, "Escaped")
        };
        let imported = ImportResult {
            format: ImportFormat::AgentOps,
            origin: None,
            collection: imported_collection.clone(),
            folders: Vec::new(),
            requests: vec![malicious],
            environments: Vec::new(),
            examples: Vec::new(),
            warnings: Vec::new(),
        };
        let store = WorkbenchStore::open(&scratch.0).unwrap();
        store.upsert_collection(&victim).unwrap();
        assert!(matches!(
            store.commit_import(&workspace, &imported),
            Err(StoreError::InvalidInput(message)) if message.contains("another collection")
        ));
        assert!(store.list_collections(&workspace).unwrap().is_empty());
        assert!(store.list_requests(&victim.id).unwrap().is_empty());
    }

    #[test]
    fn selected_import_requires_and_commits_the_dependency_chain_atomically() {
        let scratch = Scratch::new("selected-import");
        let workspace = WorkspaceId::new("target").unwrap();
        let collection = collection(&workspace, "Imported");
        let folder = Folder {
            id: FolderId::new(),
            collection_id: collection.id.clone(),
            parent_id: None,
            name: "Folder".into(),
            auth: AuthConfig::None,
            variables: Vec::new(),
            scripts: Scripts::default(),
            sort_key: 0,
            extensions: Default::default(),
        };
        let mut request = request(&collection, "Selected");
        request.folder_id = Some(folder.id.clone());
        let imported = ImportResult {
            format: ImportFormat::PostmanCollection,
            origin: None,
            collection: collection.clone(),
            folders: vec![folder.clone()],
            requests: vec![request.clone()],
            environments: Vec::new(),
            examples: Vec::new(),
            warnings: Vec::new(),
        };
        let store = WorkbenchStore::open(&scratch.0).unwrap();
        let orphan = ImportSelection {
            include_collection: true,
            request_ids: BTreeSet::from([request.id.clone()]),
            ..ImportSelection::default()
        };
        assert!(matches!(
            store.commit_import_selection(&workspace, &imported, &orphan),
            Err(StoreError::InvalidInput(message)) if message.contains("requires its folder")
        ));
        assert!(store.list_collections(&workspace).unwrap().is_empty());

        store
            .commit_import_selection(&workspace, &imported, &ImportSelection::all(&imported))
            .unwrap();
        let hydrated = store.hydrate_workspace(&workspace, 0, 0).unwrap();
        assert_eq!(hydrated.collections, vec![collection]);
        assert_eq!(hydrated.folders, vec![folder]);
        assert_eq!(hydrated.requests, vec![request]);
    }

    #[test]
    fn committed_import_cannot_persist_auth_aliases_or_unknown_extension_scalars() {
        let scratch = Scratch::new("import-secret-scan");
        let workspace = WorkspaceId::new("target").unwrap();
        let source = br#"{
          "info":{"name":"Imported","schema":"https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
          "x-auth":"extension-alias-secret",
          "item":[{"name":"Request","request":{
            "method":"POST",
            "url":"https://example.test?copy=actual-import-secret",
            "header":[{"key":"X-Copy","value":"actual-import-secret"}],
            "auth":{"type":"bearer","bearer":[{"key":"token","value":"actual-import-secret"}]},
            "body":{"mode":"raw","raw":"actual-import-secret"}
          }}]
        }"#;
        let imported = crate::import(&workspace, source).unwrap();
        let store = WorkbenchStore::open(&scratch.0).unwrap();
        store.commit_import(&workspace, &imported).unwrap();
        drop(store);

        let reopened = WorkbenchStore::open(&scratch.0).unwrap();
        let hydrated = reopened.hydrate_workspace(&workspace, 10, 10).unwrap();
        let serialized = serde_json::to_string(&(
            &hydrated.collections,
            &hydrated.folders,
            &hydrated.requests,
            &hydrated.environments,
            &hydrated.examples,
            &hydrated.history,
            &hydrated.runs,
        ))
        .unwrap();
        let database = fs::read(reopened.database_path()).unwrap();
        for secret in ["actual-import-secret", "extension-alias-secret"] {
            assert!(!serialized.contains(secret));
            assert!(
                !database
                    .windows(secret.len())
                    .any(|bytes| bytes == secret.as_bytes())
            );
        }
    }

    #[test]
    fn crud_rejects_scope_moves_and_cross_collection_edges() {
        let scratch = Scratch::new("crud-ownership");
        let first_workspace = WorkspaceId::new("first").unwrap();
        let second_workspace = WorkspaceId::new("second").unwrap();
        let first = collection(&first_workspace, "First");
        let second = collection(&second_workspace, "Second");
        let store = WorkbenchStore::open(&scratch.0).unwrap();
        store.upsert_collection(&first).unwrap();
        store.upsert_collection(&second).unwrap();

        let mut moved_collection = first.clone();
        moved_collection.workspace_id = second_workspace.clone();
        assert!(matches!(
            store.upsert_collection(&moved_collection),
            Err(StoreError::InvalidInput(message)) if message.contains("another workspace_id")
        ));

        let second_folder = Folder {
            id: FolderId::new(),
            collection_id: second.id.clone(),
            parent_id: None,
            name: "Second folder".into(),
            auth: AuthConfig::None,
            variables: Vec::new(),
            scripts: Scripts::default(),
            sort_key: 0,
            extensions: Default::default(),
        };
        store.upsert_folder(&second_folder).unwrap();
        let cross_folder = Folder {
            id: FolderId::new(),
            collection_id: first.id.clone(),
            parent_id: Some(second_folder.id.clone()),
            name: "Cross folder".into(),
            auth: AuthConfig::None,
            variables: Vec::new(),
            scripts: Scripts::default(),
            sort_key: 0,
            extensions: Default::default(),
        };
        assert!(matches!(
            store.upsert_folder(&cross_folder),
            Err(StoreError::InvalidInput(message)) if message.contains("another collection")
        ));

        let mut cross_request = request(&first, "Cross request");
        cross_request.folder_id = Some(second_folder.id.clone());
        assert!(matches!(
            store.upsert_request(&cross_request),
            Err(StoreError::InvalidInput(message)) if message.contains("another collection")
        ));

        let environment = Environment {
            label: Default::default(),
            id: EnvironmentId::new(),
            workspace_id: first_workspace.clone(),
            name: "Environment".into(),
            base_url: String::new(),
            auth: Default::default(),
            variables: Vec::new(),
            active: false,
            extensions: Default::default(),
        };
        store.upsert_environment(&environment).unwrap();
        let mut moved_environment = environment;
        moved_environment.workspace_id = second_workspace;
        assert!(matches!(
            store.upsert_environment(&moved_environment),
            Err(StoreError::InvalidInput(message)) if message.contains("another workspace_id")
        ));
    }

    #[test]
    fn import_rolls_back_collection_when_a_late_request_insert_fails() {
        let scratch = Scratch::new("import-rollback");
        let workspace = WorkspaceId::new("target").unwrap();
        let existing_collection = collection(&workspace, "Existing");
        let existing_request = request(&existing_collection, "Existing request");
        let imported_collection = collection(&workspace, "Imported");
        let mut colliding_request = request(&imported_collection, "Collision");
        colliding_request.id = existing_request.id.clone();
        let imported = ImportResult {
            format: ImportFormat::AgentOps,
            origin: None,
            collection: imported_collection.clone(),
            folders: Vec::new(),
            requests: vec![colliding_request],
            environments: Vec::new(),
            examples: Vec::new(),
            warnings: Vec::new(),
        };
        let store = WorkbenchStore::open(&scratch.0).unwrap();
        store.upsert_collection(&existing_collection).unwrap();
        store.upsert_request(&existing_request).unwrap();
        assert!(matches!(
            store.commit_import(&workspace, &imported),
            Err(StoreError::Sqlite(_))
        ));
        assert!(
            store
                .collection(&workspace, &imported_collection.id)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store
                .request(&existing_collection.id, &existing_request.id)
                .unwrap(),
            Some(existing_request)
        );
    }

    #[test]
    fn exchange_upsert_cannot_relabel_the_submitted_request() {
        let scratch = Scratch::new("immutable-exchange-request");
        let workspace = WorkspaceId::new("project").unwrap();
        let collection = collection(&workspace, "API");
        let request = request(&collection, "Submitted");
        let exchange = Exchange {
            console: Vec::new(),
            test_results: Vec::new(),
            id: super::super::ExchangeId::new(),
            workspace_id: workspace.clone(),
            request_id: Some(request.id.clone()),
            request: super::super::RedactedRequestSnapshot {
                method: "POST".into(),
                url: "https://example.test/submitted".into(),
                headers: vec![("X-Submitted".into(), "yes".into())],
                body: "submitted body".into(),
                ..Default::default()
            },
            response: None,
            error: None,
            started_at: 10,
            completed_at: 10,
        };
        let store = WorkbenchStore::open(&scratch.0).unwrap();
        store.upsert_collection(&collection).unwrap();
        store.upsert_request(&request).unwrap();
        store.record_exchange(&exchange, &[]).unwrap();

        let mut relabeled = exchange.clone();
        relabeled.request.method = "GET".into();
        relabeled.request.body.clear();
        assert!(matches!(
            store.record_exchange(&relabeled, &[]),
            Err(StoreError::InvalidInput(message)) if message.contains("request snapshot")
        ));
        let mut expected = exchange;
        expected.request.body_bytes = 14;
        assert_eq!(store.history(&workspace, 10).unwrap(), vec![expected]);
    }

    #[test]
    fn run_items_must_belong_to_the_run_collection() {
        let scratch = Scratch::new("run-request-scope");
        let workspace = WorkspaceId::new("project").unwrap();
        let first = collection(&workspace, "First");
        let second = collection(&workspace, "Second");
        let foreign_request = request(&second, "Foreign");
        let run = CollectionRun {
            id: super::super::RunId::new(),
            workspace_id: workspace,
            collection_id: first.id.clone(),
            environment_id: None,
            iteration_count: 1,
            stop_on_error: false,
            selected_folder_id: None,
            selected_request_ids: Vec::new(),
            delay_ms: 0,
            keep_variable_values: true,
            item_results: vec![super::super::RunItemResult {
                console: Vec::new(),
                request_id: foreign_request.id.clone(),
                iteration: 0,
                status: Some(200),
                duration_ms: 1,
                error: None,
                response: None,
                test_results: Vec::new(),
            }],
            status: super::super::RunStatus::Completed,
            started_at: 10,
            completed_at: Some(11),
        };
        let store = WorkbenchStore::open(&scratch.0).unwrap();
        store.upsert_collection(&first).unwrap();
        store.upsert_collection(&second).unwrap();
        store.upsert_request(&foreign_request).unwrap();

        assert!(matches!(
            store.upsert_run(&run, &[]),
            Err(StoreError::InvalidInput(message)) if message.contains("another collection")
        ));
        assert!(store.runs(&run.workspace_id, 10).unwrap().is_empty());
    }

    #[test]
    fn run_storage_policy_bounds_items_bodies_and_workspace_retention_atomically() {
        let scratch = Scratch::new("bounded-run-storage");
        let workspace = WorkspaceId::new("project").unwrap();
        let primary_collection = collection(&workspace, "API");
        let request = request(&primary_collection, "Request");
        let store = WorkbenchStore::open(&scratch.0).unwrap();
        store.upsert_collection(&primary_collection).unwrap();
        store.upsert_request(&request).unwrap();
        let policy = RunStoragePolicy {
            max_runs: 1,
            max_item_results: 1,
            max_response_body_bytes: 4,
        };
        let foreign_workspace = WorkspaceId::new("foreign").unwrap();
        let foreign_collection = collection(&foreign_workspace, "Foreign API");
        store.upsert_collection(&foreign_collection).unwrap();
        let foreign_run = CollectionRun {
            id: super::super::RunId::new(),
            workspace_id: foreign_workspace.clone(),
            collection_id: foreign_collection.id,
            environment_id: None,
            iteration_count: 0,
            stop_on_error: false,
            selected_folder_id: None,
            selected_request_ids: Vec::new(),
            delay_ms: 0,
            keep_variable_values: true,
            item_results: Vec::new(),
            status: super::super::RunStatus::Completed,
            started_at: 5,
            completed_at: Some(6),
        };
        store
            .upsert_run_with_policy(&foreign_run, &[], policy)
            .unwrap();

        for started_at in [10, 20] {
            let item = super::super::RunItemResult {
                console: Vec::new(),
                request_id: request.id.clone(),
                iteration: 0,
                status: Some(200),
                duration_ms: 1,
                error: None,
                response: Some(ResponseSnapshot {
                    status: 200,
                    headers: vec![("Content-Type".into(), "text/plain".into())],
                    body_base64: base64::engine::general_purpose::STANDARD.encode("1234567890"),
                    ..ResponseSnapshot::default()
                }),
                test_results: Vec::new(),
            };
            let run = CollectionRun {
                id: super::super::RunId::new(),
                workspace_id: workspace.clone(),
                collection_id: primary_collection.id.clone(),
                environment_id: None,
                iteration_count: 2,
                stop_on_error: false,
                selected_folder_id: None,
                selected_request_ids: Vec::new(),
                delay_ms: 0,
                keep_variable_values: true,
                item_results: vec![item.clone(), item],
                status: super::super::RunStatus::Completed,
                started_at,
                completed_at: Some(started_at + 1),
            };
            store.upsert_run_with_policy(&run, &[], policy).unwrap();
        }

        let runs = store.runs(&workspace, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].started_at, 20);
        assert_eq!(runs[0].item_results.len(), 1);
        let response = runs[0].item_results[0].response.as_ref().unwrap();
        assert_eq!(response.received_bytes, 10);
        assert_eq!(response.stored_bytes, 4);
        assert_eq!(
            response.full_body_sha256.as_deref(),
            Some("c775e7b757ede630cd0aa1113bd102661ab38829ca52a6422ab782862f268646")
        );
        assert_eq!(
            store.runs(&foreign_workspace, 10).unwrap(),
            vec![foreign_run]
        );
    }

    #[test]
    fn run_configuration_survives_store_reopen() {
        let scratch = Scratch::new("run-configuration-reopen");
        let workspace = WorkspaceId::new("project").unwrap();
        let collection = collection(&workspace, "API");
        let folder = super::super::Folder {
            id: super::super::FolderId::new(),
            collection_id: collection.id.clone(),
            parent_id: None,
            name: "Selected".into(),
            auth: AuthConfig::Inherit,
            variables: Vec::new(),
            scripts: Scripts::default(),
            sort_key: 0,
            extensions: Default::default(),
        };
        let mut request = request(&collection, "Selected request");
        request.folder_id = Some(folder.id.clone());
        let run = CollectionRun {
            id: super::super::RunId::new(),
            workspace_id: workspace.clone(),
            collection_id: collection.id.clone(),
            environment_id: None,
            iteration_count: 3,
            stop_on_error: true,
            selected_folder_id: Some(folder.id.clone()),
            selected_request_ids: vec![request.id.clone()],
            delay_ms: 250,
            keep_variable_values: true,
            item_results: Vec::new(),
            status: super::super::RunStatus::Completed,
            started_at: 10,
            completed_at: Some(20),
        };
        let store = WorkbenchStore::open(&scratch.0).unwrap();
        store.upsert_collection(&collection).unwrap();
        store.upsert_folder(&folder).unwrap();
        store.upsert_request(&request).unwrap();
        store.upsert_run(&run, &[]).unwrap();
        drop(store);

        let reopened = WorkbenchStore::open(&scratch.0).unwrap();
        assert_eq!(reopened.runs(&workspace, 10).unwrap(), vec![run]);
    }

    #[test]
    fn run_persistence_redacts_script_output_and_encoded_response_variants() {
        let scratch = Scratch::new("run-redaction-boundary");
        let workspace = WorkspaceId::new("project").unwrap();
        let collection = collection(&workspace, "API");
        let request = request(&collection, "Submitted");
        let secret = "p@ss/word+?";
        let encoded = "p%40ss%2Fword%2B%3F";
        let response_text = format!("raw={secret}&encoded={encoded}");
        let run = CollectionRun {
            id: super::super::RunId::new(),
            workspace_id: workspace.clone(),
            collection_id: collection.id.clone(),
            environment_id: None,
            iteration_count: 1,
            stop_on_error: false,
            selected_folder_id: None,
            selected_request_ids: Vec::new(),
            delay_ms: 0,
            keep_variable_values: true,
            item_results: vec![super::super::RunItemResult {
                console: Vec::new(),
                request_id: request.id.clone(),
                iteration: 0,
                status: Some(500),
                duration_ms: 1,
                error: Some(format!("script echoed {secret} and {encoded}")),
                response: Some(ResponseSnapshot {
                    status: 500,
                    headers: vec![("X-Echo".into(), encoded.into())],
                    body_base64: base64::engine::general_purpose::STANDARD.encode(response_text),
                    test_results: vec![super::super::TestResult {
                        name: format!("response must not contain {secret}"),
                        passed: false,
                        skipped: false,
                        error: Some(format!("received {encoded}")),
                    }],
                    ..ResponseSnapshot::default()
                }),
                test_results: vec![super::super::TestResult {
                    name: format!("script assertion {encoded}"),
                    passed: false,
                    skipped: false,
                    error: Some(format!("actual value was {secret}")),
                }],
            }],
            status: super::super::RunStatus::Failed,
            started_at: 10,
            completed_at: Some(11),
        };
        let store = WorkbenchStore::open(&scratch.0).unwrap();
        store.upsert_collection(&collection).unwrap();
        store.upsert_request(&request).unwrap();

        store.upsert_run(&run, &[secret.into()]).unwrap();

        let stored_json: String = store
            .conn()
            .query_row(
                "SELECT definition_json FROM workbench_runs WHERE id=?1",
                [run.id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!stored_json.contains(secret));
        assert!(!stored_json.contains(encoded));
        assert!(stored_json.contains("<redacted>"));

        let stored = store.runs(&workspace, 1).unwrap().remove(0);
        let serialized = serde_json::to_string(&stored).unwrap();
        assert!(!serialized.contains(secret));
        assert!(!serialized.contains(encoded));
        let body = base64::engine::general_purpose::STANDARD
            .decode(
                &stored.item_results[0]
                    .response
                    .as_ref()
                    .unwrap()
                    .body_base64,
            )
            .unwrap();
        let body = String::from_utf8(body).unwrap();
        assert!(!body.contains(secret));
        assert!(!body.contains(encoded));
        assert_eq!(body, "raw=<redacted>&encoded=<redacted>");
    }

    #[test]
    fn environment_only_import_commits_without_a_phantom_collection() {
        let scratch = Scratch::new("environment-only-import");
        let workspace = WorkspaceId::new("project").unwrap();
        let source = br#"{
          "name": "Local",
          "_postman_variable_scope": "environment",
          "values": [{"key":"host","value":"api.example.test","enabled":true}]
        }"#;
        let imported = crate::import(&workspace, source).unwrap();
        let store = WorkbenchStore::open(&scratch.0).unwrap();

        store.commit_import(&workspace, &imported).unwrap();
        let snapshot = store.hydrate_workspace(&workspace, 10, 10).unwrap();
        assert!(snapshot.collections.is_empty());
        // The first environment a workspace ever sees becomes the active one.
        let mut expected = imported.environments.clone();
        expected[0].active = true;
        assert_eq!(snapshot.environments, expected);

        // A later import never displaces the active environment.
        let later = crate::import(
            &workspace,
            br#"{"name":"Later","_postman_variable_scope":"environment","values":[]}"#,
        )
        .unwrap();
        store.commit_import(&workspace, &later).unwrap();
        let snapshot = store.hydrate_workspace(&workspace, 10, 10).unwrap();
        assert_eq!(
            snapshot
                .environments
                .iter()
                .filter(|environment| environment.active)
                .map(|environment| environment.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Local"]
        );
    }

    #[test]
    fn commit_import_resanitizes_staged_unknown_extensions() {
        let scratch = Scratch::new("staged-import-extension-sanitization");
        let workspace = WorkspaceId::new("project").unwrap();
        let mut collection = collection(&workspace, "Imported");
        collection.extensions.insert(
            "x-backup-token".into(),
            serde_json::json!({"value":"staged-database-secret", "enabled":true}),
        );
        collection.extensions.insert(
            "x-auth".into(),
            serde_json::json!({"value":"staged-nested-auth-secret"}),
        );
        collection.variables.push(Variable {
            id: Default::default(),
            key: "client_secret".into(),
            value: VariableValue::Plain("staged-plain-variable-secret".into()),
            enabled: true,
            description: String::new(),
        });
        let imported = ImportResult {
            format: ImportFormat::PostmanCollection,
            origin: None,
            collection,
            folders: Vec::new(),
            requests: Vec::new(),
            environments: Vec::new(),
            examples: Vec::new(),
            warnings: Vec::new(),
        };
        let store = WorkbenchStore::open(&scratch.0).unwrap();

        store.commit_import(&workspace, &imported).unwrap();
        drop(store);

        let database = fs::read(scratch.0.join(DATABASE_FILE)).unwrap();
        for secret in [
            b"staged-database-secret".as_slice(),
            b"staged-nested-auth-secret".as_slice(),
            b"staged-plain-variable-secret".as_slice(),
        ] {
            assert!(!database.windows(secret.len()).any(|bytes| bytes == secret));
        }
    }

    #[test]
    fn sign_in_saves_reject_literal_credentials_without_replacing_saved_definitions() {
        let scratch = Scratch::new("login-save-credentials");
        let workspace = WorkspaceId::new("project").unwrap();
        let collection = collection(&workspace, "API");
        let mut request = request(&collection, "Sign-in");
        request.auth = serde_json::from_value(serde_json::json!({
            "kind": "login",
            "url": "https://example.test/login",
            "body": r#"{"user":"ops","password":"{{login_password}}"}"#,
            "token_path": "access_token"
        }))
        .unwrap();
        let mut environment = Environment {
            label: Default::default(),
            id: EnvironmentId::new(),
            workspace_id: workspace.clone(),
            name: "Production".into(),
            base_url: "https://example.test".into(),
            auth: request.auth.clone(),
            variables: vec![Variable {
                id: Default::default(),
                key: "login_password".into(),
                value: VariableValue::Secret(SecretRef::new("login-password-ref").unwrap()),
                enabled: true,
                description: String::new(),
            }],
            active: true,
            extensions: Default::default(),
        };
        let store = WorkbenchStore::open(&scratch.0).unwrap();
        store.upsert_collection(&collection).unwrap();
        store.upsert_request(&request).unwrap();
        store.upsert_environment(&environment).unwrap();
        let saved_request = request.clone();
        let saved_environment = environment.clone();

        for body in [
            r#"{"user":"ops","password":"literal-login-password"}"#,
            r#"{"credentials":{"value":"literal-login-password"}}"#,
        ] {
            if let AuthConfig::Login { body: value, .. } = &mut request.auth {
                *value = body.into();
            }
            environment.auth = request.auth.clone();
            for error in [
                store.upsert_request(&request).unwrap_err(),
                store.upsert_environment(&environment).unwrap_err(),
            ] {
                let message = error.to_string();
                assert!(message.contains("{{login_password}}"), "{message}");
                assert!(message.contains("secret:login_password"), "{message}");
                assert!(!message.contains("literal-login-password"), "{message}");
            }
        }
        drop(store);
        let store = WorkbenchStore::open(&scratch.0).unwrap();
        assert_eq!(
            store.list_requests(&collection.id).unwrap(),
            vec![saved_request]
        );
        assert_eq!(
            store.list_environments(&workspace).unwrap(),
            vec![saved_environment]
        );
    }

    #[test]
    fn environment_base_url_and_auth_round_trip() {
        let scratch = Scratch::new("environment-base-url-auth");
        let workspace = WorkspaceId::new("project").unwrap();
        let environment = Environment {
            label: Default::default(),
            id: EnvironmentId::new(),
            workspace_id: workspace.clone(),
            name: "Staging".into(),
            base_url: "https://staging.example.test/api".into(),
            auth: AuthConfig::Bearer {
                token: SecretRef::new("workbench.environment.auth.token").unwrap(),
            },
            variables: Vec::new(),
            active: true,
            extensions: Default::default(),
        };
        let store = WorkbenchStore::open(&scratch.0).unwrap();
        store.upsert_environment(&environment).unwrap();
        drop(store);
        let store = WorkbenchStore::open(&scratch.0).unwrap();
        let stored = store.list_environments(&workspace).unwrap();
        assert_eq!(stored, vec![environment]);
    }
}
