//! Handing work from `swy` to the running app (`swy explain --open`).
//!
//! The app listens on a loopback port and writes `<data>/handoff.json` (owner-only) with the
//! port and a random token. `swy` reads the file, connects, and sends one JSON line holding
//! the token and the request; the app answers `ok` or an error line. A stale file (the app
//! exited) shows as a refused connection: `swy` then says the app is not running.

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _};
use tracing::{info, warn};

use crate::bus::Event;
use crate::runtime::EventSender;

/// What `swy` asks the app to do.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Handoff {
    /// Show the plan stored with a history entry.
    OpenPlan {
        /// History entry.
        history_id: i64,
        /// Its statement (for a new tab when no SQL tab is open).
        sql: String,
    },
}

#[derive(Serialize, Deserialize)]
struct HandoffFile {
    port: u16,
    token: String,
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    token: String,
    #[serde(flatten)]
    request: Handoff,
}

/// The handoff file in the app's data directory.
pub fn handoff_file(data_dir: &Path) -> PathBuf {
    data_dir.join("handoff.json")
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
    }
    std::fs::rename(tmp, path)
}

/// Start listening (the app). Requests arrive as [`Event::Handoff`].
pub(crate) async fn serve(file: PathBuf, events: EventSender) {
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", 0)).await {
        Ok(l) => l,
        Err(e) => return warn!(error = %e, "handoff listener failed"),
    };
    let Ok(port) = listener.local_addr().map(|a| a.port()) else {
        return;
    };
    let token = switchyard_store::random::random_hex(32);
    let body = serde_json::json!({ "port": port, "token": token }).to_string();
    if let Err(e) = write_private(&file, body.as_bytes()) {
        return warn!(error = %e, "handoff file not written");
    }
    info!(port, "handoff listening");
    loop {
        let Ok((sock, _)) = listener.accept().await else {
            continue;
        };
        let token = token.clone();
        let events = events.clone();
        tokio::spawn(async move {
            let (read, mut write) = sock.into_split();
            let mut line = String::new();
            let mut reader = tokio::io::BufReader::new(read).take(64 * 1024);
            if reader.read_line(&mut line).await.is_err() {
                return;
            }
            let reply = match serde_json::from_str::<Envelope>(&line) {
                Ok(env) if constant_eq(env.token.as_bytes(), token.as_bytes()) => {
                    events.emit(Event::Handoff(env.request));
                    "ok\n".to_owned()
                }
                Ok(_) => "error: bad token\n".to_owned(),
                Err(e) => format!("error: {e}\n"),
            };
            let _ = write.write_all(reply.as_bytes()).await;
        });
    }
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Errors from [`send`].
#[derive(Debug, thiserror::Error)]
pub enum HandoffError {
    /// No running app answered.
    #[error("Switchyard is not running")]
    NotRunning,
    /// The app refused or the exchange failed.
    #[error("{0}")]
    Failed(String),
}

/// Send `request` to the running app (`swy`). Blocking; short timeouts.
pub fn send(data_dir: &Path, request: Handoff) -> Result<(), HandoffError> {
    let raw =
        std::fs::read_to_string(handoff_file(data_dir)).map_err(|_| HandoffError::NotRunning)?;
    let file: HandoffFile = serde_json::from_str(&raw).map_err(|_| HandoffError::NotRunning)?;
    let addr = SocketAddr::from(([127, 0, 0, 1], file.port));
    let mut sock = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .map_err(|_| HandoffError::NotRunning)?;
    let _ = sock.set_read_timeout(Some(Duration::from_secs(5)));
    let line = serde_json::to_string(&Envelope {
        token: file.token,
        request,
    })
    .map_err(|e| HandoffError::Failed(e.to_string()))?;
    sock.write_all(format!("{line}\n").as_bytes())
        .map_err(|e| HandoffError::Failed(e.to_string()))?;
    let mut reply = String::new();
    BufReader::new(sock)
        .read_line(&mut reply)
        .map_err(|e| HandoffError::Failed(e.to_string()))?;
    match reply.trim() {
        "ok" => Ok(()),
        other => Err(HandoffError::Failed(other.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt as _;

    #[tokio::test]
    async fn round_trip_and_bad_token() {
        let dir = tempfile::tempdir().expect("dir");
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let file = handoff_file(dir.path());
        tokio::spawn(serve(file.clone(), EventSender::new(tx)));
        for _ in 0..100 {
            if file.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let data = dir.path().to_owned();
        let req = Handoff::OpenPlan {
            history_id: 7,
            sql: "select 1".into(),
        };
        let r = req.clone();
        tokio::task::spawn_blocking(move || send(&data, r))
            .await
            .expect("join")
            .expect("sent");
        match rx.next().await {
            Some(Event::Handoff(got)) => assert_eq!(got, req),
            other => panic!("{other:?}"),
        }
        // A wrong token is refused.
        let raw = std::fs::read_to_string(&file).expect("file");
        let mut v: serde_json::Value = serde_json::from_str(&raw).expect("json");
        v["token"] = "nope".into();
        std::fs::write(&file, v.to_string()).expect("write");
        let data = dir.path().to_owned();
        let err = tokio::task::spawn_blocking(move || send(&data, req))
            .await
            .expect("join")
            .expect_err("refused");
        assert!(err.to_string().contains("bad token"), "{err}");
    }

    #[test]
    fn no_file_means_not_running() {
        let dir = tempfile::tempdir().expect("dir");
        assert!(matches!(
            send(
                dir.path(),
                Handoff::OpenPlan {
                    history_id: 1,
                    sql: String::new()
                }
            ),
            Err(HandoffError::NotRunning)
        ));
    }
}
