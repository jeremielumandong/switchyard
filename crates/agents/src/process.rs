//! Finding a CLI, starting it in its own process group, and killing that group.
//!
//! Ported from Emulsion's assistant (MIT, same owner; see `docs/DECISIONS.md`). The group
//! kill goes through `kill(1)` / `taskkill` because the workspace denies `unsafe`.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(unix)]
use std::time::Duration;

/// How long a CLI has to exit after SIGTERM before it gets SIGKILL.
#[cfg(unix)]
pub(crate) const KILL_GRACE: Duration = Duration::from_secs(2);

fn home() -> Option<PathBuf> {
    #[cfg(windows)]
    if let Some(home) = std::env::var_os("USERPROFILE") {
        return Some(PathBuf::from(home));
    }
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Directories searched after PATH: a desktop-launched app often has a minimal PATH that
/// misses npm, Homebrew and per-user installs.
fn extra_dirs() -> Vec<PathBuf> {
    let mut v = vec![
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/opt/homebrew/bin"),
    ];
    if let Some(h) = home() {
        for d in [
            ".local/bin",
            ".claude/local",
            ".npm-global/bin",
            ".local/share/mise/shims",
            ".volta/bin",
            ".bun/bin",
            ".cargo/bin",
        ] {
            v.push(h.join(d));
        }
        #[cfg(windows)]
        {
            v.push(h.join("AppData/Roaming/npm"));
            v.push(h.join("AppData/Local/Programs/claude"));
        }
    }
    #[cfg(windows)]
    for key in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        if let Some(root) = std::env::var_os(key) {
            v.push(PathBuf::from(root).join("nodejs"));
        }
    }
    v
}

/// On Windows an extensionless npm shim is a shell script; use its Windows sibling.
fn resolve_path(path: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        const EXTS: [&str; 4] = ["exe", "com", "cmd", "bat"];
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            return EXTS
                .into_iter()
                .map(|e| path.with_extension(e))
                .find(|c| c.is_file());
        };
        if !EXTS.contains(&ext.to_ascii_lowercase().as_str()) {
            return None;
        }
    }
    executable(path).then(|| path.to_path_buf())
}

fn executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// The CLI's executable: `explicit` if it exists, else `binary` on PATH, else in common
/// install locations.
pub fn find_program(binary: &str, explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return resolve_path(p);
    }
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .chain(extra_dirs())
        .map(|d| d.join(binary))
        .find_map(|p| resolve_path(&p))
}

/// PATH for the child: ours plus the extra locations, so an npm-installed CLI finds node.
pub(crate) fn child_path() -> std::ffi::OsString {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    for d in extra_dirs() {
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    std::env::join_paths(dirs).unwrap_or_default()
}

/// A command for `path`. On Windows, npm's `.cmd` shims are resolved to the Node script or
/// native executable they launch, so prompts with quotes and newlines stay one argument.
pub(crate) fn command(path: &Path) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        let mut command = match npm_target(path) {
            Some((program, script)) => {
                let mut c = Command::new(program);
                if let Some(script) = script {
                    c.arg(script);
                }
                c
            }
            None => Command::new(path),
        };
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        command
    }
    #[cfg(not(windows))]
    {
        let mut command = Command::new(path);
        // Own process group, so cancel reaches the CLI's children (`swy mcp`, node).
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
        command
    }
}

/// The program and leading arguments that launch `path` without a shell: on Windows an npm
/// `.cmd` shim becomes its Node script or native executable (terminals start programs
/// directly, and `.cmd` files need `cmd.exe`).
pub(crate) fn launch_parts(path: &Path) -> (PathBuf, Vec<String>) {
    #[cfg(windows)]
    if let Some((program, script)) = npm_target(path) {
        return (
            program,
            script
                .map(|s| vec![s.to_string_lossy().into_owned()])
                .unwrap_or_default(),
        );
    }
    (path.to_owned(), Vec::new())
}

#[cfg(windows)]
fn npm_target(shim: &Path) -> Option<(PathBuf, Option<PathBuf>)> {
    let ext = shim.extension()?.to_str()?;
    if !ext.eq_ignore_ascii_case("cmd") && !ext.eq_ignore_ascii_case("bat") {
        return None;
    }
    let text = std::fs::read_to_string(shim).ok()?;
    let base = shim.parent()?;
    for quoted in text.split('"').skip(1).step_by(2) {
        let Some(relative) = quoted.strip_prefix("%dp0%") else {
            continue;
        };
        let relative = relative.trim_start_matches(['/', '\\']);
        // Only npm's standard node_modules launchers, never an arbitrary batch file.
        if !relative.starts_with("node_modules/") && !relative.starts_with("node_modules\\") {
            continue;
        }
        let target = base.join(relative);
        if !target.is_file() {
            continue;
        }
        match target.extension().and_then(|s| s.to_str()) {
            Some("exe") => return Some((target, None)),
            Some("js" | "cjs" | "mjs") => {
                let node =
                    resolve_path(&base.join("node")).or_else(|| find_program("node", None))?;
                return Some((node, Some(target)));
            }
            _ => {}
        }
    }
    None
}

/// A started CLI and its descendants.
#[derive(Debug)]
pub(crate) struct ProcessGroup {
    pid: u32,
    /// Set once the child is reaped: its pid may be reused, so never signal it again.
    pub(crate) reaped: AtomicBool,
}

impl ProcessGroup {
    pub(crate) fn new(pid: u32) -> Arc<Self> {
        Arc::new(Self {
            pid,
            reaped: AtomicBool::new(false),
        })
    }

    /// Stop the CLI and everything it started. Unix: SIGTERM to the group, SIGKILL after
    /// [`KILL_GRACE`] if the CLI is still there. Windows: `taskkill /T /F`.
    pub(crate) fn kill(self: &Arc<Self>) {
        if self.reaped.load(Ordering::SeqCst) {
            return;
        }
        #[cfg(unix)]
        {
            signal_group(self.pid, "TERM");
            let group = Arc::clone(self);
            let _ = std::thread::Builder::new()
                .name("agent-kill".into())
                .spawn(move || {
                    std::thread::sleep(KILL_GRACE);
                    // Once the leader is reaped its pid (the group id) may be reused.
                    if !group.reaped.load(Ordering::SeqCst) {
                        signal_group(group.pid, "KILL");
                    }
                });
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            let _ = Command::new("taskkill")
                .args(["/T", "/F", "/PID", &self.pid.to_string()])
                .creation_flags(0x0800_0000)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}

/// `kill -<signal> -- -<pgid>`. The group id is the child's pid (`process_group(0)`).
/// procps' `kill` needs the `--`: without it a negative pid is silently ignored.
#[cfg(unix)]
fn signal_group(pgid: u32, signal: &str) {
    let _ = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg("--")
        .arg(format!("-{pgid}"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Write a file only its owner can read, from the moment it exists (CLI configs carry the
/// session token).
pub(crate) fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.flush()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn finds_programs_on_path() {
        assert!(find_program("sh", None).is_some());
        assert!(find_program("switchyard-no-such-cli", None).is_none());
        assert!(find_program("x", Some(Path::new("/no/such/cli"))).is_none());
        assert_eq!(
            find_program("x", Some(Path::new("/bin/sh"))),
            Some(PathBuf::from("/bin/sh"))
        );
    }

    #[test]
    fn private_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("cfg.json");
        write_private(&p, b"{}").unwrap();
        assert_eq!(p.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        assert!(write_private(&p, b"{}").is_err(), "never reuses a file");
    }
}
