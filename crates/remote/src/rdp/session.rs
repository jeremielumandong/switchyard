//! One RDP session on the runtime (IronRDP): connect, check the certificate, sign in
//! with NLA, then pump graphics into the [`FrameBuffer`] and input to the server.

use std::sync::Arc;

use futures::future::BoxFuture;
use ironrdp_connector::connection_activation::ConnectionActivationState;
use ironrdp_connector::{ClientConnector, ConnectorError, ConnectorErrorKind, Credentials};
use ironrdp_core::WriteBuf;
use ironrdp_graphics::image_processing::PixelFormat;
use ironrdp_input::{Database, Operation};
use ironrdp_pdu::input::fast_path::FastPathInputEvent;
use ironrdp_session::image::DecodedImage;
use ironrdp_session::{ActiveStage, ActiveStageBuilder, ActiveStageOutput, fast_path};
use ironrdp_tokio::{
    FramedWrite as _, NetworkClient, TokioFramed, connect_begin, connect_finalize,
    mark_as_upgraded, single_sequence_step_read, split_tokio_framed,
};
use secrecy::ExposeSecret as _;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, info};

use super::cert::{self, CapturingVerifier, CertCheck, CertDecision, CertRequest};
use super::{FrameBuffer, RdpError, RdpInput, RdpSettings, SessionCommand, SessionEvent};

/// Asks the user about an untrusted server certificate.
pub trait CertPrompter: Send + Sync {
    /// Trust once, trust and pin, or reject.
    fn ask(&self, req: CertRequest) -> BoxFuture<'static, CertDecision>;
}

/// The IronRDP connector settings for `s`: TLS and NLA (CredSSP, NTLM), 32-bit colour
/// with RemoteFX, MPPC 64K bulk compression, the pointer drawn into the image, no audio
/// and no redirection (no clipboard, drives or devices reach the server).
pub fn connector_config(s: &RdpSettings) -> ironrdp_connector::Config {
    use ironrdp_pdu::gcc::KeyboardType;
    use ironrdp_pdu::rdp::capability_sets::{MajorPlatformType, client_codecs_capabilities};
    use ironrdp_pdu::rdp::client_info::{CompressionType, PerformanceFlags, TimezoneInfo};

    let platform = if cfg!(windows) {
        MajorPlatformType::WINDOWS
    } else if cfg!(target_os = "macos") {
        MajorPlatformType::MACINTOSH
    } else {
        MajorPlatformType::UNIX
    };
    let (username, domain) = sign_in_name(&s.user, s.domain.as_deref());
    ironrdp_connector::Config {
        desktop_size: ironrdp_connector::DesktopSize {
            width: s.width,
            height: s.height,
        },
        desktop_scale_factor: 0,
        // Prefer NLA; plain TLS stays allowed for servers without it (they show their own
        // sign-in screen).
        enable_tls: true,
        enable_credssp: true,
        credentials: Credentials::UsernamePassword {
            username,
            password: s.password.expose_secret().to_owned(),
        },
        domain,
        client_build: 0,
        client_name: "Switchyard".to_owned(),
        keyboard_type: KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_functional_keys_count: 12,
        keyboard_layout: 0,
        ime_file_name: String::new(),
        bitmap: Some(ironrdp_connector::BitmapConfig {
            lossy_compression: true,
            color_depth: 32,
            codecs: client_codecs_capabilities(&[]).unwrap_or_default(),
        }),
        dig_product_id: String::new(),
        client_dir: String::new(),
        alternate_shell: String::new(),
        work_dir: String::new(),
        platform,
        hardware_id: None,
        request_data: None,
        autologon: false,
        enable_audio_playback: false,
        performance_flags: PerformanceFlags::default(),
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        compression_type: Some(CompressionType::K64),
        enable_server_pointer: true,
        pointer_software_rendering: true,
        multitransport_flags: None,
    }
}

/// The account name and NetBIOS domain CredSSP gets for a user name and Domain field.
///
/// sspi refuses a `\\` in the account name once a domain is given, so the common forms
/// are split here: `DOMAIN\\user` takes its own domain (over the field), `user@domain`
/// is a UPN that needs none, and a domain typed with a stray `\\` or `@` is trimmed.
fn sign_in_name(user: &str, domain: Option<&str>) -> (String, Option<String>) {
    let field = domain
        .map(|d| d.trim().trim_matches(['\\', '@']).trim())
        .filter(|d| !d.is_empty() && !d.contains(['\\', '@']));
    let user = user.trim();
    if let Some((d, account)) = user.split_once('\\') {
        let d = d.trim();
        let domain = if d.is_empty() { field } else { Some(d) };
        return (account.trim().to_owned(), domain.map(str::to_owned));
    }
    match user.rsplit_once('@') {
        Some((account, suffix)) if !account.is_empty() && !suffix.trim().is_empty() => {
            (user.to_owned(), None)
        }
        Some((account, _)) => (account.trim().to_owned(), field.map(str::to_owned)),
        None => (user.to_owned(), field.map(str::to_owned)),
    }
}

/// No KDC access: CredSSP runs NTLM, which needs no network round trips of its own.
struct NoNetwork;

impl NetworkClient for NoNetwork {
    async fn send(
        &mut self,
        _: &ironrdp_connector::sspi::generator::NetworkRequest,
    ) -> ironrdp_connector::ConnectorResult<Vec<u8>> {
        Err(ironrdp_connector::general_err!(
            "Kerberos (KDC) sign-in is not supported by the in-app client"
        ))
    }
}

fn connector_err(e: ConnectorError) -> RdpError {
    match e.kind() {
        ConnectorErrorKind::Credssp(_) | ConnectorErrorKind::AccessDenied => {
            RdpError::Auth(e.to_string())
        }
        _ => RdpError::Protocol(e.to_string()),
    }
}

fn session_err(e: impl std::fmt::Display) -> RdpError {
    RdpError::Protocol(e.to_string())
}

/// Connect over `stream` to `server` (the real name: TLS, the certificate and the
/// sign-in target use it even through a tunnel) and run the session until it ends,
/// `commands` closes or [`SessionCommand::Close`] arrives. Returns why it ended.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    stream: TcpStream,
    server: &str,
    connection: &str,
    settings: &RdpSettings,
    pin: Option<&str>,
    prompter: &dyn CertPrompter,
    frames: &FrameBuffer,
    mut commands: mpsc::UnboundedReceiver<SessionCommand>,
    on: &(dyn Fn(SessionEvent) + Send + Sync),
) -> Result<String, RdpError> {
    stream.set_nodelay(true)?;
    let client_addr = stream.local_addr()?;
    let mut connector = ClientConnector::new(connector_config(settings), client_addr);
    let mut framed = TokioFramed::new(stream);
    on(SessionEvent::Stage("Negotiating".into()));
    let should_upgrade = connect_begin(&mut framed, &mut connector)
        .await
        .map_err(connector_err)?;

    // TLS, with the certificate decided before anything else is sent.
    on(SessionEvent::Stage("Securing the connection".into()));
    let (tcp, leftover) = framed.into_inner();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = CapturingVerifier::new(provider.clone());
    let tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| RdpError::Tls(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(verifier.clone())
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(server.to_owned())
        .map_err(|e| RdpError::Tls(format!("{server}: {e}")))?;
    let tls_stream = tokio_rustls::TlsConnector::from(Arc::new(tls))
        .connect(name, tcp)
        .await
        .map_err(|e| RdpError::Tls(e.to_string()))?;
    let seen = verifier.seen();
    let presented = cert::presented(&seen);
    match cert::check(pin, &presented) {
        CertCheck::Accept => {}
        CertCheck::Changed { pinned } => {
            return Err(RdpError::CertificateChanged {
                pinned,
                presented: presented.fingerprint,
            });
        }
        CertCheck::Ask => {
            let decision = prompter
                .ask(CertRequest {
                    server: server.to_owned(),
                    connection: connection.to_owned(),
                    cert: presented.clone(),
                })
                .await;
            let (go, pin) = cert::apply(decision, &presented);
            if !go {
                return Err(RdpError::CertificateRejected);
            }
            if let Some(fp) = pin {
                on(SessionEvent::PinCertificate(fp));
            }
        }
    }
    let server_key = cert::subject_public_key(&seen.der)
        .ok_or_else(|| RdpError::Tls("could not read the server's public key".into()))?;
    let upgraded = mark_as_upgraded(should_upgrade, &mut connector);
    let mut framed = TokioFramed::new_with_leftover(tls_stream, leftover);

    on(SessionEvent::Stage("Signing in".into()));
    let result = connect_finalize(
        upgraded,
        connector,
        &mut framed,
        &mut NoNetwork,
        ironrdp_connector::ServerName::new(server),
        server_key,
        None,
    )
    .await
    .map_err(connector_err)?;
    info!(
        width = result.desktop_size.width,
        height = result.desktop_size.height,
        "rdp session connected"
    );

    let (mut reader, mut writer) = split_tokio_framed(framed);
    let size = result.desktop_size;
    let mut image = DecodedImage::new(PixelFormat::BgrA32, size.width, size.height);
    if frames.resize(size.width, size.height) {
        on(SessionEvent::Frame);
    }
    on(SessionEvent::Connected {
        width: size.width,
        height: size.height,
    });
    let activation_factory = result.activation_factory;
    let mut stage: ActiveStage = ActiveStageBuilder {
        static_channels: result.static_channels,
        user_channel_id: result.user_channel_id,
        io_channel_id: result.io_channel_id,
        message_channel_id: result.message_channel_id,
        share_id: result.share_id,
        compression_type: result.compression_type,
        enable_server_pointer: result.enable_server_pointer,
        pointer_software_rendering: result.pointer_software_rendering,
    }
    .build();
    let mut keys = Database::new();
    let mut closing = false;

    loop {
        let outputs = tokio::select! {
            frame = reader.read_pdu() => {
                let (action, payload) = frame?;
                stage.process(&mut image, action, &payload).map_err(session_err)?
            }
            cmd = commands.recv(), if !closing => match cmd {
                Some(SessionCommand::Input(inputs)) => {
                    let events = fast_path_events(inputs, &mut keys);
                    if events.is_empty() {
                        Vec::new()
                    } else {
                        stage.process_fastpath_input(&mut image, &events).map_err(session_err)?
                    }
                }
                Some(SessionCommand::Close) | None => {
                    closing = true;
                    let mut out = Vec::new();
                    let release = keys.release_all();
                    if !release.is_empty() {
                        out.extend(stage.process_fastpath_input(&mut image, &release).map_err(session_err)?);
                    }
                    out.extend(stage.graceful_shutdown().map_err(session_err)?);
                    out
                }
            },
        };
        for out in outputs {
            match out {
                ActiveStageOutput::ResponseFrame(frame) => writer.write_all(&frame).await?,
                ActiveStageOutput::GraphicsUpdate(r) => {
                    if frames.write_rect(
                        image.data(),
                        image.stride(),
                        r.left,
                        r.top,
                        r.right,
                        r.bottom,
                    ) {
                        on(SessionEvent::Frame);
                    }
                }
                ActiveStageOutput::Terminate(reason) => return Ok(reason.description()),
                ActiveStageOutput::DeactivateAll => {
                    // Deactivation-Reactivation: run a fresh activation, then go on.
                    debug!("rdp reactivation");
                    let mut activation = activation_factory.create();
                    let mut buf = WriteBuf::new();
                    loop {
                        let written =
                            single_sequence_step_read(&mut reader, &mut activation, &mut buf)
                                .await
                                .map_err(connector_err)?;
                        if written.size().is_some() {
                            writer.write_all(buf.filled()).await?;
                        }
                        if let ConnectionActivationState::Finalized {
                            desktop_size,
                            share_id,
                            enable_server_pointer,
                            pointer_software_rendering,
                        } = activation.connection_activation_state()
                        {
                            image = DecodedImage::new(
                                PixelFormat::BgrA32,
                                desktop_size.width,
                                desktop_size.height,
                            );
                            if frames.resize(desktop_size.width, desktop_size.height) {
                                on(SessionEvent::Frame);
                            }
                            on(SessionEvent::Resized {
                                width: desktop_size.width,
                                height: desktop_size.height,
                            });
                            stage.set_fastpath_processor(
                                fast_path::ProcessorBuilder {
                                    io_channel_id: activation.io_channel_id(),
                                    user_channel_id: activation.user_channel_id(),
                                    share_id,
                                    enable_server_pointer,
                                    pointer_software_rendering,
                                    bulk_decompressor: None,
                                }
                                .build(),
                            );
                            stage.set_share_id(share_id);
                            stage.set_enable_server_pointer(enable_server_pointer);
                            break;
                        }
                    }
                }
                // The pointer is drawn into the image; no UDP transport; no auto-detect.
                ActiveStageOutput::PointerDefault
                | ActiveStageOutput::PointerHidden
                | ActiveStageOutput::PointerPosition { .. }
                | ActiveStageOutput::PointerBitmap(_)
                | ActiveStageOutput::MultitransportRequest(_)
                | ActiveStageOutput::AutoDetect(_) => {}
            }
        }
    }
}

/// The fast-path events for `inputs`; `keys` tracks what is held (so releases match
/// presses and `ReleaseAll` knows what to release).
fn fast_path_events(inputs: Vec<RdpInput>, keys: &mut Database) -> Vec<FastPathInputEvent> {
    let mut events = Vec::new();
    for i in inputs {
        match operation(i) {
            Some(op) => events.extend(keys.apply([op])),
            None => events.extend(keys.release_all()),
        }
    }
    events
}

/// The IronRDP operation for one input; `None` for [`RdpInput::ReleaseAll`].
fn operation(i: RdpInput) -> Option<Operation> {
    use ironrdp_input::{MousePosition, Scancode, WheelRotations};
    Some(match i {
        RdpInput::Key { code, down } => {
            let sc = Scancode::from_u8(code.extended, code.code);
            if down {
                Operation::KeyPressed(sc)
            } else {
                Operation::KeyReleased(sc)
            }
        }
        RdpInput::Text { ch, down } => {
            if down {
                Operation::UnicodeKeyPressed(ch)
            } else {
                Operation::UnicodeKeyReleased(ch)
            }
        }
        RdpInput::MouseMove { x, y } => Operation::MouseMove(MousePosition { x, y }),
        RdpInput::Button { button, down } => {
            let b = mouse_button(button);
            if down {
                Operation::MouseButtonPressed(b)
            } else {
                Operation::MouseButtonReleased(b)
            }
        }
        RdpInput::Wheel { vertical, units } => Operation::WheelRotations(WheelRotations {
            is_vertical: vertical,
            rotation_units: units,
        }),
        RdpInput::ReleaseAll => return None,
    })
}

fn mouse_button(b: super::MouseButton) -> ironrdp_input::MouseButton {
    use super::MouseButton as B;
    match b {
        B::Left => ironrdp_input::MouseButton::Left,
        B::Middle => ironrdp_input::MouseButton::Middle,
        B::Right => ironrdp_input::MouseButton::Right,
        B::X1 => ironrdp_input::MouseButton::X1,
        B::X2 => ironrdp_input::MouseButton::X2,
    }
}

#[cfg(test)]
mod tests {
    use secrecy::SecretString;

    use super::*;

    fn settings() -> RdpSettings {
        RdpSettings {
            user: " alice ".into(),
            domain: Some("CORP".into()),
            password: SecretString::from("s3cret".to_owned()),
            width: 1280,
            height: 720,
        }
    }

    #[test]
    fn config_asks_for_nla_and_the_requested_screen() {
        let c = connector_config(&settings());
        assert_eq!((c.desktop_size.width, c.desktop_size.height), (1280, 720));
        assert!(c.enable_credssp && c.enable_tls);
        assert_eq!(c.domain.as_deref(), Some("CORP"));
        match &c.credentials {
            Credentials::UsernamePassword { username, password } => {
                assert_eq!(username, "alice");
                assert_eq!(password, "s3cret");
            }
            Credentials::SmartCard { .. } => panic!("password sign-in expected"),
        }
        assert!(!c.enable_audio_playback, "no audio redirection");
        assert!(c.pointer_software_rendering, "pointer drawn into the image");
        assert_eq!(c.bitmap.as_ref().map(|b| b.color_depth), Some(32));
        assert!(!c.autologon);
        // A blank domain is no domain.
        let mut s = settings();
        s.domain = Some("  ".into());
        assert_eq!(connector_config(&s).domain, None);
    }

    #[test]
    fn sign_in_names_are_split_for_credssp() {
        let n = |u: &str, d: Option<&str>| {
            let (u, d) = sign_in_name(u, d);
            (u, d.unwrap_or_default())
        };
        let pair = |u: &str, d: &str| (u.to_owned(), d.to_owned());
        assert_eq!(n("alice", Some("CORP")), pair("alice", "CORP"));
        assert_eq!(n("alice", None), pair("alice", ""));
        // The user's own qualification wins over the Domain field.
        assert_eq!(n("CLI\\alice", Some("CLI")), pair("alice", "CLI"));
        assert_eq!(n("CLI\\alice", Some("cli.ad")), pair("alice", "CLI"));
        assert_eq!(n("cli.ad\\alice", None), pair("alice", "cli.ad"));
        assert_eq!(n("\\alice", Some("CORP")), pair("alice", "CORP"));
        // A UPN needs no domain.
        assert_eq!(n("alice@cli.ad", Some("CLI")), pair("alice@cli.ad", ""));
        assert_eq!(n("alice@", Some("CORP")), pair("alice", "CORP"));
        // A stray separator in the Domain field.
        assert_eq!(n("alice", Some("CLI\\")), pair("alice", "CLI"));
        assert_eq!(n("alice", Some("a@b")), pair("alice", ""));
        // Every form is one sspi accepts.
        for (u, d) in [
            ("alice", Some("CORP")),
            ("CLI\\alice", Some("CLI")),
            ("alice@cli.ad", Some("CLI")),
            ("CLI\\alice@x", None),
            ("alice", Some("x@y")),
        ] {
            let (u, d) = sign_in_name(u, d);
            ironrdp_connector::sspi::Username::new(&u, d.as_deref()).unwrap();
        }
    }

    #[test]
    fn config_debug_output_never_shows_the_password() {
        let c = connector_config(&settings());
        let dbg = format!("{c:?}");
        assert!(dbg.contains("alice"));
        assert!(!dbg.contains("s3cret"), "{dbg}");
        assert!(!format!("{:?}", settings()).contains("s3cret"));
    }

    #[test]
    fn inputs_become_fast_path_events() {
        use super::super::input::{LCTRL, ctrl_alt_del};
        let mut keys = Database::new();
        let ev = fast_path_events(
            vec![
                RdpInput::Key {
                    code: LCTRL,
                    down: true,
                },
                RdpInput::Button {
                    button: super::super::MouseButton::Left,
                    down: true,
                },
            ],
            &mut keys,
        );
        assert_eq!(ev.len(), 2);
        assert!(keys.is_key_pressed(ironrdp_input::Scancode::from_u8(false, 0x1D)));
        // Releasing a key that is not held sends nothing.
        let ev = fast_path_events(
            vec![RdpInput::Key {
                code: super::super::input::LALT,
                down: false,
            }],
            &mut keys,
        );
        assert!(ev.is_empty());
        // Release everything held: Ctrl and the left button.
        let ev = fast_path_events(vec![RdpInput::ReleaseAll], &mut keys);
        assert_eq!(ev.len(), 2);
        assert!(fast_path_events(vec![RdpInput::ReleaseAll], &mut keys).is_empty());
        // Ctrl+Alt+Del: three presses, three releases.
        assert_eq!(fast_path_events(ctrl_alt_del(), &mut keys).len(), 6);
        // Typed text and the wheel.
        let ev = fast_path_events(
            vec![
                RdpInput::Text {
                    ch: 'é',
                    down: true,
                },
                RdpInput::Text {
                    ch: 'é',
                    down: false,
                },
                RdpInput::Wheel {
                    vertical: true,
                    units: 120,
                },
                RdpInput::MouseMove { x: 5, y: 6 },
            ],
            &mut keys,
        );
        assert_eq!(ev.len(), 4);
    }
}
