//! The saved-profile domain model: Hosts, database and file connections, terminal profiles,
//! environment labels and workspaces. Secrets are referenced by [`SecretRef`], never inline.

use serde::{Deserialize, Serialize};
use switchyard_db::{DbAuthMethod, Engine, SslMode};

use crate::random::random_hex;

/// Stable identifier of a saved profile.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProfileId(pub String);

impl ProfileId {
    /// A new random id.
    pub fn new() -> Self {
        Self(random_hex(8))
    }

    /// The id as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ProfileId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ProfileId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Key of a secret in the keychain or vault. Contains no secret material.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretRef(pub String);

impl SecretRef {
    /// The conventional key for a profile's primary secret.
    pub fn for_profile(id: &ProfileId, purpose: &str) -> Self {
        Self(format!("{}:{purpose}", id.0))
    }
}

/// Environment label. Drives color and safety rules.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvironmentLabel {
    /// Production: red accent, destructive-statement confirmation, optional read-only lock.
    Production,
    /// Staging: amber.
    Staging,
    /// Development: green.
    Development,
    /// Local: neutral gray.
    #[default]
    Local,
}

impl EnvironmentLabel {
    /// All labels in UI order.
    pub const ALL: [EnvironmentLabel; 4] = [
        EnvironmentLabel::Production,
        EnvironmentLabel::Staging,
        EnvironmentLabel::Development,
        EnvironmentLabel::Local,
    ];

    /// Title-case name.
    pub fn name(self) -> &'static str {
        match self {
            EnvironmentLabel::Production => "Production",
            EnvironmentLabel::Staging => "Staging",
            EnvironmentLabel::Development => "Development",
            EnvironmentLabel::Local => "Local",
        }
    }

    /// Short badge (`PROD`, `STG`, `DEV`, `LOCAL`).
    pub fn badge(self) -> &'static str {
        match self {
            EnvironmentLabel::Production => "PROD",
            EnvironmentLabel::Staging => "STG",
            EnvironmentLabel::Development => "DEV",
            EnvironmentLabel::Local => "LOCAL",
        }
    }

    /// Whether destructive statements need confirmation.
    pub fn is_production(self) -> bool {
        self == EnvironmentLabel::Production
    }
}

/// How to authenticate to an SSH host.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "kebab-case")]
pub enum SshAuth {
    /// Password stored in the secret store.
    #[default]
    Password,
    /// Private key read in place (never copied); passphrase in the secret store if any.
    PublicKey {
        /// Path to the private key.
        key_path: String,
    },
    /// Keyboard-interactive (MFA prompts).
    KeyboardInteractive,
    /// SSH agent.
    Agent,
}

impl SshAuth {
    /// Label for selects.
    pub fn label(&self) -> &'static str {
        match self {
            SshAuth::Password => "Password",
            SshAuth::PublicKey { .. } => "Public key",
            SshAuth::KeyboardInteractive => "Keyboard-interactive",
            SshAuth::Agent => "SSH agent",
        }
    }
}

/// A machine reached over SSH.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Host {
    /// Id.
    pub id: ProfileId,
    /// Display name (alias).
    pub name: String,
    /// Address or host name.
    pub address: String,
    /// SSH port.
    pub port: u16,
    /// Login user.
    pub user: String,
    /// Authentication method.
    pub auth: SshAuth,
    /// Jump hosts, outermost first (ids of other Hosts).
    #[serde(default)]
    pub jump_hosts: Vec<ProfileId>,
    /// Environment label.
    pub environment: EnvironmentLabel,
    /// Optional custom color (hex).
    #[serde(default)]
    pub color: Option<String>,
    /// Tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Keepalive interval in seconds (0 = off).
    #[serde(default = "default_keepalive")]
    pub keepalive_secs: u32,
    /// Folder for grouping in the sidebar.
    #[serde(default)]
    pub folder: Option<String>,
    /// Password or key passphrase.
    #[serde(default)]
    pub secret: Option<SecretRef>,
}

fn default_keepalive() -> u32 {
    30
}

impl Host {
    /// A new Host with defaults.
    pub fn new(
        name: impl Into<String>,
        address: impl Into<String>,
        user: impl Into<String>,
    ) -> Self {
        Self {
            id: ProfileId::new(),
            name: name.into(),
            address: address.into(),
            port: 22,
            user: user.into(),
            auth: SshAuth::Password,
            jump_hosts: Vec::new(),
            environment: EnvironmentLabel::Local,
            color: None,
            tags: Vec::new(),
            keepalive_secs: 30,
            folder: None,
            secret: None,
        }
    }
}

/// A database endpoint, optionally reached through a Host.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DbConnection {
    /// Id.
    pub id: ProfileId,
    /// Display name.
    pub name: String,
    /// Engine.
    pub engine: Engine,
    /// Server host (as seen from the tunnel Host when `via_host` is set).
    pub server: String,
    /// Server port.
    pub port: u16,
    /// Database.
    pub database: String,
    /// User.
    pub user: String,
    /// Authentication method.
    #[serde(default)]
    pub auth: DbAuthMethod,
    /// TLS policy.
    #[serde(default)]
    pub ssl_mode: SslMode,
    /// Tunnel through this Host.
    #[serde(default)]
    pub via_host: Option<ProfileId>,
    /// Environment label.
    pub environment: EnvironmentLabel,
    /// Lock the connection read-only.
    #[serde(default)]
    pub read_only: bool,
    /// Record executed statements in history.
    #[serde(default = "yes")]
    pub history_enabled: bool,
    /// Allow coding agents (MCP) to use this connection.
    #[serde(default)]
    pub agent_access: bool,
    /// Rows fetched before pausing (None = app default).
    #[serde(default)]
    pub fetch_limit: Option<u64>,
    /// Folder for grouping in the sidebar.
    #[serde(default)]
    pub folder: Option<String>,
    /// Password.
    #[serde(default)]
    pub secret: Option<SecretRef>,
}

fn yes() -> bool {
    true
}

impl DbConnection {
    /// A new connection with engine defaults.
    pub fn new(name: impl Into<String>, engine: Engine) -> Self {
        Self {
            id: ProfileId::new(),
            name: name.into(),
            engine,
            server: "localhost".into(),
            port: engine.default_port(),
            database: String::new(),
            user: String::new(),
            auth: DbAuthMethod::Password,
            ssl_mode: SslMode::Prefer,
            via_host: None,
            environment: EnvironmentLabel::Local,
            read_only: false,
            history_enabled: true,
            agent_access: false,
            fetch_limit: None,
            folder: None,
            secret: None,
        }
    }
}

/// FTP TLS mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FtpTls {
    /// Plain FTP.
    None,
    /// Explicit TLS (FTPES, `AUTH TLS`).
    #[default]
    Explicit,
    /// Implicit TLS (FTPS on 990).
    Implicit,
}

/// FTP data connection mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FtpMode {
    /// Passive.
    #[default]
    Passive,
    /// Active.
    Active,
}

/// Protocol of a file connection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "protocol", rename_all = "kebab-case")]
pub enum FileProtocol {
    /// SFTP over a Host's SSH session.
    Sftp {
        /// The Host.
        host_id: ProfileId,
    },
    /// FTP or FTPS with its own credentials.
    Ftp {
        /// Server.
        server: String,
        /// Port.
        port: u16,
        /// TLS mode.
        tls: FtpTls,
        /// Data connection mode.
        mode: FtpMode,
        /// User.
        user: String,
    },
}

/// A file-transfer endpoint.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileConnection {
    /// Id.
    pub id: ProfileId,
    /// Display name.
    pub name: String,
    /// Protocol.
    pub protocol: FileProtocol,
    /// Remote directory opened first.
    #[serde(default)]
    pub default_path: Option<String>,
    /// Environment label.
    pub environment: EnvironmentLabel,
    /// Folder for grouping in the sidebar.
    #[serde(default)]
    pub folder: Option<String>,
    /// FTP password.
    #[serde(default)]
    pub secret: Option<SecretRef>,
}

/// A shell to open on a Host or locally.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TerminalProfile {
    /// Id.
    pub id: ProfileId,
    /// Display name.
    pub name: String,
    /// Host, or `None` for a local shell.
    #[serde(default)]
    pub host_id: Option<ProfileId>,
    /// Shell command (empty = login shell).
    #[serde(default)]
    pub shell: String,
    /// Command run after login.
    #[serde(default)]
    pub startup_command: Option<String>,
    /// Extra environment variables.
    #[serde(default)]
    pub env: Vec<(String, String)>,
}

/// An editor buffer kept across restarts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BufferState {
    /// Buffer id.
    pub id: String,
    /// Tab title.
    pub title: String,
    /// Bound connection.
    pub connection_id: Option<ProfileId>,
    /// Text.
    pub text: String,
    /// Cursor byte offset.
    pub cursor: usize,
}

/// A saved layout.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    /// Name.
    pub name: String,
    /// Open editor buffers (autosaved).
    #[serde(default)]
    pub buffers: Vec<BufferState>,
    /// Pinned connections.
    #[serde(default)]
    pub pinned: Vec<ProfileId>,
    /// Sidebar collapsed.
    #[serde(default)]
    pub sidebar_collapsed: bool,
    /// Active buffer id.
    #[serde(default)]
    pub active_buffer: Option<String>,
}

/// Any saved profile.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Profile {
    /// SSH Host.
    Host(Host),
    /// Database connection.
    Db(DbConnection),
    /// File connection.
    File(FileConnection),
    /// Terminal profile.
    Terminal(TerminalProfile),
}

impl Profile {
    /// Id.
    pub fn id(&self) -> &ProfileId {
        match self {
            Profile::Host(p) => &p.id,
            Profile::Db(p) => &p.id,
            Profile::File(p) => &p.id,
            Profile::Terminal(p) => &p.id,
        }
    }

    /// Display name.
    pub fn name(&self) -> &str {
        match self {
            Profile::Host(p) => &p.name,
            Profile::Db(p) => &p.name,
            Profile::File(p) => &p.name,
            Profile::Terminal(p) => &p.name,
        }
    }

    /// Kind key stored in SQLite.
    pub fn kind(&self) -> &'static str {
        match self {
            Profile::Host(_) => "host",
            Profile::Db(_) => "db",
            Profile::File(_) => "file",
            Profile::Terminal(_) => "terminal",
        }
    }

    /// The secret reference, if any.
    pub fn secret(&self) -> Option<&SecretRef> {
        match self {
            Profile::Host(p) => p.secret.as_ref(),
            Profile::Db(p) => p.secret.as_ref(),
            Profile::File(p) => p.secret.as_ref(),
            Profile::Terminal(_) => None,
        }
    }

    /// A copy without any secret reference (for export).
    pub fn without_secret(&self) -> Profile {
        let mut p = self.clone();
        match &mut p {
            Profile::Host(h) => h.secret = None,
            Profile::Db(d) => d.secret = None,
            Profile::File(f) => f.secret = None,
            Profile::Terminal(_) => {}
        }
        p
    }

    /// Validate fields that do not need other profiles.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let name = self.name().trim();
        if name.is_empty() {
            return Err(ValidationError::new("name", "Name is required"));
        }
        if name.len() > 120 {
            return Err(ValidationError::new("name", "Name is too long"));
        }
        match self {
            Profile::Host(h) => {
                if h.address.trim().is_empty() {
                    return Err(ValidationError::new("address", "Address is required"));
                }
                if h.port == 0 {
                    return Err(ValidationError::new("port", "Port must be 1–65535"));
                }
                if h.user.trim().is_empty() {
                    return Err(ValidationError::new("user", "User is required"));
                }
                if let SshAuth::PublicKey { key_path } = &h.auth
                    && key_path.trim().is_empty()
                {
                    return Err(ValidationError::new("key_path", "Key file is required"));
                }
                if h.jump_hosts.contains(&h.id) {
                    return Err(ValidationError::new(
                        "jump_hosts",
                        "A Host cannot jump through itself",
                    ));
                }
            }
            Profile::Db(d) => {
                if d.server.trim().is_empty() {
                    return Err(ValidationError::new("server", "Server is required"));
                }
                if d.port == 0 {
                    return Err(ValidationError::new("port", "Port must be 1–65535"));
                }
                if d.auth == DbAuthMethod::Password && d.user.trim().is_empty() {
                    return Err(ValidationError::new("user", "User is required"));
                }
                if d.fetch_limit == Some(0) {
                    return Err(ValidationError::new(
                        "fetch_limit",
                        "Fetch limit must be positive",
                    ));
                }
            }
            Profile::File(f) => {
                if let FileProtocol::Ftp {
                    server, port, user, ..
                } = &f.protocol
                {
                    if server.trim().is_empty() {
                        return Err(ValidationError::new("server", "Server is required"));
                    }
                    if *port == 0 {
                        return Err(ValidationError::new("port", "Port must be 1–65535"));
                    }
                    if user.trim().is_empty() {
                        return Err(ValidationError::new("user", "User is required"));
                    }
                }
            }
            Profile::Terminal(_) => {}
        }
        Ok(())
    }

    /// Ids of other profiles this one references.
    pub fn references(&self) -> Vec<&ProfileId> {
        match self {
            Profile::Host(h) => h.jump_hosts.iter().collect(),
            Profile::Db(d) => d.via_host.iter().collect(),
            Profile::File(f) => match &f.protocol {
                FileProtocol::Sftp { host_id } => vec![host_id],
                FileProtocol::Ftp { .. } => vec![],
            },
            Profile::Terminal(t) => t.host_id.iter().collect(),
        }
    }

    /// Environment label (terminal profiles inherit Local).
    pub fn environment(&self) -> EnvironmentLabel {
        match self {
            Profile::Host(p) => p.environment,
            Profile::Db(p) => p.environment,
            Profile::File(p) => p.environment,
            Profile::Terminal(_) => EnvironmentLabel::Local,
        }
    }
}

/// A field-level validation failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ValidationError {
    /// Field name.
    pub field: &'static str,
    /// Message for the user.
    pub message: String,
}

impl ValidationError {
    fn new(field: &'static str, message: &str) -> Self {
        Self {
            field,
            message: message.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation() {
        let mut h = Host::new("prod-db-01", "10.0.4.12", "deploy");
        assert!(Profile::Host(h.clone()).validate().is_ok());
        h.address.clear();
        assert_eq!(
            Profile::Host(h.clone()).validate().unwrap_err().field,
            "address"
        );
        h.address = "x".into();
        h.auth = SshAuth::PublicKey {
            key_path: " ".into(),
        };
        assert_eq!(
            Profile::Host(h.clone()).validate().unwrap_err().field,
            "key_path"
        );
        h.auth = SshAuth::Agent;
        h.jump_hosts = vec![h.id.clone()];
        assert_eq!(Profile::Host(h).validate().unwrap_err().field, "jump_hosts");

        let mut d = DbConnection::new("shop", Engine::Postgres);
        d.user = "app".into();
        assert!(Profile::Db(d.clone()).validate().is_ok());
        d.port = 0;
        assert_eq!(Profile::Db(d.clone()).validate().unwrap_err().field, "port");
        d.port = 5432;
        d.name = "  ".into();
        assert_eq!(Profile::Db(d).validate().unwrap_err().field, "name");

        let f = FileConnection {
            id: ProfileId::new(),
            name: "assets".into(),
            protocol: FileProtocol::Ftp {
                server: "".into(),
                port: 21,
                tls: FtpTls::Explicit,
                mode: FtpMode::Passive,
                user: "u".into(),
            },
            default_path: None,
            environment: EnvironmentLabel::Local,
            folder: None,
            secret: None,
        };
        assert_eq!(Profile::File(f).validate().unwrap_err().field, "server");
    }

    #[test]
    fn serde_round_trip() {
        let mut h = Host::new("bastion", "bastion.acme.dev", "deploy");
        h.auth = SshAuth::PublicKey {
            key_path: "~/.ssh/id_ed25519".into(),
        };
        h.environment = EnvironmentLabel::Production;
        let mut d = DbConnection::new("shop_prod", Engine::Postgres);
        d.via_host = Some(h.id.clone());
        d.secret = Some(SecretRef::for_profile(&d.id, "password"));
        let t = TerminalProfile {
            id: ProfileId::new(),
            name: "zsh".into(),
            host_id: None,
            shell: "/bin/zsh".into(),
            startup_command: None,
            env: vec![("TERM".into(), "xterm-256color".into())],
        };
        for p in [Profile::Host(h), Profile::Db(d), Profile::Terminal(t)] {
            let json = serde_json::to_string(&p).unwrap();
            let back: Profile = serde_json::from_str(&json).unwrap();
            assert_eq!(back, p);
        }
        let w = Workspace {
            name: "acme-ops".into(),
            ..Default::default()
        };
        let back: Workspace = serde_json::from_str(&serde_json::to_string(&w).unwrap()).unwrap();
        assert_eq!(back, w);
    }

    #[test]
    fn export_copy_has_no_secret() {
        let mut d = DbConnection::new("shop", Engine::Postgres);
        d.secret = Some(SecretRef::for_profile(&d.id, "password"));
        assert!(Profile::Db(d).without_secret().secret().is_none());
    }
}
