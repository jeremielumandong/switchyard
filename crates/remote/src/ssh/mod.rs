//! SSH sessions over `russh`: strict host key checking, password / public key /
//! keyboard-interactive / agent authentication, ProxyJump chains, keepalives, and one
//! shared session per Host (terminals, tunnels and SFTP all ride on it).

pub mod known_hosts;
pub mod tunnel;
mod x11;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use futures::future::BoxFuture;
use russh::Disconnect;
pub use russh::client::Msg;
use russh::client::{self, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::{PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate, load_secret_key};
pub use russh::{Channel, ChannelMsg};
use secrecy::{ExposeSecret, SecretString};
use tracing::{debug, info, warn};

pub use known_hosts::{HostKeyStatus, KnownHosts, fingerprint};
use tunnel::RemoteRoutes;
pub use tunnel::{ForwardKind, ForwardSpec, Tunnel, TunnelInfo, TunnelStatus};

/// Why an SSH operation failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SshError {
    /// TCP or protocol failure before authentication.
    #[error("could not connect to {target}: {message}")]
    Connect {
        /// `user@host:port`.
        target: String,
        /// Reason.
        message: String,
    },
    /// The server presented a different key than the stored one. The connection is blocked.
    #[error("the host key for {host} has changed (stored {stored}, received {received})")]
    HostKeyChanged {
        /// Host label.
        host: String,
        /// `address:port`.
        address: String,
        /// Stored fingerprint.
        stored: String,
        /// Received fingerprint.
        received: String,
        /// Where the stored key lives.
        location: String,
    },
    /// The key is marked revoked.
    #[error("the host key for {0} is revoked")]
    HostKeyRevoked(String),
    /// The user declined an unknown key.
    #[error("the host key for {0} was not trusted")]
    HostKeyRejected(String),
    /// Every authentication attempt failed.
    #[error("authentication failed for {target}: {message}")]
    Auth {
        /// `user@host`.
        target: String,
        /// What was tried.
        message: String,
    },
    /// The user cancelled a prompt.
    #[error("cancelled")]
    Cancelled,
    /// Opening or using a channel failed.
    #[error("channel error: {0}")]
    Channel(String),
    /// File access (keys, known_hosts).
    #[error("{0}")]
    Io(String),
}

/// How to authenticate to a Host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SshAuthMethod {
    /// Password (from the keychain or a prompt).
    Password,
    /// A private key file read in place; the passphrase comes from the keychain or a prompt.
    PublicKey {
        /// Path (`~` is expanded).
        key_path: String,
    },
    /// Keyboard-interactive (MFA codes and similar prompts).
    KeyboardInteractive,
    /// Keys held by an SSH agent (OpenSSH's, 1Password's, …).
    Agent,
}

/// One Host to connect to.
#[derive(Clone)]
pub struct SshTarget {
    /// Stable id (the Host profile id); one session is shared per id.
    pub id: String,
    /// Display name.
    pub label: String,
    /// Host name or address.
    pub address: String,
    /// Port.
    pub port: u16,
    /// User.
    pub user: String,
    /// Authentication method.
    pub auth: SshAuthMethod,
    /// Password or key passphrase from the keychain, when stored.
    pub secret: Option<SecretString>,
    /// Keepalive interval (0 disables).
    pub keepalive: Duration,
    /// The hop before this one (ProxyJump); its own `jump` continues the chain.
    pub jump: Option<Box<SshTarget>>,
    /// Agent socket (OpenSSH `IdentityAgent`; on Windows a pipe or `pageant`); `None` tries
    /// `SSH_AUTH_SOCK`, then the 1Password agent (Windows: the OpenSSH agent service, then
    /// Pageant).
    pub agent_socket: Option<String>,
    /// Public key file choosing which agent key to use (1Password holds many; servers
    /// stop after a few failed keys).
    pub agent_key: Option<String>,
    /// Let the server use this machine's SSH agent (`ssh -A`): only for servers you trust,
    /// since their root can use your keys while you are connected.
    pub forward_agent: bool,
    /// Forward X11 (`ssh -X`) to `DISPLAY` (on Windows, VcXsrv/X410 on `localhost:0`).
    pub forward_x11: bool,
    /// X display to forward to instead of `DISPLAY` (`:1`, `localhost:0`).
    pub x11_display: Option<String>,
}

impl std::fmt::Debug for SshTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshTarget")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("auth", &self.auth)
            .field("jump", &self.jump.as_ref().map(|j| &j.label))
            .finish_non_exhaustive()
    }
}

impl SshTarget {
    fn display(&self) -> String {
        format!("{}@{}:{}", self.user, self.address, self.port)
    }
}

/// A host key the user must decide on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostKeyRequest {
    /// Host label.
    pub host: String,
    /// `address:port`.
    pub address: String,
    /// Key algorithm (`ED25519`, `ECDSA`, `RSA`).
    pub algorithm: String,
    /// `SHA256:…` fingerprint.
    pub fingerprint: String,
}

/// The user's answer to an unknown key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostKeyDecision {
    /// Connect this time only.
    TrustOnce,
    /// Connect and remember the key.
    TrustAndSave,
    /// Do not connect.
    Reject,
}

/// One keyboard-interactive round.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InteractiveRequest {
    /// Host label.
    pub host: String,
    /// Server-provided name.
    pub name: String,
    /// Server-provided instructions.
    pub instructions: String,
    /// Prompts and whether the answer may be echoed.
    pub prompts: Vec<(String, bool)>,
}

/// Asks the user. Implemented by core over the event bus.
pub trait SshPrompter: Send + Sync {
    /// Decide on an unknown host key.
    fn host_key(&self, req: HostKeyRequest) -> BoxFuture<'static, HostKeyDecision>;
    /// A password or passphrase (`None` = cancelled).
    fn secret(&self, host: String, prompt: String) -> BoxFuture<'static, Option<SecretString>>;
    /// Answers for a keyboard-interactive round (`None` = cancelled).
    fn interactive(&self, req: InteractiveRequest)
    -> BoxFuture<'static, Option<Vec<SecretString>>>;
}

#[derive(Default)]
struct KeyOutcome {
    failure: Option<SshError>,
}

struct ClientHandler {
    known: KnownHosts,
    accept_changed: Option<String>,
    label: String,
    address: String,
    port: u16,
    prompter: Arc<dyn SshPrompter>,
    outcome: Arc<Mutex<KeyOutcome>>,
    /// Remote forwards on this connection.
    routes: Arc<RemoteRoutes>,
    /// Agents to hand forwarded agent channels to; `None` refuses them.
    agent_forward: Option<Vec<AgentEndpoint>>,
    /// X11 forwarding; `None` refuses X11 channels.
    x11: Option<Arc<x11::X11Forward>>,
}

fn record(outcome: &Mutex<KeyOutcome>, e: SshError) {
    if let Ok(mut o) = outcome.lock() {
        o.failure = Some(e);
    }
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            record(
                &self.outcome,
                SshError::Connect {
                    target: self.label.clone(),
                    message: "host certificates are not supported yet".into(),
                },
            );
            return Ok(false);
        };
        let addr = format!("{}:{}", self.address, self.port);
        match self.known.check(&self.address, self.port, key) {
            HostKeyStatus::Known => Ok(true),
            HostKeyStatus::Revoked => {
                record(&self.outcome, SshError::HostKeyRevoked(self.label.clone()));
                Ok(false)
            }
            HostKeyStatus::Changed { .. }
                if self.accept_changed.as_deref() == Some(fingerprint(key).as_str()) =>
            {
                // The user confirmed this exact new key after seeing the warning.
                if let Err(e) = self.known.replace(&self.address, self.port, key) {
                    record(&self.outcome, e);
                    return Ok(false);
                }
                info!(host = %self.label, "replaced stored host key after confirmation");
                Ok(true)
            }
            HostKeyStatus::Changed { stored, location } => {
                record(
                    &self.outcome,
                    SshError::HostKeyChanged {
                        host: self.label.clone(),
                        address: addr,
                        stored,
                        received: fingerprint(key),
                        location,
                    },
                );
                Ok(false)
            }
            HostKeyStatus::Unknown => {
                let req = HostKeyRequest {
                    host: self.label.clone(),
                    address: addr,
                    algorithm: key_kind(key).to_ascii_uppercase(),
                    fingerprint: fingerprint(key),
                };
                match self.prompter.host_key(req).await {
                    HostKeyDecision::TrustAndSave => {
                        if let Err(e) = self.known.trust(&self.address, self.port, key) {
                            record(&self.outcome, e);
                            return Ok(false);
                        }
                        Ok(true)
                    }
                    HostKeyDecision::TrustOnce => Ok(true),
                    HostKeyDecision::Reject => {
                        record(&self.outcome, SshError::HostKeyRejected(self.label.clone()));
                        Ok(false)
                    }
                }
            }
        }
    }

    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: Channel<Msg>,
        _connected_address: &str,
        connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        self.routes.open(connected_port, channel, reply);
        Ok(())
    }

    async fn server_channel_open_agent_forward(
        &mut self,
        channel: Channel<Msg>,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        // Dropping `reply` rejects: forwarding was not asked for on this Host.
        let Some(endpoints) = self.agent_forward.clone() else {
            warn!(host = %self.label, "refused an agent channel the Host did not ask for");
            return Ok(());
        };
        tokio::spawn(async move {
            match agent_stream(&endpoints).await {
                Ok(mut agent) => {
                    reply.accept().await;
                    let mut ch = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut ch, &mut agent).await;
                }
                Err(e) => {
                    warn!(error = %e, "forwarded agent request: no local agent");
                    reply.reject(russh::ChannelOpenFailure::ConnectFailed).await;
                }
            }
        });
        Ok(())
    }

    async fn server_channel_open_x11(
        &mut self,
        channel: Channel<Msg>,
        _originator_address: &str,
        _originator_port: u32,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        match &self.x11 {
            Some(x) => x.open(channel, reply),
            None => warn!(host = %self.label, "refused an X11 channel the Host did not ask for"),
        }
        Ok(())
    }
}

fn key_kind(key: &PublicKey) -> &'static str {
    use russh::keys::Algorithm;
    match key.algorithm() {
        Algorithm::Ed25519 => "ed25519",
        Algorithm::Ecdsa { .. } => "ecdsa",
        Algorithm::Rsa { .. } => "rsa",
        _ => "key",
    }
}

fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

/// An authenticated SSH session.
pub struct SshConn {
    handle: Option<Handle<ClientHandler>>,
    /// Keeps the previous hop alive while this session uses it.
    _jump: Option<Arc<SshConn>>,
    /// Host id.
    pub id: String,
    /// Host label.
    pub label: String,
    /// `user@address · ed25519 · via bastion`.
    pub description: String,
    /// Where the server's forwarded connections go (remote forwards).
    routes: Arc<RemoteRoutes>,
    /// Remote forward requests go one at a time (a port-0 request learns its port only
    /// from the reply).
    forward_requests: tokio::sync::Mutex<()>,
    /// Ask for agent forwarding on shell and exec channels.
    forward_agent: bool,
    /// X11 forwarding for shell and exec channels.
    x11: Option<Arc<x11::X11Forward>>,
}

impl std::fmt::Debug for SshConn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshConn")
            .field("label", &self.label)
            .field("closed", &self.is_closed())
            .finish()
    }
}

impl Drop for SshConn {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take()
            && let Ok(rt) = tokio::runtime::Handle::try_current()
        {
            rt.spawn(async move {
                let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
            });
        }
    }
}

impl SshConn {
    fn handle(&self) -> Result<&Handle<ClientHandler>, SshError> {
        self.handle
            .as_ref()
            .ok_or_else(|| SshError::Channel("session closed".into()))
    }

    /// Whether the connection dropped.
    pub fn is_closed(&self) -> bool {
        self.handle.as_ref().is_none_or(Handle::is_closed)
    }

    /// Ask for the Host's agent and X11 forwarding on a session channel. Refusals are not
    /// errors: the shell still works without them.
    async fn request_forwarding(&self, ch: &Channel<Msg>) {
        if self.forward_agent
            && let Err(e) = ch.agent_forward(false).await
        {
            warn!(host = %self.label, error = %e, "agent forwarding request failed");
        }
        if let Some(x) = &self.x11
            && let Err(e) = ch
                .request_x11(false, false, x11::MIT_COOKIE, x.fake_cookie(), x.screen())
                .await
        {
            warn!(host = %self.label, error = %e, "X11 forwarding request failed");
        }
    }

    /// An interactive shell with a PTY of `cols × rows`.
    pub async fn open_shell(&self, cols: u16, rows: u16) -> Result<Channel<Msg>, SshError> {
        self.open_shell_with_env(cols, rows, &[]).await
    }

    /// [`Self::open_shell`] with environment variables sent before the shell starts. A
    /// server that does not accept a variable (OpenSSH `AcceptEnv`) ignores it.
    pub async fn open_shell_with_env(
        &self,
        cols: u16,
        rows: u16,
        env: &[(String, String)],
    ) -> Result<Channel<Msg>, SshError> {
        let ch = self
            .handle()?
            .channel_open_session()
            .await
            .map_err(|e| SshError::Channel(e.to_string()))?;
        ch.request_pty(
            true,
            "xterm-256color",
            u32::from(cols),
            u32::from(rows),
            0,
            0,
            &[],
        )
        .await
        .map_err(|e| SshError::Channel(e.to_string()))?;
        self.request_forwarding(&ch).await;
        for (name, value) in env {
            // No reply wanted: servers answer refused variables with a failure only.
            let _ = ch.set_env(false, name.as_str(), value.as_str()).await;
        }
        ch.request_shell(true)
            .await
            .map_err(|e| SshError::Channel(e.to_string()))?;
        Ok(ch)
    }

    /// A channel running the server's `sftp` subsystem.
    pub async fn open_sftp(&self) -> Result<Channel<Msg>, SshError> {
        let ch = self
            .handle()?
            .channel_open_session()
            .await
            .map_err(|e| SshError::Channel(e.to_string()))?;
        ch.request_subsystem(true, "sftp")
            .await
            .map_err(|e| SshError::Channel(format!("sftp: {e}")))?;
        Ok(ch)
    }

    /// A `direct-tcpip` channel to `host:port` as seen from the server (tunnels, jumps).
    pub async fn direct_tcpip(&self, host: &str, port: u16) -> Result<Channel<Msg>, SshError> {
        self.handle()?
            .channel_open_direct_tcpip(host, u32::from(port), "127.0.0.1", 0)
            .await
            .map_err(|e| SshError::Channel(format!("{host}:{port}: {e}")))
    }

    /// Run a command and collect its exit code and standard output (tests, probes).
    pub async fn exec(&self, command: &str) -> Result<(Option<u32>, Vec<u8>), SshError> {
        let mut ch = self
            .handle()?
            .channel_open_session()
            .await
            .map_err(|e| SshError::Channel(e.to_string()))?;
        self.request_forwarding(&ch).await;
        ch.exec(true, command)
            .await
            .map_err(|e| SshError::Channel(e.to_string()))?;
        let mut out = Vec::new();
        let mut code = None;
        while let Some(msg) = ch.wait().await {
            match msg {
                ChannelMsg::Data { data } => out.extend_from_slice(&data),
                ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
                _ => {}
            }
        }
        Ok((code, out))
    }
}

/// What a command run with [`SshConn::run_command`] printed and returned.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandOutput {
    /// Exit status; `None` when the server sent none (killed by a signal, or timed out).
    pub exit_status: Option<u32>,
    /// Standard output, up to the cap.
    pub stdout: Vec<u8>,
    /// Standard error, up to the cap.
    pub stderr: Vec<u8>,
    /// Output past the cap was dropped.
    pub truncated: bool,
    /// The command was still running at the deadline; its channel was closed.
    pub timed_out: bool,
}

impl SshConn {
    /// Run `command` in its own channel (no PTY), keeping at most `cap` bytes of each
    /// stream. At `timeout` the channel is closed and what arrived so far is returned.
    pub async fn run_command(
        &self,
        command: &str,
        cap: usize,
        timeout: Duration,
    ) -> Result<CommandOutput, SshError> {
        let mut ch = self
            .handle()?
            .channel_open_session()
            .await
            .map_err(|e| SshError::Channel(e.to_string()))?;
        ch.exec(true, command)
            .await
            .map_err(|e| SshError::Channel(e.to_string()))?;
        let mut out = CommandOutput::default();
        let keep = |buf: &mut Vec<u8>, data: &[u8], truncated: &mut bool| {
            let room = cap.saturating_sub(buf.len());
            if data.len() > room {
                *truncated = true;
            }
            buf.extend_from_slice(&data[..data.len().min(room)]);
        };
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match tokio::time::timeout_at(deadline, ch.wait()).await {
                Ok(Some(ChannelMsg::Data { data })) => {
                    keep(&mut out.stdout, &data, &mut out.truncated);
                }
                Ok(Some(ChannelMsg::ExtendedData { data, .. })) => {
                    keep(&mut out.stderr, &data, &mut out.truncated);
                }
                Ok(Some(ChannelMsg::ExitStatus { exit_status })) => {
                    out.exit_status = Some(exit_status);
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => {
                    out.timed_out = true;
                    let _ = ch.close().await;
                    break;
                }
            }
        }
        Ok(out)
    }
}

/// Connect and authenticate one hop. `via` is the previous hop, when jumping.
async fn connect_one(
    target: &SshTarget,
    via: Option<Arc<SshConn>>,
    known: &KnownHosts,
    prompter: &Arc<dyn SshPrompter>,
    agent_socket: Option<&std::path::Path>,
    accept_changed: Option<String>,
) -> Result<SshConn, SshError> {
    let keepalive = (!target.keepalive.is_zero()).then_some(target.keepalive);
    let config = Arc::new(client::Config {
        keepalive_interval: keepalive,
        keepalive_max: 3,
        nodelay: true,
        ..Default::default()
    });
    let outcome: Arc<Mutex<KeyOutcome>> = Arc::default();
    let routes: Arc<RemoteRoutes> = Arc::default();
    let agent_forward = target
        .forward_agent
        .then(|| agent_candidates_platform(target.agent_socket.as_deref(), agent_socket));
    let x11 = if target.forward_x11 {
        match x11::local_display(target.x11_display.as_deref()).map(x11::X11Forward::new) {
            Some(Ok(x)) => Some(x),
            Some(Err(e)) => {
                warn!(host = %target.label, error = %e, "X11 forwarding off");
                None
            }
            None => {
                warn!(host = %target.label, "X11 forwarding off: no X display (set DISPLAY or the Host's X display)");
                None
            }
        }
    } else {
        None
    };
    let handler = ClientHandler {
        routes: routes.clone(),
        agent_forward,
        x11: x11.clone(),
        known: known.clone(),
        accept_changed,
        label: target.label.clone(),
        address: target.address.clone(),
        port: target.port,
        prompter: prompter.clone(),
        outcome: outcome.clone(),
    };
    let connect_err = |e: russh::Error| {
        outcome
            .lock()
            .ok()
            .and_then(|mut o| o.failure.take())
            .unwrap_or_else(|| SshError::Connect {
                target: target.display(),
                message: e.to_string(),
            })
    };
    let fut = async {
        match &via {
            Some(prev) => {
                let ch = prev.direct_tcpip(&target.address, target.port).await?;
                client::connect_stream(config, ch.into_stream(), handler)
                    .await
                    .map_err(connect_err)
            }
            None => client::connect(config, (target.address.as_str(), target.port), handler)
                .await
                .map_err(connect_err),
        }
    };
    // Prompts (host key) happen inside the handshake, so the timeout is generous.
    let mut handle = tokio::time::timeout(Duration::from_secs(300), fut)
        .await
        .map_err(|_| SshError::Connect {
            target: target.display(),
            message: "timed out".into(),
        })??;

    let method = authenticate(&mut handle, target, prompter, agent_socket).await?;
    let description = match &via {
        Some(prev) => format!(
            "{}@{} · {method} · via {}",
            target.user, target.address, prev.label
        ),
        None => format!("{}@{} · {method}", target.user, target.address),
    };
    info!(host = %target.label, %method, "ssh session open");
    Ok(SshConn {
        handle: Some(handle),
        _jump: via,
        id: target.id.clone(),
        label: target.label.clone(),
        description,
        routes,
        forward_requests: tokio::sync::Mutex::new(()),
        forward_agent: target.forward_agent,
        x11,
    })
}

fn auth_failed(target: &SshTarget, message: impl Into<String>) -> SshError {
    SshError::Auth {
        target: format!("{}@{}", target.user, target.address),
        message: message.into(),
    }
}

/// Authenticate; returns a short label of the method that worked.
async fn authenticate(
    handle: &mut Handle<ClientHandler>,
    target: &SshTarget,
    prompter: &Arc<dyn SshPrompter>,
    agent_socket: Option<&std::path::Path>,
) -> Result<String, SshError> {
    let user = target.user.clone();
    let err = |e: russh::Error| auth_failed(target, e.to_string());
    match &target.auth {
        SshAuthMethod::Password => {
            let password = match &target.secret {
                Some(s) => s.clone(),
                None => prompter
                    .secret(
                        target.label.clone(),
                        format!("Password for {}@{}", target.user, target.address),
                    )
                    .await
                    .ok_or(SshError::Cancelled)?,
            };
            let r = handle
                .authenticate_password(user, password.expose_secret())
                .await
                .map_err(err)?;
            if r.success() {
                Ok("password".into())
            } else {
                Err(auth_failed(target, "the password was rejected"))
            }
        }
        SshAuthMethod::PublicKey { key_path } if is_public_key_file(&expand_home(key_path)) => {
            let path = expand_home(key_path);
            agent_auth(handle, target, agent_socket, Some(&path)).await
        }
        SshAuthMethod::PublicKey { key_path } => {
            let path = expand_home(key_path);
            let key = match load_secret_key(&path, None) {
                Ok(k) => k,
                Err(russh::keys::Error::KeyIsEncrypted) => {
                    let pass = match &target.secret {
                        Some(s) => s.clone(),
                        None => prompter
                            .secret(
                                target.label.clone(),
                                format!("Passphrase for {}", path.display()),
                            )
                            .await
                            .ok_or(SshError::Cancelled)?,
                    };
                    load_secret_key(&path, Some(pass.expose_secret())).map_err(|e| {
                        auth_failed(target, format!("could not unlock {}: {e}", path.display()))
                    })?
                }
                Err(e) => {
                    return Err(SshError::Io(format!(
                        "could not read key {}: {e}",
                        path.display()
                    )));
                }
            };
            let kind = key_kind(key.public_key());
            let hash = handle
                .best_supported_rsa_hash()
                .await
                .map_err(err)?
                .flatten();
            let r = handle
                .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                .await
                .map_err(err)?;
            if r.success() {
                Ok(kind.into())
            } else {
                Err(auth_failed(
                    target,
                    format!("the server did not accept {}", path.display()),
                ))
            }
        }
        SshAuthMethod::KeyboardInteractive => {
            let mut resp = handle
                .authenticate_keyboard_interactive_start(user, None::<String>)
                .await
                .map_err(err)?;
            for _round in 0..10 {
                match resp {
                    KeyboardInteractiveAuthResponse::Success => {
                        return Ok("keyboard-interactive".into());
                    }
                    KeyboardInteractiveAuthResponse::Failure { .. } => {
                        return Err(auth_failed(target, "the answers were rejected"));
                    }
                    KeyboardInteractiveAuthResponse::InfoRequest {
                        name,
                        instructions,
                        prompts,
                    } => {
                        let answers: Vec<String> = if prompts.is_empty() {
                            Vec::new()
                        } else {
                            let req = InteractiveRequest {
                                host: target.label.clone(),
                                name,
                                instructions,
                                prompts: prompts.into_iter().map(|p| (p.prompt, p.echo)).collect(),
                            };
                            prompter
                                .interactive(req)
                                .await
                                .ok_or(SshError::Cancelled)?
                                .iter()
                                .map(|s| s.expose_secret().to_owned())
                                .collect()
                        };
                        resp = handle
                            .authenticate_keyboard_interactive_respond(answers)
                            .await
                            .map_err(err)?;
                    }
                }
            }
            Err(auth_failed(target, "too many prompts"))
        }
        SshAuthMethod::Agent => {
            let key = target.agent_key.as_deref().map(expand_home);
            agent_auth(handle, target, agent_socket, key.as_deref()).await
        }
    }
}

/// Whether `path` holds a public key: OpenSSH configs for agents (1Password) point
/// `IdentityFile` at the `.pub` file to pick the agent key.
fn is_public_key_file(path: &std::path::Path) -> bool {
    if path.extension().is_some_and(|e| e == "pub") {
        return true;
    }
    std::fs::read(path).is_ok_and(|b| {
        let head = String::from_utf8_lossy(&b[..b.len().min(64)]).into_owned();
        ["ssh-", "ecdsa-", "sk-"]
            .iter()
            .any(|p| head.starts_with(p))
    })
}

/// 1Password's SSH agent socket on this platform (Linux, macOS).
pub fn one_password_sockets() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let home = PathBuf::from(home);
    vec![
        home.join(".1password").join("agent.sock"),
        home.join("Library/Group Containers/2BUA8C4S2C.com.1password/t/agent.sock"),
    ]
}

/// Where to reach an SSH agent.
#[derive(Clone, Debug, PartialEq, Eq)]
enum AgentEndpoint {
    /// A Unix socket, or a named pipe on Windows (OpenSSH's agent service, 1Password).
    Socket(PathBuf),
    /// PuTTY's Pageant (Windows only).
    Pageant,
}

impl AgentEndpoint {
    /// A short name for the status line.
    fn name(&self) -> &'static str {
        match self {
            Self::Pageant => "Pageant",
            Self::Socket(p) => agent_name(p),
        }
    }

    fn display(&self) -> String {
        match self {
            Self::Pageant => "Pageant".into(),
            Self::Socket(p) => p.display().to_string(),
        }
    }
}

/// Agent sockets to try, in order (Unix).
#[cfg(unix)]
fn agent_candidates(explicit: Option<&str>, manager: Option<&std::path::Path>) -> Vec<PathBuf> {
    if let Some(e) = explicit.map(str::trim).filter(|e| !e.is_empty()) {
        // `IdentityAgent SSH_AUTH_SOCK` means the environment variable.
        if e == "SSH_AUTH_SOCK" || e == "$SSH_AUTH_SOCK" {
            return std::env::var_os("SSH_AUTH_SOCK")
                .map(PathBuf::from)
                .into_iter()
                .collect();
        }
        return vec![expand_home(e)];
    }
    if let Some(m) = manager {
        return vec![m.to_owned()];
    }
    let mut out: Vec<PathBuf> = std::env::var_os("SSH_AUTH_SOCK")
        .map(PathBuf::from)
        .into_iter()
        .collect();
    for p in one_password_sockets() {
        if p.exists() && !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// The OpenSSH for Windows agent service's pipe (1Password serves the same one).
const OPENSSH_AGENT_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";

fn is_pipe(s: &str) -> bool {
    let s = s.to_ascii_lowercase();
    s.starts_with(r"\\.\pipe\") || s.starts_with("//./pipe/")
}

/// Agents to try on Windows, in order. `explicit` is the Host's agent field: a pipe
/// path, `pageant`, or `SSH_AUTH_SOCK`; without one, a pipe in `SSH_AUTH_SOCK`, then the
/// OpenSSH agent service, then Pageant.
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_agent_candidates(
    explicit: Option<&str>,
    manager: Option<&std::path::Path>,
    env_sock: Option<&str>,
) -> Vec<AgentEndpoint> {
    let env_pipe = env_sock
        .map(str::trim)
        .filter(|s| is_pipe(s))
        .map(|s| AgentEndpoint::Socket(PathBuf::from(s)));
    if let Some(e) = explicit.map(str::trim).filter(|e| !e.is_empty()) {
        if e.eq_ignore_ascii_case("pageant") {
            return vec![AgentEndpoint::Pageant];
        }
        if e == "SSH_AUTH_SOCK" || e == "$SSH_AUTH_SOCK" {
            return env_pipe.into_iter().collect();
        }
        if is_pipe(e) {
            return vec![AgentEndpoint::Socket(PathBuf::from(e))];
        }
        // A Unix socket path from an imported config means nothing here: fall through.
    }
    if let Some(m) = manager {
        return vec![AgentEndpoint::Socket(m.to_owned())];
    }
    let mut out: Vec<AgentEndpoint> = env_pipe.into_iter().collect();
    let service = AgentEndpoint::Socket(PathBuf::from(OPENSSH_AGENT_PIPE));
    if !out.iter().any(|c| match c {
        AgentEndpoint::Socket(p) => p.to_string_lossy().eq_ignore_ascii_case(OPENSSH_AGENT_PIPE),
        AgentEndpoint::Pageant => false,
    }) {
        out.push(service);
    }
    out.push(AgentEndpoint::Pageant);
    out
}

#[cfg(windows)]
fn agent_candidates_platform(
    explicit: Option<&str>,
    manager: Option<&std::path::Path>,
) -> Vec<AgentEndpoint> {
    let env = std::env::var("SSH_AUTH_SOCK").ok();
    windows_agent_candidates(explicit, manager, env.as_deref())
}

#[cfg(unix)]
fn agent_candidates_platform(
    explicit: Option<&str>,
    manager: Option<&std::path::Path>,
) -> Vec<AgentEndpoint> {
    agent_candidates(explicit, manager)
        .into_iter()
        .map(AgentEndpoint::Socket)
        .collect()
}

#[cfg(unix)]
async fn connect_agent(
    socket: &std::path::Path,
) -> Result<russh::keys::agent::client::AgentClient<tokio::net::UnixStream>, String> {
    russh::keys::agent::client::AgentClient::connect_uds(socket)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(windows)]
async fn connect_agent(
    pipe: &std::path::Path,
) -> Result<
    russh::keys::agent::client::AgentClient<tokio::net::windows::named_pipe::NamedPipeClient>,
    String,
> {
    // 1Password and the OpenSSH agent service both serve this pipe.
    russh::keys::agent::client::AgentClient::connect_named_pipe(pipe)
        .await
        .map_err(|e| e.to_string())
}

/// A raw byte stream to the first local agent that answers (agent forwarding).
async fn agent_stream(endpoints: &[AgentEndpoint]) -> Result<Box<dyn x11::Duplex>, String> {
    let mut tried = Vec::new();
    for e in endpoints {
        let r: Result<Box<dyn x11::Duplex>, String> = match e {
            #[cfg(unix)]
            AgentEndpoint::Socket(p) => tokio::net::UnixStream::connect(p)
                .await
                .map(|s| Box::new(s) as Box<dyn x11::Duplex>)
                .map_err(|e| e.to_string()),
            #[cfg(windows)]
            AgentEndpoint::Socket(p) => tokio::net::windows::named_pipe::ClientOptions::new()
                .open(p)
                .map(|s| Box::new(s) as Box<dyn x11::Duplex>)
                .map_err(|e| e.to_string()),
            #[cfg(not(any(unix, windows)))]
            AgentEndpoint::Socket(_) => Err("no agent sockets on this platform".into()),
            // Pageant speaks window messages, not a byte stream.
            AgentEndpoint::Pageant => Err("Pageant cannot be forwarded".into()),
        };
        match r {
            Ok(s) => return Ok(s),
            Err(err) => tried.push(format!("{}: {err}", e.display())),
        }
    }
    Err(if tried.is_empty() {
        "no SSH agent found".into()
    } else {
        tried.join("; ")
    })
}

/// A short name for the agent behind `socket`, for the status line.
fn agent_name(socket: &std::path::Path) -> &'static str {
    if socket
        .to_string_lossy()
        .to_lowercase()
        .contains("1password")
    {
        "1Password agent"
    } else {
        "agent"
    }
}

/// Sign in with the keys `agent` holds (only `wanted`, when given). `Err` says why not.
async fn sign_with_agent<S>(
    handle: &mut Handle<ClientHandler>,
    target: &SshTarget,
    mut agent: russh::keys::agent::client::AgentClient<S>,
    wanted: Option<&PublicKey>,
    hash: Option<russh::keys::HashAlg>,
    name: &str,
) -> Result<String, String>
where
    S: russh::keys::agent::client::AgentStream + Send + Unpin + 'static,
{
    let ids = agent
        .request_identities()
        .await
        .map_err(|e| e.to_string())?;
    let keys: Vec<PublicKey> = ids
        .into_iter()
        .filter_map(|id| match id {
            russh::keys::agent::AgentIdentity::PublicKey { key, .. } => Some(key),
            _ => None,
        })
        .filter(|k| wanted.is_none_or(|w| w.key_data() == k.key_data()))
        .collect();
    if keys.is_empty() {
        return Err(if wanted.is_some() {
            "does not hold the chosen key".into()
        } else {
            "holds no keys".into()
        });
    }
    // The agent may ask the user to approve (1Password shows its own dialog).
    tracing::info!(host = %target.label, agent = name, keys = keys.len(), "agent sign-in");
    for key in keys {
        let kind = key_kind(&key);
        let ok = handle
            .authenticate_publickey_with(target.user.clone(), key, hash, &mut agent)
            .await
            .map(|r| r.success())
            .unwrap_or(false);
        if ok {
            return Ok(format!("{name} · {kind}"));
        }
    }
    Err("no key was accepted".into())
}

async fn agent_auth(
    handle: &mut Handle<ClientHandler>,
    target: &SshTarget,
    manager_socket: Option<&std::path::Path>,
    only_key: Option<&std::path::Path>,
) -> Result<String, SshError> {
    let wanted = match only_key.map(|p| (p, std::fs::read_to_string(p))) {
        Some((p, Ok(text))) => Some(
            PublicKey::from_openssh(text.trim())
                .map_err(|e| SshError::Io(format!("{} is not a public key: {e}", p.display())))?,
        ),
        // No such file (an imported `IdentityFile` without its `.pub`): offer every key.
        Some((p, Err(e))) => {
            tracing::warn!(path = %p.display(), error = %e, "agent key file unreadable; offering all keys");
            None
        }
        None => None,
    };
    let candidates = agent_candidates_platform(target.agent_socket.as_deref(), manager_socket);
    if candidates.is_empty() {
        return Err(auth_failed(
            target,
            "no SSH agent found: SSH_AUTH_SOCK is not set and the 1Password agent \
             (~/.1password/agent.sock) is not running; set the agent socket on the Host",
        ));
    }
    let hash = handle
        .best_supported_rsa_hash()
        .await
        .map_err(|e| auth_failed(target, e.to_string()))?
        .flatten();
    let mut tried = Vec::new();
    for endpoint in candidates {
        let name = endpoint.name();
        let result = match &endpoint {
            AgentEndpoint::Socket(socket) => match connect_agent(socket).await {
                Ok(agent) => {
                    sign_with_agent(handle, target, agent, wanted.as_ref(), hash, name).await
                }
                Err(e) => Err(e),
            },
            #[cfg(windows)]
            AgentEndpoint::Pageant => {
                match russh::keys::agent::client::AgentClient::connect_pageant().await {
                    Ok(agent) => {
                        sign_with_agent(handle, target, agent, wanted.as_ref(), hash, name).await
                    }
                    Err(e) => Err(e.to_string()),
                }
            }
            #[cfg(not(windows))]
            AgentEndpoint::Pageant => Err("Pageant is only available on Windows".into()),
        };
        match result {
            Ok(method) => return Ok(method),
            Err(e) => tried.push(format!("{} ({name}): {e}", endpoint.display())),
        }
    }
    Err(auth_failed(target, tried.join("; ")))
}

type Slot = Arc<tokio::sync::Mutex<Weak<SshConn>>>;

/// How long a new session stays up without users.
const SESSION_LINGER: Duration = Duration::from_secs(60);

/// Shares one session per Host. Sessions close when the last user drops them.
pub struct SshManager {
    known: KnownHosts,
    prompter: Arc<dyn SshPrompter>,
    slots: Mutex<HashMap<String, Slot>>,
    agent_socket: Option<PathBuf>,
    accepted: Mutex<HashMap<String, String>>,
}

impl SshManager {
    /// A manager using `known` for host keys and `prompter` for questions.
    pub fn new(known: KnownHosts, prompter: Arc<dyn SshPrompter>) -> Self {
        Self {
            known,
            prompter,
            slots: Mutex::default(),
            agent_socket: None,
            accepted: Mutex::default(),
        }
    }

    /// After the user confirmed a changed key, accept exactly `fingerprint` for Host `id`
    /// on its next connection and store it in Switchyard's known_hosts.
    pub fn accept_changed_key(&self, id: &str, fingerprint: &str) {
        self.accepted
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id.to_owned(), fingerprint.to_owned());
    }

    /// Use this agent socket instead of `SSH_AUTH_SOCK` (tests).
    pub fn with_agent_socket(mut self, path: PathBuf) -> Self {
        self.agent_socket = Some(path);
        self
    }

    /// The host key files in use.
    pub fn known_hosts(&self) -> &KnownHosts {
        &self.known
    }

    fn slot(&self, id: &str) -> Slot {
        let mut slots = self.slots.lock().unwrap_or_else(|p| p.into_inner());
        slots.entry(id.to_owned()).or_default().clone()
    }

    /// The live session for `target`, connecting (through its jump chain) when needed.
    pub fn session<'a>(
        &'a self,
        target: &'a SshTarget,
    ) -> BoxFuture<'a, Result<Arc<SshConn>, SshError>> {
        Box::pin(async move {
            let slot = self.slot(&target.id);
            // Holding the per-Host lock while connecting means a second terminal waits
            // for the first login instead of logging in again.
            let mut guard = slot.lock().await;
            if let Some(conn) = guard.upgrade()
                && !conn.is_closed()
            {
                debug!(host = %target.label, "reusing ssh session");
                return Ok(conn);
            }
            let via = match &target.jump {
                Some(j) => Some(self.session(j).await?),
                None => None,
            };
            let accept = self
                .accepted
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&target.id);
            let conn = Arc::new(
                connect_one(
                    target,
                    via,
                    &self.known,
                    &self.prompter,
                    self.agent_socket.as_deref(),
                    accept,
                )
                .await?,
            );
            *guard = Arc::downgrade(&conn);
            // Keep a fresh session for a minute even when nothing uses it yet, so a test
            // followed by "connect", or reopening a terminal, does not log in (and ask for
            // an MFA code) again.
            let linger = conn.clone();
            tokio::spawn(async move {
                tokio::time::sleep(SESSION_LINGER).await;
                drop(linger);
            });
            Ok(conn)
        })
    }

    /// Whether a live session exists for a Host.
    pub fn is_connected(&self, id: &str) -> bool {
        let slot = {
            let slots = self.slots.lock().unwrap_or_else(|p| p.into_inner());
            slots.get(id).cloned()
        };
        slot.and_then(|s| s.try_lock().ok().and_then(|g| g.upgrade()))
            .is_some_and(|c| !c.is_closed())
    }
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::path::Path;

    #[test]
    fn agent_socket_choice() {
        // An explicit socket (IdentityAgent) is the only one tried; `~` expands.
        let home = std::env::var("HOME").unwrap();
        assert_eq!(
            agent_candidates(Some("~/.1password/agent.sock"), None),
            vec![PathBuf::from(format!("{home}/.1password/agent.sock"))]
        );
        // Without one: SSH_AUTH_SOCK first, then 1Password's socket when it exists.
        let c = agent_candidates(None, None);
        if let Some(env) = std::env::var_os("SSH_AUTH_SOCK") {
            assert_eq!(c[0], PathBuf::from(env));
        }
        assert!(
            c.iter().all(|p| p.exists()
                || Some(p.as_os_str()) == std::env::var_os("SSH_AUTH_SOCK").as_deref())
        );
        assert_eq!(
            agent_name(Path::new("/x/.1password/agent.sock")),
            "1Password agent"
        );
        assert_eq!(agent_name(Path::new("/tmp/ssh-x/agent.1")), "agent");
    }

    #[test]
    fn public_key_files_select_the_agent() {
        let t = tempfile::tempdir().unwrap();
        let pubf = t.path().join("id");
        std::fs::write(&pubf, "ssh-ed25519 AAAAC3Nza... me@laptop\n").unwrap();
        assert!(is_public_key_file(&pubf), "by content");
        assert!(is_public_key_file(Path::new("/nope/key.pub")), "by name");
        let private = t.path().join("id_priv");
        std::fs::write(&private, "-----BEGIN OPENSSH PRIVATE KEY-----\n").unwrap();
        assert!(!is_public_key_file(&private));
    }
}

#[cfg(test)]
mod windows_agent_tests {
    use super::*;

    fn pipe(p: &str) -> AgentEndpoint {
        AgentEndpoint::Socket(PathBuf::from(p))
    }

    #[test]
    fn default_order_is_env_pipe_then_service_then_pageant() {
        assert_eq!(
            windows_agent_candidates(None, None, None),
            [pipe(OPENSSH_AGENT_PIPE), AgentEndpoint::Pageant]
        );
        let custom = r"\\.\pipe\my-agent";
        assert_eq!(
            windows_agent_candidates(None, None, Some(custom)),
            [
                pipe(custom),
                pipe(OPENSSH_AGENT_PIPE),
                AgentEndpoint::Pageant
            ]
        );
        // The service pipe in SSH_AUTH_SOCK is not tried twice; a Unix path there is ignored.
        assert_eq!(
            windows_agent_candidates(None, None, Some(r"\\.\PIPE\openssh-ssh-agent")),
            [pipe(r"\\.\PIPE\openssh-ssh-agent"), AgentEndpoint::Pageant]
        );
        assert_eq!(
            windows_agent_candidates(None, None, Some("/tmp/ssh-x/agent.1")),
            [pipe(OPENSSH_AGENT_PIPE), AgentEndpoint::Pageant]
        );
    }

    #[test]
    fn the_host_field_picks_one_agent() {
        assert_eq!(
            windows_agent_candidates(Some(" Pageant "), None, None),
            [AgentEndpoint::Pageant]
        );
        assert_eq!(
            windows_agent_candidates(Some("//./pipe/agent"), None, None),
            [pipe("//./pipe/agent")]
        );
        assert_eq!(
            windows_agent_candidates(Some("SSH_AUTH_SOCK"), None, Some(r"\\.\pipe\a")),
            [pipe(r"\\.\pipe\a")]
        );
        // An imported Unix socket (1Password on macOS) falls back to the defaults.
        assert_eq!(
            windows_agent_candidates(Some("~/.1password/agent.sock"), None, None),
            [pipe(OPENSSH_AGENT_PIPE), AgentEndpoint::Pageant]
        );
        assert_eq!(AgentEndpoint::Pageant.name(), "Pageant");
    }
}
