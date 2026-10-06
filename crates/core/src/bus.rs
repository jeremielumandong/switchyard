//! Commands the UI sends to the runtime, and events it receives back.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use secrecy::SecretString;
use switchyard_db::guard::Destructive;
use switchyard_db::{
    CatalogChunk, ColumnMeta, Completion, DbError, IntrospectScope, Notice, RowBatch, Value,
};
use switchyard_drivers::{Component, InstallProgress};
use switchyard_remote::FileEntry;
use switchyard_remote::ssh::{HostKeyDecision, HostKeyRequest, InteractiveRequest, TunnelInfo};
use switchyard_store::{
    BufferState, DbConnection, HistoryEntry, Host, Profile, ProfileId, Workspace,
};
use switchyard_term::{TermSize, Terminal};

/// Identifies one UI request so its answer can be matched.
pub type RequestId = u64;
/// An open database session.
pub type SessionId = u64;
/// A running query (one Run click; may contain several statements).
pub type QueryId = u64;
/// An open terminal (local shell or SSH channel).
pub type TermId = u64;

/// The user's answer to a runtime prompt.
#[derive(Debug)]
pub enum PromptAnswer {
    /// Unknown host key.
    HostKey(HostKeyDecision),
    /// Password or passphrase (`None` = cancelled).
    Secret(Option<SecretString>),
    /// Keyboard-interactive answers (`None` = cancelled).
    Interactive(Option<Vec<SecretString>>),
    /// Stop waiting (a Microsoft Entra sign-in dialog was cancelled).
    Cancel,
}

/// Connection state of an SSH terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TermStatus {
    /// Connecting (or waiting on a prompt).
    Connecting,
    /// Connected; `description` is `user@host · method · via …`.
    Connected {
        /// Description.
        description: String,
    },
    /// The connection dropped; retrying.
    Reconnecting {
        /// Attempt number (1-based).
        attempt: u32,
        /// Attempts before giving up.
        of: u32,
        /// Seconds until this attempt.
        in_secs: u64,
    },
}

/// What a terminal connects to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TermTarget {
    /// A local shell; `profile` picks a saved terminal profile.
    Local {
        /// Terminal profile (shell, environment), or the login shell.
        profile: Option<ProfileId>,
    },
    /// A shell on a saved Host over SSH.
    Host(ProfileId),
}

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
    /// Log in to a Host under edit (not necessarily saved) and report the result as
    /// [`Event::TestResult`].
    TestHost {
        /// Request id.
        request: RequestId,
        /// Host under edit.
        host: Host,
        /// Password or passphrase typed in the form (falls back to the stored one).
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
    /// Read a UI setting ([`Event::Setting`]).
    LoadSetting {
        /// Key.
        key: String,
    },
    /// List a local directory.
    ListLocalDir {
        /// Request id.
        request: RequestId,
        /// Directory.
        path: PathBuf,
    },
    /// Detect optional native components ([`Event::Components`]).
    DetectComponents,
    /// Install a component with the strategy its manifest gives for this machine.
    InstallComponent {
        /// Component id.
        id: String,
        /// The user accepted the component's click-through license.
        accept_license: bool,
    },
    /// Install a component from a pre-downloaded archive (checked against the manifest).
    InstallComponentFromFile {
        /// Component id.
        id: String,
        /// Archive.
        path: PathBuf,
    },
    /// Use a library the user already has.
    UseComponentPath {
        /// Component id.
        id: String,
        /// Library file or folder.
        path: PathBuf,
    },
    /// Remove what Switchyard installed (or forget a chosen path).
    RemoveComponent {
        /// Component id.
        id: String,
    },
    /// Download component archives from an internal mirror (`None` = vendor URLs).
    SetDriverMirror {
        /// Base URL.
        url: Option<String>,
    },
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
    /// Open a terminal.
    OpenTerminal {
        /// Terminal id chosen by the UI.
        term: TermId,
        /// Where it connects.
        target: TermTarget,
        /// Initial size in cells.
        size: TermSize,
    },
    /// Send bytes to a terminal's program.
    TerminalInput {
        /// Terminal.
        term: TermId,
        /// Bytes.
        bytes: Vec<u8>,
    },
    /// The terminal view changed size.
    TerminalResize {
        /// Terminal.
        term: TermId,
        /// New size in cells.
        size: TermSize,
    },
    /// Close a terminal.
    CloseTerminal {
        /// Terminal.
        term: TermId,
    },
    /// Retry a dropped SSH terminal now instead of waiting for the backoff.
    ReconnectTerminal {
        /// Terminal.
        term: TermId,
    },
    /// Stop a tunnel; sessions using it end.
    StopTunnel {
        /// Tunnel id.
        id: u64,
    },
    /// Report the live tunnels ([`Event::Tunnels`]).
    ListTunnels,
    /// Answer a prompt the runtime raised.
    AnswerPrompt {
        /// The prompt's request id.
        request: RequestId,
        /// The answer.
        answer: PromptAnswer,
    },
    /// After a changed-key warning: trust exactly `fingerprint` for Host `host` on its next
    /// connection (stored in Switchyard's known_hosts, never the user's file).
    AcceptChangedHostKey {
        /// Host profile.
        host: ProfileId,
        /// The new key's fingerprint, as shown to the user.
        fingerprint: String,
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
    /// A UI setting, answering [`Command::LoadSetting`].
    Setting {
        /// Key.
        key: String,
        /// JSON value, if set.
        value: Option<serde_json::Value>,
    },
    /// Native component status.
    Components(Vec<Component>),
    /// An install is under way.
    ComponentProgress {
        /// Component id.
        id: String,
        /// Step.
        progress: InstallProgress,
    },
    /// A component is ready (installed, found at a chosen path).
    ComponentInstalled {
        /// Its new state.
        component: Component,
    },
    /// Installing, using a path, or removing failed.
    ComponentFailed {
        /// Component id.
        id: String,
        /// Why.
        message: String,
        /// The command to run in a terminal, when elevation is not available here.
        command: Option<String>,
    },
    /// Result of [`Command::ApplyEdits`]: rows changed, or why nothing was.
    EditsApplied {
        /// Request id.
        request: RequestId,
        /// Rows changed or error.
        result: Result<u64, String>,
        /// Time taken.
        elapsed: Duration,
    },
    /// A terminal is ready; the view draws from `terminal`.
    TerminalOpened {
        /// Terminal.
        term: TermId,
        /// Shared terminal state.
        terminal: Terminal,
        /// Short description (`deploy@10.0.4.12 · ed25519`, `/bin/zsh`).
        description: String,
    },
    /// An SSH terminal's connection state changed.
    TerminalStatus {
        /// Terminal.
        term: TermId,
        /// State.
        status: TermStatus,
    },
    /// The server's host key differs from the stored one; the connection is blocked.
    HostKeyChanged {
        /// Terminal that hit it, if any.
        term: Option<TermId>,
        /// Host profile.
        host_id: ProfileId,
        /// Host label.
        host: String,
        /// `address:port`.
        address: String,
        /// Stored fingerprint.
        stored: String,
        /// Received fingerprint.
        received: String,
        /// Where the stored key is (`file:line`).
        location: String,
    },
    /// Unknown host key: trust it?
    HostKeyPrompt {
        /// Answer with this id.
        request: RequestId,
        /// The key.
        key: HostKeyRequest,
    },
    /// A password or key passphrase is needed.
    SecretPrompt {
        /// Answer with this id.
        request: RequestId,
        /// Host label.
        host: String,
        /// Prompt text.
        prompt: String,
    },
    /// Keyboard-interactive questions (MFA).
    InteractivePrompt {
        /// Answer with this id.
        request: RequestId,
        /// The questions.
        req: InteractiveRequest,
    },
    /// Microsoft Entra sign-in in the browser: open `url`, then wait. Answer with
    /// [`PromptAnswer::Cancel`] to give up; [`Event::PromptClosed`] ends the wait.
    EntraSignIn {
        /// Request id.
        request: RequestId,
        /// Connection name.
        connection: String,
        /// Microsoft's sign-in page.
        url: String,
    },
    /// Microsoft Entra device-code sign-in: show the code and where to enter it.
    EntraDeviceCode {
        /// Request id.
        request: RequestId,
        /// Connection name.
        connection: String,
        /// The code to enter.
        code: String,
        /// Where to enter it.
        url: String,
        /// Microsoft's instruction text.
        message: String,
    },
    /// A prompt the runtime raised is no longer needed (sign-in finished or failed).
    PromptClosed {
        /// Request id.
        request: RequestId,
    },
    /// Live tunnels (sent when they open, stop, or their counters change).
    Tunnels(Vec<TunnelInfo>),
    /// A terminal could not be opened.
    TerminalFailed {
        /// Terminal.
        term: TermId,
        /// Why.
        message: String,
    },
    /// New output to draw (coalesced until the next snapshot).
    TerminalWake {
        /// Terminal.
        term: TermId,
    },
    /// The program set the title.
    TerminalTitle {
        /// Terminal.
        term: TermId,
        /// Title (empty = reset).
        title: String,
    },
    /// Bell.
    TerminalBell {
        /// Terminal.
        term: TermId,
    },
    /// The program asked to copy text (OSC 52).
    TerminalClipboard {
        /// Terminal.
        term: TermId,
        /// Text.
        text: String,
    },
    /// The program ended or the connection closed.
    TerminalExited {
        /// Terminal.
        term: TermId,
        /// Exit code, when known.
        code: Option<u32>,
        /// Why, when the connection failed rather than the program exiting.
        message: Option<String>,
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
