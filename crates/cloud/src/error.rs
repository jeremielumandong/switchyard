//! Errors from cloud services.

use switchyard_remote::FsError;

/// Cloud service errors.
#[derive(Debug, thiserror::Error)]
pub enum CloudError {
    /// The service could not be reached (DNS, TLS, timeout).
    #[error("{0}")]
    Network(String),
    /// Credentials are missing, expired or refused.
    #[error("{0}")]
    Auth(String),
    /// The service answered with an error.
    #[error("{message}")]
    Api {
        /// HTTP status.
        status: u16,
        /// Service error code (`NoSuchKey`, `ResourceNotFoundException`), when given.
        code: Option<String>,
        /// Message for the user.
        message: String,
    },
    /// A settings or input problem found before any request.
    #[error("{0}")]
    Invalid(String),
    /// The connection is read-only.
    #[error("this connection is read-only")]
    ReadOnly,
    /// Not supported by this service.
    #[error("not supported: {0}")]
    Unsupported(&'static str),
    /// Local I/O.
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl CloudError {
    /// Whether the service said the item does not exist.
    pub fn is_not_found(&self) -> bool {
        matches!(self, CloudError::Api { status: 404, .. })
    }

    pub(crate) fn api(status: u16, code: Option<String>, message: impl Into<String>) -> Self {
        let mut message = message.into();
        if message.trim().is_empty() {
            message = match status {
                401 => "Not signed in or the credentials were refused (401)".into(),
                403 => "Access denied (403): the credentials lack permission for this".into(),
                404 => "Not found (404)".into(),
                409 => "Conflict (409)".into(),
                412 => "Changed by someone else since it was loaded (412)".into(),
                s => format!("The service answered {s}"),
            };
        }
        if let Some(c) = &code
            && !message.contains(c.as_str())
        {
            message = format!("{c}: {message}");
        }
        CloudError::Api {
            status,
            code,
            message,
        }
    }
}

impl From<CloudError> for FsError {
    fn from(e: CloudError) -> Self {
        match e {
            CloudError::Io(e) => FsError::Io(e),
            CloudError::Unsupported(what) => FsError::Unsupported(what),
            e if e.is_not_found() => FsError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                e.to_string(),
            )),
            e => FsError::Remote(e.to_string()),
        }
    }
}

/// Result alias.
pub type Result<T> = std::result::Result<T, CloudError>;
