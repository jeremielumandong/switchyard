//! Switchyard core: the tokio runtime owned by the app, the command/event bus between UI
//! and runtime, and services for profiles, secrets, sessions, queries, history and the
//! schema cache. The desktop app and the `swy` CLI share this crate.

pub mod bus;
mod entra;
pub mod env;
pub mod error;
pub mod prompts;
pub mod runtime;
pub mod service;
pub mod terminals;

pub use bus::{
    Command, Event, FetchLimit, PromptAnswer, QueryEvent, QueryId, RequestId, SessionId,
    StatementRequest, TermId, TermStatus, TermTarget,
};
pub use error::{CoreError, Result};
pub use runtime::{Core, EventReceiver, EventSender, RuntimeHandle};
pub use service::{SecretBackendChoice, ServiceConfig};

/// Re-exported database contracts.
pub use switchyard_db as db;
/// Tunnel endpoints are defined in `db` so drivers stay independent of `remote`.
pub use switchyard_db::TunnelEndpoint;
/// Re-exported Driver Manager.
pub use switchyard_drivers as drivers;
/// Re-exported remote layer.
pub use switchyard_remote as remote;
/// Re-exported store and model.
pub use switchyard_store as store;
/// Re-exported terminal state.
pub use switchyard_term as term;
