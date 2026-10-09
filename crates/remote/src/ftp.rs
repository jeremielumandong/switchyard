//! [`RemoteFs`] over FTP and FTPS (`suppaftp`): plain FTP, explicit TLS (`AUTH TLS`) and
//! implicit TLS, in passive or active mode.
//!
//! Browsing (list, stat, rename, delete, mkdir) shares one control connection, reopened when
//! the server drops it (idle timeouts). Every read or write stream opens its own connection,
//! so the transfer queue can run several transfers at once and each ends with its own
//! completion reply. Resume uses `REST` before `RETR` (download) and `APPE` (upload; `REST` +
//! `STOR` when the partial file is longer than the resume point).
//!
//! TLS verifies the server against the OS trust store (which honours `SSL_CERT_FILE`), plus
//! an optional per-connection PEM ([`FtpConfig::trusted_ca_pem`]), like database connections.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use std::time::{Duration, UNIX_EPOCH};

use futures::future::BoxFuture;
use rustls::pki_types::pem::PemObject as _;
use secrecy::{ExposeSecret as _, SecretString};
use suppaftp::list::File as ListFile;
use suppaftp::list::{ListParser, PosixPexQuery};
use suppaftp::tokio::{
    AsyncRustlsConnector, AsyncRustlsStream, ImplAsyncFtpStream, TransferStream,
};
use suppaftp::types::FileType;
use suppaftp::{FtpError, Mode, Status};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::fs::{EntryKind, FileEntry, FsError, FsReader, FsWriter, RemoteFs, sort_entries};
use crate::sftp::posix;

/// A control connection. Plain FTP uses the same type and simply never starts TLS.
type Ctl = ImplAsyncFtpStream<AsyncRustlsStream>;
type Transfer = TransferStream<AsyncRustlsStream>;

/// How long a finished transfer waits for the server to acknowledge `QUIT`.
const QUIT_TIMEOUT: Duration = Duration::from_secs(2);
/// How long active mode waits for the server to connect back.
const ACTIVE_TIMEOUT: Duration = Duration::from_secs(30);

/// TLS on an FTP connection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FtpSecurity {
    /// Plain FTP: credentials and data in clear text.
    None,
    /// Explicit TLS (FTPES): `AUTH TLS` on the normal port, data protected (`PROT P`).
    #[default]
    Explicit,
    /// Implicit TLS (FTPS, usually port 990): TLS from the first byte.
    Implicit,
}

/// Who opens the data connection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FtpDataMode {
    /// The client connects to the server (`PASV`, or `EPSV` over IPv6).
    #[default]
    Passive,
    /// The server connects back to the client (`PORT` / `EPRT`).
    Active,
}

/// Where and how to connect.
#[derive(Clone)]
pub struct FtpConfig {
    /// Server name or address; also the TLS server name.
    pub host: String,
    /// Control port (21, or 990 for implicit TLS).
    pub port: u16,
    /// User name.
    pub user: String,
    /// Password.
    pub password: SecretString,
    /// TLS mode.
    pub security: FtpSecurity,
    /// Data connection mode.
    pub mode: FtpDataMode,
    /// Extra certificate (PEM) trusted for this connection only: a company CA or a pinned
    /// self-signed server certificate. Verification stays on.
    pub trusted_ca_pem: Option<String>,
    /// Limit for connecting, TLS and login.
    pub connect_timeout: Duration,
}

impl FtpConfig {
    /// A config with the defaults: explicit TLS, passive mode, 15 s connect timeout.
    pub fn new(
        host: impl Into<String>,
        port: u16,
        user: impl Into<String>,
        password: SecretString,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            user: user.into(),
            password,
            security: FtpSecurity::default(),
            mode: FtpDataMode::default(),
            trusted_ca_pem: None,
            connect_timeout: Duration::from_secs(15),
        }
    }
}

impl std::fmt::Debug for FtpConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // No password, no certificate body.
        f.debug_struct("FtpConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("security", &self.security)
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

/// An FTP or FTPS file system.
pub struct FtpFs {
    cfg: FtpConfig,
    tls: Option<Arc<rustls::ClientConfig>>,
    label: String,
    home: PathBuf,
    /// The server lists with `MLSD` / `MLST` (RFC 3659).
    mlsx: bool,
    /// The browsing connection; `None` after it was lost.
    ctl: tokio::sync::Mutex<Option<Ctl>>,
}

impl std::fmt::Debug for FtpFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FtpFs")
            .field("label", &self.label)
            .field("security", &self.cfg.security)
            .finish()
    }
}

/// A rustls client config that verifies against the OS store plus `extra_pem`.
fn tls_config(extra_pem: Option<&str>) -> Result<rustls::ClientConfig, FsError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
    if let Some(pem) = extra_pem {
        let certs = rustls::pki_types::CertificateDer::pem_slice_iter(pem.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                FsError::Remote(format!("the trusted certificate is not valid PEM: {e}"))
            })?;
        if certs.is_empty() {
            return Err(FsError::Remote(
                "the trusted certificate file holds no certificate".into(),
            ));
        }
        roots.add_parsable_certificates(certs);
    }
    rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| FsError::Remote(e.to_string()))
        .map(|b| b.with_root_certificates(roots).with_no_client_auth())
}

/// The server's reply text, without trailing line breaks.
fn reply(r: &suppaftp::types::Response) -> String {
    let body = String::from_utf8_lossy(&r.body);
    let body = body.trim();
    if body.is_empty() {
        format!("server replied {}", r.status.code())
    } else if body.starts_with(&r.status.code().to_string()) {
        body.to_owned()
    } else {
        format!("{} {body}", r.status.code())
    }
}

fn err(e: FtpError) -> FsError {
    match e {
        FtpError::ConnectionError(io) => FsError::Io(io),
        FtpError::UnexpectedResponse(r) => FsError::Remote(reply(&r)),
        FtpError::SecureError(s) => FsError::Remote(format!("TLS: {s}")),
        other => FsError::Remote(other.to_string()),
    }
}

fn io_err(e: FtpError) -> std::io::Error {
    match e {
        FtpError::ConnectionError(io) => io,
        other => std::io::Error::other(err(other).to_string()),
    }
}

/// Whether the control connection is gone (closed, reset, or `421` idle timeout).
fn lost(e: &FtpError) -> bool {
    match e {
        FtpError::ConnectionError(_) | FtpError::SecureError(_) | FtpError::BadResponse => true,
        FtpError::UnexpectedResponse(r) => r.status == Status::NotAvailable,
        _ => false,
    }
}

fn mode_bits(f: &ListFile) -> Option<u32> {
    let mut m = 0;
    for (shift, who) in [
        (6, PosixPexQuery::Owner),
        (3, PosixPexQuery::Group),
        (0, PosixPexQuery::Others),
    ] {
        let bits = u32::from(f.can_read(who)) << 2
            | u32::from(f.can_write(who)) << 1
            | u32::from(f.can_execute(who));
        m |= bits << shift;
    }
    // MLSD and DOS listings carry no permissions.
    (f.uid().is_some() || m != 0).then_some(m)
}

fn entry_of(f: &ListFile, kind: EntryKind) -> FileEntry {
    FileEntry {
        name: f.name().to_owned(),
        size: if kind == EntryKind::Dir {
            0
        } else {
            f.size() as u64
        },
        kind,
        modified_ms: f
            .modified()
            .duration_since(UNIX_EPOCH)
            .ok()
            .filter(|d| !d.is_zero())
            .map(|d| d.as_millis() as i64),
        mode: mode_bits(f),
    }
}

/// Parse one listing line (`MLSD` facts, or `LIST` in POSIX or DOS form).
fn parse_line(line: &str, mlsx: bool) -> Option<ListFile> {
    if mlsx {
        // `cdir` / `pdir` facts name the directory itself and its parent.
        let facts = line
            .split(' ')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if facts.contains("type=cdir") || facts.contains("type=pdir") {
            return None;
        }
        ListParser::parse_mlsd(line).ok()
    } else {
        // `ls -l` starts with `total 12`; the parser would take it for a DOS line.
        if line.starts_with("total ") {
            return None;
        }
        ListFile::try_from(line).ok()
    }
}

impl FtpFs {
    /// Connect, log in and find the home directory. `label` names the connection in the UI.
    pub async fn connect(cfg: FtpConfig, label: impl Into<String>) -> Result<Self, FsError> {
        let tls = match cfg.security {
            FtpSecurity::None => None,
            _ => Some(Arc::new(tls_config(cfg.trusted_ca_pem.as_deref())?)),
        };
        let mut ctl = open(&cfg, tls.as_ref()).await?;
        let home = ctl.pwd().await.map_err(err)?;
        let mlsx = ctl
            .feat()
            .await
            .map(|f| f.keys().any(|k| k.eq_ignore_ascii_case("MLST")))
            .unwrap_or(false);
        Ok(Self {
            cfg,
            tls,
            label: label.into(),
            home: PathBuf::from(if home.is_empty() { "/".into() } else { home }),
            mlsx,
            ctl: tokio::sync::Mutex::new(Some(ctl)),
        })
    }

    /// Start browsing in `dir` instead of the login folder (a relative path is taken from
    /// the login folder).
    pub fn set_home(&mut self, dir: &str) {
        let dir = dir.trim();
        if dir.is_empty() {
            return;
        }
        self.home = if dir.starts_with('/') {
            PathBuf::from(dir)
        } else {
            crate::sftp::join(&self.home, dir)
        };
    }

    /// The TLS mode, for "Test connection".
    pub fn describe(&self) -> String {
        match self.cfg.security {
            FtpSecurity::None => "FTP".into(),
            FtpSecurity::Explicit => "FTPS · explicit TLS".into(),
            FtpSecurity::Implicit => "FTPS · implicit TLS".into(),
        }
    }

    /// Run `op` on the browsing connection, reconnecting once if the server dropped it.
    async fn run<T, F>(&self, op: F) -> Result<T, FsError>
    where
        F: for<'c> Fn(&'c mut Ctl) -> BoxFuture<'c, Result<T, FtpError>>,
    {
        let mut guard = self.ctl.lock().await;
        let mut retried = false;
        loop {
            if guard.is_none() {
                *guard = Some(open(&self.cfg, self.tls.as_ref()).await?);
            }
            let Some(ctl) = guard.as_mut() else {
                return Err(FsError::Remote("not connected".into()));
            };
            match op(ctl).await {
                Ok(v) => return Ok(v),
                Err(e) if lost(&e) => {
                    *guard = None;
                    if retried {
                        return Err(err(e));
                    }
                    retried = true;
                }
                Err(e) => return Err(err(e)),
            }
        }
    }

    /// A new connection for one transfer.
    async fn transfer_conn(&self) -> Result<Ctl, FsError> {
        open(&self.cfg, self.tls.as_ref()).await
    }

    /// Whether `path` is a directory (`CWD` succeeds).
    async fn is_dir(&self, path: &Path) -> Result<bool, FsError> {
        let p = posix(path);
        self.run(move |c| {
            let p = p.clone();
            Box::pin(async move {
                match c.cwd(&p).await {
                    Ok(()) => Ok(true),
                    Err(FtpError::UnexpectedResponse(_)) => Ok(false),
                    Err(e) => Err(e),
                }
            })
        })
        .await
    }
}

/// Open, secure, log in and switch to binary.
async fn open(cfg: &FtpConfig, tls: Option<&Arc<rustls::ClientConfig>>) -> Result<Ctl, FsError> {
    let connector = || {
        tls.map(|t| AsyncRustlsConnector::from(tokio_rustls::TlsConnector::from(t.clone())))
            .ok_or_else(|| FsError::Remote("TLS is not configured".into()))
    };
    let work = async {
        let addr: SocketAddr = tokio::net::lookup_host((cfg.host.as_str(), cfg.port))
            .await?
            .next()
            .ok_or_else(|| FsError::Remote(format!("{} did not resolve", cfg.host)))?;
        let mut ctl = match cfg.security {
            FtpSecurity::Implicit => {
                let mut ctl = Ctl::connect_secure_implicit(addr, connector()?, &cfg.host)
                    .await
                    .map_err(err)?;
                // Unlike `into_secure`, this does not protect the data connections, which
                // suppaftp still wraps in TLS: ask for that (RFC 4217).
                ctl.custom_command("PBSZ 0", &[Status::CommandOk])
                    .await
                    .map_err(err)?;
                ctl.custom_command("PROT P", &[Status::CommandOk])
                    .await
                    .map_err(err)?;
                ctl
            }
            FtpSecurity::None | FtpSecurity::Explicit => {
                let tcp = tokio::net::TcpStream::connect(addr).await?;
                let ctl = Ctl::connect_with_stream(tcp).await.map_err(err)?;
                if cfg.security == FtpSecurity::Explicit {
                    ctl.into_secure(connector()?, &cfg.host)
                        .await
                        .map_err(err)?
                } else {
                    ctl
                }
            }
        };
        ctl = match cfg.mode {
            FtpDataMode::Active => ctl.active_mode(ACTIVE_TIMEOUT),
            FtpDataMode::Passive => {
                if addr.is_ipv6() {
                    ctl.set_mode(Mode::ExtendedPassive);
                } else {
                    // Servers behind NAT announce a private address: use the one we reached.
                    ctl.set_mode(Mode::Passive);
                    ctl.set_passive_nat_workaround(true);
                }
                ctl
            }
        };
        ctl.login(cfg.user.as_str(), cfg.password.expose_secret())
            .await
            .map_err(err)?;
        ctl.transfer_type(FileType::Binary).await.map_err(err)?;
        Ok::<_, FsError>(ctl)
    };
    match tokio::time::timeout(cfg.connect_timeout, work).await {
        Ok(r) => r,
        Err(_) => Err(FsError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("connecting to {}:{} timed out", cfg.host, cfg.port),
        ))),
    }
}

impl RemoteFs for FtpFs {
    fn name(&self) -> &str {
        &self.label
    }

    fn home(&self) -> PathBuf {
        self.home.clone()
    }

    fn list<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<Vec<FileEntry>, FsError>> {
        Box::pin(async move {
            let p = posix(path);
            let mlsx = self.mlsx;
            let lines = self
                .run(move |c| {
                    let p = p.clone();
                    Box::pin(async move {
                        if mlsx {
                            return c.mlsd(Some(&p)).await;
                        }
                        // LIST arguments are not paths on every server: list the current
                        // directory. `-a` shows dot files; fall back where it is refused.
                        c.cwd(&p).await?;
                        match c.list(Some("-a")).await {
                            Ok(l) => Ok(l),
                            Err(FtpError::UnexpectedResponse(_)) => c.list(None).await,
                            Err(e) => Err(e),
                        }
                    })
                })
                .await?;
            let mut out = Vec::new();
            for line in lines {
                let Some(f) = parse_line(&line, mlsx) else {
                    continue;
                };
                if matches!(f.name(), "." | "..") || f.name().contains('/') {
                    continue;
                }
                let kind = if f.is_directory() {
                    EntryKind::Dir
                } else if f.is_symlink() {
                    let target = f
                        .symlink()
                        .map(|t| t.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    // Links to directories browse like directories.
                    if self.is_dir(&crate::sftp::join(path, f.name())).await? {
                        EntryKind::Dir
                    } else {
                        EntryKind::Symlink(target)
                    }
                } else {
                    EntryKind::File
                };
                out.push(entry_of(&f, kind));
            }
            sort_entries(&mut out);
            Ok(out)
        })
    }

    fn mkdir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<(), FsError>> {
        Box::pin(async move {
            let p = posix(path);
            self.run(move |c| {
                let p = p.clone();
                Box::pin(async move { c.mkdir(&p).await })
            })
            .await
        })
    }

    fn rename<'a>(&'a self, from: &'a Path, to: &'a Path) -> BoxFuture<'a, Result<(), FsError>> {
        Box::pin(async move {
            let (f, t) = (posix(from), posix(to));
            self.run(move |c| {
                let (f, t) = (f.clone(), t.clone());
                Box::pin(async move { c.rename(&f, &t).await })
            })
            .await
        })
    }

    fn delete<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<(), FsError>> {
        Box::pin(async move {
            let dir = self.is_dir(path).await?;
            let p = posix(path);
            self.run(move |c| {
                let p = p.clone();
                Box::pin(async move {
                    if dir {
                        // Leave the directory before removing it.
                        c.cwd("/").await?;
                        c.rmdir(&p).await
                    } else {
                        c.rm(&p).await
                    }
                })
            })
            .await
        })
    }

    fn stat<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<FileEntry, FsError>> {
        Box::pin(async move {
            let name = crate::fs::file_name(path);
            let p = posix(path);
            if self.mlsx {
                let q = p.clone();
                let line = self
                    .run(move |c| {
                        let q = q.clone();
                        Box::pin(async move { c.mlst(Some(&q)).await })
                    })
                    .await?;
                if let Ok(f) = ListParser::parse_mlst(&line) {
                    let kind = if f.is_directory() {
                        EntryKind::Dir
                    } else {
                        EntryKind::File
                    };
                    let mut e = entry_of(&f, kind);
                    e.name = name;
                    return Ok(e);
                }
            }
            // SIZE answers for files only; a directory is something we can CWD into.
            let q = p.clone();
            let file = self
                .run(move |c| {
                    let q = q.clone();
                    Box::pin(async move {
                        match c.size(&q).await {
                            Ok(size) => {
                                let modified = c.mdtm(&q).await.ok();
                                Ok(Some((size as u64, modified)))
                            }
                            Err(FtpError::UnexpectedResponse(_)) => Ok(None),
                            Err(e) => Err(e),
                        }
                    })
                })
                .await?;
            if let Some((size, modified)) = file {
                return Ok(FileEntry {
                    name,
                    kind: EntryKind::File,
                    size,
                    modified_ms: modified.map(|t| t.and_utc().timestamp_millis()),
                    mode: None,
                });
            }
            if self.is_dir(path).await? {
                return Ok(FileEntry {
                    name,
                    kind: EntryKind::Dir,
                    size: 0,
                    modified_ms: None,
                    mode: None,
                });
            }
            Err(FsError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{p}: no such file or directory"),
            )))
        })
    }

    fn open_read<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<FsReader, FsError>> {
        self.open_read_from(path, 0)
    }

    fn open_read_from<'a>(
        &'a self,
        path: &'a Path,
        offset: u64,
    ) -> BoxFuture<'a, Result<FsReader, FsError>> {
        Box::pin(async move {
            let mut ctl = self.transfer_conn().await?;
            if offset > 0 {
                ctl.resume_transfer(offset as usize).await.map_err(err)?;
            }
            let stream = ctl.retr_as_stream(posix(path)).await.map_err(err)?;
            Ok(Box::new(FtpStream::new(ctl, stream)) as FsReader)
        })
    }

    fn create<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<FsWriter, FsError>> {
        Box::pin(async move {
            let mut ctl = self.transfer_conn().await?;
            let stream = ctl.put_with_stream(posix(path)).await.map_err(err)?;
            Ok(Box::new(FtpStream::new(ctl, stream)) as FsWriter)
        })
    }

    fn open_write_from<'a>(
        &'a self,
        path: &'a Path,
        offset: u64,
    ) -> BoxFuture<'a, Result<FsWriter, FsError>> {
        Box::pin(async move {
            if offset == 0 {
                return self.create(path).await;
            }
            let p = posix(path);
            let mut ctl = self.transfer_conn().await?;
            let have = match ctl.size(&p).await {
                Ok(n) => n as u64,
                Err(FtpError::UnexpectedResponse(_)) => 0,
                Err(e) => return Err(err(e)),
            };
            let stream = if have == offset {
                ctl.append_with_stream(&p).await.map_err(err)?
            } else if have > offset {
                // Overwrite from the resume point. Servers keep any bytes past the end
                // of what is sent; the transfer queue only resumes at the partial's size.
                ctl.resume_transfer(offset as usize).await.map_err(err)?;
                ctl.put_with_stream(&p).await.map_err(err)?
            } else {
                return Err(FsError::Remote(format!(
                    "{p} holds {have} bytes, fewer than the resume point ({offset})"
                )));
            };
            Ok(Box::new(FtpStream::new(ctl, stream)) as FsWriter)
        })
    }
}

enum State {
    Open(Box<Ctl>, Transfer),
    Finishing(BoxFuture<'static, Result<(), FtpError>>),
    Done,
}

/// One transfer on its own connection: reads or writes the data connection, then (at end of
/// file, or on shutdown) closes it and checks the server's completion reply.
struct FtpStream {
    state: State,
}

impl FtpStream {
    fn new(ctl: Ctl, stream: Transfer) -> Self {
        Self {
            state: State::Open(Box::new(ctl), stream),
        }
    }

    /// Start (or keep) finishing; ready when the server confirmed the transfer.
    fn poll_finish(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        loop {
            match &mut self.state {
                State::Open(..) => {
                    let State::Open(mut ctl, stream) =
                        std::mem::replace(&mut self.state, State::Done)
                    else {
                        continue;
                    };
                    self.state = State::Finishing(Box::pin(async move {
                        stream.finish().await?;
                        // Best effort, briefly: the transfer already succeeded, and some
                        // servers are slow to answer QUIT after a TLS transfer.
                        let _ = tokio::time::timeout(QUIT_TIMEOUT, ctl.quit()).await;
                        Ok(())
                    }));
                }
                State::Finishing(f) => {
                    let r = ready!(f.as_mut().poll(cx));
                    self.state = State::Done;
                    return Poll::Ready(r.map_err(io_err));
                }
                State::Done => return Poll::Ready(Ok(())),
            }
        }
    }
}

fn closed() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the transfer is finished")
}

impl AsyncRead for FtpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        match &mut this.state {
            State::Open(_, stream) => {
                if buf.remaining() == 0 {
                    return Poll::Ready(Ok(()));
                }
                let before = buf.filled().len();
                ready!(Pin::new(stream).poll_read(cx, buf))?;
                if buf.filled().len() > before {
                    return Poll::Ready(Ok(()));
                }
                // End of file: the transfer only succeeded if the server says so.
                this.poll_finish(cx)
            }
            State::Finishing(_) => this.poll_finish(cx),
            State::Done => Poll::Ready(Ok(())),
        }
    }
}

impl AsyncWrite for FtpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match &mut self.get_mut().state {
            State::Open(_, stream) => Pin::new(stream).poll_write(cx, buf),
            _ => Poll::Ready(Err(closed())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut self.get_mut().state {
            State::Open(_, stream) => Pin::new(stream).poll_flush(cx),
            _ => Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.get_mut().poll_finish(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_list_and_mlsd_lines() {
        let f = parse_line(
            "-rw-r--r--    1 1000     1000           10 Oct 09 12:00 a.txt",
            false,
        )
        .unwrap();
        let e = entry_of(&f, EntryKind::File);
        assert_eq!(
            (e.name.as_str(), e.size, e.mode),
            ("a.txt", 10, Some(0o644))
        );
        assert!(e.modified_ms.is_some());
        let d = parse_line(
            "drwxr-xr-x    2 1000     1000         4096 Oct 09 12:00 b dir",
            false,
        )
        .unwrap();
        assert!(d.is_directory());
        assert_eq!(d.name(), "b dir");
        assert!(parse_line("total 8", false).is_none());
        let m = parse_line("type=file;size=3;modify=20261009120000; c.bin", true).unwrap();
        assert_eq!((m.name(), m.size()), ("c.bin", 3));
        assert!(parse_line("type=cdir;modify=20261009120000; /home/x", true).is_none());
    }

    #[test]
    fn config_debug_hides_the_password() {
        let cfg = FtpConfig::new("ftp.example", 21, "deploy", SecretString::from("hunter2"));
        let s = format!("{cfg:?}");
        assert!(!s.contains("hunter2") && !s.contains("deploy"), "{s}");
    }

    #[test]
    fn reply_text_keeps_the_code_once() {
        let r = suppaftp::types::Response::new(
            Status::FileUnavailable,
            b"550 Failed to open file.\r\n".to_vec(),
        );
        assert_eq!(reply(&r), "550 Failed to open file.");
        assert!(lost(&FtpError::UnexpectedResponse(
            suppaftp::types::Response::new(Status::NotAvailable, b"421 Timeout.".to_vec())
        )));
    }
}
