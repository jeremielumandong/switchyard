//! Handing work from `swy` to the running app (`swy explain --open`).
//!
//! The app listens on a loopback port and writes `<data>/handoff.json` (owner-only) with the
//! port and a random token. `swy` reads the file, connects, and sends one JSON line holding
//! the token and the request; the app answers `ok` or an error line. A stale file (the app
//! exited) shows as a refused connection: `swy` then says the app is not running.
//!
//! `swy mcp` also asks the app to run a coding agent's shell command on a Host
//! ([`ask_agent_command`]): the app checks the run's session token, shows the command for
//! approval, runs it over the Host's SSH session and answers with one JSON line. Closing
//! the connection (the agent's run ended) withdraws the request.

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::future::BoxFuture;
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

/// A coding agent's shell command on a Host, sent by `swy mcp`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCommand {
    /// The run's session token (see [`crate::agent_run`]); proves the app started the run.
    pub session_token: String,
    /// Host name, as the agent was shown it.
    pub host: String,
    /// The shell command.
    pub command: String,
    /// Stop the command after this many seconds.
    pub timeout_secs: u64,
}

impl std::fmt::Debug for AgentCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentCommand")
            .field("host", &self.host)
            .field("command", &self.command)
            .field("timeout_secs", &self.timeout_secs)
            .finish_non_exhaustive()
    }
}

/// What an approved command printed and returned.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCommandOutput {
    /// Exit status (`None`: killed or timed out).
    pub exit_status: Option<u32>,
    /// Standard output (lossy UTF-8).
    pub stdout: String,
    /// Standard error (lossy UTF-8).
    pub stderr: String,
    /// Output past the cap was dropped.
    pub truncated: bool,
    /// Stopped at the timeout.
    pub timed_out: bool,
}

/// Answers [`AgentCommand`]s in the app (approval, then the command).
pub type AgentResponder = std::sync::Arc<
    dyn Fn(AgentCommand) -> BoxFuture<'static, Result<AgentCommandOutput, String>> + Send + Sync,
>;

#[derive(Serialize, Deserialize)]
struct AgentEnvelope {
    token: String,
    agent_command: AgentCommand,
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
pub(crate) async fn serve(file: PathBuf, events: EventSender, agent: Option<AgentResponder>) {
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
        let agent = agent.clone();
        tokio::spawn(async move {
            let (read, mut write) = sock.into_split();
            let mut line = String::new();
            let mut reader = tokio::io::BufReader::new(read).take(64 * 1024);
            if reader.read_line(&mut line).await.is_err() {
                return;
            }
            if let Ok(env) = serde_json::from_str::<AgentEnvelope>(&line) {
                let result = if !constant_eq(env.token.as_bytes(), token.as_bytes()) {
                    Err("bad token".to_owned())
                } else if let Some(agent) = agent {
                    let mut probe = [0u8; 1];
                    tokio::select! {
                        r = agent(env.agent_command) => r,
                        // `swy` hung up (its run ended): drop the request.
                        _ = reader.read(&mut probe) => return,
                    }
                } else {
                    Err("this Switchyard does not run agent commands".to_owned())
                };
                let mut reply = serde_json::to_string(&result).unwrap_or_default();
                reply.push('\n');
                let _ = write.write_all(reply.as_bytes()).await;
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

/// Ask the running app to run a coding agent's command on a Host (`swy mcp`). Waits up to
/// `wait` for the user's approval and the command. Blocking.
pub fn ask_agent_command(
    data_dir: &Path,
    request: AgentCommand,
    wait: Duration,
) -> Result<AgentCommandOutput, HandoffError> {
    let raw =
        std::fs::read_to_string(handoff_file(data_dir)).map_err(|_| HandoffError::NotRunning)?;
    let file: HandoffFile = serde_json::from_str(&raw).map_err(|_| HandoffError::NotRunning)?;
    let addr = SocketAddr::from(([127, 0, 0, 1], file.port));
    let mut sock = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .map_err(|_| HandoffError::NotRunning)?;
    let _ = sock.set_read_timeout(Some(wait));
    let line = serde_json::to_string(&AgentEnvelope {
        token: file.token,
        agent_command: request,
    })
    .map_err(|e| HandoffError::Failed(e.to_string()))?;
    sock.write_all(format!("{line}\n").as_bytes())
        .map_err(|e| HandoffError::Failed(e.to_string()))?;
    let mut reply = String::new();
    BufReader::new(sock).read_line(&mut reply).map_err(|e| {
        HandoffError::Failed(match e.kind() {
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                "no answer from Switchyard in time".to_owned()
            }
            _ => e.to_string(),
        })
    })?;
    let result: Result<AgentCommandOutput, String> = serde_json::from_str(reply.trim())
        .map_err(|_| HandoffError::Failed("Switchyard closed the request".to_owned()))?;
    result.map_err(HandoffError::Failed)
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
        tokio::spawn(serve(file.clone(), EventSender::new(tx), None));
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

    #[tokio::test]
    async fn agent_commands_reach_the_responder() {
        let dir = tempfile::tempdir().expect("dir");
        let (tx, _rx) = futures::channel::mpsc::unbounded();
        let file = handoff_file(dir.path());
        let responder: AgentResponder = std::sync::Arc::new(|req: AgentCommand| {
            Box::pin(async move {
                if req.command == "deny" {
                    return Err("declined".to_owned());
                }
                Ok(AgentCommandOutput {
                    exit_status: Some(0),
                    stdout: format!("{} on {}", req.command, req.host),
                    ..Default::default()
                })
            })
        });
        tokio::spawn(serve(file.clone(), EventSender::new(tx), Some(responder)));
        for _ in 0..100 {
            if file.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let ask = |command: &str| {
            let data = dir.path().to_owned();
            let req = AgentCommand {
                session_token: "t".into(),
                host: "web".into(),
                command: command.into(),
                timeout_secs: 5,
            };
            tokio::task::spawn_blocking(move || {
                ask_agent_command(&data, req, Duration::from_secs(5))
            })
        };
        let out = ask("uptime").await.expect("join").expect("ran");
        assert_eq!(out.stdout, "uptime on web");
        let err = ask("deny").await.expect("join").expect_err("refused");
        assert_eq!(err.to_string(), "declined");
        // The session token never shows in logs.
        let req = AgentCommand {
            session_token: "secret-token".into(),
            host: "web".into(),
            command: "ls".into(),
            timeout_secs: 1,
        };
        assert!(!format!("{req:?}").contains("secret-token"));
    }
}
