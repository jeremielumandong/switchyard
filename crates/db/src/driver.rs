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
    /// Integrated authentication as the signed-in user: Windows SSPI, or Kerberos (a
    /// `kinit` ticket) on Linux and macOS.
    Integrated,
    /// A Windows / Active Directory account given as `DOMAIN\user` and password (SSPI on
    /// Windows, NTLM elsewhere).
    WindowsPassword,
    /// Microsoft Entra ID: sign in through the system browser (MFA, conditional access).
    EntraInteractive,
    /// Microsoft Entra ID: enter a code at microsoft.com/devicelogin on any device (MFA).
    EntraDeviceCode,
    /// Microsoft Entra ID user name and password (no MFA).
    EntraPassword,
    /// Microsoft Entra ID service principal: application (client) id and client secret.
    EntraServicePrincipal,
    /// Snowflake key-pair authentication: a signed JWT from an RSA private key file
    /// (`DbConfig::options["private_key_path"]`); the stored secret is the key's
    /// passphrase, if it has one.
    KeyPair,
    /// A bearer token issued by the server (Snowflake programmatic access token), stored
    /// like a password.
    AccessToken,
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
            Self::Password
                | Self::WindowsPassword
                | Self::EntraPassword
                | Self::EntraServicePrincipal
                | Self::KeyPair
                | Self::AccessToken
        )
    }

    /// Label for the connection editor.
    pub fn label(self) -> &'static str {
        match self {
            Self::Password => "SQL login",
            Self::Integrated => "Integrated (current user)",
            Self::WindowsPassword => "Windows account (DOMAIN\\user)",
            Self::EntraInteractive => "Microsoft Entra · browser (MFA)",
            Self::EntraDeviceCode => "Microsoft Entra · device code (MFA)",
            Self::EntraPassword => "Microsoft Entra · password",
            Self::EntraServicePrincipal => "Microsoft Entra · service principal",
            Self::KeyPair => "Key pair (private key file)",
            Self::AccessToken => "Programmatic access token",
        }
    }
}

/// One client-side security handshake (e.g. Kerberos through GSSAPI): each step takes the
/// server's last token, none at first, and returns the next token to send.
pub trait SecurityContext: Send {
    /// One step.
    fn step(&mut self, input: Option<&[u8]>) -> std::result::Result<Option<Vec<u8>>, String>;
}

/// Starts security handshakes for a service principal name such as `MSSQLSvc/db:1433`.
pub trait SecurityProvider: Send + Sync {
    /// A new handshake with `spn`.
    fn start(&self, spn: &str) -> std::result::Result<Box<dyn SecurityContext>, String>;
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
    /// Integrated authentication through a library loaded at runtime (Kerberos on Linux and
    /// macOS); set by core for [`DbAuthMethod::Integrated`] where needed.
    pub security: Option<std::sync::Arc<dyn SecurityProvider>>,
    /// Engine-specific settings that have no field of their own (Snowflake `warehouse`,
    /// `role`, `schema`, `private_key_path`). Never secrets.
    pub options: std::collections::BTreeMap<String, String>,
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
            security: None,
            options: std::collections::BTreeMap::new(),
        }
    }

    /// An engine-specific option, trimmed; `None` when unset or blank.
    pub fn option(&self, key: &str) -> Option<&str> {
        self.options
            .get(key)
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    const AUTH: [DbAuthMethod; 9] = [
        DbAuthMethod::Password,
        DbAuthMethod::Integrated,
        DbAuthMethod::WindowsPassword,
        DbAuthMethod::EntraInteractive,
        DbAuthMethod::EntraDeviceCode,
        DbAuthMethod::EntraPassword,
        DbAuthMethod::EntraServicePrincipal,
        DbAuthMethod::KeyPair,
        DbAuthMethod::AccessToken,
    ];

    #[test]
    fn debug_never_shows_host_user_or_secrets() {
        let mut cfg = DbConfig::new(Engine::Postgres, "db.internal.example", "shop");
        cfg.user = "alice".into();
        cfg.password = Some(SecretString::from("hunter2-password"));
        cfg.access_token = Some(SecretString::from("eyJ-token"));
        cfg.trusted_ca_pem = Some("-----BEGIN CERTIFICATE-----".into());
        cfg.options.insert("role".into(), "ANALYST".into());
        let shown = format!("{cfg:?} {cfg:#?}");
        for hidden in [
            "db.internal.example",
            "alice",
            "hunter2",
            "eyJ-token",
            "CERTIFICATE",
            "ANALYST",
        ] {
            assert!(!shown.contains(hidden), "{hidden} leaked: {shown}");
        }
        assert!(shown.contains("shop"), "{shown}");
        assert!(shown.contains("Postgres"), "{shown}");
    }

    #[test]
    fn new_uses_the_engine_port_and_safe_defaults() {
        for (engine, port) in [
            (Engine::Postgres, 5432),
            (Engine::SqlServer, 1433),
            (Engine::MySql, 3306),
            (Engine::Redis, 6379),
            (Engine::Sqlite, 0),
        ] {
            let cfg = DbConfig::new(engine, "h", "d");
            assert_eq!(cfg.port, port, "{engine:?}");
            assert_eq!(cfg.ssl_mode, SslMode::Prefer);
            assert_eq!(cfg.auth, DbAuthMethod::Password);
            assert!(!cfg.read_only);
            assert!(cfg.password.is_none() && cfg.access_token.is_none());
        }
    }

    #[test]
    fn options_are_trimmed_and_blank_means_unset() {
        let mut cfg = DbConfig::new(Engine::Snowflake, "acct", "db");
        cfg.options
            .insert("warehouse".into(), "  COMPUTE_WH \n".into());
        cfg.options.insert("role".into(), "   ".into());
        assert_eq!(cfg.option("warehouse"), Some("COMPUTE_WH"));
        assert_eq!(cfg.option("role"), None);
        assert_eq!(cfg.option("schema"), None);
    }

    #[test]
    fn entra_methods_get_tokens_and_secret_methods_get_a_secret() {
        for m in AUTH {
            let entra = m.is_entra();
            assert_eq!(
                entra,
                m.label().starts_with("Microsoft Entra"),
                "{m:?}: label and is_entra disagree"
            );
            // Interactive sign-ins have nothing to store; everything else that is not
            // integrated keeps a password, client secret, passphrase or token.
            let expect_secret = !matches!(
                m,
                DbAuthMethod::Integrated
                    | DbAuthMethod::EntraInteractive
                    | DbAuthMethod::EntraDeviceCode
            );
            assert_eq!(m.uses_secret(), expect_secret, "{m:?}");
        }
    }

    #[test]
    fn stored_names_stay_stable() {
        // Profiles persist these; renaming one would break saved connections.
        let auth: Vec<String> = AUTH
            .iter()
            .map(|m| serde_json::to_string(m).unwrap_or_default())
            .collect();
        assert_eq!(
            auth,
            [
                "\"password\"",
                "\"integrated\"",
                "\"windows-password\"",
                "\"entra-interactive\"",
                "\"entra-device-code\"",
                "\"entra-password\"",
                "\"entra-service-principal\"",
                "\"key-pair\"",
                "\"access-token\"",
            ]
        );
        for m in AUTH {
            let back: DbAuthMethod =
                serde_json::from_str(&serde_json::to_string(&m).unwrap_or_default())
                    .unwrap_or_default();
            assert_eq!(back, m);
        }
        for mode in SslMode::ALL {
            assert_eq!(
                serde_json::to_string(&mode).unwrap_or_default(),
                format!("\"{}\"", mode.label())
            );
        }
    }

    #[tokio::test]
    async fn cancel_sets_the_flag_then_asks_the_server() {
        let calls = Arc::new(AtomicUsize::new(0));
        let requested = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&calls);
        let flag = Arc::clone(&requested);
        let handle = CancelHandle::new(Arc::clone(&requested), move || {
            // The flag is already up when the server-side cancel runs.
            assert!(flag.load(Ordering::SeqCst));
            seen.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        });
        let clone = handle.clone();
        assert!(!handle.is_requested());
        assert_eq!(format!("{handle:?}"), "CancelHandle { requested: false }");
        assert!(clone.cancel().await.is_ok());
        assert!(handle.is_requested());
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let only = CancelHandle::flag_only(Arc::new(AtomicBool::new(false)));
        assert!(only.cancel().await.is_ok());
        assert!(only.is_requested());
    }
}
