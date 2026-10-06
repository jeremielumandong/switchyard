//! Local port forwards: a listener on `127.0.0.1:<ephemeral>` whose connections are carried
//! to `remote_host:remote_port` through `direct-tcpip` channels of the Host's shared session.
//!
//! A tunnel keeps its SSH session alive while it exists, and logs in again (through the
//! shared [`SshManager`]) when a new connection arrives after the session dropped.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use super::{SshConn, SshError, SshManager, SshTarget};

/// What a tunnel is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TunnelStatus {
    /// Listening; the SSH session is up.
    Active,
    /// The session dropped; the next connection logs in again.
    Reconnecting,
    /// The last attempt to reach the target failed.
    Failed(String),
    /// Stopped by the user.
    Stopped,
}

/// A point-in-time view for the tunnel manager.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TunnelInfo {
    /// Tunnel id.
    pub id: u64,
    /// Host profile id.
    pub host_id: String,
    /// Host label.
    pub host: String,
    /// Local port.
    pub local_port: u16,
    /// `host:port` as seen from the server.
    pub remote: String,
    /// Status.
    pub status: TunnelStatus,
    /// Bytes sent towards the server.
    pub bytes_up: u64,
    /// Bytes received from the server.
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
    error: Mutex<Option<String>>,
}

/// A running local forward. Dropping it stops it.
pub struct Tunnel {
    id: u64,
    target: SshTarget,
    remote_host: String,
    remote_port: u16,
    local: SocketAddr,
    stats: Arc<Stats>,
    conn: Arc<tokio::sync::Mutex<Option<Arc<SshConn>>>>,
    accept: JoinHandle<()>,
    forwards: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl std::fmt::Debug for Tunnel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tunnel")
            .field("id", &self.id)
            .field("local", &self.local)
            .field(
                "remote",
                &format_args!("{}:{}", self.remote_host, self.remote_port),
            )
            .finish()
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.stop();
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
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
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

impl Tunnel {
    /// Log in to the Host (prompting if needed), then listen on an ephemeral local port.
    pub async fn open(
        id: u64,
        manager: Arc<SshManager>,
        target: SshTarget,
        remote_host: String,
        remote_port: u16,
    ) -> Result<Self, SshError> {
        let conn = manager.session(&target).await?;
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| SshError::Io(format!("could not open a local port: {e}")))?;
        let local = listener
            .local_addr()
            .map_err(|e| SshError::Io(e.to_string()))?;
        info!(host = %target.label, port = local.port(), %remote_host, remote_port, "tunnel open");
        let stats: Arc<Stats> = Arc::default();
        let slot = Arc::new(tokio::sync::Mutex::new(Some(conn)));
        let forwards: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::default();
        let accept = {
            let (stats, slot, forwards) = (stats.clone(), slot.clone(), forwards.clone());
            let (target, rhost) = (target.clone(), remote_host.clone());
            tokio::spawn(async move {
                while let Ok((sock, _)) = listener.accept().await {
                    let (stats, slot, manager) = (stats.clone(), slot.clone(), manager.clone());
                    let (target, rhost) = (target.clone(), rhost.clone());
                    let h = tokio::spawn(async move {
                        forward(sock, &manager, &target, &slot, &rhost, remote_port, &stats).await;
                    });
                    let mut f = forwards.lock().unwrap_or_else(|p| p.into_inner());
                    f.retain(|h| !h.is_finished());
                    f.push(h);
                }
            })
        };
        Ok(Self {
            id,
            target,
            remote_host,
            remote_port,
            local,
            stats,
            conn: slot,
            accept,
            forwards,
        })
    }

    /// Tunnel id.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The local address to connect to.
    pub fn local(&self) -> SocketAddr {
        self.local
    }

    /// Host profile id.
    pub fn host_id(&self) -> &str {
        &self.target.id
    }

    /// `(host, port)` the tunnel reaches.
    pub fn remote(&self) -> (&str, u16) {
        (&self.remote_host, self.remote_port)
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
        self.accept.abort();
        for h in self
            .forwards
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain(..)
        {
            h.abort();
        }
        info!(port = self.local.port(), "tunnel stopped");
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
        } else if session_up {
            TunnelStatus::Active
        } else {
            TunnelStatus::Reconnecting
        };
        TunnelInfo {
            id: self.id,
            host_id: self.target.id.clone(),
            host: self.target.label.clone(),
            local_port: self.local.port(),
            remote: format!("{}:{}", self.remote_host, self.remote_port),
            status,
            bytes_up: self.stats.up.load(Ordering::Relaxed),
            bytes_down: self.stats.down.load(Ordering::Relaxed),
            connections: self.stats.active.load(Ordering::Relaxed),
        }
    }
}

async fn forward(
    sock: TcpStream,
    manager: &SshManager,
    target: &SshTarget,
    slot: &tokio::sync::Mutex<Option<Arc<SshConn>>>,
    remote_host: &str,
    remote_port: u16,
    stats: &Stats,
) {
    let _ = sock.set_nodelay(true);
    let channel = match session(manager, target, slot).await {
        Ok(conn) => conn.direct_tcpip(remote_host, remote_port).await,
        Err(e) => Err(e),
    };
    let channel = match channel {
        Ok(c) => {
            *stats.error.lock().unwrap_or_else(|p| p.into_inner()) = None;
            c
        }
        Err(e) => {
            warn!(error = %e, "tunnel could not reach its target");
            *stats.error.lock().unwrap_or_else(|p| p.into_inner()) = Some(e.to_string());
            return;
        }
    };
    stats.active.fetch_add(1, Ordering::Relaxed);
    let (sr, sw) = sock.into_split();
    let (cr, cw) = tokio::io::split(channel.into_stream());
    tokio::join!(pump(sr, cw, &stats.up), pump(cr, sw, &stats.down));
    stats.active.fetch_sub(1, Ordering::Relaxed);
    debug!("tunnel connection closed");
}
