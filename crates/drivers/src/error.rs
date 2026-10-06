//! Driver Manager errors.

use thiserror::Error;

/// Result alias.
pub type Result<T, E = DriverError> = std::result::Result<T, E>;

/// Why a Driver Manager operation failed.
#[derive(Debug, Error)]
pub enum DriverError {
    /// The manifest could not be read.
    #[error("driver manifest: {0}")]
    Manifest(String),
    /// The manifest signature does not verify against Switchyard's key.
    #[error("the driver manifest signature does not verify: {0}")]
    Signature(String),
    /// A download or file does not match the manifest's SHA-256.
    #[error("{file} does not match the signed manifest (SHA-256 {actual}, expected {expected})")]
    Checksum {
        /// File name.
        file: String,
        /// SHA-256 from the manifest.
        expected: String,
        /// SHA-256 of what arrived.
        actual: String,
    },
    /// Download failed.
    #[error("download failed: {0}")]
    Download(String),
    /// The archive is malformed or unsafe (absolute paths, `..`, links leaving the folder).
    #[error("archive: {0}")]
    Archive(String),
    /// File system failure.
    #[error("{0}")]
    Io(String),
    /// No such component in the manifest.
    #[error("unknown component {0}")]
    Unknown(String),
    /// Nothing can be installed automatically here.
    #[error("{0}")]
    NoStrategy(String),
    /// A click-through license must be accepted first.
    #[error("the license for {0} must be accepted before downloading")]
    LicenseRequired(String),
    /// A chosen path does not contain the component.
    #[error("{0}")]
    BadPath(String),
    /// The library exists but could not be loaded.
    #[error("could not load {path}: {message}")]
    Load {
        /// Library path.
        path: String,
        /// Loader message.
        message: String,
    },
    /// A package-manager command failed.
    #[error("{0}")]
    Command(String),
    /// Elevation is not available; the user must run the command in a terminal.
    #[error("run this in a terminal: {0}")]
    NeedsTerminal(String),
}

impl From<std::io::Error> for DriverError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}
