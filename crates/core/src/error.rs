//! Core errors.

use switchyard_db::DbError;
use switchyard_store::StoreError;

/// Result alias.
pub type Result<T, E = CoreError> = std::result::Result<T, E>;

/// Errors surfaced by core services.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// Store or secret failure.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// Database failure.
    #[error(transparent)]
    Db(#[from] DbError),
    /// Cloud service failure.
    #[error(transparent)]
    Cloud(#[from] switchyard_cloud::CloudError),
    /// Something that does not exist.
    #[error("{0} not found")]
    NotFound(String),
    /// Not supported (yet).
    #[error("{0}")]
    Unsupported(String),
    /// The runtime could not start.
    #[error("startup failed: {0}")]
    Startup(String),
    /// Internal failure (task panicked, channel closed).
    #[error("internal error: {0}")]
    Internal(String),
}
