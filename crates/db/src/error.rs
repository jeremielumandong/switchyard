//! Error types for database work.

use serde::{Deserialize, Serialize};

/// Result alias for this crate.
pub type Result<T, E = DbError> = std::result::Result<T, E>;

/// Where the server says an error happened in the statement text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorPosition {
    /// 1-based character offset into the statement (PostgreSQL).
    Offset(u32),
    /// 1-based line number within the batch (SQL Server).
    Line(u32),
}

/// A structured error reported by the database server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServerError {
    /// Severity as reported (`ERROR`, `FATAL`, ...).
    pub severity: String,
    /// SQLSTATE or engine error number.
    pub code: Option<String>,
    /// Primary message.
    pub message: String,
    /// Optional detail line.
    pub detail: Option<String>,
    /// Optional hint line.
    pub hint: Option<String>,
    /// Position within the statement, when known.
    pub position: Option<ErrorPosition>,
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.code {
            Some(code) => write!(f, "{} {}: {}", self.severity, code, self.message),
            None => write!(f, "{}: {}", self.severity, self.message),
        }
    }
}

/// Errors produced by drivers and sessions. Messages never contain credentials.
#[derive(Clone, Debug, thiserror::Error)]
pub enum DbError {
    /// Could not establish a connection.
    #[error("connection failed: {0}")]
    Connect(String),
    /// TLS setup or verification failed.
    #[error("TLS error: {0}")]
    Tls(String),
    /// The server rejected a statement.
    #[error("{0}")]
    Server(Box<ServerError>),
    /// The statement was cancelled by the user.
    #[error("query cancelled")]
    Cancelled,
    /// The session is closed.
    #[error("session closed")]
    Closed,
    /// The operation or type is not supported by this driver.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// A parameter could not be bound.
    #[error("parameter error: {0}")]
    Param(String),
    /// Anything else from the wire protocol.
    #[error("protocol error: {0}")]
    Protocol(String),
}

impl DbError {
    /// The server error, if this is one.
    pub fn as_server(&self) -> Option<&ServerError> {
        match self {
            DbError::Server(e) => Some(e.as_ref()),
            _ => None,
        }
    }
}
