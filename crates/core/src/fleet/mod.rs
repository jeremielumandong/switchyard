//! Fleet: tools for people managing many servers. Run one command (or a saved macro) on
//! many Hosts at once, a dashboard of their health, file push, SSH key rollout and
//! Ansible inventory import.
//!
//! Everything rides on each Host's shared SSH session ([`switchyard_remote::ssh::SshManager`]),
//! opened on demand with the usual prompts. None of this is reachable from the MCP
//! server or `swy`: agents keep `run_ssh_command` with its per-command approval.

pub mod ansible;
pub mod keys;
pub mod probe;
pub mod runner;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use switchyard_store::ProfileId;

pub use runner::{FleetHost, HostStatus};

/// Identifies one Fleet run (a command, a probe round, a push, a key change).
pub type FleetRunId = u64;

/// Setting key of how many Hosts a run works on at once.
pub const CONCURRENCY_KEY: &str = "fleet.concurrency";
/// Hosts worked on at once unless the setting says otherwise.
pub const DEFAULT_CONCURRENCY: usize = 8;
/// Default per-Host timeout of a command, seconds.
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// Timeout of one dashboard probe, seconds.
pub const PROBE_TIMEOUT_SECS: u64 = 20;
/// Bytes of each output stream kept per Host.
pub const OUTPUT_CAP: usize = 1024 * 1024;
/// History tag of every Fleet action.
pub const HISTORY_TAG: &str = "fleet";

/// What to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FleetAction {
    /// A shell command line.
    Command(String),
    /// A saved terminal macro (its id); its lines run as one script.
    Macro(String),
}

/// What to do with `~/.ssh/authorized_keys`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyOp {
    /// List the keys present; with a key, report whether each Host has it.
    Audit(Option<String>),
    /// Add the key unless present (after a backup).
    Add(String),
    /// Remove the key (after a backup); never the session's own key.
    Remove(String),
}

impl KeyOp {
    /// Whether it changes files.
    pub fn changes(&self) -> bool {
        !matches!(self, KeyOp::Audit(_))
    }
}

/// What a push does when the target exists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Overwrite {
    /// Replace it.
    #[default]
    Replace,
    /// Leave it and report the Host as skipped.
    Skip,
    /// Keep both: `name (1).ext`.
    KeepBoth,
}

/// A Fleet request from the UI.
#[derive(Clone, Debug)]
pub enum FleetCommand {
    /// Run a command or macro on `hosts` ([`FleetEvent`]s tagged `run`).
    Run {
        /// Run id.
        run: FleetRunId,
        /// Hosts.
        hosts: Vec<ProfileId>,
        /// What to run.
        action: FleetAction,
        /// Hosts at once; `None` uses the setting ([`CONCURRENCY_KEY`]).
        concurrency: Option<usize>,
        /// Per-Host timeout, seconds.
        timeout_secs: u64,
        /// The user typed the Production confirmation (needed when any Host is
        /// Production).
        confirmed: bool,
    },
    /// Probe `hosts` for the dashboard (read-only).
    Probe {
        /// Run id.
        run: FleetRunId,
        /// Hosts.
        hosts: Vec<ProfileId>,
    },
    /// Upload a local file or folder into the same folder on every Host (SFTP).
    Push {
        /// Run id.
        run: FleetRunId,
        /// Hosts.
        hosts: Vec<ProfileId>,
        /// Local file or folder.
        local: PathBuf,
        /// Remote folder (`~` is the Host's home).
        remote_dir: String,
        /// Existing target policy.
        overwrite: Overwrite,
        /// `chmod` mode for what was uploaded (octal, `755`), recursive for folders.
        chmod: Option<String>,
        /// Hosts at once; `None` uses the setting.
        concurrency: Option<usize>,
        /// Production confirmation typed.
        confirmed: bool,
    },
    /// Audit, add or remove an SSH public key.
    Keys {
        /// Run id.
        run: FleetRunId,
        /// Hosts.
        hosts: Vec<ProfileId>,
        /// What to do.
        op: KeyOp,
        /// Hosts at once; `None` uses the setting.
        concurrency: Option<usize>,
        /// Production confirmation typed (changes only).
        confirmed: bool,
    },
    /// Stop every Host of a run (cancel-all).
    Cancel {
        /// Run id.
        run: FleetRunId,
    },
    /// Read an Ansible inventory and answer [`FleetEvent::InventoryPreview`].
    PreviewInventory {
        /// Inventory file (INI or YAML).
        path: PathBuf,
    },
    /// Import Hosts from an Ansible inventory: the names in `only`, or every new one.
    ImportInventory {
        /// Inventory file.
        path: PathBuf,
        /// Names to import.
        only: Option<Vec<String>>,
    },
}

/// One Host's finished command (or push / key change).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostOutcome {
    /// Final status.
    pub status: HostStatus,
    /// Exit status of a command.
    pub exit_status: Option<u32>,
    /// Job time, ms.
    pub duration_ms: u64,
    /// First non-empty output line (or the error).
    pub first_line: String,
    /// Error text.
    pub error: Option<String>,
    /// Digest of exit status + output, to find Hosts that differ from the majority.
    pub digest: Option<u64>,
    /// Output past [`OUTPUT_CAP`] was dropped.
    pub truncated: bool,
}

/// A Host's `authorized_keys` after an audit or change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyReport {
    /// What happened (`added`, `already present`, `removed 2 lines`, `not present`, …).
    pub summary: String,
    /// Whether the Host has the key asked about (`None` for a plain audit).
    pub has_key: Option<bool>,
    /// Keys present after the operation.
    pub keys: Vec<keys::AuthorizedKey>,
    /// Fingerprint of the key the session signed in with.
    pub session_key: Option<String>,
    /// A backup was written.
    pub backed_up: bool,
}

/// Runtime answers for Fleet.
#[derive(Clone, Debug)]
pub enum FleetEvent {
    /// A run started with these Hosts (resolved names, Hosts not found left out).
    Started {
        /// Run.
        run: FleetRunId,
        /// Hosts in order.
        hosts: Vec<FleetHost>,
    },
    /// The run was refused before it started.
    Refused {
        /// Run.
        run: FleetRunId,
        /// Why.
        message: String,
    },
    /// A Host changed status.
    Status {
        /// Run.
        run: FleetRunId,
        /// Host.
        host: ProfileId,
        /// Status.
        status: HostStatus,
    },
    /// A piece of a Host's output (batched).
    Output {
        /// Run.
        run: FleetRunId,
        /// Host.
        host: ProfileId,
        /// Standard error.
        stderr: bool,
        /// Text (lossy UTF-8).
        text: String,
    },
    /// Push progress of a Host.
    Progress {
        /// Run.
        run: FleetRunId,
        /// Host.
        host: ProfileId,
        /// Bytes so far.
        done: u64,
        /// Total bytes.
        total: Option<u64>,
    },
    /// A Host finished.
    Done {
        /// Run.
        run: FleetRunId,
        /// Host.
        host: ProfileId,
        /// Outcome.
        outcome: HostOutcome,
    },
    /// A Host's probe answered.
    Probed {
        /// Run.
        run: FleetRunId,
        /// Host.
        host: ProfileId,
        /// What it found, or why it could not.
        result: Result<probe::ProbeReport, String>,
    },
    /// A Host's key operation finished.
    Keys {
        /// Run.
        run: FleetRunId,
        /// Host.
        host: ProfileId,
        /// Report, or why it failed.
        result: Result<KeyReport, String>,
    },
    /// Every Host of the run is done.
    Finished {
        /// Run.
        run: FleetRunId,
        /// Cancel-all was used.
        cancelled: bool,
    },
    /// What an inventory import would add.
    InventoryPreview {
        /// The file read.
        path: PathBuf,
        /// Entries, or why the file could not be read.
        result: Result<Vec<crate::ssh_import::SshImportCandidate>, String>,
    },
}

/// The command lines of a terminal macro, for running without a terminal: each typed
/// line (Enter = `\r`) becomes a script line. Refused when it holds keys that only mean
/// something to an interactive program (arrows, Ctrl keys, Escape).
pub fn macro_script(input: &[u8]) -> Result<String, String> {
    let text = String::from_utf8_lossy(input);
    if text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\r' | '\n' | '\t'))
    {
        return Err(
            "this macro presses keys that only work in a terminal (arrows, Ctrl, Escape)".into(),
        );
    }
    let lines: Vec<&str> = text
        .split(['\r', '\n'])
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect();
    if lines.is_empty() {
        return Err("the macro types no command".into());
    }
    Ok(lines.join("\n"))
}

/// `'text'` for a POSIX shell.
pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// Whether `mode` is an octal `chmod` mode (`644`, `0755`).
pub fn valid_mode(mode: &str) -> bool {
    (3..=4).contains(&mode.len()) && mode.chars().all(|c| ('0'..='7').contains(&c))
}

/// The first non-empty line of `text`, at most 200 characters.
pub fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .chars()
        .take(200)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macros_become_scripts() {
        assert_eq!(
            macro_script(b"cd /srv\rgit pull\r\r").as_deref(),
            Ok("cd /srv\ngit pull")
        );
        assert!(macro_script(b"vim x\x1b:wq\r").is_err());
        assert!(macro_script(b"\r\r").is_err());
    }

    #[test]
    fn small_helpers() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert!(valid_mode("644") && valid_mode("0755"));
        assert!(!valid_mode("u+x") && !valid_mode("888") && !valid_mode("12"));
        assert_eq!(first_line("\n  \n hello \nworld"), "hello");
    }
}
