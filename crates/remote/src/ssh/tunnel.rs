//! Port forwards through a Host's shared session:
//!
//! - **local** (`-L`): a listener on this machine whose connections reach `host:port` as
//!   seen from the server, through `direct-tcpip` channels;
//! - **remote** (`-R`): the server listens (`tcpip-forward`) and its connections arrive as
//!   `forwarded-tcpip` channels that reach `host:port` as seen from this machine;
//! - **dynamic** (`-D`): a local SOCKS4/4a/5 proxy whose CONNECT targets are reached from
//!   the server.
//!
//! A tunnel keeps its SSH session alive while it exists. Local and dynamic tunnels log in
//! again (through the shared [`SshManager`]) when a connection arrives after the session
//! dropped; a remote tunnel watches its session and asks the server again after a reconnect.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client::{ChannelOpenHandle, Msg};
use russh::{Channel, ChannelOpenFailure};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use super::{SshConn, SshError, SshManager, SshTarget};

/// What a tunnel is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TunnelStatus {
    /// Listening; the SSH session is up.
    Active,
    /// The session dropped; the tunnel logs in again.
    Reconnecting,
    /// The last attempt to reach the target (or to listen on the server) failed.
    Failed(String),
    /// Stopped by the user.
    Stopped,
}

/// The direction of a forward.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ForwardKind {
    /// `-L`: listen here, connect from the server.
    Local,
    /// `-R`: listen on the server, connect from here.
    Remote,
    /// `-D`: SOCKS proxy here, connect from the server.
    Dynamic,
}

impl ForwardKind {
    /// Short label (`L`, `R`, `D`) as OpenSSH's flags.
    pub fn flag(self) -> &'static str {
        match self {
            ForwardKind::Local => "L",
            ForwardKind::Remote => "R",
            ForwardKind::Dynamic => "D",
        }
    }
}

/// What to forward.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForwardSpec {
    /// Listen on `bind_address:bind_port` here (port 0 = any free port); connections reach
    /// `host:port` as seen from the server.
    Local {
        /// Local address to listen on (`127.0.0.1` keeps it private to this machine).
        bind_address: String,
        /// Local port; 0 picks a free one.
        bind_port: u16,
        /// Target host as the server resolves it.
        host: String,
        /// Target port.
        port: u16,
    },
    /// The server listens on `bind_address:bind_port` (port 0 = the server picks;
    /// non-loopback addresses need `GatewayPorts` on the server); connections reach
    /// `host:port` as seen from this machine.
    Remote {
        /// Address on the server (`localhost`, `0.0.0.0`, …).
        bind_address: String,
        /// Port on the server; 0 lets it pick.
        bind_port: u16,
        /// Target host as this machine resolves it.
        host: String,
        /// Target port.
        port: u16,
    },
    /// A SOCKS4/4a/5 proxy on `bind_address:bind_port` here; targets are reached from the
    /// server.
    Dynamic {
        /// Local address to listen on.
        bind_address: String,
        /// Local port; 0 picks a free one.
        bind_port: u16,
    },
}

impl ForwardSpec {
    /// The kind of forward.
    pub fn kind(&self) -> ForwardKind {
        match self {
            ForwardSpec::Local { .. } => ForwardKind::Local,
            ForwardSpec::Remote { .. } => ForwardKind::Remote,
            ForwardSpec::Dynamic { .. } => ForwardKind::Dynamic,
        }
    }

    /// `host:port` the forward reaches, or `SOCKS` for a dynamic one.
    pub fn target(&self) -> String {
        match self {
            ForwardSpec::Local { host, port, .. } | ForwardSpec::Remote { host, port, .. } => {
                format!("{host}:{port}")
            }
            ForwardSpec::Dynamic { .. } => "SOCKS".into(),
        }
    }
}

/// A point-in-time view for the tunnel manager.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TunnelInfo {
    /// Tunnel id.
    pub id: u64,
    /// The Host's saved forward this tunnel runs, if it is one (not a DB connection's).
    pub forward_id: Option<String>,
    /// Host profile id.
    pub host_id: String,
    /// Host label.
    pub host: String,
    /// Direction.
    pub kind: ForwardKind,
    /// Port listened on: here for local and dynamic forwards, on the server for remote.
    pub local_port: u16,
    /// `address:port` listened on, prefixed with the Host for remote forwards.
    pub listen: String,
    /// `host:port` reached (`SOCKS` for dynamic forwards).
    pub remote: String,
    /// Status.
    pub status: TunnelStatus,
    /// Bytes sent towards the target.
    pub bytes_up: u64,
    /// Bytes received from the target.
    pub bytes_down: u64,
    /// Open forwarded connections.
    pub connections: usize,
}

#[derive(Default)]
struct Stats {
    up: AtomicU64,
    down: AtomicU64,
    active: AtomicUsize,
    stopped: AtomicBool,
    reconnecting: AtomicBool,
    error: Mutex<Option<String>>,
}

impl Stats {
    fn set_error(&self, e: Option<String>) {
        *self.error.lock().unwrap_or_else(|p| p.into_inner()) = e;
    }
}

/// Where the server's `forwarded-tcpip` channels go, per server port.
#[derive(Default)]
pub(super) struct RemoteRoutes {
    routes: Mutex<HashMap<u32, RemoteRoute>>,
}

#[derive(Clone)]
pub(super) struct RemoteRoute {
    host: String,
    port: u16,
    stats: Arc<Stats>,
}

/// Pending route for a request with port 0 (the server reports the port only afterwards).
const PENDING: u32 = 0;

impl RemoteRoutes {
    fn insert(&self, port: u32, route: RemoteRoute) {
        self.lock().insert(port, route);
    }

    fn remove(&self, port: u32) {
        self.lock().remove(&port);
    }

    fn rekey(&self, from: u32, to: u32) {
        let mut r = self.lock();
        if let Some(route) = r.remove(&from) {
            r.insert(to, route);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u32, RemoteRoute>> {
        self.routes.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Serve a `forwarded-tcpip` channel the server opened for `port`.
    pub(super) fn open(&self, port: u32, channel: Channel<Msg>, reply: ChannelOpenHandle) {
        let route = {
            let r = self.lock();
            r.get(&port).or_else(|| r.get(&PENDING)).cloned()
        };
        let Some(route) = route else {
            // A forward we did not ask for (or already cancelled): drop rejects it.
            debug!(port, "rejected an unexpected forwarded connection");
            return;
        };
        tokio::spawn(async move {
            let sock = tokio::time::timeout(
                Duration::from_secs(10),
                TcpStream::connect((route.host.as_str(), route.port)),
            )
            .await
            .map_err(|_| "timed out".to_owned())
            .and_then(|r| r.map_err(|e| e.to_string()));
            match sock {
                Ok(sock) => {
                    route.stats.set_error(None);
                    reply.accept().await;
                    let _ = sock.set_nodelay(true);
                    serve(sock, channel, &route.stats).await;
                }
                Err(e) => {
                    let msg = format!("{}:{}: {e}", route.host, route.port);
                    warn!(error = %msg, "remote forward could not reach its target");
                    route.stats.set_error(Some(msg));
                    reply.reject(ChannelOpenFailure::ConnectFailed).await;
                }
            }
        });
    }
}

async fn session(
    manager: &SshManager,
    target: &SshTarget,
    slot: &tokio::sync::Mutex<Option<Arc<SshConn>>>,
) -> Result<Arc<SshConn>, SshError> {
    let mut guard = slot.lock().await;
    if let Some(c) = guard.as_ref()
        && !c.is_closed()
    {
        return Ok(c.clone());
    }
    let c = manager.session(target).await?;
    *guard = Some(c.clone());
    Ok(c)
}

/// Copy one direction, counting bytes.
async fn pump<R, W>(mut from: R, mut to: W, counter: &AtomicU64)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        let n = match from.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if to.write_all(&buf[..n]).await.is_err() {
            break;
        }
        counter.fetch_add(n as u64, Ordering::Relaxed);
    }
    let _ = to.shutdown().await;
}

/// Carry a connection over a channel until either side closes. "Up" is towards the target.
async fn serve(sock: TcpStream, channel: Channel<Msg>, stats: &Stats) {
    stats.active.fetch_add(1, Ordering::Relaxed);
    let (sr, sw) = sock.into_split();
    let (cr, cw) = tokio::io::split(channel.into_stream());
    tokio::join!(pump(sr, cw, &stats.up), pump(cr, sw, &stats.down));
    stats.active.fetch_sub(1, Ordering::Relaxed);
    debug!("forwarded connection closed");
}

/// A running forward. Dropping it stops it.
pub struct Tunnel {
    id: u64,
    forward_id: Option<String>,
    target: SshTarget,
    spec: ForwardSpec,
    /// The port actually listened on (here, or on the server for remote forwards).
    port: Arc<AtomicU16>,
    stats: Arc<Stats>,
    conn: Arc<tokio::sync::Mutex<Option<Arc<SshConn>>>>,
    task: JoinHandle<()>,
    forwards: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl std::fmt::Debug for Tunnel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tunnel")
            .field("id", &self.id)
            .field("kind", &self.spec.kind())
            .field("port", &self.port.load(Ordering::Relaxed))
            .field("target", &self.spec.target())
            .finish()
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn listen(bind_address: &str, bind_port: u16) -> Result<(TcpListener, SocketAddr), SshError> {
    let addr = if bind_address.trim().is_empty() {
        "127.0.0.1"
    } else {
        bind_address.trim()
    };
    let listener = TcpListener::bind((addr, bind_port))
        .await
        .map_err(|e| SshError::Io(format!("could not listen on {addr}:{bind_port}: {e}")))?;
    let local = listener
        .local_addr()
        .map_err(|e| SshError::Io(e.to_string()))?;
    Ok((listener, local))
}

/// Ask the server to listen, routing its connections to `host:port` here. Returns the
/// server's port.
async fn request_remote(
    conn: &SshConn,
    bind_address: &str,
    bind_port: u16,
    route: RemoteRoute,
) -> Result<u16, SshError> {
    let _one_at_a_time = conn.forward_requests.lock().await;
    let key = u32::from(bind_port);
    conn.routes.insert(key, route);
    match conn.handle()?.tcpip_forward(bind_address, key).await {
        Ok(port) => {
            // Port 0 asked: the server's reply names the port; requests for a fixed port
            // may answer 0.
            let actual = if key == PENDING { port } else { key };
            if actual != key {
                conn.routes.rekey(key, actual);
            }
            u16::try_from(actual)
                .map_err(|_| SshError::Channel(format!("server picked port {actual}")))
        }
        Err(e) => {
            conn.routes.remove(key);
            Err(SshError::Channel(format!(
                "the server refused to listen on {bind_address}:{bind_port}: {e}"
            )))
        }
    }
}

impl Tunnel {
    /// A local forward on a free `127.0.0.1` port to `remote_host:remote_port` (database
    /// connections through a Host).
    pub async fn open(
        id: u64,
        manager: Arc<SshManager>,
        target: SshTarget,
        remote_host: String,
        remote_port: u16,
    ) -> Result<Self, SshError> {
        let spec = ForwardSpec::Local {
            bind_address: "127.0.0.1".into(),
            bind_port: 0,
            host: remote_host,
            port: remote_port,
        };
        Self::start(id, None, manager, target, spec).await
    }

    /// Log in to the Host (prompting if needed) and start `spec`. `forward_id` names the
    /// Host's saved forward it runs.
    pub async fn start(
        id: u64,
        forward_id: Option<String>,
        manager: Arc<SshManager>,
        target: SshTarget,
        spec: ForwardSpec,
    ) -> Result<Self, SshError> {
        let conn = manager.session(&target).await?;
        let stats: Arc<Stats> = Arc::default();
        let slot = Arc::new(tokio::sync::Mutex::new(Some(conn.clone())));
        let forwards: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::default();
        let port = Arc::new(AtomicU16::new(0));
        let task = match &spec {
            ForwardSpec::Local {
                bind_address,
                bind_port,
                ..
            }
            | ForwardSpec::Dynamic {
                bind_address,
                bind_port,
            } => {
                let (listener, local) = listen(bind_address, *bind_port).await?;
                port.store(local.port(), Ordering::Relaxed);
                info!(host = %target.label, kind = spec.kind().flag(), listen = %local, to = %spec.target(), "tunnel open");
                let ctx = Accept {
                    manager,
                    target: target.clone(),
                    slot: slot.clone(),
                    stats: stats.clone(),
                    spec: spec.clone(),
                };
                let forwards = forwards.clone();
                tokio::spawn(async move {
                    while let Ok((sock, _)) = listener.accept().await {
                        let ctx = ctx.clone();
                        let h = tokio::spawn(async move { ctx.connection(sock).await });
                        let mut f = forwards.lock().unwrap_or_else(|p| p.into_inner());
                        f.retain(|h| !h.is_finished());
                        f.push(h);
                    }
                })
            }
            ForwardSpec::Remote {
                bind_address,
                bind_port,
                host,
                port: target_port,
            } => {
                let route = RemoteRoute {
                    host: host.clone(),
                    port: *target_port,
                    stats: stats.clone(),
                };
                let actual = request_remote(&conn, bind_address, *bind_port, route.clone()).await?;
                port.store(actual, Ordering::Relaxed);
                info!(host = %target.label, kind = "R", port = actual, to = %spec.target(), "tunnel open");
                tokio::spawn(supervise_remote(
                    manager,
                    target.clone(),
                    slot.clone(),
                    stats.clone(),
                    bind_address.clone(),
                    port.clone(),
                    route,
                ))
            }
        };
        Ok(Self {
            id,
            forward_id,
            target,
            spec,
            port,
            stats,
            conn: slot,
            task,
            forwards,
        })
    }

    /// Tunnel id.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The saved forward this tunnel runs, if any.
    pub fn forward_id(&self) -> Option<&str> {
        self.forward_id.as_deref()
    }

    /// What it forwards.
    pub fn spec(&self) -> &ForwardSpec {
        &self.spec
    }

    /// The local address to connect to (local and dynamic forwards).
    pub fn local(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.port.load(Ordering::Relaxed)))
    }

    /// The port listened on (here, or on the server for a remote forward).
    pub fn port(&self) -> u16 {
        self.port.load(Ordering::Relaxed)
    }

    /// Host profile id.
    pub fn host_id(&self) -> &str {
        &self.target.id
    }

    /// `(host, port)` a local forward reaches; `("", 0)` for other kinds.
    pub fn remote(&self) -> (&str, u16) {
        match &self.spec {
            ForwardSpec::Local { host, port, .. } => (host, *port),
            _ => ("", 0),
        }
    }

    /// Whether [`Tunnel::stop`] was called.
    pub fn is_stopped(&self) -> bool {
        self.stats.stopped.load(Ordering::SeqCst)
    }

    /// Stop listening and cut every forwarded connection.
    pub fn stop(&self) {
        if self.stats.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        self.task.abort();
        for h in self
            .forwards
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain(..)
        {
            h.abort();
        }
        if let ForwardSpec::Remote { bind_address, .. } = &self.spec
            && let Ok(guard) = self.conn.try_lock()
            && let Some(conn) = guard.clone()
            && let Ok(rt) = tokio::runtime::Handle::try_current()
        {
            let (addr, port) = (bind_address.clone(), u32::from(self.port()));
            rt.spawn(async move {
                conn.routes.remove(port);
                if let Ok(h) = conn.handle() {
                    let _ = h.cancel_tcpip_forward(addr, port).await;
                }
            });
        }
        info!(
            port = self.port(),
            kind = self.spec.kind().flag(),
            "tunnel stopped"
        );
    }

    /// Current numbers for the tunnel manager.
    pub fn info(&self) -> TunnelInfo {
        let session_up = self
            .conn
            .try_lock()
            .map(|g| g.as_ref().is_some_and(|c| !c.is_closed()))
            // Locked = a login is in progress.
            .unwrap_or(false);
        let error = self
            .stats
            .error
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let status = if self.is_stopped() {
            TunnelStatus::Stopped
        } else if let Some(e) = error {
            TunnelStatus::Failed(e)
        } else if session_up && !self.stats.reconnecting.load(Ordering::Relaxed) {
            TunnelStatus::Active
        } else {
            TunnelStatus::Reconnecting
        };
        let port = self.port();
        let listen = match &self.spec {
            ForwardSpec::Local { bind_address, .. } | ForwardSpec::Dynamic { bind_address, .. } => {
                format!("{}:{port}", display_bind(bind_address))
            }
            ForwardSpec::Remote { bind_address, .. } => {
                format!(
                    "{} {}:{port}",
                    self.target.label,
                    display_bind(bind_address)
                )
            }
        };
        TunnelInfo {
            id: self.id,
            forward_id: self.forward_id.clone(),
            host_id: self.target.id.clone(),
            host: self.target.label.clone(),
            kind: self.spec.kind(),
            local_port: port,
            listen,
            remote: self.spec.target(),
            status,
            bytes_up: self.stats.up.load(Ordering::Relaxed),
            bytes_down: self.stats.down.load(Ordering::Relaxed),
            connections: self.stats.active.load(Ordering::Relaxed),
        }
    }
}

fn display_bind(addr: &str) -> &str {
    if addr.trim().is_empty() {
        "127.0.0.1"
    } else {
        addr.trim()
    }
}

/// Keeps a remote forward alive: after the session drops, log in again (with backoff) and
/// ask the server to listen on the same port.
async fn supervise_remote(
    manager: Arc<SshManager>,
    target: SshTarget,
    slot: Arc<tokio::sync::Mutex<Option<Arc<SshConn>>>>,
    stats: Arc<Stats>,
    bind_address: String,
    port: Arc<AtomicU16>,
    route: RemoteRoute,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let up = slot.lock().await.as_ref().is_some_and(|c| !c.is_closed());
        if up {
            continue;
        }
        stats.reconnecting.store(true, Ordering::Relaxed);
        let result = match session(&manager, &target, &slot).await {
            Ok(conn) => {
                request_remote(
                    &conn,
                    &bind_address,
                    port.load(Ordering::Relaxed),
                    route.clone(),
                )
                .await
            }
            Err(e) => Err(e),
        };
        match result {
            Ok(p) => {
                port.store(p, Ordering::Relaxed);
                stats.reconnecting.store(false, Ordering::Relaxed);
                stats.set_error(None);
                backoff = Duration::from_secs(1);
                info!(host = %target.label, port = p, "remote forward restored");
            }
            Err(e) => {
                stats.set_error(Some(e.to_string()));
                // Drop a session that came up but refused the forward, so the next round
                // logs in again.
                *slot.lock().await = None;
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}

/// What every accepted connection of a local or dynamic forward needs.
#[derive(Clone)]
struct Accept {
    manager: Arc<SshManager>,
    target: SshTarget,
    slot: Arc<tokio::sync::Mutex<Option<Arc<SshConn>>>>,
    stats: Arc<Stats>,
    spec: ForwardSpec,
}

impl Accept {
    async fn channel(&self, host: &str, port: u16) -> Option<Channel<Msg>> {
        let channel = match session(&self.manager, &self.target, &self.slot).await {
            Ok(conn) => conn.direct_tcpip(host, port).await,
            Err(e) => Err(e),
        };
        match channel {
            Ok(c) => {
                self.stats.set_error(None);
                Some(c)
            }
            Err(e) => {
                warn!(error = %e, "tunnel could not reach its target");
                self.stats.set_error(Some(e.to_string()));
                None
            }
        }
    }

    async fn connection(&self, mut sock: TcpStream) {
        let _ = sock.set_nodelay(true);
        match &self.spec {
            ForwardSpec::Local { host, port, .. } => {
                if let Some(ch) = self.channel(host, *port).await {
                    serve(sock, ch, &self.stats).await;
                }
            }
            ForwardSpec::Dynamic { .. } => {
                let request = match socks::handshake(&mut sock).await {
                    Ok(r) => r,
                    Err(e) => {
                        debug!(error = %e, "SOCKS handshake failed");
                        return;
                    }
                };
                let ch = self.channel(&request.host, request.port).await;
                if socks::reply(&mut sock, request.version, ch.is_some())
                    .await
                    .is_err()
                {
                    return;
                }
                if let Some(ch) = ch {
                    serve(sock, ch, &self.stats).await;
                }
            }
            ForwardSpec::Remote { .. } => {}
        }
    }
}

/// The server side of SOCKS4, 4a and 5 (no authentication, CONNECT only).
pub(crate) mod socks {
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    /// A CONNECT request.
    #[derive(Debug, PartialEq, Eq)]
    pub(crate) struct Request {
        pub version: u8,
        pub host: String,
        pub port: u16,
    }

    fn bad(msg: &str) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::InvalidData, msg.to_owned())
    }

    async fn read_until_nul<S: AsyncRead + Unpin>(s: &mut S) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            let b = s.read_u8().await?;
            if b == 0 {
                return Ok(out);
            }
            if out.len() >= 255 {
                return Err(bad("SOCKS4 field too long"));
            }
            out.push(b);
        }
    }

    /// Read a client's greeting and CONNECT request. Unsupported requests are answered
    /// (refused) here and return an error.
    pub(crate) async fn handshake<S: AsyncRead + AsyncWrite + Unpin>(
        s: &mut S,
    ) -> std::io::Result<Request> {
        match s.read_u8().await? {
            5 => {
                let n = s.read_u8().await?;
                let mut methods = vec![0u8; usize::from(n)];
                s.read_exact(&mut methods).await?;
                if !methods.contains(&0) {
                    s.write_all(&[5, 0xFF]).await?;
                    return Err(bad("SOCKS5 client offers no 'no authentication' method"));
                }
                s.write_all(&[5, 0]).await?;
                let mut head = [0u8; 4];
                s.read_exact(&mut head).await?;
                let [ver, cmd, _, atyp] = head;
                if ver != 5 {
                    return Err(bad("bad SOCKS5 request"));
                }
                let host = match atyp {
                    1 => {
                        let mut a = [0u8; 4];
                        s.read_exact(&mut a).await?;
                        std::net::Ipv4Addr::from(a).to_string()
                    }
                    3 => {
                        let len = s.read_u8().await?;
                        let mut d = vec![0u8; usize::from(len)];
                        s.read_exact(&mut d).await?;
                        String::from_utf8(d).map_err(|_| bad("SOCKS5 domain is not UTF-8"))?
                    }
                    4 => {
                        let mut a = [0u8; 16];
                        s.read_exact(&mut a).await?;
                        std::net::Ipv6Addr::from(a).to_string()
                    }
                    _ => {
                        s.write_all(&[5, 8, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
                        return Err(bad("unsupported SOCKS5 address type"));
                    }
                };
                let port = s.read_u16().await?;
                if cmd != 1 {
                    s.write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
                    return Err(bad("only SOCKS CONNECT is supported"));
                }
                Ok(Request {
                    version: 5,
                    host,
                    port,
                })
            }
            4 => {
                let cmd = s.read_u8().await?;
                let port = s.read_u16().await?;
                let mut ip = [0u8; 4];
                s.read_exact(&mut ip).await?;
                let _user = read_until_nul(s).await?;
                // 4a: 0.0.0.x (x ≠ 0) means a domain name follows.
                let host = if ip[..3] == [0, 0, 0] && ip[3] != 0 {
                    String::from_utf8(read_until_nul(s).await?)
                        .map_err(|_| bad("SOCKS4a domain is not UTF-8"))?
                } else {
                    std::net::Ipv4Addr::from(ip).to_string()
                };
                if cmd != 1 {
                    s.write_all(&[0, 0x5B, 0, 0, 0, 0, 0, 0]).await?;
                    return Err(bad("only SOCKS CONNECT is supported"));
                }
                Ok(Request {
                    version: 4,
                    host,
                    port,
                })
            }
            _ => Err(bad("not a SOCKS client")),
        }
    }

    /// Answer a CONNECT request.
    pub(crate) async fn reply<S: AsyncWrite + Unpin>(
        s: &mut S,
        version: u8,
        ok: bool,
    ) -> std::io::Result<()> {
        if version == 5 {
            // Bound address unknown on our side: 0.0.0.0:0, which clients ignore.
            let code = if ok { 0 } else { 5 };
            s.write_all(&[5, code, 0, 1, 0, 0, 0, 0, 0, 0]).await
        } else {
            let code = if ok { 0x5A } else { 0x5B };
            s.write_all(&[0, code, 0, 0, 0, 0, 0, 0]).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::socks::{Request, handshake, reply};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn run(client_sends: &[u8]) -> (std::io::Result<Request>, Vec<u8>) {
        let (mut client, mut server) = tokio::io::duplex(1024);
        client.write_all(client_sends).await.unwrap();
        let r = handshake(&mut server).await;
        drop(server);
        let mut got = Vec::new();
        client.read_to_end(&mut got).await.unwrap();
        (r, got)
    }

    #[tokio::test]
    async fn socks5_domain_ipv4_ipv6() {
        let mut msg = vec![5, 1, 0, 5, 1, 0, 3, 11];
        msg.extend_from_slice(b"example.org");
        msg.extend_from_slice(&443u16.to_be_bytes());
        let (r, sent) = run(&msg).await;
        assert_eq!(
            r.unwrap(),
            Request {
                version: 5,
                host: "example.org".into(),
                port: 443
            }
        );
        assert_eq!(sent, [5, 0]);

        let (r, _) = run(&[5, 1, 0, 5, 1, 0, 1, 10, 0, 0, 7, 0, 80]).await;
        assert_eq!(r.unwrap().host, "10.0.0.7");

        let mut v6 = vec![5, 1, 0, 5, 1, 0, 4];
        v6.extend_from_slice(&std::net::Ipv6Addr::LOCALHOST.octets());
        v6.extend_from_slice(&22u16.to_be_bytes());
        let (r, _) = run(&v6).await;
        assert_eq!(r.unwrap().host, "::1");
    }

    #[tokio::test]
    async fn socks5_refusals() {
        // Only username/password offered.
        let (r, sent) = run(&[5, 1, 2]).await;
        assert!(r.is_err());
        assert_eq!(sent, [5, 0xFF]);
        // BIND is not supported.
        let (r, sent) = run(&[5, 1, 0, 5, 2, 0, 1, 1, 2, 3, 4, 0, 80]).await;
        assert!(r.is_err());
        assert_eq!(sent, [5, 0, 5, 7, 0, 1, 0, 0, 0, 0, 0, 0]);
    }

    #[tokio::test]
    async fn socks4_and_4a() {
        let (r, _) = run(&[4, 1, 0, 80, 192, 168, 1, 2, b'u', 0]).await;
        assert_eq!(
            r.unwrap(),
            Request {
                version: 4,
                host: "192.168.1.2".into(),
                port: 80
            }
        );
        let mut a = vec![4, 1, 0x1F, 0x90, 0, 0, 0, 1, 0];
        a.extend_from_slice(b"db.internal\0");
        let (r, _) = run(&a).await;
        assert_eq!(
            r.unwrap(),
            Request {
                version: 4,
                host: "db.internal".into(),
                port: 8080
            }
        );
    }

    #[tokio::test]
    async fn replies() {
        let mut v = Vec::new();
        reply(&mut v, 5, true).await.unwrap();
        reply(&mut v, 4, false).await.unwrap();
        assert_eq!(v, [5, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0x5B, 0, 0, 0, 0, 0, 0]);
    }
}
