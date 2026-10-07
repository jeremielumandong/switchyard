//! X11 forwarding: programs on the server draw on this machine's X display.
//!
//! As OpenSSH does, the server gets a random *fake* MIT-MAGIC-COOKIE-1. Each forwarded
//! connection's setup packet must carry that cookie; it is replaced by the display's real
//! cookie (from `xauth list`) or by no authentication when there is none, and the rest of
//! the connection is copied as is. The real cookie never reaches the server.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use russh::client::{ChannelOpenHandle, Msg};
use russh::{Channel, ChannelOpenFailure};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::{debug, warn};

/// The only authentication protocol forwarded.
pub(crate) const MIT_COOKIE: &str = "MIT-MAGIC-COOKIE-1";

/// Where the local X server listens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Display {
    /// A Unix socket (`:0` → `/tmp/.X11-unix/X0`; XQuartz's launchd path as is).
    Unix(PathBuf),
    /// TCP (`host:N` → port 6000 + N; VcXsrv/X410 on Windows use `localhost:0`).
    Tcp(String, u16),
}

/// A parsed `DISPLAY`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DisplaySpec {
    pub display: Display,
    /// The display number (`:N`).
    pub number: u32,
    /// The screen (`.S`), sent to the server.
    pub screen: u32,
    /// The string `xauth` looks the cookie up by.
    pub name: String,
}

/// Parse `DISPLAY` (`:0`, `:0.1`, `unix:0`, `localhost:10.0`,
/// `/private/tmp/com.apple.launchd.x/org.xquartz:0`).
pub(crate) fn parse_display(s: &str) -> Option<DisplaySpec> {
    let s = s.trim();
    let colon = s.rfind(':')?;
    let (host, rest) = (&s[..colon], &s[colon + 1..]);
    let (num, screen) = match rest.split_once('.') {
        Some((n, sc)) => (n, sc.parse().ok()?),
        None => (rest, 0),
    };
    let number: u32 = num.parse().ok()?;
    let display = if host.starts_with('/') {
        // XQuartz: the socket file's name ends in ":<n>".
        Display::Unix(PathBuf::from(format!("{host}:{number}")))
    } else if host.is_empty() || host == "unix" {
        Display::Unix(PathBuf::from(format!("/tmp/.X11-unix/X{number}")))
    } else {
        Display::Tcp(host.to_owned(), u16::try_from(6000 + number).ok()?)
    };
    Some(DisplaySpec {
        display,
        number,
        screen,
        name: s.to_owned(),
    })
}

/// The display to forward to: the Host's override, else `DISPLAY`, else on Windows (no
/// `DISPLAY` by default) the first display VcXsrv, X410 and Xming use.
pub(crate) fn local_display(overridden: Option<&str>) -> Option<DisplaySpec> {
    if let Some(d) = overridden.map(str::trim).filter(|d| !d.is_empty()) {
        return parse_display(d);
    }
    match std::env::var("DISPLAY") {
        Ok(d) if !d.trim().is_empty() => parse_display(&d),
        _ if cfg!(windows) => parse_display("localhost:0.0"),
        _ => None,
    }
}

/// One session's X11 forwarding.
#[derive(Debug)]
pub(crate) struct X11Forward {
    spec: DisplaySpec,
    fake: [u8; 16],
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

impl X11Forward {
    /// Forwarding to `spec` with a new random fake cookie.
    pub(crate) fn new(spec: DisplaySpec) -> Result<Arc<Self>, String> {
        use ring::rand::SecureRandom as _;
        let mut fake = [0u8; 16];
        ring::rand::SystemRandom::new()
            .fill(&mut fake)
            .map_err(|_| "no random source for the X11 cookie".to_owned())?;
        Ok(Arc::new(Self { spec, fake }))
    }

    /// The fake cookie, hex, for `x11-req`.
    pub(crate) fn fake_cookie(&self) -> String {
        hex(&self.fake)
    }

    /// The screen number for `x11-req`.
    pub(crate) fn screen(&self) -> u32 {
        self.spec.screen
    }

    /// Serve an `x11` channel the server opened.
    pub(crate) fn open(self: &Arc<Self>, channel: Channel<Msg>, reply: ChannelOpenHandle) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let display = match this.connect().await {
                Ok(d) => d,
                Err(e) => {
                    warn!(error = %e, "X11 display not reachable");
                    reply.reject(ChannelOpenFailure::ConnectFailed).await;
                    return;
                }
            };
            reply.accept().await;
            let real = real_cookie(&this.spec.name).await;
            let mut stream = channel.into_stream();
            let mut display = display;
            match rewrite_setup(&mut stream, &this.fake, real.as_deref()).await {
                Ok(setup) => {
                    if display.write_all(&setup).await.is_err() {
                        return;
                    }
                    let _ = tokio::io::copy_bidirectional(&mut stream, &mut display).await;
                }
                Err(e) => debug!(error = %e, "X11 connection refused"),
            }
        });
    }

    async fn connect(&self) -> Result<Box<dyn Duplex>, String> {
        let fut = async {
            match &self.spec.display {
                #[cfg(unix)]
                Display::Unix(p) => tokio::net::UnixStream::connect(p)
                    .await
                    .map(|s| Box::new(s) as Box<dyn Duplex>)
                    .map_err(|e| format!("{}: {e}", p.display())),
                #[cfg(not(unix))]
                Display::Unix(p) => {
                    Err(format!("{}: Unix sockets need a Unix system", p.display()))
                }
                Display::Tcp(h, port) => tokio::net::TcpStream::connect((h.as_str(), *port))
                    .await
                    .map(|s| {
                        let _ = s.set_nodelay(true);
                        Box::new(s) as Box<dyn Duplex>
                    })
                    .map_err(|e| format!("{h}:{port}: {e}")),
            }
        };
        tokio::time::timeout(Duration::from_secs(10), fut)
            .await
            .map_err(|_| "timed out".to_owned())?
    }
}

/// A byte stream both ways.
pub(crate) trait Duplex: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Duplex for T {}

/// The display's real MIT cookie from `xauth list <display>`; `None` without one (the X
/// server may then allow the connection by other means, e.g. XQuartz or `xhost`).
async fn real_cookie(display: &str) -> Option<Vec<u8>> {
    let display = display.to_owned();
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new("xauth")
            .args(["list", &display])
            .stderr(std::process::Stdio::null())
            .output()
    })
    .await
    .ok()?
    .ok()?;
    parse_xauth(&String::from_utf8_lossy(&out.stdout))
}

/// The first MIT cookie in `xauth list` output (`host/unix:0  MIT-MAGIC-COOKIE-1  <hex>`).
pub(crate) fn parse_xauth(text: &str) -> Option<Vec<u8>> {
    text.lines().find_map(|l| {
        let mut parts = l.split_whitespace();
        let (_, proto, data) = (parts.next()?, parts.next()?, parts.next()?);
        (proto == MIT_COOKIE).then(|| unhex(data)).flatten()
    })
}

fn pad4(n: usize) -> usize {
    (4 - n % 4) % 4
}

/// Read a client's connection setup, check its cookie against `fake`, and return the setup
/// rewritten with `real` (or with no authentication).
pub(crate) async fn rewrite_setup<S: AsyncRead + Unpin>(
    s: &mut S,
    fake: &[u8],
    real: Option<&[u8]>,
) -> std::io::Result<Vec<u8>> {
    let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::PermissionDenied, m.to_owned());
    let mut head = [0u8; 12];
    s.read_exact(&mut head).await?;
    let big = match head[0] {
        b'B' => true,
        b'l' => false,
        _ => return Err(bad("not an X11 connection")),
    };
    let u16_at = |i: usize| {
        let b = [head[i], head[i + 1]];
        usize::from(if big {
            u16::from_be_bytes(b)
        } else {
            u16::from_le_bytes(b)
        })
    };
    let (name_len, data_len) = (u16_at(6), u16_at(8));
    let mut name = vec![0u8; name_len + pad4(name_len)];
    s.read_exact(&mut name).await?;
    let mut data = vec![0u8; data_len + pad4(data_len)];
    s.read_exact(&mut data).await?;
    if &name[..name_len] != MIT_COOKIE.as_bytes() || data[..data_len] != *fake {
        return Err(bad("wrong X11 authentication cookie"));
    }
    let (new_name, new_data): (&[u8], &[u8]) = match real {
        Some(c) => (MIT_COOKIE.as_bytes(), c),
        None => (b"", b""),
    };
    let put = |n: usize| {
        let n = u16::try_from(n).unwrap_or(0);
        if big {
            n.to_be_bytes()
        } else {
            n.to_le_bytes()
        }
    };
    let mut out = Vec::with_capacity(12 + 16 + 20);
    out.extend_from_slice(&head[..6]);
    out.extend_from_slice(&put(new_name.len()));
    out.extend_from_slice(&put(new_data.len()));
    out.extend_from_slice(&head[10..12]);
    out.extend_from_slice(new_name);
    out.resize(out.len() + pad4(new_name.len()), 0);
    out.extend_from_slice(new_data);
    out.resize(out.len() + pad4(new_data.len()), 0);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displays() {
        let d = parse_display(":0").unwrap();
        assert_eq!(d.display, Display::Unix("/tmp/.X11-unix/X0".into()));
        assert_eq!((d.number, d.screen), (0, 0));
        let d = parse_display("unix:3.1").unwrap();
        assert_eq!(d.display, Display::Unix("/tmp/.X11-unix/X3".into()));
        assert_eq!(d.screen, 1);
        let d = parse_display("localhost:10.0").unwrap();
        assert_eq!(d.display, Display::Tcp("localhost".into(), 6010));
        let d = parse_display("/private/tmp/com.apple.launchd.ab/org.xquartz:0").unwrap();
        assert_eq!(
            d.display,
            Display::Unix("/private/tmp/com.apple.launchd.ab/org.xquartz:0".into())
        );
        assert!(parse_display("nonsense").is_none());
        assert!(parse_display(":x").is_none());
    }

    #[test]
    fn xauth_output() {
        let out = "box/unix:0  MIT-MAGIC-COOKIE-1  00ff10\nbox/unix:0  XDM-AUTHORIZATION-1  aa\n";
        assert_eq!(parse_xauth(out), Some(vec![0, 255, 16]));
        assert_eq!(parse_xauth("box/unix:0  XDM-AUTHORIZATION-1  aa"), None);
        assert_eq!(parse_xauth(""), None);
    }

    fn setup(big: bool, name: &[u8], data: &[u8]) -> Vec<u8> {
        let n = |v: usize| {
            let v = v as u16;
            if big {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        };
        let mut p = vec![if big { b'B' } else { b'l' }, 0];
        p.extend_from_slice(&n(11));
        p.extend_from_slice(&n(0));
        p.extend_from_slice(&n(name.len()));
        p.extend_from_slice(&n(data.len()));
        p.extend_from_slice(&[0, 0]);
        p.extend_from_slice(name);
        p.resize(p.len() + pad4(name.len()), 0);
        p.extend_from_slice(data);
        p.resize(p.len() + pad4(data.len()), 0);
        p
    }

    #[tokio::test]
    async fn cookie_is_swapped_or_removed() {
        let fake = [7u8; 16];
        let real = [9u8; 16];
        for big in [true, false] {
            let pkt = setup(big, MIT_COOKIE.as_bytes(), &fake);
            let out = rewrite_setup(&mut &pkt[..], &fake, Some(&real))
                .await
                .unwrap();
            assert_eq!(out, setup(big, MIT_COOKIE.as_bytes(), &real));
            let out = rewrite_setup(&mut &pkt[..], &fake, None).await.unwrap();
            assert_eq!(out, setup(big, b"", b""));
        }
    }

    #[tokio::test]
    async fn wrong_or_missing_cookie_is_refused() {
        let fake = [7u8; 16];
        let pkt = setup(false, MIT_COOKIE.as_bytes(), &[8u8; 16]);
        assert!(rewrite_setup(&mut &pkt[..], &fake, None).await.is_err());
        let pkt = setup(false, b"", b"");
        assert!(rewrite_setup(&mut &pkt[..], &fake, None).await.is_err());
        assert!(
            rewrite_setup(&mut &b"GET / HTTP/1.1\r\n"[..], &fake, None)
                .await
                .is_err()
        );
    }
}
