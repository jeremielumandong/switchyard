//! Headless API Workbench runtime.
//!
//! The shared, GPUI-free runner used by both the desktop Workbench panel and
//! the `agentops-workbench` sidecar. It owns everything between a saved
//! request (or collection) and its persisted exchange/run: the authenticated
//! loopback transport to the agent service ([`transport`]), session secrets
//! and the environment/data parsers ([`secrets`]), managed OAuth renewal
//! ([`oauth`]), sign-in sessions ([`login`]), single sends ([`send`]),
//! collection runs ([`collection`]) and workspace persistence
//! ([`workspace`]).
//!
//! Nothing here opens a browser or a file picker: a send that needs either
//! fails closed with a message pointing at the desktop app.

pub mod collection;
pub mod login;
pub mod oauth;
pub mod secrets;
pub mod send;
pub mod transport;
pub mod workspace;

pub use collection::{
    MAX_DELAY_MS, MAX_ITERATIONS, NextRunItem, RunControl, RunOutcome, RunPreparation,
    RunPreparationInput, RunSession, prepare_collection_run, run_collection, select_run_requests,
};
pub use login::{LoginChain, LoginOwner, LoginSession, replace_bearer, session_status};
pub use oauth::{
    BrowserAuthorization, PKCE_NEEDS_BROWSER, describe_missing_auth_secret, pkce_needs_browser,
    refresh_expired_oauth_token,
};
pub use secrets::{
    DraftSecrets, auth_secret_reference, compile_context, parse_cookie_pairs, parse_data_rows,
    parse_session_variables,
};
pub use send::{
    OperationAbortGuard, PreparedStandaloneSend, StandaloneSendInput, StandaloneSendResult,
    exchange_from_send, execute_standalone_send, folder_chain, folder_is_within, now_millis,
    now_seconds, prepare_standalone_send, rerun_response_tests,
    rerun_response_tests_with_redactions,
};
pub use transport::{
    FileCapabilities, NativeWorkbenchTransport, OAuthTokenRequest, OAuthTokenResponse,
    OperationPhase, PickerFileGrant, Redirect, Response, ScriptConsoleEntry, ScriptNextRequest,
    ScriptRequestView, ScriptResponseView, ScriptResult, ScriptScopes, ScriptTestResult,
    USER_DATA_DIR_ENV, WorkbenchTransport, default_user_data_dir, exchange_oauth_token,
    response_from_snapshot, response_snapshot, set_process_defaults,
};
pub use workspace::{
    StorageCommand, TerminalCommand, WorkspaceData, WorkspaceList, create_workspace, execute,
    list_workspaces, mark_workspace_opened, persist_terminal, preview_request_url_updates,
    workspace_id_for,
};
