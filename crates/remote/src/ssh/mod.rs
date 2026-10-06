//! SSH sessions over `russh`: strict host key checking, password / public key /
//! keyboard-interactive / agent authentication, ProxyJump chains, keepalives, and one
//! shared session per Host (terminals, tunnels and SFTP all ride on it).

pub mod known_hosts;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use futures::future::BoxFuture;
use russh::client::{self, Handle, KeyboardInteractiveAuthResponse, Msg};
use russh::keys::{PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate, load_secret_key};
use russh::{Channel, ChannelMsg, Disconnect};
use secrecy::{ExposeSecret, SecretString};
use tracing::{debug, info};

pub use known_hosts::{HostKeyStatus, KnownHosts, fingerprint};

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
    /// Keys held by the SSH agent.
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
    label: String,
    address: String,
    port: u16,
    prompter: Arc<dyn SshPrompter>,
    outcome: Arc<Mutex<KeyOutcome>>,
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

    /// An interactive shell with a PTY of `cols × rows`.
    pub async fn open_shell(&self, cols: u16, rows: u16) -> Result<Channel<Msg>, SshError> {
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
        ch.request_shell(true)
            .await
            .map_err(|e| SshError::Channel(e.to_string()))?;
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

/// Connect and authenticate one hop. `via` is the previous hop, when jumping.
async fn connect_one(
    target: &SshTarget,
    via: Option<Arc<SshConn>>,
    known: &KnownHosts,
    prompter: &Arc<dyn SshPrompter>,
    agent_socket: Option<&std::path::Path>,
) -> Result<SshConn, SshError> {
    let keepalive = (!target.keepalive.is_zero()).then_some(target.keepalive);
    let config = Arc::new(client::Config {
        keepalive_interval: keepalive,
        keepalive_max: 3,
        nodelay: true,
        ..Default::default()
    });
    let outcome: Arc<Mutex<KeyOutcome>> = Arc::default();
    let handler = ClientHandler {
        known: known.clone(),
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
        SshAuthMethod::Agent => agent_auth(handle, target, agent_socket).await,
    }
}

#[cfg(unix)]
async fn connect_agent(
    socket: Option<&std::path::Path>,
) -> Result<russh::keys::agent::client::AgentClient<tokio::net::UnixStream>, String> {
    use russh::keys::agent::client::AgentClient;
    match socket {
        Some(p) => AgentClient::connect_uds(p).await,
        None => AgentClient::connect_env().await,
    }
    .map_err(|e| format!("no SSH agent ({e}); is SSH_AUTH_SOCK set?"))
}

#[cfg(windows)]
async fn connect_agent(
    _socket: Option<&std::path::Path>,
) -> Result<
    russh::keys::agent::client::AgentClient<tokio::net::windows::named_pipe::NamedPipeClient>,
    String,
> {
    russh::keys::agent::client::AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent")
        .await
        .map_err(|e| format!("the OpenSSH agent service is not running ({e})"))
}

async fn agent_auth(
    handle: &mut Handle<ClientHandler>,
    target: &SshTarget,
    socket: Option<&std::path::Path>,
) -> Result<String, SshError> {
    let mut agent = connect_agent(socket)
        .await
        .map_err(|m| auth_failed(target, m))?;
    let ids = agent
        .request_identities()
        .await
        .map_err(|e| auth_failed(target, e.to_string()))?;
    if ids.is_empty() {
        return Err(auth_failed(target, "the SSH agent holds no keys"));
    }
    let hash = handle
        .best_supported_rsa_hash()
        .await
        .map_err(|e| auth_failed(target, e.to_string()))?
        .flatten();
    for id in ids {
        let russh::keys::agent::AgentIdentity::PublicKey { key, .. } = id else {
            continue;
        };
        let kind = key_kind(&key);
        let ok = handle
            .authenticate_publickey_with(target.user.clone(), key, hash, &mut agent)
            .await
            .map(|r| r.success())
            .unwrap_or(false);
        if ok {
            return Ok(format!("agent · {kind}"));
        }
    }
    Err(auth_failed(target, "no agent key was accepted"))
}

type Slot = Arc<tokio::sync::Mutex<Weak<SshConn>>>;

/// Shares one session per Host. Sessions close when the last user drops them.
pub struct SshManager {
    known: KnownHosts,
    prompter: Arc<dyn SshPrompter>,
    slots: Mutex<HashMap<String, Slot>>,
    agent_socket: Option<PathBuf>,
}

impl SshManager {
    /// A manager using `known` for host keys and `prompter` for questions.
    pub fn new(known: KnownHosts, prompter: Arc<dyn SshPrompter>) -> Self {
        Self {
            known,
            prompter,
            slots: Mutex::default(),
            agent_socket: None,
        }
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
            let conn = Arc::new(
                connect_one(
                    target,
                    via,
                    &self.known,
                    &self.prompter,
                    self.agent_socket.as_deref(),
                )
                .await?,
            );
            *guard = Arc::downgrade(&conn);
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
