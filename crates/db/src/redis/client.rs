//! One connection to a Redis server: TCP or TLS (rustls, verification always on), optionally
//! through an SSH tunnel, with `AUTH` and `SELECT` done at connect.

use std::sync::Arc;
use std::time::Duration;

use secrecy::ExposeSecret as _;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tracing::debug;

use super::resp::{self, Reply};
use crate::driver::{DbConfig, SslMode, TunnelEndpoint};
use crate::error::{DbError, Result};

trait Io: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}

/// How long a single command may take before the connection is dropped.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// A connected, authenticated client on one logical database.
pub struct RedisClient {
    io: Box<dyn Io>,
    buf: Vec<u8>,
    /// A command timed out or the stream failed mid-reply: the next reply can't be matched
    /// to its request, so the client refuses further commands.
    broken: bool,
    version: String,
    db: u32,
    read_only: bool,
    timeout: Duration,
}

/// The logical database a profile's Database field names (empty means 0).
pub fn database_index(database: &str) -> Result<u32> {
    let d = database.trim();
    if d.is_empty() {
        return Ok(0);
    }
    d.parse()
        .map_err(|_| DbError::Param("Database must be a number (0, 1, 2…)".into()))
}

impl RedisClient {
    /// Connect, authenticate and select the database from `cfg`. `cfg.database` is the
    /// logical database index; `cfg.user` an ACL user (empty for the `default` user).
    pub async fn connect(cfg: &DbConfig, tunnel: Option<&TunnelEndpoint>) -> Result<Self> {
        let db = database_index(&cfg.database)?;
        let (host, port) = match tunnel {
            Some(t) => (t.host.as_str(), t.port),
            None => (cfg.host.as_str(), cfg.port),
        };
        let tcp = tokio::time::timeout(cfg.connect_timeout, TcpStream::connect((host, port)))
            .await
            .map_err(|_| DbError::Connect(format!("timed out connecting to port {port}")))?
            .map_err(|e| DbError::Connect(e.to_string()))?;
        let _ = tcp.set_nodelay(true);
        // Redis has no STARTTLS: a server either speaks TLS on its port or it doesn't, so
        // Prefer can't fall back safely and means plain TCP.
        let io: Box<dyn Io> = match cfg.ssl_mode {
            SslMode::Require | SslMode::VerifyFull => {
                let tls = crate::tls::client_config_with(cfg.trusted_ca_pem.as_deref())?;
                // The certificate names the real server, also through a tunnel.
                let name = rustls::pki_types::ServerName::try_from(cfg.host.trim().to_owned())
                    .map_err(|e| DbError::Tls(format!("server name: {e}")))?;
                let stream = tokio::time::timeout(
                    cfg.connect_timeout,
                    tokio_rustls::TlsConnector::from(Arc::new(tls)).connect(name, tcp),
                )
                .await
                .map_err(|_| DbError::Tls("timed out during the TLS handshake".into()))?
                .map_err(|e| DbError::Tls(e.to_string()))?;
                Box::new(stream)
            }
            SslMode::Disable | SslMode::Prefer => Box::new(tcp),
        };
        let mut c = Self {
            io,
            buf: Vec::with_capacity(8 * 1024),
            broken: false,
            version: String::new(),
            db,
            read_only: cfg.read_only,
            timeout: COMMAND_TIMEOUT,
        };
        if let Some(pw) = &cfg.password {
            let pw = pw.expose_secret();
            let user = cfg.user.trim();
            let r = if user.is_empty() {
                c.call(&[b"AUTH".as_slice(), pw.as_bytes()]).await?
            } else {
                c.call(&[b"AUTH".as_slice(), user.as_bytes(), pw.as_bytes()])
                    .await?
            };
            if let Reply::Error(e) = r {
                // The server's text never contains the password.
                return Err(DbError::Connect(format!("authentication failed: {e}")));
            }
        }
        if db != 0 {
            c.call(&["SELECT", &db.to_string()]).await?.ok()?;
        }
        // Shows up in CLIENT LIST; older servers or restricted ACLs may refuse, which is fine.
        let _ = c.call(&["CLIENT", "SETNAME", "switchyard"]).await?;
        let info = c.call(&["INFO", "server"]).await?;
        c.version = match &info {
            Reply::Error(e) if e.starts_with("NOAUTH") => {
                return Err(DbError::Connect(
                    "the server needs a password (NOAUTH)".into(),
                ));
            }
            other => other
                .bytes()
                .and_then(|b| info_field(b, "redis_version"))
                .map_or_else(|| "Redis".to_owned(), |v| format!("Redis {v}")),
        };
        debug!(version = %c.version, db, "redis connected");
        Ok(c)
    }

    /// Server version text (`Redis 7.2.4`).
    pub fn server_version(&self) -> &str {
        &self.version
    }

    /// The selected logical database.
    pub fn db(&self) -> u32 {
        self.db
    }

    /// A command timed out or the stream failed: reconnect before the next command.
    pub fn is_broken(&self) -> bool {
        self.broken
    }

    /// The connection is locked read-only.
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// Send one command and read its reply. A server error comes back as
    /// [`Reply::Error`]; use [`Reply::ok`] to turn it into an error.
    pub async fn call<A: AsRef<[u8]>>(&mut self, args: &[A]) -> Result<Reply> {
        let mut v = self.pipeline(&[args]).await?;
        v.pop().ok_or_else(|| DbError::Protocol("no reply".into()))
    }

    /// Send several commands at once and read their replies in order.
    pub async fn pipeline<A: AsRef<[u8]>>(&mut self, cmds: &[&[A]]) -> Result<Vec<Reply>> {
        if self.broken {
            return Err(DbError::Closed);
        }
        let mut out = Vec::new();
        for c in cmds {
            out.extend_from_slice(&resp::encode(c));
        }
        let n = cmds.len();
        let timeout = self.timeout;
        let r = tokio::time::timeout(timeout, async {
            self.io.write_all(&out).await.map_err(io_err)?;
            self.io.flush().await.map_err(io_err)?;
            let mut replies = Vec::with_capacity(n);
            while replies.len() < n {
                replies.push(self.read_reply().await?);
            }
            Ok::<_, DbError>(replies)
        })
        .await;
        match r {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => {
                self.broken = true;
                Err(e)
            }
            Err(_) => {
                self.broken = true;
                Err(DbError::Connect(format!(
                    "no reply within {} s; reconnect to continue",
                    timeout.as_secs()
                )))
            }
        }
    }

    async fn read_reply(&mut self) -> Result<Reply> {
        loop {
            if let Some((r, used)) = resp::decode(&self.buf)? {
                self.buf.drain(..used);
                return Ok(r);
            }
            let mut chunk = [0u8; 16 * 1024];
            let n = self.io.read(&mut chunk).await.map_err(io_err)?;
            if n == 0 {
                return Err(DbError::Closed);
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

fn io_err(e: std::io::Error) -> DbError {
    DbError::Connect(e.to_string())
}

/// `field:value` from an `INFO` reply.
pub fn info_field(info: &[u8], field: &str) -> Option<String> {
    String::from_utf8_lossy(info).lines().find_map(|l| {
        l.strip_prefix(field)
            .and_then(|r| r.strip_prefix(':'))
            .map(|v| v.trim().to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_index_parses() {
        assert_eq!(database_index("").unwrap(), 0);
        assert_eq!(database_index(" 3 ").unwrap(), 3);
        assert!(database_index("cache").is_err());
        assert!(database_index("-1").is_err());
    }

    #[test]
    fn reads_info_fields() {
        let info = b"# Server\r\nredis_version:7.2.4\r\nredis_mode:standalone\r\n";
        assert_eq!(info_field(info, "redis_version").as_deref(), Some("7.2.4"));
        assert_eq!(
            info_field(info, "redis_mode").as_deref(),
            Some("standalone")
        );
        assert_eq!(info_field(info, "missing"), None);
    }
}
