//! Store errors.

use crate::model::ValidationError;

/// Result alias.
pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// Errors from the profile store and secret backends. Messages never contain secrets.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// SQLite failure.
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// JSON encode/decode failure.
    #[error("invalid data: {0}")]
    Json(#[from] serde_json::Error),
    /// File system failure.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// A profile failed validation.
    #[error("{0}")]
    Validation(#[from] ValidationError),
    /// A referenced profile does not exist.
    #[error("{0} not found")]
    NotFound(String),
    /// The profile is still referenced by others.
    #[error("{0} is used by {1}")]
    InUse(String, String),
    /// Keychain failure.
    #[error("keychain error: {0}")]
    Keychain(String),
    /// The vault is locked; unlock it with the master password.
    #[error("the secret vault is locked")]
    VaultLocked,
    /// Wrong master password or corrupted vault.
    #[error("wrong master password or damaged vault")]
    BadPassword,
    /// The store was created by a newer Switchyard.
    #[error("profile store version {0} is newer than this app supports")]
    TooNew(i64),
}
