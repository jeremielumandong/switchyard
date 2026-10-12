//! Running `docker` and `kubectl` on this computer with `tokio::process`. Every child is
//! killed when its future or handle is dropped (`kill_on_drop`), so dropping a call
//! cancels it.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt as _;
use tokio::process::{Child, Command};

use crate::{ContainersError, Invocation, LineSplitter, Result, Tool};

/// Characters of error output kept in a message.
const ERROR_CHARS: usize = 2_000;
/// Lines delivered at once while streaming, and the longest a line waits.
const BATCH_LINES: usize = 500;
const BATCH_WAIT: Duration = Duration::from_millis(80);

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// `~/x` as a path under the home folder; anything else unchanged.
pub fn expand_home(arg: &str) -> String {
    match (arg.strip_prefix("~/"), home()) {
        (Some(rest), Some(h)) => h.join(rest).to_string_lossy().into_owned(),
        _ => arg.to_owned(),
    }
}

/// Folders searched after `PATH`: apps started from the desktop often get a short `PATH`
/// without Homebrew's or Docker Desktop's folders.
fn extra_dirs() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        v.push(PathBuf::from(
            r"C:\Program Files\Docker\Docker\resources\bin",
        ));
    } else {
        v.extend(
            [
                "/opt/homebrew/bin",
                "/usr/local/bin",
                "/usr/bin",
                "/snap/bin",
                "/Applications/Docker.app/Contents/Resources/bin",
            ]
            .map(PathBuf::from),
        );
        if let Some(h) = home() {
            v.extend([
                h.join(".docker/bin"),
                h.join(".rd/bin"),
                h.join(".orbstack/bin"),
                h.join(".local/bin"),
                h.join("google-cloud-sdk/bin"),
                h.join(".azure-kubelogin"),
            ]);
        }
        v.extend(
            [
                "/usr/lib/google-cloud-sdk/bin",
                "/opt/homebrew/share/google-cloud-sdk/bin",
                "/usr/local/share/google-cloud-sdk/bin",
            ]
            .map(PathBuf::from),
        );
        if let Some(h) = home() {
            v.push(h.join(".krew/bin"));
        }
    }
    v
}

fn executable_names(name: &str) -> Vec<String> {
    if cfg!(windows) && Path::new(name).extension().is_none() {
        vec![format!("{name}.exe"), name.to_owned()]
    } else {
        vec![name.to_owned()]
    }
}

/// The executable for `tool`: the connection's own path or name (`configured`), else
/// the tool's name, looked up on `PATH` and in the usual install folders. `None` when it
/// is not installed.
pub fn find_program(tool: Tool, configured: Option<&str>) -> Option<PathBuf> {
    let name = configured
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(expand_home)
        .unwrap_or_else(|| tool.program().to_owned());
    find_named(&name)
}

/// A program by name (looked up like [`find_program`]) or path (`~/` allowed).
pub fn find_named(name: &str) -> Option<PathBuf> {
    let name = expand_home(name.trim());
    if name.is_empty() {
        return None;
    }
    if name.contains('/') || name.contains('\\') {
        let p = PathBuf::from(&name);
        return p.is_file().then_some(p);
    }
    let path_dirs = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default();
    path_dirs
        .into_iter()
        .chain(extra_dirs())
        .flat_map(|d| executable_names(&name).into_iter().map(move |n| d.join(n)))
        .find(|p| p.is_file())
}

/// A command for `inv` (home paths expanded, no stdin, output piped, killed on drop).
fn command(program: &Path, inv: &Invocation) -> Command {
    let mut c = Command::new(program);
    c.args(inv.args.iter().map(|a| expand_home(a)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        // No console window flashing up for each call.
        c.creation_flags(0x0800_0000);
    }
    c
}

/// Start `inv` with its output piped. Dropping the child kills it.
pub fn spawn(program: &Path, inv: &Invocation) -> Result<Child> {
    command(program, inv).spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => ContainersError::Missing(inv.tool),
        _ => ContainersError::Io(format!("{}: {e}", inv.tool.program())),
    })
}

/// The message for a failed call: its error output, else its output, else the code.
pub fn error_text(stderr: &str, stdout: &str, code: Option<i32>) -> String {
    let text = match stderr.trim() {
        "" => stdout.trim(),
        e => e,
    };
    if text.is_empty() {
        return match code {
            Some(c) => format!("exited with code {c}"),
            None => "stopped by a signal".into(),
        };
    }
    let mut out: String = text.chars().take(ERROR_CHARS).collect();
    if out.len() < text.len() {
        out.push('…');
    }
    out
}

/// Run `inv` to completion and return its standard output; a non-zero exit is
/// [`ContainersError::Failed`] with its error output. Stops it at `timeout`.
pub async fn run(program: &Path, inv: &Invocation, timeout: Duration) -> Result<String> {
    let child = spawn(program, inv)?;
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(r) => r.map_err(|e| ContainersError::Io(e.to_string()))?,
        Err(_) => {
            return Err(ContainersError::TimedOut {
                tool: inv.tool,
                secs: timeout.as_secs(),
            });
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if out.status.success() {
        Ok(stdout)
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        Err(ContainersError::Failed {
            tool: inv.tool,
            code: out.status.code(),
            message: error_text(&stderr, &stdout, out.status.code()),
        })
    }
}

/// Collects lines from two pipes and hands them over in batches.
pub struct Batcher<F: FnMut(Vec<String>)> {
    pending: Vec<String>,
    since: Instant,
    deliver: F,
}

impl<F: FnMut(Vec<String>)> Batcher<F> {
    /// A batcher calling `deliver` with each batch.
    pub fn new(deliver: F) -> Self {
        Self {
            pending: Vec::new(),
            since: Instant::now(),
            deliver,
        }
    }

    /// Lines waiting to be delivered.
    pub fn pending(&mut self) -> &mut Vec<String> {
        &mut self.pending
    }

    /// Deliver when enough lines wait or the oldest waited long enough.
    pub fn maybe_flush(&mut self) {
        if self.pending.len() >= BATCH_LINES || self.since.elapsed() >= BATCH_WAIT {
            self.flush();
        }
    }

    /// Deliver what waits.
    pub fn flush(&mut self) {
        if !self.pending.is_empty() {
            (self.deliver)(std::mem::take(&mut self.pending));
        }
        self.since = Instant::now();
    }

    /// How long until waiting lines must go out (`None` when nothing waits).
    pub fn deadline(&self) -> Option<Duration> {
        (!self.pending.is_empty()).then(|| BATCH_WAIT.saturating_sub(self.since.elapsed()))
    }
}

/// Run `inv` and hand its output lines (standard output and error, as they come) to
/// `on_lines` in batches until it exits. A non-zero exit is an error with the last
/// lines of its error output. Dropping the future kills the process.
pub async fn stream(
    program: &Path,
    inv: &Invocation,
    on_lines: impl FnMut(Vec<String>),
) -> Result<()> {
    let mut child = spawn(program, inv)?;
    let (Some(mut out), Some(mut err)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(ContainersError::Io("no output pipe".into()));
    };
    let mut batch = Batcher::new(on_lines);
    let (mut so, mut se) = (LineSplitter::new(), LineSplitter::new());
    let (mut out_open, mut err_open) = (true, true);
    let mut err_tail: Vec<String> = Vec::new();
    let (mut b1, mut b2) = (vec![0u8; 16 * 1024], vec![0u8; 16 * 1024]);
    while out_open || err_open {
        let wait = batch.deadline().unwrap_or(Duration::from_secs(3600));
        tokio::select! {
            r = out.read(&mut b1), if out_open => match r {
                Ok(0) | Err(_) => {
                    out_open = false;
                    so.flush(batch.pending());
                }
                Ok(n) => so.feed(&b1[..n], batch.pending()),
            },
            r = err.read(&mut b2), if err_open => match r {
                Ok(0) | Err(_) => {
                    err_open = false;
                    let start = batch.pending().len();
                    se.flush(batch.pending());
                    err_tail.extend(batch.pending()[start..].iter().cloned());
                }
                Ok(n) => {
                    let start = batch.pending().len();
                    se.feed(&b2[..n], batch.pending());
                    err_tail.extend(batch.pending()[start..].iter().cloned());
                    let keep = err_tail.len().saturating_sub(20);
                    err_tail.drain(..keep);
                }
            },
            () = tokio::time::sleep(wait) => {}
        }
        batch.maybe_flush();
    }
    batch.flush();
    let status = child
        .wait()
        .await
        .map_err(|e| ContainersError::Io(e.to_string()))?;
    if status.success() {
        Ok(())
    } else {
        Err(ContainersError::Failed {
            tool: inv.tool,
            code: status.code(),
            message: error_text(&err_tail.join("\n"), "", status.code()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh() -> Option<PathBuf> {
        let p = PathBuf::from("/bin/sh");
        p.is_file().then_some(p)
    }

    fn inv(script: &str) -> Invocation {
        Invocation {
            tool: Tool::Docker,
            args: vec!["-c".into(), script.into()],
        }
    }

    #[test]
    fn home_paths_expand() {
        if let Some(h) = home() {
            assert_eq!(
                expand_home("~/.kube/config"),
                h.join(".kube/config").to_string_lossy()
            );
        }
        assert_eq!(expand_home("/etc/x"), "/etc/x");
        assert_eq!(expand_home("a~/b"), "a~/b");
    }

    #[test]
    fn missing_programs_are_none() {
        assert_eq!(
            find_program(Tool::Docker, Some("/nonexistent/docker-xyz")),
            None
        );
        assert_eq!(
            find_program(Tool::Kubectl, Some("kubectl-not-installed-xyz")),
            None
        );
        if cfg!(unix) {
            assert_eq!(
                find_program(Tool::Docker, Some("/bin/sh")),
                Some(PathBuf::from("/bin/sh"))
            );
        }
    }

    #[test]
    fn error_text_prefers_stderr() {
        assert_eq!(error_text(" boom \n", "out", Some(1)), "boom");
        assert_eq!(error_text("", " out ", Some(1)), "out");
        assert_eq!(error_text("", "", Some(3)), "exited with code 3");
        assert_eq!(error_text("", "", None), "stopped by a signal");
        let long = "x".repeat(5000);
        assert_eq!(
            error_text(&long, "", Some(1)).chars().count(),
            ERROR_CHARS + 1
        );
    }

    #[tokio::test]
    async fn run_collects_output_and_errors() {
        let Some(sh) = sh() else { return };
        let ok = run(&sh, &inv("echo hello"), Duration::from_secs(10)).await;
        assert_eq!(ok, Ok("hello\n".to_owned()));
        let failed = run(&sh, &inv("echo nope >&2; exit 3"), Duration::from_secs(10)).await;
        assert_eq!(
            failed,
            Err(ContainersError::Failed {
                tool: Tool::Docker,
                code: Some(3),
                message: "nope".into()
            })
        );
        let slow = run(&sh, &inv("sleep 5"), Duration::from_millis(200)).await;
        assert!(matches!(slow, Err(ContainersError::TimedOut { .. })));
        let missing = run(
            Path::new("/nonexistent/kubectl"),
            &Invocation {
                tool: Tool::Kubectl,
                args: vec![],
            },
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(missing, Err(ContainersError::Missing(Tool::Kubectl)));
    }

    #[tokio::test]
    async fn stream_delivers_both_pipes_in_batches() {
        let Some(sh) = sh() else { return };
        let mut got: Vec<String> = Vec::new();
        let r = stream(&sh, &inv("echo one; echo two >&2; printf three"), |lines| {
            got.extend(lines)
        })
        .await;
        assert_eq!(r, Ok(()));
        got.sort();
        assert_eq!(got, ["one", "three", "two"]);
        let mut got = Vec::new();
        let r = stream(&sh, &inv("echo out; echo bad >&2; exit 2"), |l| {
            got.extend(l)
        })
        .await;
        assert_eq!(
            r,
            Err(ContainersError::Failed {
                tool: Tool::Docker,
                code: Some(2),
                message: "bad".into()
            })
        );
    }

    #[tokio::test]
    async fn dropping_a_stream_kills_the_process() {
        let Some(sh) = sh() else { return };
        let started = Instant::now();
        let r = tokio::time::timeout(
            Duration::from_millis(300),
            stream(
                &sh,
                &inv("while true; do echo tick; sleep 0.05; done"),
                |_| {},
            ),
        )
        .await;
        assert!(r.is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
