//! Persistent state: the profile model, the SQLite profile store (profiles, buffers,
//! history, schema cache, settings, snippets) and secret storage (OS keychain or encrypted vault).

pub mod error;
pub mod model;
pub mod paths;
pub mod random;
pub mod secrets;
pub mod snippets;
pub mod store;

pub use error::{Result, StoreError};
pub use model::{
    BufferState, DbConnection, EnvironmentLabel, FileConnection, FileProtocol, ForwardDirection,
    FtpMode, FtpTls, Host, PortForward, Profile, ProfileId, SecretRef, SshAuth, TerminalProfile,
    ValidationError, Workspace,
};
pub use paths::AppPaths;
pub use secrets::{KeychainStore, MemoryStore, SecretStore, VaultStore};
pub use snippets::Snippet;
pub use store::{HistoryEntry, HistoryStatus, ProfileExport, Store, now_ms};
