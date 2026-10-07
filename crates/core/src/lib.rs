//! Switchyard core: the tokio runtime owned by the app, the command/event bus between UI
//! and runtime, and services for profiles, secrets, sessions, queries, history and the
//! schema cache. The desktop app and the `swy` CLI share this crate.

pub mod api_secrets;
pub mod bus;
pub mod components;
mod entra;
pub mod env;
pub mod error;
pub mod files;
pub mod handoff;
pub mod prompts;
pub mod runtime;
pub mod service;
pub mod ssh_import;
pub mod terminals;

pub use bus::{
    Command, Event, FetchLimit, FsOp, FsRef, OnConflict, PromptAnswer, QueryEvent, QueryId,
    RequestId, SaveError, SessionId, StatementRequest, TermId, TermStatus, TermTarget, TextFile,
    TransferError,
};
pub use error::{CoreError, Result};
pub use runtime::{Core, EventReceiver, EventSender, RuntimeHandle};
pub use service::agent::{AgentRows, cell_json};
pub use service::{SecretBackendChoice, ServiceConfig};

/// The API workspace, re-exported for the app.
pub use switchyard_api as api;
/// Re-exported database contracts.
pub use switchyard_db as db;
/// Tunnel endpoints are defined in `db` so drivers stay independent of `remote`.
pub use switchyard_db::TunnelEndpoint;
/// Re-exported Driver Manager.
pub use switchyard_drivers as drivers;
/// Query plans, re-exported for the app.
pub use switchyard_plan as plan;
/// Re-exported remote layer.
pub use switchyard_remote as remote;
/// Re-exported store and model.
pub use switchyard_store as store;
/// Re-exported terminal state.
pub use switchyard_term as term;
