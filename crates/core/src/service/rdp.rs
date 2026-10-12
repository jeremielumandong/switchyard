//! In-app RDP sessions on the runtime ([`crate::servers::embedded`]): sign-in from the
//! remote desktop's own secret or shared credential (asked when missing), the shared SSH
//! session's tunnel when the profile goes through a Host, the certificate check through
//! the prompter with per-connection pins, and history tagged `rdp`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use switchyard_store::{Profile, ProfileId, StoreError};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{Service, lock};
use crate::bus::{Event, RequestId};
use crate::servers::embedded::{RdpCommand, RdpEvent, RdpState};
use switchyard_remote::rdp::SessionCommand;

/// A running session, reachable from the UI.
pub(crate) struct RdpHandle {
    commands: mpsc::UnboundedSender<SessionCommand>,
    cancel: CancellationToken,
    connected: Arc<AtomicBool>,
}

/// Running sessions by the tab's session id.
pub(crate) type RdpSessions = std::sync::Mutex<HashMap<RequestId, RdpHandle>>;

impl Service {
    fn rdp_state(&self, session: RequestId, state: RdpState) {
        self.emit(Event::Rdp(RdpEvent::State { session, state }));
    }

    /// Deliver input or a close in order (called from the command loop, not a task).
    pub(super) fn rdp_now(&self, cmd: &RdpCommand) -> bool {
        match cmd {
            RdpCommand::Input { session, input } => {
                if let Some(h) = lock(&self.rdp_sessions).get(session) {
                    let _ = h.commands.send(SessionCommand::Input(input.clone()));
                }
                true
            }
            RdpCommand::Close { session } => {
                if let Some(h) = lock(&self.rdp_sessions).get(session) {
                    if h.connected.load(Ordering::Relaxed) {
                        // Sign out of the window politely; the cancel is the fallback.
                        let _ = h.commands.send(SessionCommand::Close);
                        let cancel = h.cancel.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                            cancel.cancel();
                        });
                    } else {
                        h.cancel.cancel();
                    }
                }
                true
            }
            _ => false,
        }
    }

    pub(super) async fn rdp(&self, cmd: RdpCommand) {
        match cmd {
            RdpCommand::Open {
                session,
                id,
                width,
                height,
            } => self.rdp_open(session, id, width, height).await,
            RdpCommand::ForgetPin { id } => match self.rdp_set_pin(&id, None).await {
                Ok(()) => self.emit(Event::Toast(
                    "Pinned certificate removed; the next connection asks again".into(),
                )),
                Err(e) => self.error("Remote desktop", e),
            },
            cmd @ (RdpCommand::Input { .. } | RdpCommand::Close { .. }) => {
                self.rdp_now(&cmd);
            }
        }
    }

    /// Save (or clear) the certificate pinned to remote desktop `id`.
    async fn rdp_set_pin(&self, id: &ProfileId, pin: Option<String>) -> crate::Result<()> {
        let i = id.clone();
        self.with_store(move |s| match s.profile(&i)? {
            Some(Profile::Desktop(mut d)) => {
                d.tls_pin = pin;
                s.save_profile(&Profile::Desktop(d))
            }
            _ => Err(StoreError::NotFound(format!("remote desktop {}", i.0))),
        })
        .await?;
        self.emit_profiles().await;
        Ok(())
    }

    #[cfg(not(feature = "rdp-embedded"))]
    async fn rdp_open(&self, session: RequestId, _: ProfileId, _: u32, _: u32) {
        self.rdp_state(
            session,
            RdpState::Failed {
                message: "This build has no in-app RDP client; use Open with the system client"
                    .into(),
                cert_changed: false,
            },
        );
    }

    #[cfg(feature = "rdp-embedded")]
    async fn rdp_open(&self, session: RequestId, id: ProfileId, width: u32, height: u32) {
        use secrecy::SecretString;
        use switchyard_remote::rdp::{self, FrameBuffer, RdpError, RdpSettings, SessionEvent};
        use switchyard_remote::ssh::SshPrompter as _;
        use switchyard_store::{DesktopProtocol, DesktopScreen, HistoryEntry, HistoryStatus};
        use tracing::Instrument as _;

        let fail = |message: String| {
            self.rdp_state(
                session,
                RdpState::Failed {
                    message,
                    cert_changed: false,
                },
            )
        };
        self.rdp_state(session, RdpState::Connecting("Preparing".into()));
        let i = id.clone();
        let d = match self.with_store(move |s| s.profile(&i)).await {
            Ok(Some(Profile::Desktop(d))) if d.protocol == DesktopProtocol::Rdp => d,
            Ok(_) => return fail("This remote desktop no longer exists".into()),
            Err(e) => return fail(e.to_string()),
        };
        if d.gateway.is_some() {
            return fail(
                "Connections through an RD Gateway open with the system client for now \
                 (Servers → RDP)"
                    .into(),
            );
        }
        let (login, _) = match self.desktop_login(&d).await {
            Ok(l) => l,
            Err(e) => return fail(e.to_string()),
        };
        if login.user.is_empty() {
            return fail(
                "Set a user name (or a shared credential) in the remote desktop's settings".into(),
            );
        }
        let address = d.address.trim().to_owned();
        let password: SecretString = match login.password {
            Some(p) => p,
            None => {
                self.rdp_state(
                    session,
                    RdpState::Connecting("Waiting for the password".into()),
                );
                match self
                    .prompter
                    .secret(
                        d.name.clone(),
                        format!("Password for {} on {address}", login.user),
                    )
                    .await
                {
                    Some(p) => p,
                    None => return self.rdp_state(session, RdpState::Ended("Cancelled".into())),
                }
            }
        };
        let (w, h) = match d.display.screen {
            DesktopScreen::Window { width, height } => {
                rdp::input::desktop_size(u32::from(width), u32::from(height))
            }
            DesktopScreen::FullScreen => rdp::input::desktop_size(width, height),
        };
        let settings = RdpSettings {
            user: login.user.clone(),
            domain: login.domain.clone(),
            password,
            width: w,
            height: h,
        };

        let cancel = CancellationToken::new();
        let (tx, rx) = mpsc::unbounded_channel();
        let connected = Arc::new(AtomicBool::new(false));
        lock(&self.rdp_sessions).insert(
            session,
            RdpHandle {
                commands: tx,
                cancel: cancel.clone(),
                connected: connected.clone(),
            },
        );

        // Through an SSH tunnel on the Host's shared session: connect to its local end.
        self.rdp_state(session, RdpState::Connecting("Connecting".into()));
        let tunnel = match &d.via_host {
            Some(host) => {
                let t = tokio::select! {
                    t = self.tunnel_for(host, &address, d.port) => t,
                    () = cancel.cancelled() => {
                        lock(&self.rdp_sessions).remove(&session);
                        return self.rdp_state(session, RdpState::Ended("Cancelled".into()));
                    }
                };
                match t {
                    Ok(t) => Some(t),
                    Err(e) => {
                        lock(&self.rdp_sessions).remove(&session);
                        return fail(format!("SSH tunnel: {e}"));
                    }
                }
            }
            None => None,
        };
        let target = match &tunnel {
            Some(t) => (t.local().ip().to_string(), t.local().port()),
            None => (address.clone(), d.port),
        };
        let connect = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            tokio::net::TcpStream::connect((target.0.as_str(), target.1)),
        );
        let stream = tokio::select! {
            r = connect => match r {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    lock(&self.rdp_sessions).remove(&session);
                    return fail(format!("{address}:{}: {e}", d.port));
                }
                Err(_) => {
                    lock(&self.rdp_sessions).remove(&session);
                    return fail(format!("{address}:{}: timed out", d.port));
                }
            },
            () = cancel.cancelled() => {
                lock(&self.rdp_sessions).remove(&session);
                return self.rdp_state(session, RdpState::Ended("Cancelled".into()));
            }
        };

        // History, tagged `rdp`: who connected where, never the password.
        let mut entry = HistoryEntry {
            id: 0,
            connection_id: Some(d.id.clone()),
            connection_name: d.name.clone(),
            sql: format!(
                "RDP in-app {}@{address}:{}{}",
                login.user,
                d.port,
                if tunnel.is_some() {
                    " (SSH tunnel)"
                } else {
                    ""
                }
            ),
            started_at: switchyard_store::now_ms(),
            duration_ms: 0,
            rows: None,
            affected: None,
            status: HistoryStatus::Ok,
            error: None,
            tags: vec![d.protocol.history_tag().to_owned()],
            has_plan: false,
        };

        let frames = FrameBuffer::new();
        let (pin_tx, mut pin_rx) = mpsc::unbounded_channel::<String>();
        let events = self.events.clone();
        let fb = frames.clone();
        let up = connected.clone();
        let on = move |ev: SessionEvent| match ev {
            SessionEvent::Stage(s) => {
                events.emit(Event::Rdp(RdpEvent::State {
                    session,
                    state: RdpState::Connecting(s),
                }));
            }
            SessionEvent::Connected { width, height } | SessionEvent::Resized { width, height } => {
                up.store(true, Ordering::Relaxed);
                events.emit(Event::Rdp(RdpEvent::State {
                    session,
                    state: RdpState::Connected { width, height },
                }));
            }
            SessionEvent::Frame => events.emit(Event::Rdp(RdpEvent::Frame {
                session,
                frames: fb.clone(),
            })),
            SessionEvent::PinCertificate(fp) => {
                let _ = pin_tx.send(fp);
            }
        };
        let span = tracing::info_span!("rdp", session, server = %d.name);
        let run = rdp::run(
            stream,
            &address,
            &d.name,
            &settings,
            d.tls_pin.as_deref(),
            self.prompter.as_ref(),
            &frames,
            rx,
            &on,
        )
        .instrument(span);
        tokio::pin!(run);
        let result = loop {
            tokio::select! {
                r = &mut run => break r.map_err(|e| (e.to_string(), matches!(e, RdpError::CertificateChanged { .. }))),
                Some(fp) = pin_rx.recv() => {
                    if let Err(e) = self.rdp_set_pin(&d.id, Some(fp)).await {
                        self.error("Pin certificate", e);
                    }
                }
                () = cancel.cancelled() => break Ok("Disconnected".to_owned()),
            }
        };
        lock(&self.rdp_sessions).remove(&session);
        let was_up = connected.load(Ordering::Relaxed);
        entry.duration_ms = switchyard_store::now_ms() - entry.started_at;
        if let Err((e, _)) = &result {
            entry.status = HistoryStatus::Error;
            entry.error = Some(e.clone());
        }
        if let Err(e) = self.with_store(move |s| s.add_history(&entry)).await {
            tracing::warn!(error = %e, "history write failed");
        }
        drop(tunnel);
        if was_up {
            self.touch_connected(d.id.clone()).await;
        }
        match result {
            Ok(reason) => self.rdp_state(session, RdpState::Ended(reason)),
            Err((message, cert_changed)) => self.rdp_state(
                session,
                RdpState::Failed {
                    message,
                    cert_changed,
                },
            ),
        }
        self.emit(Event::Tunnels(self.tunnel_infos()));
    }
}

#[cfg(test)]
mod tests {
    use switchyard_store::{DesktopProtocol, DesktopServer, RdpGateway};

    use super::*;
    use crate::runtime::EventSender;
    use crate::service::ServiceConfig;

    type Rx = futures::channel::mpsc::UnboundedReceiver<Event>;

    fn service() -> (Service, Rx) {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        (
            Service::new(ServiceConfig::in_memory(), EventSender::new(tx)).unwrap(),
            rx,
        )
    }

    /// The last session state emitted.
    fn last_state(rx: &mut Rx) -> Option<RdpState> {
        let mut last = None;
        while let Ok(ev) = rx.try_recv() {
            if let Event::Rdp(RdpEvent::State { state, .. }) = ev {
                last = Some(state);
            }
        }
        last
    }

    #[cfg(feature = "rdp-embedded")]
    #[tokio::test]
    async fn open_refuses_gateways_and_missing_user_names_before_connecting() {
        let (svc, mut rx) = service();
        let mut d = DesktopServer::new("dc01", DesktopProtocol::Rdp);
        d.address = "192.0.2.1".into();
        d.user = "admin".into();
        d.gateway = Some(RdpGateway {
            host: "gw.example.com".into(),
            ..RdpGateway::default()
        });
        svc.save_profile(1, Profile::Desktop(d.clone()), Some("pw".into()))
            .await;
        svc.rdp(RdpCommand::Open {
            session: 7,
            id: d.id.clone(),
            width: 1024,
            height: 768,
        })
        .await;
        match last_state(&mut rx) {
            Some(RdpState::Failed { message, .. }) => assert!(message.contains("Gateway")),
            other => panic!("{other:?}"),
        }

        d.gateway = None;
        d.user = " ".into();
        svc.save_profile(2, Profile::Desktop(d.clone()), None).await;
        svc.rdp(RdpCommand::Open {
            session: 8,
            id: d.id.clone(),
            width: 1024,
            height: 768,
        })
        .await;
        match last_state(&mut rx) {
            Some(RdpState::Failed { message, .. }) => assert!(message.contains("user name")),
            other => panic!("{other:?}"),
        }
        assert!(lock(&svc.rdp_sessions).is_empty());
        // No such session: input and close are taken and dropped.
        assert!(svc.rdp_now(&RdpCommand::Close { session: 8 }));
        assert!(!svc.rdp_now(&RdpCommand::ForgetPin { id: d.id.clone() }));
    }

    #[tokio::test]
    async fn pins_are_saved_on_the_profile_and_forgotten() {
        let (svc, _rx) = service();
        let mut d = DesktopServer::new("dc01", DesktopProtocol::Rdp);
        d.address = "192.0.2.1".into();
        svc.save_profile(1, Profile::Desktop(d.clone()), None).await;
        svc.rdp_set_pin(&d.id, Some("AB:CD".into())).await.unwrap();
        let pin = |p: Option<Profile>| match p {
            Some(Profile::Desktop(d)) => d.tls_pin,
            _ => panic!("desktop expected"),
        };
        let i = d.id.clone();
        let stored = svc.with_store(move |s| s.profile(&i)).await.unwrap();
        assert_eq!(pin(stored), Some("AB:CD".into()));
        svc.rdp(RdpCommand::ForgetPin { id: d.id.clone() }).await;
        let i = d.id.clone();
        let stored = svc.with_store(move |s| s.profile(&i)).await.unwrap();
        assert_eq!(pin(stored), None);
        // Not a remote desktop: an error, nothing saved.
        assert!(svc.rdp_set_pin(&ProfileId::new(), None).await.is_err());
    }
}
