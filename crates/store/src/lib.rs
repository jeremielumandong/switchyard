//! Persistent state: the profile model, the SQLite profile store (profiles, buffers,
//! history, schema cache, settings, snippets, favorites, terminal macros, runbooks, Host
//! tags, workspaces) and secret storage (OS keychain or encrypted vault).

pub mod credentials;
pub mod desktop;
pub mod error;
pub mod favorites;
pub mod macros;
pub mod model;
pub mod named_secrets;
pub mod paths;
pub mod random;
pub mod runbooks;
pub mod secrets;
pub mod snippets;
pub mod store;
pub mod tags;
pub mod workspace_file;
pub mod workspaces;

pub use error::{Result, StoreError};
pub use favorites::Favorite;
pub use macros::Macro;
pub use model::{
    BufferState, CloudAuth, CloudConnection, CloudProvider, CloudService, ContainerConnection,
    ContainerPlatform, Credential, CredentialKind, DbConnection, DesktopDisplay, DesktopProtocol,
    DesktopScreen, DesktopServer, EnvironmentLabel, FileConnection, FileProtocol, ForwardDirection,
    FtpMode, FtpTls, Host, HostPatch, PortForward, Profile, ProfileId, RancherSource, RdpGateway,
    SecretRef, SshAuth, TerminalColors, TerminalProfile, ValidationError, Workspace,
};
pub use named_secrets::{NamedSecret, NamedSecretSource};
pub use paths::AppPaths;
pub use runbooks::{
    ApiRequestRef, HttpStep, Runbook, RunbookParam, RunbookStep, ShellTarget, StepKind,
};
pub use secrets::{KeychainStore, MemoryStore, SecretStore, VaultStore};
pub use snippets::Snippet;
pub use store::{HistoryEntry, HistoryStatus, ProfileExport, Store, now_ms};
pub use workspaces::{ALL_ID, SavedTab, ServerFacts, WorkspaceScope};
