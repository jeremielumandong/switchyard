//! The [`Driver`] and [`DbSession`] contracts.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::future::BoxFuture;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

use crate::catalog::{CatalogChunk, IntrospectScope};
use crate::dialect::Dialect;
use crate::error::Result;
use crate::stream::ResultStream;
use crate::value::{Engine, Value};

/// Identifier of an optional native component checked by the Driver Manager
/// (for example `gssapi`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ComponentId(pub String);

/// A local endpoint that forwards to the database through an SSH tunnel.
///
/// Produced by `switchyard-remote` and re-exported by `switchyard-core`; defined here so
/// drivers do not depend on the remote layer.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TunnelEndpoint {
    /// Local address to connect to (usually `127.0.0.1`).
    pub host: String,
    /// Local port.
    pub port: u16,
}

/// TLS policy for a database connection. Verification is always on when TLS is used.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SslMode {
    /// Never use TLS.
    Disable,
    /// Use TLS if the server supports it.
    #[default]
    Prefer,
    /// Require TLS with certificate verification.
    Require,
    /// Require TLS and verify the certificate matches the host name.
    VerifyFull,
}

impl SslMode {
    /// All modes in UI order.
    pub const ALL: [SslMode; 4] = [
        SslMode::Disable,
        SslMode::Prefer,
        SslMode::Require,
        SslMode::VerifyFull,
    ];

    /// Label for selects.
    pub fn label(self) -> &'static str {
        match self {
            SslMode::Disable => "disable",
            SslMode::Prefer => "prefer",
            SslMode::Require => "require",
            SslMode::VerifyFull => "verify-full",
        }
    }
}

/// How to authenticate to the database.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DbAuthMethod {
    /// User name and password.
    #[default]
    Password,
    /// Integrated (Kerberos / Active Directory) authentication.
    Integrated,
    /// Microsoft Entra ID: sign in through the system browser (MFA, conditional access).
    EntraInteractive,
    /// Microsoft Entra ID: enter a code at microsoft.com/devicelogin on any device (MFA).
    EntraDeviceCode,
    /// Microsoft Entra ID user name and password (no MFA).
    EntraPassword,
    /// Microsoft Entra ID service principal: application (client) id and client secret.
    EntraServicePrincipal,
}

impl DbAuthMethod {
    /// Whether this is a Microsoft Entra ID method (an access token is needed to connect).
    pub fn is_entra(self) -> bool {
        matches!(
            self,
            Self::EntraInteractive
                | Self::EntraDeviceCode
                | Self::EntraPassword
                | Self::EntraServicePrincipal
        )
    }

    /// Whether the method uses a stored password or client secret.
    pub fn uses_secret(self) -> bool {
        matches!(
            self,
            Self::Password | Self::EntraPassword | Self::EntraServicePrincipal
        )
    }

    /// Label for the connection editor.
    pub fn label(self) -> &'static str {
        match self {
            Self::Password => "SQL login",
            Self::Integrated => "Integrated (Kerberos / AD)",
            Self::EntraInteractive => "Microsoft Entra · browser (MFA)",
            Self::EntraDeviceCode => "Microsoft Entra · device code (MFA)",
            Self::EntraPassword => "Microsoft Entra · password",
            Self::EntraServicePrincipal => "Microsoft Entra · service principal",
        }
    }
}

/// Everything a driver needs to connect. Built by core from a saved profile and the
/// secret store; never persisted as-is.
#[derive(Clone)]
pub struct DbConfig {
    /// Engine.
    pub engine: Engine,
    /// Server host name or address.
    pub host: String,
    /// Server port.
    pub port: u16,
    /// Database name.
    pub database: String,
    /// User name.
    pub user: String,
    /// Password, when using password auth.
    pub password: Option<SecretString>,
    /// Authentication method.
    pub auth: DbAuthMethod,
    /// Microsoft Entra ID access token for the database, obtained by core for the
    /// `Entra*` methods.
    pub access_token: Option<SecretString>,
    /// TLS policy.
    pub ssl_mode: SslMode,
    /// Connect timeout.
    pub connect_timeout: Duration,
    /// Open the session read-only.
    pub read_only: bool,
    /// Name reported to the server.
    pub application_name: String,
    /// Extra certificate authority (PEM) trusted for this connection only, e.g. a
    /// company CA or a pinned self-signed server certificate. Verification stays on.
    pub trusted_ca_pem: Option<String>,
}

impl DbConfig {
    /// A config with defaults for `engine`.
    pub fn new(engine: Engine, host: impl Into<String>, database: impl Into<String>) -> Self {
        Self {
            engine,
            host: host.into(),
            port: engine.default_port(),
            database: database.into(),
            user: String::new(),
            password: None,
            auth: DbAuthMethod::Password,
            access_token: None,
            ssl_mode: SslMode::Prefer,
            connect_timeout: Duration::from_secs(10),
            read_only: false,
            application_name: "Switchyard".into(),
            trusted_ca_pem: None,
        }
    }
}

impl fmt::Debug for DbConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Hosts and users stay out of logs; the password is never shown.
        f.debug_struct("DbConfig")
            .field("engine", &self.engine)
            .field("database", &self.database)
            .field("ssl_mode", &self.ssl_mode)
            .field("read_only", &self.read_only)
            .finish_non_exhaustive()
    }
}

type CancelFn = dyn Fn() -> BoxFuture<'static, Result<()>> + Send + Sync;

/// A handle that cancels whatever its session is running. Cheap to clone; safe to call
/// from any thread while the query streams.
#[derive(Clone)]
pub struct CancelHandle {
    requested: Arc<AtomicBool>,
    cancel: Arc<CancelFn>,
}

impl CancelHandle {
    /// Wrap an engine-specific cancel function.
    pub fn new(
        requested: Arc<AtomicBool>,
        cancel: impl Fn() -> BoxFuture<'static, Result<()>> + Send + Sync + 'static,
    ) -> Self {
        Self {
            requested,
            cancel: Arc::new(cancel),
        }
    }

    /// A handle that only sets the flag (for drivers without server-side cancel).
    pub fn flag_only(requested: Arc<AtomicBool>) -> Self {
        Self::new(requested, || Box::pin(async { Ok(()) }))
    }

    /// Request cancellation: sets the local flag and asks the server to stop.
    pub async fn cancel(&self) -> Result<()> {
        self.requested.store(true, Ordering::SeqCst);
        (self.cancel)().await
    }

    /// Whether cancellation was requested.
    pub fn is_requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }
}

impl fmt::Debug for CancelHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancelHandle")
            .field("requested", &self.is_requested())
            .finish()
    }
}

/// A database engine implementation.
pub trait Driver: Send + Sync {
    /// The engine.
    fn engine(&self) -> Engine;
    /// The dialect used for splitting, quoting and catalog queries.
    fn dialect(&self) -> &dyn Dialect;
    /// Optional native components this configuration needs (checked by the Driver Manager).
    fn requirements(&self, cfg: &DbConfig) -> Vec<ComponentId>;
    /// Open a session, directly or through a tunnel endpoint.
    fn connect<'a>(
        &'a self,
        cfg: &'a DbConfig,
        via: Option<TunnelEndpoint>,
    ) -> BoxFuture<'a, Result<Box<dyn DbSession>>>;
}

/// An open database session.
pub trait DbSession: Send {
    /// Execute one statement (or batch) and stream its results.
    fn execute<'a>(
        &'a mut self,
        sql: &'a str,
        params: &'a [Value],
    ) -> BoxFuture<'a, Result<ResultStream>>;
    /// A handle that cancels the running statement.
    fn cancel_handle(&self) -> CancelHandle;
    /// Load one catalog scope.
    fn introspect(&mut self, scope: IntrospectScope) -> BoxFuture<'_, Result<CatalogChunk>>;
    /// Begin a transaction.
    fn begin(&mut self) -> BoxFuture<'_, Result<()>>;
    /// Commit the open transaction.
    fn commit(&mut self) -> BoxFuture<'_, Result<()>>;
    /// Roll back the open transaction.
    fn rollback(&mut self) -> BoxFuture<'_, Result<()>>;
    /// Whether a transaction is open.
    fn in_transaction(&self) -> bool;
    /// Server version string, e.g. `PostgreSQL 16.4`.
    fn server_version(&self) -> String;
    /// Whether the underlying connection is closed.
    fn is_closed(&self) -> bool;
}
