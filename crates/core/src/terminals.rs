//! Open terminals: where each one's input goes. Output is parsed on the I/O side straight
//! into the [`Terminal`] the UI holds; the UI is only woken (coalesced) to redraw.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use std::time::Duration;

use switchyard_remote::ssh::{ChannelMsg, SshError, SshManager, SshTarget};
use switchyard_store::ProfileId;
use switchyard_term::{
    EventSink, Feeder, LocalShell, PtyInput, TermEvent, TermSize, Terminal, spawn_local,
};
use tokio::sync::mpsc;
use tracing::{info, warn};

use std::path::{Path, PathBuf};

use switchyard_term::SessionLog;

use crate::bus::{Event, TermId, TermLogState, TermStatus};
use crate::term_settings::{LogSettings, open_session_log};

/// Write a stopped log's last line and flush it.
fn finish_log(log: Option<SessionLog>) {
    if let Some(mut log) = log
        && let Err(e) = log.finish()
    {
        warn!(error = %e, "could not finish the terminal log");
    }
}

/// Reconnect attempts after an established SSH session drops.
const RECONNECT_ATTEMPTS: u32 = 5;
use crate::runtime::EventSender;

/// Input for a terminal's program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TermInput {
    /// Bytes (keys, pastes, replies to queries).
    Data(Vec<u8>),
    /// Window size changed.
    Resize(TermSize),
    /// Close the terminal.
    Close,
    /// Retry a dropped connection now.
    Reconnect,
}

/// Delivers input to a terminal's program (PTY writer thread or SSH channel task).
pub type InputFn = Arc<dyn Fn(TermInput) + Send + Sync>;

/// Registry of open terminals.
#[derive(Default)]
pub struct Terminals {
    inputs: Mutex<HashMap<TermId, InputFn>>,
    /// Each terminal's state and a name for its log files (Host name or "local").
    terms: Mutex<HashMap<TermId, (Terminal, String)>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The sink that turns terminal events into bus events and answers terminal queries.
pub fn event_sink(term: TermId, events: EventSender, input: InputFn) -> EventSink {
    Arc::new(move |ev| match ev {
        TermEvent::Wakeup => events.emit(Event::TerminalWake { term }),
        TermEvent::Reply(bytes) => input(TermInput::Data(bytes)),
        TermEvent::Title(title) => events.emit(Event::TerminalTitle { term, title }),
        TermEvent::ResetTitle => events.emit(Event::TerminalTitle {
            term,
            title: String::new(),
        }),
        TermEvent::Bell => events.emit(Event::TerminalBell { term }),
        TermEvent::Clipboard(text) => events.emit(Event::TerminalClipboard { term, text }),
        TermEvent::LogFailed(message) => events.emit(Event::TerminalLog {
            term,
            state: TermLogState::Failed(message),
        }),
    })
}

impl Terminals {
    /// Register a terminal's input.
    pub fn insert(&self, term: TermId, input: InputFn) {
        lock(&self.inputs).insert(term, input);
    }

    /// Send input; ignored for unknown (already closed) terminals.
    pub fn send(&self, term: TermId, msg: TermInput) {
        let input = lock(&self.inputs).get(&term).cloned();
        if let Some(f) = input {
            f(msg);
        }
    }

    /// Close and forget a terminal.
    pub fn close(&self, term: TermId) {
        if let Some(f) = lock(&self.inputs).remove(&term) {
            f(TermInput::Close);
        }
        self.forget(term);
    }

    /// Forget a terminal whose program ended.
    pub fn remove(&self, term: TermId) {
        lock(&self.inputs).remove(&term);
        self.forget(term);
    }

    /// Keep a terminal's state for logging; `name` names its log files.
    pub fn track(&self, term: TermId, terminal: &Terminal, name: &str) {
        lock(&self.terms).insert(term, (terminal.clone(), name.to_owned()));
    }

    /// Drop a closed terminal's state, finishing its log.
    fn forget(&self, term: TermId) {
        if let Some((t, _)) = lock(&self.terms).remove(&term) {
            finish_log(t.set_log(None));
        }
    }

    /// Start logging a terminal's output to a new file (blocking file I/O; called from
    /// a blocking task). Replaces a log already running.
    pub fn start_log(
        &self,
        term: TermId,
        settings: &LogSettings,
        default_dir: &Path,
    ) -> Result<PathBuf, String> {
        let (terminal, name) = lock(&self.terms)
            .get(&term)
            .cloned()
            .ok_or_else(|| "the terminal is closed".to_owned())?;
        let (log, path) = open_session_log(settings, &name, default_dir)
            .map_err(|e| format!("could not create the log file: {e}"))?;
        info!(term, path = %path.display(), "terminal log started");
        finish_log(terminal.set_log(Some(log)));
        Ok(path)
    }

    /// Stop logging a terminal. Returns whether it was logging.
    pub fn stop_log(&self, term: TermId) -> bool {
        let terminal = lock(&self.terms).get(&term).map(|(t, _)| t.clone());
        let old = terminal.and_then(|t| t.set_log(None));
        let was = old.is_some();
        finish_log(old);
        was
    }

    /// Start a local shell. Returns the terminal for the UI.
    pub fn open_local(
        self: &Arc<Self>,
        term: TermId,
        shell: LocalShell,
        size: TermSize,
        scrollback: usize,
        events: EventSender,
        guards: Vec<Box<dyn Send>>,
    ) -> switchyard_term::Result<Terminal> {
        let (tx, rx) = std::sync::mpsc::channel::<PtyInput>();
        let input: InputFn = Arc::new(move |msg| {
            let _ = tx.send(match msg {
                TermInput::Data(b) => PtyInput::Data(b),
                TermInput::Resize(s) => PtyInput::Resize(s),
                TermInput::Close => PtyInput::Close,
                TermInput::Reconnect => return,
            });
        });
        let sink = event_sink(term, events.clone(), input.clone());
        let (terminal, feeder) = switchyard_term::new_terminal(size, scrollback, sink);
        let registry = self.clone();
        self.track(term, &terminal, "local");
        let spawned = spawn_local(
            &shell,
            size,
            feeder,
            rx,
            Box::new(move |code| {
                // The program ended: release what it needed (an agent's run directory and
                // session token).
                drop(guards);
                registry.remove(term);
                events.emit(Event::TerminalExited {
                    term,
                    code,
                    message: None,
                });
            }),
        );
        if let Err(e) = spawned {
            self.forget(term);
            return Err(e);
        }
        self.insert(term, input);
        Ok(terminal)
    }
}

/// How one shell channel ended.
enum ShellEnd {
    /// The program exited (or the server closed the channel cleanly).
    Exited(Option<u32>),
    /// The connection dropped.
    Lost,
    /// The user closed the terminal.
    Closed,
}

async fn run_shell(
    conn: &switchyard_remote::ssh::SshConn,
    size: &mut TermSize,
    feeder: &mut Feeder,
    input: &mut mpsc::UnboundedReceiver<TermInput>,
    startup: &[u8],
    env: &[(String, String)],
) -> Result<ShellEnd, SshError> {
    let mut ch = conn.open_shell_with_env(size.cols, size.rows, env).await?;
    if !startup.is_empty() {
        // Typed like keystrokes; the shell reads them once its prompt is up.
        let _ = ch.data(startup).await;
    }
    let mut code = None;
    loop {
        tokio::select! {
            msg = ch.wait() => match msg {
                Some(ChannelMsg::Data { data }) => feeder.feed(&data),
                Some(ChannelMsg::ExtendedData { data, .. }) => feeder.feed(&data),
                Some(ChannelMsg::ExitStatus { exit_status }) => code = Some(exit_status),
                // OpenSSH sends EOF before the exit status; the channel is over at Close.
                Some(ChannelMsg::Close) | None => {
                    if code.is_none() && conn.is_closed() {
                        return Ok(ShellEnd::Lost);
                    }
                    return Ok(ShellEnd::Exited(code));
                }
                Some(_) => {}
            },
            msg = input.recv() => match msg {
                Some(TermInput::Data(bytes)) => {
                    if ch.data(&bytes[..]).await.is_err() {
                        return Ok(if conn.is_closed() { ShellEnd::Lost } else { ShellEnd::Exited(None) });
                    }
                }
                Some(TermInput::Resize(s)) => {
                    *size = s;
                    let _ = ch.window_change(u32::from(s.cols), u32::from(s.rows), 0, 0).await;
                }
                Some(TermInput::Reconnect) => {}
                Some(TermInput::Close) | None => {
                    let _ = ch.close().await;
                    return Ok(ShellEnd::Closed);
                }
            },
        }
    }
}

/// Everything an SSH terminal needs.
pub struct SshTerminalSpec {
    /// Terminal id.
    pub term: TermId,
    /// Host profile id.
    pub host_id: ProfileId,
    /// Where to connect.
    pub target: SshTarget,
    /// Initial size.
    pub size: TermSize,
    /// Typed into every new shell (each reconnect too): see [`shell_startup`].
    pub startup: Vec<u8>,
    /// Environment variables sent with every new shell.
    pub env: Vec<(String, String)>,
}

/// A folder as a POSIX shell word: single-quoted, with a leading `~` or `~/` left
/// outside the quotes so it still expands.
fn shell_dir(dir: &str) -> String {
    let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    match dir {
        "~" => "~".to_owned(),
        d if d.starts_with("~/") => format!("~/{}", quote(&d[2..])),
        d => quote(d),
    }
}

/// What to type into a new shell on a Host: `cd` to its start folder, its startup
/// command, then its connect macro.
pub fn shell_startup(
    start_directory: Option<&str>,
    startup_command: Option<&str>,
    connect_macro: Option<&[u8]>,
) -> Vec<u8> {
    let mut out = Vec::new();
    if let Some(dir) = start_directory.map(str::trim).filter(|d| !d.is_empty()) {
        out.extend_from_slice(format!("cd -- {}\r", shell_dir(dir)).as_bytes());
    }
    if let Some(cmd) = startup_command.map(str::trim).filter(|c| !c.is_empty()) {
        out.extend_from_slice(cmd.as_bytes());
        out.push(b'\r');
    }
    if let Some(m) = connect_macro {
        out.extend_from_slice(m);
    }
    out
}

impl Terminals {
    /// Open a shell on a Host. The terminal is returned right away; connecting, prompts
    /// and reconnects are reported as [`Event::TerminalStatus`].
    pub fn open_ssh(
        self: &Arc<Self>,
        spec: SshTerminalSpec,
        ssh: Arc<SshManager>,
        events: EventSender,
    ) -> Terminal {
        let SshTerminalSpec {
            term,
            host_id,
            target,
            mut size,
            startup,
            env,
        } = spec;
        let (tx, mut rx) = mpsc::unbounded_channel::<TermInput>();
        let input: InputFn = Arc::new(move |msg| {
            let _ = tx.send(msg);
        });
        let sink = event_sink(term, events.clone(), input.clone());
        let (terminal, mut feeder) =
            switchyard_term::new_terminal(size, switchyard_term::DEFAULT_SCROLLBACK, sink);
        self.insert(term, input);
        self.track(term, &terminal, &target.label);
        let registry = self.clone();
        tokio::spawn(async move {
            let mut attempt = 0u32;
            let mut ever_connected = false;
            let end_message = loop {
                events.emit(Event::TerminalStatus {
                    term,
                    status: TermStatus::Connecting,
                });
                let conn = match ssh.session(&target).await {
                    Ok(c) => c,
                    Err(SshError::HostKeyChanged {
                        host,
                        address,
                        stored,
                        received,
                        location,
                    }) => {
                        events.emit(Event::HostKeyChanged {
                            term: Some(term),
                            host_id: host_id.clone(),
                            host,
                            address,
                            stored,
                            received,
                            location,
                        });
                        break Some("Blocked: the host key has changed".to_owned());
                    }
                    // Only an established session that dropped is retried; a first
                    // connection that fails is reported as is.
                    Err(e @ SshError::Connect { .. }) if ever_connected => {
                        warn!(error = %e, "ssh reconnect failed");
                        match backoff(&mut attempt, term, &events, &mut rx).await {
                            Some(true) => continue,
                            Some(false) => break Some(format!("Connection lost: {e}")),
                            None => break None,
                        }
                    }
                    Err(e) => break Some(e.to_string()),
                };
                if ever_connected {
                    // Mark where the new session starts in the kept scrollback.
                    feeder.feed(b"\r\n\x1b[2m-- reconnected --\x1b[0m\r\n");
                }
                ever_connected = true;
                attempt = 0;
                events.emit(Event::TerminalStatus {
                    term,
                    status: TermStatus::Connected {
                        description: conn.description.clone(),
                    },
                });
                match run_shell(&conn, &mut size, &mut feeder, &mut rx, &startup, &env).await {
                    Ok(ShellEnd::Exited(code)) => {
                        registry.remove(term);
                        events.emit(Event::TerminalExited {
                            term,
                            code,
                            message: None,
                        });
                        return;
                    }
                    Ok(ShellEnd::Closed) => return,
                    Ok(ShellEnd::Lost) => {
                        info!(host = %target.label, "ssh connection lost");
                        drop(conn);
                        match backoff(&mut attempt, term, &events, &mut rx).await {
                            Some(true) => continue,
                            Some(false) => {
                                break Some(format!(
                                    "Connection lost; gave up after {RECONNECT_ATTEMPTS} attempts"
                                ));
                            }
                            None => break None,
                        }
                    }
                    Err(e) => break Some(e.to_string()),
                }
            };
            registry.remove(term);
            if let Some(message) = end_message {
                events.emit(Event::TerminalExited {
                    term,
                    code: None,
                    message: Some(message),
                });
            }
        });
        terminal
    }
}

/// Wait before the next reconnect. `Some(true)` = try again, `Some(false)` = give up,
/// `None` = the user closed the terminal.
async fn backoff(
    attempt: &mut u32,
    term: TermId,
    events: &EventSender,
    input: &mut mpsc::UnboundedReceiver<TermInput>,
) -> Option<bool> {
    *attempt += 1;
    if *attempt > RECONNECT_ATTEMPTS {
        return Some(false);
    }
    let secs = 1u64 << (*attempt - 1);
    events.emit(Event::TerminalStatus {
        term,
        status: TermStatus::Reconnecting {
            attempt: *attempt,
            of: RECONNECT_ATTEMPTS,
            in_secs: secs,
        },
    });
    let sleep = tokio::time::sleep(Duration::from_secs(secs));
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            () = &mut sleep => return Some(true),
            msg = input.recv() => match msg {
                Some(TermInput::Reconnect) => return Some(true),
                Some(TermInput::Close) | None => return None,
                // Typing while disconnected goes nowhere.
                Some(_) => {}
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_cds_runs_the_command_then_the_macro() {
        assert!(shell_startup(None, Some("  "), None).is_empty());
        assert_eq!(
            shell_startup(Some("/srv/my app"), Some("tmux attach"), Some(b"ls\r")),
            b"cd -- '/srv/my app'\rtmux attach\rls\r"
        );
        assert_eq!(
            shell_startup(Some("~/it's"), None, None),
            b"cd -- ~/'it'\\''s'\r"
        );
        assert_eq!(shell_startup(Some("~"), None, None), b"cd -- ~\r");
    }
}
