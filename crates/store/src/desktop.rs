//! Remote desktops (RDP and VNC servers) opened with the system's client (MX-12). The
//! profile holds the address, sign-in (its own secret or a shared [`crate::Credential`]),
//! an optional RD Gateway, an optional SSH tunnel through a saved Host and display
//! options. Secrets are referenced, never inline.

use serde::{Deserialize, Serialize};

use crate::model::{EnvironmentLabel, ProfileId, SecretRef};

/// The remote desktop protocol.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DesktopProtocol {
    /// Microsoft Remote Desktop Protocol.
    #[default]
    Rdp,
    /// VNC (RFB).
    Vnc,
}

impl DesktopProtocol {
    /// `RDP` / `VNC`.
    pub fn badge(self) -> &'static str {
        match self {
            DesktopProtocol::Rdp => "RDP",
            DesktopProtocol::Vnc => "VNC",
        }
    }

    /// The protocol's usual port.
    pub fn default_port(self) -> u16 {
        match self {
            DesktopProtocol::Rdp => 3389,
            DesktopProtocol::Vnc => 5900,
        }
    }

    /// History tag of a launch (`rdp`, `vnc`).
    pub fn history_tag(self) -> &'static str {
        match self {
            DesktopProtocol::Rdp => "rdp",
            DesktopProtocol::Vnc => "vnc",
        }
    }
}

/// An RD Gateway in front of the server.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RdpGateway {
    /// Gateway host name (`gw.example.com`, optionally `:port`).
    pub host: String,
    /// Gateway user; empty uses the server's sign-in.
    #[serde(default)]
    pub user: String,
    /// A shared credential for the gateway; `None` uses the server's sign-in.
    #[serde(default)]
    pub credential: Option<ProfileId>,
}

/// Window size of the remote session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case")]
pub enum DesktopScreen {
    /// Full screen.
    #[default]
    FullScreen,
    /// A window of this size.
    Window {
        /// Width in pixels.
        width: u16,
        /// Height in pixels.
        height: u16,
    },
}

/// Display and redirection options.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopDisplay {
    /// Full screen or a window size.
    #[serde(default)]
    pub screen: DesktopScreen,
    /// Span all monitors (RDP).
    #[serde(default)]
    pub multi_monitor: bool,
    /// Share the clipboard.
    #[serde(default = "yes")]
    pub clipboard: bool,
    /// Make local drives available in the session (RDP).
    #[serde(default)]
    pub drives: bool,
}

fn yes() -> bool {
    true
}

impl Default for DesktopDisplay {
    fn default() -> Self {
        Self {
            screen: DesktopScreen::FullScreen,
            multi_monitor: false,
            clipboard: true,
            drives: false,
        }
    }
}

/// A remote desktop server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DesktopServer {
    /// Id.
    pub id: ProfileId,
    /// Display name.
    pub name: String,
    /// RDP or VNC.
    pub protocol: DesktopProtocol,
    /// Address or host name (as seen from the tunnel Host when `via_host` is set).
    pub address: String,
    /// Port.
    pub port: u16,
    /// User (RDP); empty lets the client ask.
    #[serde(default)]
    pub user: String,
    /// Windows domain (RDP).
    #[serde(default)]
    pub domain: Option<String>,
    /// A shared credential instead of the fields above and the stored secret.
    #[serde(default)]
    pub credential: Option<ProfileId>,
    /// Password (RDP: the user's; VNC: the server's).
    #[serde(default)]
    pub secret: Option<SecretRef>,
    /// RD Gateway (RDP).
    #[serde(default)]
    pub gateway: Option<RdpGateway>,
    /// Reach the server through an SSH tunnel on this Host; the client connects to
    /// `127.0.0.1:<local port>`.
    #[serde(default)]
    pub via_host: Option<ProfileId>,
    /// Display options.
    #[serde(default)]
    pub display: DesktopDisplay,
    /// Connect to the console / admin session (`mstsc /admin`).
    #[serde(default)]
    pub admin_session: bool,
    /// Environment label.
    #[serde(default)]
    pub environment: EnvironmentLabel,
    /// Folder for grouping.
    #[serde(default)]
    pub folder: Option<String>,
    /// Tags (normalized like Host tags).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Operating system shown in the Servers view (`Windows Server 2022`).
    #[serde(default)]
    pub os: Option<String>,
    /// SHA-256 fingerprint (`AB:CD:…`) of the TLS certificate the user pinned for the
    /// in-app RDP client; a different certificate is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_pin: Option<String>,
}

impl DesktopServer {
    /// A new server with the protocol's defaults.
    pub fn new(name: impl Into<String>, protocol: DesktopProtocol) -> Self {
        Self {
            id: ProfileId::new(),
            name: name.into(),
            protocol,
            address: String::new(),
            port: protocol.default_port(),
            user: String::new(),
            domain: None,
            credential: None,
            secret: None,
            gateway: None,
            via_host: None,
            display: DesktopDisplay::default(),
            admin_session: false,
            environment: EnvironmentLabel::Development,
            folder: None,
            tags: Vec::new(),
            os: None,
            tls_pin: None,
        }
    }
}
