//! The in-app RDP client (IronRDP): input mapping, the shared desktop image, the TLS
//! certificate decision and, with the `rdp-embedded` feature, the session itself.
//!
//! The session runs on the runtime: it connects (TLS, then NLA / CredSSP with NTLM),
//! decodes the server's graphics into a [`FrameBuffer`] the UI copies from, and turns
//! [`RdpInput`] into fast-path input. Nothing here touches the UI.

pub mod cert;
pub mod frame;
pub mod input;
#[cfg(feature = "rdp-embedded")]
mod session;

use secrecy::SecretString;

pub use cert::{CertDecision, CertRequest, PresentedCert};
pub use frame::{FrameBuffer, FrameSnapshot};
pub use input::{KeyMods, MouseButton, RdpInput, ScaleMode, Scancode};
#[cfg(feature = "rdp-embedded")]
pub use session::{CertPrompter, connector_config, run};

/// Whether this build has the in-app RDP client.
pub const EMBEDDED: bool = cfg!(feature = "rdp-embedded");

/// Sign-in and screen of one connection.
#[derive(Clone)]
pub struct RdpSettings {
    /// User name (without the domain).
    pub user: String,
    /// Windows domain.
    pub domain: Option<String>,
    /// Password.
    pub password: SecretString,
    /// Desktop width to ask for.
    pub width: u16,
    /// Desktop height to ask for.
    pub height: u16,
}

impl std::fmt::Debug for RdpSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RdpSettings")
            .field("user", &self.user)
            .field("domain", &self.domain)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

/// What a running session reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionEvent {
    /// A connection step started (`Securing the connection`, `Signing in`, …).
    Stage(String),
    /// The session is up; the desktop has this size.
    Connected {
        /// Width.
        width: u16,
        /// Height.
        height: u16,
    },
    /// The desktop changed size (the server reactivated the session).
    Resized {
        /// Width.
        width: u16,
        /// Height.
        height: u16,
    },
    /// The image changed: take it from the [`FrameBuffer`].
    Frame,
    /// The user chose to pin this certificate fingerprint to the connection.
    PinCertificate(String),
}

/// What the session is told by the runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionCommand {
    /// Keyboard and mouse input.
    Input(Vec<RdpInput>),
    /// Sign out of the session window (disconnect politely).
    Close,
}

/// Errors of the in-app RDP client.
#[derive(Debug, thiserror::Error)]
pub enum RdpError {
    /// Network.
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// TLS handshake.
    #[error("TLS: {0}")]
    Tls(String),
    /// The certificate differs from the pinned one.
    #[error(
        "the server's certificate changed (pinned {pinned}, presented {presented}). \
         If you expected this, remove the pinned certificate and connect again."
    )]
    CertificateChanged {
        /// Pinned fingerprint.
        pinned: String,
        /// Presented fingerprint.
        presented: String,
    },
    /// The user did not trust the certificate.
    #[error("the server's certificate was not trusted")]
    CertificateRejected,
    /// Sign-in refused.
    #[error("sign-in failed: {0}")]
    Auth(String),
    /// The RDP exchange failed.
    #[error("{0}")]
    Protocol(String),
}
