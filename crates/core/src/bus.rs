//! Commands the UI sends to the runtime, and events it receives back.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use secrecy::SecretString;
use switchyard_db::guard::Destructive;
use switchyard_db::{
    CatalogChunk, ColumnMeta, Completion, DbError, IntrospectScope, Notice, RowBatch, Value,
};
use switchyard_drivers::Component;
use switchyard_remote::FileEntry;
use switchyard_store::{BufferState, DbConnection, HistoryEntry, Profile, ProfileId, Workspace};

/// Identifies one UI request so its answer can be matched.
pub type RequestId = u64;
/// An open database session.
pub type SessionId = u64;
/// A running query (one Run click; may contain several statements).
pub type QueryId = u64;

/// One statement to execute.
#[derive(Clone, Debug)]
pub struct StatementRequest {
    /// SQL text.
    pub sql: String,
    /// Parameter values in binding order.
    pub params: Vec<Value>,
    /// Byte offset of the statement in the editor buffer (for error positions).
    pub offset: usize,
}

/// How many rows to fetch before pausing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FetchLimit {
    /// Stop reading after this many rows; `FetchMore` resumes.
    Rows(usize),
    /// Read everything.
    All,
}

/// Commands from the UI. Every command is handled on the runtime; none blocks the UI.
#[derive(Debug)]
pub enum Command {
    /// Test command: wait, then answer with [`Event::Pong`].
    Ping {
        /// Echoed back.
        id: RequestId,
        /// Delay before answering.
        delay: Duration,
    },
    /// Load every saved profile.
    LoadProfiles,
    /// Validate and save a profile; store `secret` in the secret store when given.
    SaveProfile {
        /// Request id.
        request: RequestId,
        /// The profile.
        profile: Profile,
        /// New password or passphrase.
        secret: Option<SecretString>,
    },
    /// Delete a profile and its secret.
    DeleteProfile {
        /// Profile id.
        id: ProfileId,
    },
    /// Persist sidebar order.
    ReorderProfiles {
        /// Ids in order.
        ids: Vec<ProfileId>,
    },
    /// Export profiles (no secrets) to a JSON file.
    ExportProfiles {
        /// Destination.
        path: PathBuf,
    },
    /// Import profiles from a JSON file.
    ImportProfiles {
        /// Source.
        path: PathBuf,
    },
    /// Unlock the fallback vault with the master password.
    UnlockVault {
        /// Master password.
        password: SecretString,
    },
    /// Try connecting with an unsaved profile.
    TestConnection {
        /// Request id.
        request: RequestId,
        /// Profile under edit.
        connection: DbConnection,
        /// Password typed in the form (falls back to the stored one).
        secret: Option<SecretString>,
    },
    /// Open a session for a saved connection.
    OpenSession {
        /// New session id chosen by the UI.
        session: SessionId,
        /// Connection.
        connection: ProfileId,
    },
    /// Close a session.
    CloseSession {
        /// Session.
        session: SessionId,
    },
    /// Execute statements in order, streaming results.
    Execute {
        /// Session.
        session: SessionId,
        /// New query id chosen by the UI.
        query: QueryId,
        /// Statements.
        statements: Vec<StatementRequest>,
        /// History tags.
        tags: Vec<String>,
        /// The user confirmed destructive statements on a Production connection.
        confirmed_destructive: bool,
        /// Fetch limit for each result set.
        fetch_limit: FetchLimit,
    },
    /// Continue a query paused at its fetch limit.
    FetchMore {
        /// Query.
        query: QueryId,
        /// Fetch everything that remains.
        all: bool,
    },
    /// Cancel a running query.
    Cancel {
        /// Query.
        query: QueryId,
    },
    /// Begin a manual transaction.
    Begin {
        /// Session.
        session: SessionId,
    },
    /// Commit the open transaction.
    Commit {
        /// Session.
        session: SessionId,
    },
    /// Roll back the open transaction.
    Rollback {
        /// Session.
        session: SessionId,
    },
    /// Load a catalog scope (from cache unless `refresh`).
    Introspect {
        /// Session.
        session: SessionId,
        /// Scope.
        scope: IntrospectScope,
        /// Bypass the cache.
        refresh: bool,
    },
    /// Search query history.
    SearchHistory {
        /// Request id.
        request: RequestId,
        /// Search text.
        query: String,
        /// Restrict to a connection.
        connection: Option<ProfileId>,
    },
    /// Autosave an editor buffer.
    SaveBuffer {
        /// Buffer.
        buffer: BufferState,
        /// Tab position.
        position: i64,
    },
    /// Forget a closed buffer.
    DeleteBuffer {
        /// Buffer id.
        id: String,
    },
    /// Load the workspace (layout and buffers).
    LoadWorkspace,
    /// Save the workspace layout.
    SaveWorkspace(Workspace),
    /// Save a UI setting.
    SetSetting {
        /// Key.
        key: String,
        /// JSON value.
        value: serde_json::Value,
    },
    /// List a local directory.
    ListLocalDir {
        /// Request id.
        request: RequestId,
        /// Directory.
        path: PathBuf,
    },
    /// Detect optional native components.
    DetectComponents,
    /// Write a text file (exports).
    WriteFile {
        /// Destination.
        path: PathBuf,
        /// Contents.
        contents: String,
    },
    /// Import Hosts from `~/.ssh/config`.
    ImportSshConfig,
    /// Apply staged inline edits in one transaction. Each statement must change exactly
    /// one row; otherwise everything is rolled back.
    ApplyEdits {
        /// Session.
        session: SessionId,
        /// Request id.
        request: RequestId,
        /// UPDATE statements.
        statements: Vec<String>,
    },
}

/// Events from a running query.
#[derive(Clone, Debug)]
pub enum QueryEvent {
    /// A statement started.
    StatementStarted {
        /// Index in the request.
        index: usize,
    },
    /// Columns of a new result set.
    Columns(Arc<[ColumnMeta]>),
    /// Rows.
    Rows(RowBatch),
    /// Server notice.
    Notice(Notice),
    /// A further result set follows.
    NextResultSet,
    /// Reading paused at the fetch limit.
    Paused {
        /// Rows loaded in the current result set.
        rows: usize,
    },
    /// A statement finished.
    StatementDone {
        /// Index.
        index: usize,
        /// Completion info.
        completion: Completion,
    },
    /// The statement needs confirmation on Production; nothing ran.
    NeedsConfirmation {
        /// Index.
        index: usize,
        /// Findings.
        destructive: Vec<Destructive>,
    },
    /// A statement failed; later statements did not run.
    Failed {
        /// Index.
        index: usize,
        /// Error.
        error: DbError,
        /// Absolute (line, column) in the buffer, when known.
        location: Option<(u32, u32)>,
    },
    /// The query ended (after success, failure or cancel).
    Finished {
        /// Total time.
        elapsed: Duration,
        /// Whether it was cancelled.
        cancelled: bool,
    },
}

/// Events for the UI.
#[derive(Clone, Debug)]
pub enum Event {
    /// Answer to [`Command::Ping`].
    Pong {
        /// Request id.
        id: RequestId,
    },
    /// All profiles, in sidebar order.
    Profiles(Vec<Profile>),
    /// A profile was saved.
    ProfileSaved {
        /// Request id.
        request: RequestId,
        /// Profile id.
        id: ProfileId,
    },
    /// Saving failed.
    ProfileError {
        /// Request id.
        request: RequestId,
        /// Field, when a validation error.
        field: Option<&'static str>,
        /// Message.
        message: String,
    },
    /// Secret backend state.
    SecretBackend {
        /// Backend name.
        name: &'static str,
        /// Whether it needs unlocking.
        locked: bool,
    },
    /// Result of a connection test.
    TestResult {
        /// Request id.
        request: RequestId,
        /// Summary on success, message on failure.
        result: Result<String, String>,
    },
    /// A session opened.
    SessionOpened {
        /// Session.
        session: SessionId,
        /// Server version.
        server_version: String,
    },
    /// A session failed to open.
    SessionFailed {
        /// Session.
        session: SessionId,
        /// Message.
        message: String,
    },
    /// A query event.
    Query {
        /// Query.
        query: QueryId,
        /// Event.
        event: QueryEvent,
    },
    /// Transaction state changed.
    Transaction {
        /// Session.
        session: SessionId,
        /// Whether a transaction is open.
        open: bool,
        /// Statements run in it.
        statements: u32,
    },
    /// A catalog scope loaded.
    Catalog {
        /// Session.
        session: SessionId,
        /// Scope.
        scope: IntrospectScope,
        /// Data or error.
        result: Result<CatalogChunk, String>,
        /// When the data was fetched (ms since epoch).
        cached_at: i64,
    },
    /// History search results.
    History {
        /// Request id.
        request: RequestId,
        /// Entries, newest first.
        entries: Vec<HistoryEntry>,
    },
    /// The workspace.
    Workspace(Workspace),
    /// A directory listing.
    DirListing {
        /// Request id.
        request: RequestId,
        /// Directory.
        path: PathBuf,
        /// Entries or error.
        result: Result<Vec<FileEntry>, String>,
    },
    /// Native component status.
    Components(Vec<Component>),
    /// Result of [`Command::ApplyEdits`]: rows changed, or why nothing was.
    EditsApplied {
        /// Request id.
        request: RequestId,
        /// Rows changed or error.
        result: Result<u64, String>,
        /// Time taken.
        elapsed: Duration,
    },
    /// A short confirmation for a toast.
    Toast(String),
    /// A background failure.
    Error {
        /// What was being done.
        context: String,
        /// Message.
        message: String,
    },
}
