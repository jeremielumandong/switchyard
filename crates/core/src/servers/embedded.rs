//! The in-app RDP tab's commands and events. The session runs on the runtime
//! (`switchyard_remote::rdp`); the UI only sends input and copies the desktop image out of
//! the shared [`FrameBuffer`] when told it changed.

use switchyard_remote::rdp::{FrameBuffer, RdpInput};
use switchyard_store::ProfileId;

use crate::bus::RequestId;

/// Whether this build has the in-app RDP client (`rdp-embedded` feature).
pub const RDP_EMBEDDED: bool = cfg!(feature = "rdp-embedded");

/// A request from the RDP tab ([`crate::Command::Rdp`]).
#[derive(Clone, Debug)]
pub enum RdpCommand {
    /// Connect remote desktop `id` in the tab `session`. `width` × `height` is the tab's
    /// size in pixels, used when the profile asks for full screen.
    Open {
        /// The tab's session id.
        session: RequestId,
        /// The remote desktop.
        id: ProfileId,
        /// Tab width in pixels.
        width: u32,
        /// Tab height in pixels.
        height: u32,
    },
    /// Keyboard and mouse input, in order.
    Input {
        /// Session.
        session: RequestId,
        /// Inputs.
        input: Vec<RdpInput>,
    },
    /// Stop connecting, or disconnect.
    Close {
        /// Session.
        session: RequestId,
    },
    /// Remove the certificate pinned to remote desktop `id` (after a changed-certificate
    /// refusal); the next connection asks again.
    ForgetPin {
        /// The remote desktop.
        id: ProfileId,
    },
}

impl RdpCommand {
    /// The session a per-session command belongs to.
    pub fn session(&self) -> Option<RequestId> {
        match self {
            RdpCommand::Open { session, .. }
            | RdpCommand::Input { session, .. }
            | RdpCommand::Close { session } => Some(*session),
            RdpCommand::ForgetPin { .. } => None,
        }
    }
}

/// Where an RDP session is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RdpState {
    /// Connecting: the current step.
    Connecting(String),
    /// Connected; the desktop's size.
    Connected {
        /// Width.
        width: u16,
        /// Height.
        height: u16,
    },
    /// Ended normally (why).
    Ended(String),
    /// Failed.
    Failed {
        /// Why.
        message: String,
        /// The certificate differs from the pinned one ([`RdpCommand::ForgetPin`] helps).
        cert_changed: bool,
    },
}

/// RDP tab events ([`crate::Event::Rdp`]).
#[derive(Clone, Debug)]
pub enum RdpEvent {
    /// The session's state changed.
    State {
        /// Session.
        session: RequestId,
        /// State.
        state: RdpState,
    },
    /// The desktop image changed: copy it with [`FrameBuffer::take`].
    Frame {
        /// Session.
        session: RequestId,
        /// The image.
        frames: FrameBuffer,
    },
}
