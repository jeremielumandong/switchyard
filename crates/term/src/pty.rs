//! Local shells on a pseudo-terminal (`portable-pty`). Two threads per shell: one reads
//! program output into the [`Feeder`], one writes input, resizes and closes the PTY.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tracing::{debug, warn};

use crate::error::{Result, TermError};
use crate::terminal::{Feeder, TermSize};

/// What to run.
#[derive(Clone, Debug, Default)]
pub struct LocalShell {
    /// Program; the user's login shell when `None`.
    pub program: Option<String>,
    /// Arguments.
    pub args: Vec<String>,
    /// Working directory; the home directory when `None`.
    pub cwd: Option<PathBuf>,
    /// Extra environment.
    pub env: Vec<(String, String)>,
}

/// Messages to the PTY's writer thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PtyInput {
    /// Bytes for the program.
    Data(Vec<u8>),
    /// New window size.
    Resize(TermSize),
    /// Kill the program and close the PTY.
    Close,
}

fn pty_size(size: TermSize) -> PtySize {
    PtySize {
        rows: size.rows.max(1),
        cols: size.cols.max(2),
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// Start `shell` on a new PTY. Output goes to `feeder`; `input` drives the PTY; `on_exit`
/// runs once with the exit code after the program ends.
pub fn spawn_local(
    shell: &LocalShell,
    size: TermSize,
    mut feeder: Feeder,
    input: Receiver<PtyInput>,
    on_exit: Box<dyn FnOnce(Option<u32>) + Send>,
) -> Result<()> {
    let system = native_pty_system();
    let pair = system
        .openpty(pty_size(size))
        .map_err(|e| TermError::Spawn(e.to_string()))?;
    let mut cmd = match &shell.program {
        Some(p) => {
            let mut c = CommandBuilder::new(p);
            c.args(&shell.args);
            c
        }
        None => CommandBuilder::new_default_prog(),
    };
    match &shell.cwd {
        Some(dir) => cmd.cwd(dir),
        None => {
            if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
            {
                cmd.cwd(home);
            }
        }
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("TERM_PROGRAM", "Switchyard");
    for (k, v) in &shell.env {
        cmd.env(k, v);
    }
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| TermError::Spawn(e.to_string()))?;
    // The slave end must close here, or the reader never sees EOF when the shell exits.
    drop(pair.slave);
    let master = pair.master;
    let mut reader = master
        .try_clone_reader()
        .map_err(|e| TermError::Spawn(e.to_string()))?;
    let mut writer = master
        .take_writer()
        .map_err(|e| TermError::Spawn(e.to_string()))?;
    let mut killer = child.clone_killer();

    std::thread::Builder::new()
        .name("pty-read".into())
        .spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => feeder.feed(&buf[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            let code = child.wait().ok().map(|s| s.exit_code());
            debug!(?code, "local shell exited");
            on_exit(code);
        })
        .map_err(|e| TermError::Spawn(e.to_string()))?;

    std::thread::Builder::new()
        .name("pty-write".into())
        .spawn(move || {
            while let Ok(msg) = input.recv() {
                match msg {
                    PtyInput::Data(bytes) => {
                        if writer
                            .write_all(&bytes)
                            .and_then(|()| writer.flush())
                            .is_err()
                        {
                            break;
                        }
                    }
                    PtyInput::Resize(size) => {
                        if let Err(e) = master.resize(pty_size(size)) {
                            warn!(error = %e, "pty resize failed");
                        }
                    }
                    PtyInput::Close => break,
                }
            }
            let _ = killer.kill();
        })
        .map_err(|e| TermError::Spawn(e.to_string()))?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::terminal::{EventSink, TermEvent, new_terminal};
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[test]
    fn runs_a_shell_and_reports_exit() {
        let sink: EventSink = Arc::new(|_: TermEvent| {});
        let (term, feeder) = new_terminal(TermSize { cols: 40, rows: 5 }, 100, sink);
        let (tx, rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let shell = LocalShell {
            program: Some("/bin/sh".into()),
            ..LocalShell::default()
        };
        spawn_local(
            &shell,
            TermSize { cols: 40, rows: 5 },
            feeder,
            rx,
            Box::new(move |code| {
                let _ = done_tx.send(code);
            }),
        )
        .expect("spawn");
        tx.send(PtyInput::Data(b"echo hi-$((40+2)); exit 3\n".to_vec()))
            .expect("send");
        let code = done_rx.recv_timeout(Duration::from_secs(10)).expect("exit");
        assert_eq!(code, Some(3));
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let s = term.snapshot();
            if s.lines.iter().any(|l| l.text.trim_end().ends_with("hi-42")) {
                break;
            }
            assert!(Instant::now() < deadline, "output: {:?}", s.lines);
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn resize_reaches_the_program() {
        let sink: EventSink = Arc::new(|_: TermEvent| {});
        let (term, feeder) = new_terminal(TermSize { cols: 40, rows: 5 }, 100, sink);
        let (tx, rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        spawn_local(
            &LocalShell {
                program: Some("/bin/sh".into()),
                ..LocalShell::default()
            },
            TermSize { cols: 40, rows: 5 },
            feeder,
            rx,
            Box::new(move |c| {
                let _ = done_tx.send(c);
            }),
        )
        .expect("spawn");
        term.resize(TermSize { cols: 57, rows: 9 });
        tx.send(PtyInput::Resize(TermSize { cols: 57, rows: 9 }))
            .expect("send");
        tx.send(PtyInput::Data(b"stty size; exit\n".to_vec()))
            .expect("send");
        done_rx.recv_timeout(Duration::from_secs(10)).expect("exit");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !term
            .snapshot()
            .lines
            .iter()
            .any(|l| l.text.trim_end().ends_with("9 57"))
        {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
