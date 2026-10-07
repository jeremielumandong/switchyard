//! The shared runner: one CLI process per run.
//!
//! A run gets a private empty temp directory, the adapter's config inside it, and the CLI
//! started in its own process group with the prompt on stdin. Output is parsed on reader
//! threads into [`AgentEvent`]s on a channel, so nothing blocks the caller. When the
//! process ends, the directory is removed and the request's guards (the `swy mcp` session
//! token) are dropped before the final [`AgentEvent::Exited`].

use std::collections::VecDeque;
use std::io::{BufRead as _, BufReader, Read, Write as _};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::adapter::{AgentAdapter, McpServer, RunContext};
use crate::process::{self, ProcessGroup};
use crate::workdir::{self, WorkDir};
use crate::{AgentError, AgentEvent, AgentKind, SYSTEM_PROMPT};

/// Stderr lines kept to explain a failed run.
const STDERR_TAIL: usize = 12;

/// One run's inputs.
pub struct RunRequest {
    /// The CLI's executable; `None` looks the adapter's program up on PATH.
    pub program: Option<PathBuf>,
    /// The user's request.
    pub prompt: String,
    /// Conversation to continue (from [`crate::RunSummary::session_id`]).
    pub resume: Option<String>,
    /// Model override.
    pub model: Option<String>,
    /// Switchyard's MCP server, with its session token.
    pub mcp: McpServer,
    /// Parent of the run's temp directory; `None` is the system temp directory.
    pub temp_root: Option<PathBuf>,
    /// Dropped when the process has ended (before [`AgentEvent::Exited`]): the session
    /// token's revocation, for one.
    pub guards: Vec<Box<dyn Send>>,
}

/// Cancels a run from anywhere.
#[derive(Clone, Debug)]
pub struct CancelHandle {
    group: Arc<ProcessGroup>,
    cancelled: Arc<AtomicBool>,
}

impl CancelHandle {
    /// Kill the CLI and its process tree. The run still ends with `Error` and `Exited`.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.group.kill();
    }
}

/// A started run. Dropping it cancels the run.
pub struct AgentRun {
    kind: AgentKind,
    events: mpsc::UnboundedReceiver<AgentEvent>,
    cancel: CancelHandle,
}

impl AgentRun {
    /// Which CLI is running.
    pub fn kind(&self) -> AgentKind {
        self.kind
    }

    /// The next event; `None` after [`AgentEvent::Exited`].
    pub async fn next(&mut self) -> Option<AgentEvent> {
        self.events.recv().await
    }

    /// The next event, blocking this thread (not for async code).
    pub fn blocking_next(&mut self) -> Option<AgentEvent> {
        self.events.blocking_recv()
    }

    /// A handle that cancels this run.
    pub fn cancel_handle(&self) -> CancelHandle {
        self.cancel.clone()
    }

    /// Kill the CLI and its process tree.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

impl Drop for AgentRun {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Read `r` line by line (lossy UTF-8, so one bad byte does not stop the stream).
fn for_each_line(r: impl Read, mut f: impl FnMut(&str)) {
    let mut r = BufReader::new(r);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match r.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                let line = String::from_utf8_lossy(&buf);
                f(line.trim_end_matches(['\n', '\r']));
            }
        }
    }
}

fn spawn_thread(name: &str, f: impl FnOnce() + Send + 'static) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(f)
        .map(drop)
}

/// Start a run.
pub fn start(adapter: Arc<dyn AgentAdapter>, req: RunRequest) -> Result<AgentRun, AgentError> {
    let kind = adapter.kind();
    let program = process::find_program(adapter.program(), req.program.as_deref())
        .ok_or_else(|| AgentError::NotFound(kind.display_name().into()))?;
    let root = req.temp_root.clone().unwrap_or_else(workdir::default_root);
    let workdir = WorkDir::create(&root)?;
    let invocation = adapter.prepare(&RunContext {
        workdir: workdir.path(),
        prompt: &req.prompt,
        resume: req.resume.as_deref(),
        model: req.model.as_deref(),
        mcp: &req.mcp,
        system_prompt: SYSTEM_PROMPT,
    })?;

    let mut cmd = process::command(&program);
    cmd.args(&invocation.args)
        .current_dir(workdir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("PATH", process::child_path())
        .env("NO_COLOR", "1")
        .env("FORCE_COLOR", "0");
    for (k, v) in &invocation.env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn()?;
    let group = ProcessGroup::new(child.id());
    let cancel = CancelHandle {
        group: Arc::clone(&group),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    tracing::info!(agent = kind.id(), pid = child.id(), "agent run started");

    let (Some(mut stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        cancel.cancel();
        return Err(AgentError::Io(std::io::Error::other("no pipes to the CLI")));
    };
    let input = invocation.stdin.unwrap_or_default();
    // Its own thread: a large prompt must not block on a CLI that reads slowly. Dropping
    // stdin afterwards tells the CLI no more input is coming.
    spawn_thread("agent-stdin", move || {
        let _ = stdin.write_all(input.as_bytes());
        let _ = stdin.flush();
    })?;

    let (tx, rx) = mpsc::unbounded_channel();
    let outcome = Arc::new(AtomicBool::new(false));
    let (out_tx, out_outcome) = (tx.clone(), Arc::clone(&outcome));
    let mut parser = adapter.parser();
    let out = std::thread::Builder::new()
        .name("agent-stdout".into())
        .spawn(move || {
            for_each_line(stdout, |line| {
                for e in parser.feed(line) {
                    if e.is_outcome() {
                        out_outcome.store(true, Ordering::SeqCst);
                    }
                    let _ = out_tx.send(e);
                }
            });
        })?;
    let tail = Arc::new(Mutex::new(VecDeque::new()));
    let (err_tx, err_tail) = (tx.clone(), Arc::clone(&tail));
    let err = std::thread::Builder::new()
        .name("agent-stderr".into())
        .spawn(move || {
            for_each_line(stderr, |line| {
                let line = line.trim();
                if line.is_empty() {
                    return;
                }
                if let Ok(mut t) = err_tail.lock() {
                    if t.len() == STDERR_TAIL {
                        t.pop_front();
                    }
                    t.push_back(line.to_owned());
                }
                let _ = err_tx.send(AgentEvent::Log(line.to_owned()));
            });
        })?;

    let cancelled = Arc::clone(&cancel.cancelled);
    let guards = req.guards;
    spawn_thread("agent-wait", move || {
        let code = child.wait().ok().and_then(|s| s.code());
        group.reaped.store(true, Ordering::SeqCst);
        // All output before the outcome.
        let _ = out.join();
        let _ = err.join();
        if !outcome.load(Ordering::SeqCst) {
            let name = kind.display_name();
            let msg = if cancelled.load(Ordering::SeqCst) {
                "The run was cancelled.".to_owned()
            } else {
                let mut m = match code {
                    Some(0) => format!("{name} ended without an answer."),
                    Some(c) => format!("{name} exited with status {c}."),
                    None => format!("{name} was stopped by a signal."),
                };
                if let Ok(t) = tail.lock()
                    && !t.is_empty()
                {
                    m.push('\n');
                    m.push_str(&t.iter().cloned().collect::<Vec<_>>().join("\n"));
                }
                m
            };
            let _ = tx.send(AgentEvent::Error(msg));
        }
        drop(guards);
        drop(workdir);
        tracing::info!(agent = kind.id(), ?code, "agent run ended");
        let _ = tx.send(AgentEvent::Exited(code));
    })?;

    Ok(AgentRun {
        kind,
        events: rx,
        cancel,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::adapter::{Invocation, StreamParser};
    use std::path::Path;
    use std::time::{Duration, Instant};

    /// A "CLI" that is a shell script; each stdout line becomes `Text`, "DONE" ends.
    struct Script(String);

    struct Lines;
    impl StreamParser for Lines {
        fn feed(&mut self, line: &str) -> Vec<AgentEvent> {
            match line {
                "DONE" => vec![AgentEvent::Done(Default::default())],
                l => vec![AgentEvent::Text(l.to_owned())],
            }
        }
    }

    impl AgentAdapter for Script {
        fn kind(&self) -> AgentKind {
            AgentKind::Custom
        }
        fn program(&self) -> &str {
            "sh"
        }
        fn prepare(&self, ctx: &RunContext<'_>) -> Result<Invocation, AgentError> {
            std::fs::write(ctx.workdir.join("config"), "x")?;
            Ok(Invocation {
                args: vec!["-c".into(), self.0.clone()],
                env: vec![("FAKE".into(), "1".into())],
                stdin: Some(ctx.prompt.to_owned()),
            })
        }
        fn parser(&self) -> Box<dyn StreamParser> {
            Box::new(Lines)
        }
    }

    struct Flag(Arc<AtomicBool>);
    impl Drop for Flag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn request(root: &Path, dropped: &Arc<AtomicBool>) -> RunRequest {
        RunRequest {
            program: None,
            prompt: "hello prompt".into(),
            resume: None,
            model: None,
            mcp: McpServer {
                command: "swy".into(),
                args: vec!["mcp".into()],
                env: vec![],
            },
            temp_root: Some(root.to_path_buf()),
            guards: vec![Box::new(Flag(Arc::clone(dropped)))],
        }
    }

    fn collect(run: &mut AgentRun) -> Vec<AgentEvent> {
        let mut v = Vec::new();
        while let Some(e) = run.blocking_next() {
            v.push(e);
        }
        v
    }

    fn rundirs(root: &Path) -> usize {
        std::fs::read_dir(root).map_or(0, |d| d.count())
    }

    #[test]
    fn runs_in_a_private_dir_and_cleans_up() {
        let root = tempfile::tempdir().unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        // Reads the prompt from stdin, shows its cwd holds only the adapter's config.
        let script = Script(
            "read p; echo \"got $p\"; ls; echo \"fake=$FAKE\"; echo oops >&2; echo DONE".into(),
        );
        let mut run = start(Arc::new(script), request(root.path(), &dropped)).unwrap();
        let events = collect(&mut run);
        // stderr interleaves freely with stdout.
        let stdout: Vec<_> = events
            .iter()
            .filter(|e| !matches!(e, AgentEvent::Log(_)))
            .cloned()
            .collect();
        assert_eq!(
            stdout,
            [
                AgentEvent::Text("got hello prompt".into()),
                AgentEvent::Text("config".into()),
                AgentEvent::Text("fake=1".into()),
                AgentEvent::Done(Default::default()),
                AgentEvent::Exited(Some(0)),
            ]
        );
        assert!(events.contains(&AgentEvent::Log("oops".into())));
        assert_eq!(events.last(), Some(&AgentEvent::Exited(Some(0))));
        assert!(dropped.load(Ordering::SeqCst), "guards dropped");
        assert_eq!(rundirs(root.path()), 0, "run directory removed");
    }

    #[test]
    fn a_failed_run_reports_status_and_stderr() {
        let root = tempfile::tempdir().unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        let script = Script("echo 'not logged in' >&2; exit 3".into());
        let mut run = start(Arc::new(script), request(root.path(), &dropped)).unwrap();
        let events = collect(&mut run);
        let err = events.iter().find_map(|e| match e {
            AgentEvent::Error(m) => Some(m.clone()),
            _ => None,
        });
        let err = err.expect("an error");
        assert!(
            err.contains("status 3") && err.contains("not logged in"),
            "{err}"
        );
        assert_eq!(events.last(), Some(&AgentEvent::Exited(Some(3))));
        assert!(dropped.load(Ordering::SeqCst));
    }

    fn alive(pid: &str) -> bool {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    #[test]
    fn cancel_kills_the_process_tree() {
        let root = tempfile::tempdir().unwrap();
        let pids = tempfile::tempdir().unwrap();
        let pidfile = pids.path().join("child");
        let dropped = Arc::new(AtomicBool::new(false));
        let script = Script(format!(
            "sleep 30 & echo $! > {}; echo started; wait",
            pidfile.display()
        ));
        let mut run = start(Arc::new(script), request(root.path(), &dropped)).unwrap();
        assert_eq!(
            run.blocking_next(),
            Some(AgentEvent::Text("started".into()))
        );
        let child = std::fs::read_to_string(&pidfile).unwrap().trim().to_owned();
        assert!(alive(&child));
        let started = Instant::now();
        run.cancel();
        let events = collect(&mut run);
        assert_eq!(
            events,
            [
                AgentEvent::Error("The run was cancelled.".into()),
                AgentEvent::Exited(None)
            ]
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive(&child) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive(&child), "the CLI's children are gone too");
        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(rundirs(root.path()), 0);
    }

    #[test]
    fn missing_cli_is_reported() {
        struct Missing;
        impl AgentAdapter for Missing {
            fn kind(&self) -> AgentKind {
                AgentKind::Gemini
            }
            fn program(&self) -> &str {
                "switchyard-no-such-cli"
            }
            fn prepare(&self, _: &RunContext<'_>) -> Result<Invocation, AgentError> {
                Ok(Invocation::default())
            }
            fn parser(&self) -> Box<dyn StreamParser> {
                Box::new(Lines)
            }
        }
        let root = tempfile::tempdir().unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        let Err(e) = start(Arc::new(Missing), request(root.path(), &dropped)) else {
            panic!("started");
        };
        assert!(matches!(e, AgentError::NotFound(_)), "{e}");
        assert!(
            dropped.load(Ordering::SeqCst),
            "guards dropped on failure too"
        );
    }
}
