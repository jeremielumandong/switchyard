//! Terminal errors.

/// Errors from terminals and PTYs.
#[derive(Debug, thiserror::Error)]
pub enum TermError {
    /// The PTY or program could not be started.
    #[error("could not start the shell: {0}")]
    Spawn(String),
}

/// Result alias.
pub type Result<T> = std::result::Result<T, TermError>;
