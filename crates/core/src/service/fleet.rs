//! Fleet commands on the runtime: run a command on many Hosts, dashboard probes, file
//! push and SSH key rollout, each through [`crate::fleet::runner`] on the Hosts' shared
//! SSH sessions; Ansible inventory import. Every run, push and key change is written to
//! history tagged `fleet` (probes and audits only read, and are not).

use std::collections::{HashMap, HashSet};
use std::hash::{Hash as _, Hasher as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use switchyard_remote::ssh::{SshConn, SshTarget};
use switchyard_remote::{LocalFs, RemoteFs, SftpFs};
use switchyard_store::{HistoryEntry, HistoryStatus, Host, Profile, ProfileId, now_ms};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::{Service, lock};
use crate::bus::{Event, OnConflict, TransferError};
use crate::fleet::keys::{self, KeyEditError, PublicKeyLine};
use crate::fleet::probe::{self, ProbeReport};
use crate::fleet::runner::{
    FleetExecutor, FleetHost, HostResult, HostStatus, OutputSink, RunLimits, Update, run_hosts,
};
use crate::fleet::{
    CONCURRENCY_KEY, DEFAULT_CONCURRENCY, FleetAction, FleetCommand, FleetEvent, FleetRunId,
    HISTORY_TAG, HostOutcome, KeyOp, KeyReport, OUTPUT_CAP, Overwrite, PROBE_TIMEOUT_SECS,
    first_line, macro_script, shell_quote, valid_mode,
};

/// Running Fleet runs, to cancel.
pub(crate) type FleetRuns = Mutex<HashMap<FleetRunId, CancellationToken>>;

/// A job longer than this is stopped by the runner's timeout, never by the SSH layer.
const NO_SSH_TIMEOUT: Duration = Duration::from_secs(24 * 3600);

/// Output is sent to the UI in batches this often.
const FLUSH_EVERY: Duration = Duration::from_millis(120);

/// Removes a run from [`FleetRuns`] however it ends.
struct RunGuard<'a> {
    service: &'a Service,
    run: FleetRunId,
}

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        lock(&self.service.fleet_runs).remove(&self.run);
    }
}

/// Output waiting to be sent, per Host: (stdout, stderr).
#[derive(Default)]
struct Batcher {
    bufs: Mutex<HashMap<ProfileId, (Vec<u8>, Vec<u8>)>>,
}

/// The longest valid UTF-8 prefix of `buf` (lossy for invalid bytes), keeping an
/// incomplete character at the end for the next piece.
fn take_utf8(buf: &mut Vec<u8>) -> String {
    let keep = match std::str::from_utf8(buf) {
        Ok(_) => 0,
        Err(e) if e.error_len().is_none() => buf.len() - e.valid_up_to(),
        Err(_) => 0,
    };
    let rest = buf.split_off(buf.len() - keep);
    let text = String::from_utf8_lossy(buf).into_owned();
    *buf = rest;
    text
}

impl Batcher {
    fn push(&self, host: &ProfileId, stderr: bool, bytes: &[u8]) {
        let mut bufs = lock(&self.bufs);
        let (out, err) = bufs.entry(host.clone()).or_default();
        if stderr { err } else { out }.extend_from_slice(bytes);
    }

    /// Send what is waiting (one Host, or all).
    fn flush(&self, run: FleetRunId, only: Option<&ProfileId>, emit: &dyn Fn(Event)) {
        let mut bufs = lock(&self.bufs);
        for (host, (out, err)) in bufs.iter_mut() {
            if only.is_some_and(|o| o != host) {
                continue;
            }
            for (stderr, buf) in [(false, out), (true, err)] {
                if buf.is_empty() {
                    continue;
                }
                let text = take_utf8(buf);
                if !text.is_empty() {
                    emit(Event::Fleet(FleetEvent::Output {
                        run,
                        host: host.clone(),
                        stderr,
                        text,
                    }));
                }
            }
        }
    }
}

/// Opens each Host's shared SSH session.
struct Sessions<'a> {
    service: &'a Service,
    targets: HashMap<ProfileId, SshTarget>,
}

impl Sessions<'_> {
    async fn open(&self, host: &FleetHost) -> Result<Arc<SshConn>, String> {
        let target = self
            .targets
            .get(&host.id)
            .ok_or_else(|| format!("{} is not a saved Host", host.name))?;
        self.service
            .ssh
            .session(target)
            .await
            .map_err(|e| e.to_string())
    }
}

/// What a command printed and returned.
struct CommandRun {
    exit_status: Option<u32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
}

struct CommandExec<'a> {
    sessions: Sessions<'a>,
    script: String,
}

impl FleetExecutor for CommandExec<'_> {
    type Session = Arc<SshConn>;
    type Output = CommandRun;

    fn connect<'a>(&'a self, host: &'a FleetHost) -> BoxFuture<'a, Result<Arc<SshConn>, String>> {
        Box::pin(self.sessions.open(host))
    }

    fn run<'a>(
        &'a self,
        conn: Arc<SshConn>,
        _host: &'a FleetHost,
        stop: CancellationToken,
        output: &'a OutputSink<'a>,
    ) -> BoxFuture<'a, Result<CommandRun, String>> {
        Box::pin(async move {
            let mut sink = |stderr: bool, b: &[u8]| output(stderr, b);
            let o = conn
                .run_command_streaming(
                    &self.script,
                    None,
                    OUTPUT_CAP,
                    NO_SSH_TIMEOUT,
                    stop.cancelled(),
                    &mut sink,
                )
                .await
                .map_err(|e| e.to_string())?;
            Ok(CommandRun {
                exit_status: o.exit_status,
                stdout: o.stdout,
                stderr: o.stderr,
                truncated: o.truncated,
            })
        })
    }

    fn status(&self, o: &CommandRun) -> HostStatus {
        if o.exit_status == Some(0) {
            HostStatus::Ok
        } else {
            HostStatus::Failed
        }
    }
}

struct ProbeExec<'a> {
    sessions: Sessions<'a>,
}

impl FleetExecutor for ProbeExec<'_> {
    type Session = Arc<SshConn>;
    type Output = ProbeReport;

    fn connect<'a>(&'a self, host: &'a FleetHost) -> BoxFuture<'a, Result<Arc<SshConn>, String>> {
        Box::pin(self.sessions.open(host))
    }

    fn run<'a>(
        &'a self,
        conn: Arc<SshConn>,
        _host: &'a FleetHost,
        stop: CancellationToken,
        _output: &'a OutputSink<'a>,
    ) -> BoxFuture<'a, Result<ProbeReport, String>> {
        Box::pin(async move {
            let o = conn
                .run_command_streaming(
                    probe::PROBE_SCRIPT,
                    None,
                    256 * 1024,
                    NO_SSH_TIMEOUT,
                    stop.cancelled(),
                    &mut |_, _| {},
                )
                .await
                .map_err(|e| e.to_string())?;
            let text = String::from_utf8_lossy(&o.stdout);
            if !text.contains("@@end") && !o.cancelled {
                return Err(format!(
                    "the probe did not finish: {}",
                    first_line(&String::from_utf8_lossy(&o.stderr))
                ));
            }
            Ok(probe::parse_probe(&text))
        })
    }
}

struct KeysExec<'a> {
    sessions: Sessions<'a>,
    op: KeyOp,
    key: Option<PublicKeyLine>,
}

/// Read `authorized_keys` ("" when missing).
async fn read_keys(conn: &SshConn, stop: &CancellationToken) -> Result<String, String> {
    let o = conn
        .run_command_streaming(
            keys::READ_COMMAND,
            None,
            OUTPUT_CAP,
            NO_SSH_TIMEOUT,
            stop.cancelled(),
            &mut |_, _| {},
        )
        .await
        .map_err(|e| e.to_string())?;
    match o.exit_status {
        Some(0) if !o.truncated => Ok(String::from_utf8_lossy(&o.stdout).into_owned()),
        Some(0) => Err("authorized_keys is too large".into()),
        Some(keys::MISSING_STATUS) => Ok(String::new()),
        _ if o.cancelled => Err("stopped".into()),
        _ => Err(format!(
            "could not read authorized_keys: {}",
            first_line(&String::from_utf8_lossy(&o.stderr))
        )),
    }
}

/// Replace `authorized_keys` with `text` (backup first).
async fn write_keys(conn: &SshConn, text: &str, stop: &CancellationToken) -> Result<(), String> {
    let o = conn
        .run_command_streaming(
            keys::WRITE_COMMAND,
            Some(text.as_bytes()),
            64 * 1024,
            NO_SSH_TIMEOUT,
            stop.cancelled(),
            &mut |_, _| {},
        )
        .await
        .map_err(|e| e.to_string())?;
    if o.exit_status == Some(0) {
        Ok(())
    } else {
        Err(format!(
            "could not write authorized_keys: {}",
            first_line(&String::from_utf8_lossy(&o.stderr))
        ))
    }
}

impl FleetExecutor for KeysExec<'_> {
    type Session = Arc<SshConn>;
    type Output = KeyReport;

    fn connect<'a>(&'a self, host: &'a FleetHost) -> BoxFuture<'a, Result<Arc<SshConn>, String>> {
        Box::pin(self.sessions.open(host))
    }

    fn run<'a>(
        &'a self,
        conn: Arc<SshConn>,
        _host: &'a FleetHost,
        stop: CancellationToken,
        _output: &'a OutputSink<'a>,
    ) -> BoxFuture<'a, Result<KeyReport, String>> {
        Box::pin(async move {
            let session_key = conn.auth_key_fingerprint();
            let text = read_keys(&conn, &stop).await?;
            let report = |summary: String, text: &str, backed_up: bool| KeyReport {
                summary,
                has_key: self.key.as_ref().map(|k| keys::has_key(text, k)),
                keys: keys::parse_authorized_keys(text),
                session_key: session_key.clone(),
                backed_up,
            };
            match (&self.op, &self.key) {
                (KeyOp::Audit(_), _) => {
                    let n = keys::parse_authorized_keys(&text).len();
                    Ok(report(format!("{n} keys"), &text, false))
                }
                (KeyOp::Add(_), Some(k)) => match keys::add_key(&text, k) {
                    None => Ok(report("already present".into(), &text, false)),
                    Some(new) => {
                        write_keys(&conn, &new, &stop).await?;
                        Ok(report("added".into(), &new, true))
                    }
                },
                (KeyOp::Remove(_), Some(k)) => {
                    match keys::remove_key(&text, k, session_key.as_deref()) {
                        Err(e) => Err(e.to_string()),
                        Ok(None) => Ok(report("not present".into(), &text, false)),
                        Ok(Some((new, n))) => {
                            write_keys(&conn, &new, &stop).await?;
                            let lines = if n == 1 { "line" } else { "lines" };
                            Ok(report(format!("removed ({n} {lines})"), &new, true))
                        }
                    }
                }
                _ => Err(KeyEditError::NotAKey.to_string()),
            }
        })
    }

    fn status(&self, r: &KeyReport) -> HostStatus {
        if self.op.changes() && !r.backed_up {
            HostStatus::Skipped
        } else {
            HostStatus::Ok
        }
    }
}

struct PushExec<'a> {
    sessions: Sessions<'a>,
    service: &'a Service,
    run: FleetRunId,
    local: PathBuf,
    remote_dir: String,
    overwrite: Overwrite,
    chmod: Option<String>,
}

/// Where a push went, or that it was skipped.
struct Pushed {
    target: Option<String>,
    note: String,
}

impl PushExec<'_> {
    /// The Host's SFTP file system, shared with the Files tab.
    async fn sftp(&self, host: &FleetHost, conn: Arc<SshConn>) -> Result<Arc<SftpFs>, String> {
        let mut open = self.service.files.sftp.lock().await;
        if let Some(f) = open.get(&host.id.0)
            && !f.is_closed()
        {
            return Ok(f.clone());
        }
        let f = Arc::new(
            SftpFs::open(conn, host.name.clone())
                .await
                .map_err(|e| format!("SFTP on {}: {e}", host.name))?,
        );
        open.insert(host.id.0.clone(), f.clone());
        Ok(f)
    }
}

impl FleetExecutor for PushExec<'_> {
    type Session = (Arc<SshConn>, Arc<SftpFs>);
    type Output = Pushed;

    fn connect<'a>(
        &'a self,
        host: &'a FleetHost,
    ) -> BoxFuture<'a, Result<(Arc<SshConn>, Arc<SftpFs>), String>> {
        Box::pin(async move {
            let conn = self.sessions.open(host).await?;
            let fs = self.sftp(host, conn.clone()).await?;
            Ok((conn, fs))
        })
    }

    fn run<'a>(
        &'a self,
        (conn, fs): (Arc<SshConn>, Arc<SftpFs>),
        host: &'a FleetHost,
        stop: CancellationToken,
        _output: &'a OutputSink<'a>,
    ) -> BoxFuture<'a, Result<Pushed, String>> {
        Box::pin(async move {
            let control = AtomicU8::new(0);
            let dir = crate::files::expand_path(Path::new(&self.remote_dir), &fs.home(), true);
            let (run, id, events) = (self.run, host.id.clone(), self.service.events.clone());
            let progress = move |done: u64, total: Option<u64>| {
                events.emit(Event::Fleet(FleetEvent::Progress {
                    run,
                    host: id.clone(),
                    done,
                    total,
                }))
            };
            let on_conflict = match self.overwrite {
                Overwrite::Replace => OnConflict::Replace,
                Overwrite::Skip => OnConflict::Ask,
                Overwrite::KeepBoth => OnConflict::KeepBoth,
            };
            let copy = crate::files::transfer(
                &LocalFs,
                cfg!(unix),
                &self.local,
                fs.as_ref(),
                true,
                &dir,
                on_conflict,
                false,
                &control,
                &progress,
            );
            let result = tokio::select! {
                r = copy => r,
                () = stop.cancelled() => {
                    // Let the copy see the flag and delete its partial file.
                    control.store(crate::files::CANCEL, Ordering::SeqCst);
                    Err(TransferError::Cancelled)
                }
            };
            let target = match result {
                Ok(t) => t,
                Err(TransferError::Exists(name)) if self.overwrite == Overwrite::Skip => {
                    return Ok(Pushed {
                        target: None,
                        note: format!("{name} exists; skipped"),
                    });
                }
                Err(TransferError::Partial(_)) if self.overwrite == Overwrite::Skip => {
                    return Ok(Pushed {
                        target: None,
                        note: "an interrupted copy exists; skipped".into(),
                    });
                }
                Err(TransferError::Exists(name)) => return Err(format!("{name} exists")),
                Err(TransferError::Failed(e)) => return Err(e),
                Err(e) => return Err(format!("{e:?}").to_lowercase()),
            };
            let target = target.to_string_lossy().replace('\\', "/");
            let mut note = format!("uploaded to {target}");
            if let Some(mode) = &self.chmod {
                let recursive = std::fs::metadata(&self.local).is_ok_and(|m| m.is_dir());
                let cmd = format!(
                    "chmod {}{mode} -- {}",
                    if recursive { "-R " } else { "" },
                    shell_quote(&target)
                );
                let o = conn
                    .run_command_streaming(
                        &cmd,
                        None,
                        64 * 1024,
                        NO_SSH_TIMEOUT,
                        stop.cancelled(),
                        &mut |_, _| {},
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                if o.exit_status != Some(0) {
                    return Err(format!(
                        "uploaded, but chmod failed: {}",
                        first_line(&String::from_utf8_lossy(&o.stderr))
                    ));
                }
                note.push_str(&format!(" · chmod {mode}"));
            }
            Ok(Pushed {
                target: Some(target),
                note,
            })
        })
    }

    fn status(&self, p: &Pushed) -> HostStatus {
        if p.target.is_some() {
            HostStatus::Ok
        } else {
            HostStatus::Skipped
        }
    }
}

fn digest(exit: Option<u32>, stdout: &[u8], stderr: &[u8]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    exit.hash(&mut h);
    stdout.hash(&mut h);
    stderr.hash(&mut h);
    h.finish()
}

fn outcome_of<O>(r: &HostResult<O>, exit_status: Option<u32>, line: String) -> HostOutcome {
    HostOutcome {
        status: r.status,
        exit_status,
        duration_ms: r.elapsed.as_millis() as u64,
        first_line: match (&r.error, line.is_empty()) {
            (Some(e), true) => first_line(e),
            _ => line,
        },
        error: r.error.clone(),
        digest: None,
        truncated: false,
    }
}

impl Service {
    /// [`crate::Command::Fleet`].
    pub(super) async fn fleet(&self, cmd: FleetCommand) {
        match cmd {
            FleetCommand::Cancel { run } => {
                if let Some(t) = lock(&self.fleet_runs).get(&run) {
                    t.cancel();
                }
            }
            FleetCommand::PreviewInventory { path } => {
                let result = self.preview_inventory(&path).await;
                self.emit(Event::Fleet(FleetEvent::InventoryPreview { path, result }));
            }
            FleetCommand::ImportInventory { path, only } => {
                match self.import_inventory(&path, only).await {
                    Ok(n) => {
                        self.emit_profiles().await;
                        self.emit(Event::Toast(match n {
                            1 => "Imported 1 Host".into(),
                            n => format!("Imported {n} Hosts"),
                        }));
                    }
                    Err(e) => self.error("Inventory import", e),
                }
            }
            FleetCommand::Run {
                run,
                hosts,
                action,
                concurrency,
                timeout_secs,
                confirmed,
            } => {
                self.fleet_run(run, hosts, action, concurrency, timeout_secs, confirmed)
                    .await
            }
            FleetCommand::Probe { run, hosts } => self.fleet_probe(run, hosts).await,
            FleetCommand::Push {
                run,
                hosts,
                local,
                remote_dir,
                overwrite,
                chmod,
                concurrency,
                confirmed,
            } => {
                let job = PushJob {
                    local,
                    remote_dir,
                    overwrite,
                    chmod,
                };
                self.fleet_push(run, hosts, job, concurrency, confirmed)
                    .await
            }
            FleetCommand::Keys {
                run,
                hosts,
                op,
                concurrency,
                confirmed,
            } => {
                self.fleet_keys(run, hosts, op, concurrency, confirmed)
                    .await
            }
        }
    }

    fn refuse(&self, run: FleetRunId, message: impl Into<String>) {
        self.emit(Event::Fleet(FleetEvent::Refused {
            run,
            message: message.into(),
        }));
    }

    /// Resolve Hosts and their SSH targets, refusing Production without confirmation.
    async fn fleet_prepare(
        &self,
        run: FleetRunId,
        ids: &[ProfileId],
        needs_confirmation: bool,
        confirmed: bool,
    ) -> Option<(Vec<Host>, HashMap<ProfileId, SshTarget>)> {
        let profiles = match self.with_store(|s| s.profiles()).await {
            Ok(p) => p,
            Err(e) => {
                self.refuse(run, e.to_string());
                return None;
            }
        };
        let mut seen = HashSet::new();
        let hosts: Vec<Host> = ids
            .iter()
            .filter(|id| seen.insert((*id).clone()))
            .filter_map(|id| {
                profiles.iter().find_map(|p| match p {
                    Profile::Host(h) if &h.id == id => Some(h.clone()),
                    _ => None,
                })
            })
            .collect();
        if hosts.is_empty() {
            self.refuse(run, "no Hosts selected");
            return None;
        }
        let production = hosts
            .iter()
            .filter(|h| h.environment.is_production())
            .count();
        if needs_confirmation && production > 0 && !confirmed {
            self.refuse(
                run,
                format!("{production} Production Hosts selected: type the confirmation first"),
            );
            return None;
        }
        let mut targets = HashMap::new();
        for h in &hosts {
            match self.ssh_target(&h.id).await {
                Ok(t) => {
                    targets.insert(h.id.clone(), t);
                }
                // Reported per Host as unreachable.
                Err(e) => warn!(host = %h.name, error = %e, "fleet: no SSH target"),
            }
        }
        Some((hosts, targets))
    }

    async fn concurrency(&self, asked: Option<usize>) -> usize {
        match asked {
            Some(n) => n,
            None => self
                .with_store(|s| s.setting::<usize>(CONCURRENCY_KEY))
                .await
                .ok()
                .flatten()
                .unwrap_or(DEFAULT_CONCURRENCY),
        }
    }

    /// Register the run's cancel token and announce its Hosts.
    fn start_run(&self, run: FleetRunId, hosts: &[Host]) -> (CancellationToken, Vec<FleetHost>) {
        let token = CancellationToken::new();
        lock(&self.fleet_runs).insert(run, token.clone());
        let list: Vec<FleetHost> = hosts
            .iter()
            .map(|h| FleetHost {
                id: h.id.clone(),
                name: h.name.clone(),
            })
            .collect();
        self.emit(Event::Fleet(FleetEvent::Started {
            run,
            hosts: list.clone(),
        }));
        (token, list)
    }

    fn emit_status<O>(&self, run: FleetRunId, u: &Update<'_, O>) {
        if let Update::Status(h, status) = u {
            self.emit(Event::Fleet(FleetEvent::Status {
                run,
                host: h.id.clone(),
                status: *status,
            }));
        }
    }

    async fn fleet_run(
        &self,
        run: FleetRunId,
        ids: Vec<ProfileId>,
        action: FleetAction,
        concurrency: Option<usize>,
        timeout_secs: u64,
        confirmed: bool,
    ) {
        let (script, label) = match &action {
            FleetAction::Command(c) if c.trim().is_empty() => {
                return self.refuse(run, "the command is empty");
            }
            FleetAction::Command(c) => (c.trim().to_owned(), c.trim().to_owned()),
            FleetAction::Macro(id) => {
                let list = self.with_store(|s| s.macros()).await.unwrap_or_default();
                let Some(m) = list.into_iter().find(|m| &m.id == id) else {
                    return self.refuse(run, "the macro no longer exists");
                };
                match macro_script(&m.input) {
                    Ok(s) => (s.clone(), format!("macro {}: {s}", m.name)),
                    Err(e) => return self.refuse(run, format!("macro {}: {e}", m.name)),
                }
            }
        };
        let Some((hosts, targets)) = self.fleet_prepare(run, &ids, true, confirmed).await else {
            return;
        };
        let limits = RunLimits {
            concurrency: self.concurrency(concurrency).await,
            timeout: Duration::from_secs(timeout_secs.max(1)),
        };
        info!(
            run,
            hosts = hosts.len(),
            concurrency = limits.concurrency,
            "fleet run"
        );
        let (token, list) = self.start_run(run, &hosts);
        let _guard = RunGuard { service: self, run };
        let exec = CommandExec {
            sessions: Sessions {
                service: self,
                targets,
            },
            script,
        };
        let batcher = Arc::new(Batcher::default());
        let flusher = {
            let (b, events) = (batcher.clone(), self.events.clone());
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(FLUSH_EVERY).await;
                    b.flush(run, None, &|e| events.emit(e));
                }
            })
        };
        let done: Mutex<Vec<(Host, HostOutcome)>> = Mutex::default();
        let report = |u: Update<'_, CommandRun>| match u {
            Update::Output(h, stderr, bytes) => batcher.push(&h.id, stderr, bytes),
            Update::Done(r) => {
                batcher.flush(run, Some(&r.host.id), &|e| self.emit(e));
                let (exit, line, dig, truncated) = match &r.output {
                    Some(o) => {
                        let text = if o.stdout.is_empty() {
                            &o.stderr
                        } else {
                            &o.stdout
                        };
                        (
                            o.exit_status,
                            first_line(&String::from_utf8_lossy(text)),
                            Some(digest(o.exit_status, &o.stdout, &o.stderr)),
                            o.truncated,
                        )
                    }
                    None => (None, String::new(), None, false),
                };
                let mut outcome = outcome_of(&r, exit, line);
                outcome.digest = dig.filter(|_| r.status.is_final() && r.output.is_some());
                outcome.truncated = truncated;
                if outcome.error.is_none()
                    && let Some(c) = exit.filter(|c| *c != 0)
                {
                    outcome.error = Some(format!("exit status {c}"));
                }
                if let Some(h) = hosts.iter().find(|h| h.id == r.host.id) {
                    lock(&done).push((h.clone(), outcome.clone()));
                }
                self.emit(Event::Fleet(FleetEvent::Done {
                    run,
                    host: r.host.id,
                    outcome,
                }));
            }
            u => self.emit_status(run, &u),
        };
        run_hosts(&exec, list, limits, token.clone(), &report).await;
        flusher.abort();
        batcher.flush(run, None, &|e| self.emit(e));
        let cancelled = token.is_cancelled();
        let done = std::mem::take(&mut *lock(&done));
        for (h, o) in done {
            self.fleet_history(&h, label.clone(), &o).await;
        }
        self.emit(Event::Fleet(FleetEvent::Finished { run, cancelled }));
    }

    async fn fleet_probe(&self, run: FleetRunId, ids: Vec<ProfileId>) {
        let Some((hosts, targets)) = self.fleet_prepare(run, &ids, false, false).await else {
            return;
        };
        let limits = RunLimits {
            concurrency: self.concurrency(None).await,
            timeout: Duration::from_secs(PROBE_TIMEOUT_SECS),
        };
        let (token, list) = self.start_run(run, &hosts);
        let _guard = RunGuard { service: self, run };
        let exec = ProbeExec {
            sessions: Sessions {
                service: self,
                targets,
            },
        };
        let report = |u: Update<'_, ProbeReport>| match u {
            Update::Done(r) => {
                let result = match (r.output, r.status) {
                    (Some(p), HostStatus::Ok) => Ok(p),
                    (_, status) => Err(r.error.unwrap_or_else(|| status.label().to_owned())),
                };
                self.emit(Event::Fleet(FleetEvent::Probed {
                    run,
                    host: r.host.id,
                    result,
                }));
            }
            Update::Output(..) => {}
            u => self.emit_status(run, &u),
        };
        run_hosts(&exec, list, limits, token.clone(), &report).await;
        self.emit(Event::Fleet(FleetEvent::Finished {
            run,
            cancelled: token.is_cancelled(),
        }));
    }

    async fn fleet_keys(
        &self,
        run: FleetRunId,
        ids: Vec<ProfileId>,
        op: KeyOp,
        concurrency: Option<usize>,
        confirmed: bool,
    ) {
        let key_text = match &op {
            KeyOp::Audit(k) => k.clone().filter(|k| !k.trim().is_empty()),
            KeyOp::Add(k) | KeyOp::Remove(k) => Some(k.clone()),
        };
        let key = match key_text.as_deref().map(keys::parse_public_key) {
            None => None,
            Some(Ok(k)) => Some(k),
            Some(Err(e)) => return self.refuse(run, e.to_string()),
        };
        let Some((hosts, targets)) = self.fleet_prepare(run, &ids, op.changes(), confirmed).await
        else {
            return;
        };
        let limits = RunLimits {
            concurrency: self.concurrency(concurrency).await,
            timeout: Duration::from_secs(60),
        };
        let (token, list) = self.start_run(run, &hosts);
        let _guard = RunGuard { service: self, run };
        let label = match (&op, &key) {
            (KeyOp::Add(_), Some(k)) => format!(
                "authorized_keys add {}",
                k.fingerprint().unwrap_or_default()
            ),
            (KeyOp::Remove(_), Some(k)) => format!(
                "authorized_keys remove {}",
                k.fingerprint().unwrap_or_default()
            ),
            _ => String::new(),
        };
        let changes = op.changes();
        let exec = KeysExec {
            sessions: Sessions {
                service: self,
                targets,
            },
            op,
            key,
        };
        let done: Mutex<Vec<(Host, HostOutcome)>> = Mutex::default();
        let report = |u: Update<'_, KeyReport>| match u {
            Update::Done(r) => {
                let line = r
                    .output
                    .as_ref()
                    .map(|k| k.summary.clone())
                    .unwrap_or_default();
                let outcome = outcome_of(&r, None, line);
                if let Some(h) = hosts.iter().find(|h| h.id == r.host.id) {
                    lock(&done).push((h.clone(), outcome.clone()));
                }
                let result = match r.output {
                    Some(k) => Ok(k),
                    None => Err(r.error.unwrap_or_else(|| r.status.label().to_owned())),
                };
                self.emit(Event::Fleet(FleetEvent::Keys {
                    run,
                    host: r.host.id.clone(),
                    result,
                }));
                self.emit(Event::Fleet(FleetEvent::Done {
                    run,
                    host: r.host.id,
                    outcome,
                }));
            }
            Update::Output(..) => {}
            u => self.emit_status(run, &u),
        };
        run_hosts(&exec, list, limits, token.clone(), &report).await;
        if changes {
            let done = std::mem::take(&mut *lock(&done));
            for (h, o) in done {
                self.fleet_history(&h, label.clone(), &o).await;
            }
        }
        self.emit(Event::Fleet(FleetEvent::Finished {
            run,
            cancelled: token.is_cancelled(),
        }));
    }

    async fn fleet_push(
        &self,
        run: FleetRunId,
        ids: Vec<ProfileId>,
        job: PushJob,
        concurrency: Option<usize>,
        confirmed: bool,
    ) {
        if job.remote_dir.trim().is_empty() {
            return self.refuse(run, "choose the remote folder");
        }
        if let Some(m) = &job.chmod
            && !valid_mode(m)
        {
            return self.refuse(run, format!("{m} is not an octal mode such as 644 or 755"));
        }
        let local = job.local.clone();
        if tokio::fs::metadata(&local).await.is_err() {
            return self.refuse(run, format!("{} does not exist", local.display()));
        }
        let Some((hosts, targets)) = self.fleet_prepare(run, &ids, true, confirmed).await else {
            return;
        };
        let limits = RunLimits {
            concurrency: self.concurrency(concurrency).await,
            // Large uploads: the user stops them with cancel-all.
            timeout: NO_SSH_TIMEOUT,
        };
        let (token, list) = self.start_run(run, &hosts);
        let _guard = RunGuard { service: self, run };
        let label = format!("push {} → {}", local.display(), job.remote_dir.trim());
        let exec = PushExec {
            sessions: Sessions {
                service: self,
                targets,
            },
            service: self,
            run,
            local,
            remote_dir: job.remote_dir.trim().to_owned(),
            overwrite: job.overwrite,
            chmod: job.chmod,
        };
        let done: Mutex<Vec<(Host, HostOutcome)>> = Mutex::default();
        let report = |u: Update<'_, Pushed>| match u {
            Update::Done(r) => {
                let line = r
                    .output
                    .as_ref()
                    .map(|p| p.note.clone())
                    .unwrap_or_default();
                let outcome = outcome_of(&r, None, line);
                if let Some(h) = hosts.iter().find(|h| h.id == r.host.id) {
                    lock(&done).push((h.clone(), outcome.clone()));
                }
                self.emit(Event::Fleet(FleetEvent::Done {
                    run,
                    host: r.host.id,
                    outcome,
                }));
            }
            Update::Output(..) => {}
            u => self.emit_status(run, &u),
        };
        run_hosts(&exec, list, limits, token.clone(), &report).await;
        let done = std::mem::take(&mut *lock(&done));
        for (h, o) in done {
            self.fleet_history(&h, label.clone(), &o).await;
        }
        self.emit(Event::Fleet(FleetEvent::Finished {
            run,
            cancelled: token.is_cancelled(),
        }));
    }

    /// One history entry per Host, tagged `fleet`.
    async fn fleet_history(&self, host: &Host, text: String, o: &HostOutcome) {
        let entry = HistoryEntry {
            id: 0,
            connection_id: Some(host.id.clone()),
            connection_name: host.name.clone(),
            sql: text,
            started_at: now_ms() - o.duration_ms as i64,
            duration_ms: o.duration_ms as i64,
            rows: None,
            affected: None,
            status: match o.status {
                HostStatus::Ok | HostStatus::Skipped => HistoryStatus::Ok,
                HostStatus::Cancelled => HistoryStatus::Cancelled,
                _ => HistoryStatus::Error,
            },
            error: o
                .error
                .clone()
                .or_else(|| o.status.is_failure().then(|| o.status.label().to_owned())),
            tags: vec![HISTORY_TAG.to_owned(), "ssh".to_owned()],
            has_plan: false,
        };
        if let Err(e) = self.with_store(move |s| s.add_history(&entry)).await {
            warn!(error = %e, "fleet history write failed");
        }
    }

    async fn read_inventory(
        &self,
        path: &Path,
    ) -> Result<Vec<crate::fleet::ansible::InventoryHost>, String> {
        let text = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| format!("{}: {e}", path.display()))?;
        crate::fleet::ansible::parse_inventory(&text).map_err(|e| e.to_string())
    }

    async fn preview_inventory(
        &self,
        path: &Path,
    ) -> Result<Vec<crate::ssh_import::SshImportCandidate>, String> {
        let hosts = self.read_inventory(path).await?;
        let saved: HashSet<String> = self
            .saved_host_names()
            .await
            .map_err(|e| e.to_string())?
            .into_keys()
            .collect();
        Ok(crate::fleet::ansible::candidates(
            &hosts,
            &saved,
            &Self::default_ssh_user(),
        ))
    }

    async fn import_inventory(
        &self,
        path: &Path,
        only: Option<Vec<String>>,
    ) -> Result<usize, String> {
        let hosts = self.read_inventory(path).await?;
        let saved = self.saved_host_names().await.map_err(|e| e.to_string())?;
        let only: Option<HashSet<String>> = only.map(|o| o.into_iter().collect());
        let new =
            crate::fleet::ansible::plan(&hosts, &saved, only.as_ref(), &Self::default_ssh_user());
        self.with_store(move |s| {
            for h in &new {
                s.save_profile(&Profile::Host(h.clone()))?;
            }
            Ok(new.len())
        })
        .await
        .map_err(|e| e.to_string())
    }
}

/// A push's parameters.
struct PushJob {
    local: PathBuf,
    remote_dir: String,
    overwrite: Overwrite,
    chmod: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_split_across_pieces() {
        let mut buf = "héllo".as_bytes()[..2].to_vec();
        assert_eq!(take_utf8(&mut buf), "h");
        assert_eq!(buf, [0xc3]);
        buf.extend_from_slice(&"héllo".as_bytes()[2..]);
        assert_eq!(take_utf8(&mut buf), "éllo");
        assert!(buf.is_empty());
        let mut bad = vec![b'a', 0xff, b'b'];
        assert_eq!(take_utf8(&mut bad), "a\u{fffd}b");
    }

    #[test]
    fn digests_follow_output_and_exit() {
        assert_eq!(digest(Some(0), b"a", b""), digest(Some(0), b"a", b""));
        assert_ne!(digest(Some(0), b"a", b""), digest(Some(1), b"a", b""));
        assert_ne!(digest(Some(0), b"a", b""), digest(Some(0), b"b", b""));
    }
}
