//! The Fleet tab's state without GPUI: which Hosts a selection holds, the Production
//! confirmation, per-Host rows built from [`FleetEvent`]s, streamed output, the "failed
//! only" / "differs from majority" filters, copy-all text and dashboard sorting.

use switchyard_core::fleet::keys::AuthorizedKey;
use switchyard_core::fleet::probe::{ProbeFlag, ProbeReport, format_uptime};
use switchyard_core::fleet::runner::outliers;
use switchyard_core::fleet::{FleetEvent, FleetRunId, HostStatus, KeyReport};
use switchyard_core::store::{EnvironmentLabel, Host, ProfileId};

/// What the user types to act on Production Hosts.
pub const CONFIRM_WORD: &str = "PRODUCTION";

/// Lines kept of one Host's output.
pub const MAX_LINES: usize = 100_000;

/// Which Hosts a Fleet tab works on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pick {
    /// Every saved Host.
    All,
    /// The Hosts of a session folder.
    Folder(String),
    /// The Hosts carrying a tag.
    Tag(String),
    /// Hosts chosen one by one.
    Hosts(Vec<ProfileId>),
}

impl Pick {
    /// The Hosts it selects, in sidebar order.
    pub fn hosts<'a>(&self, all: impl Iterator<Item = &'a Host>) -> Vec<&'a Host> {
        all.filter(|h| match self {
            Pick::All => true,
            Pick::Folder(f) => h.folder.as_deref().map(str::trim) == Some(f.as_str()),
            Pick::Tag(t) => switchyard_core::store::tags::has_tag(&h.tags, t),
            Pick::Hosts(ids) => ids.contains(&h.id),
        })
        .collect()
    }

    /// Short description (`folder Prod`, `#role:web`, `3 Hosts`).
    pub fn label(&self, count: usize) -> String {
        match self {
            Pick::All => "all Hosts".into(),
            Pick::Folder(f) => format!("folder {f}"),
            Pick::Tag(t) => format!("#{t}"),
            Pick::Hosts(_) if count == 1 => "1 Host".into(),
            Pick::Hosts(_) => format!("{count} Hosts"),
        }
    }
}

/// How many of `hosts` are Production.
pub fn production_count(hosts: &[&Host]) -> usize {
    hosts
        .iter()
        .filter(|h| h.environment.is_production())
        .count()
}

/// Whether the typed confirmation is right ([`CONFIRM_WORD`], case-sensitive, spaces
/// around ignored).
pub fn confirmation_ok(typed: &str) -> bool {
    typed.trim() == CONFIRM_WORD
}

/// A Host's output, kept as lines (stdout and stderr interleaved by arrival).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutputLog {
    /// Complete lines: (stderr, text).
    pub lines: Vec<(bool, String)>,
    partial: [String; 2],
    /// Lines past [`MAX_LINES`] were dropped.
    pub truncated: bool,
}

impl OutputLog {
    /// Add a piece of output.
    pub fn push(&mut self, stderr: bool, text: &str) {
        let slot = usize::from(stderr);
        let mut buf = std::mem::take(&mut self.partial[slot]);
        buf.push_str(text);
        let mut rest = buf.as_str();
        while let Some(at) = rest.find('\n') {
            let line = rest[..at].trim_end_matches('\r');
            if self.lines.len() < MAX_LINES {
                self.lines.push((stderr, line.to_owned()));
            } else {
                self.truncated = true;
            }
            rest = &rest[at + 1..];
        }
        self.partial[slot] = rest.to_owned();
    }

    /// Every line, unfinished ones last.
    pub fn all(&self) -> Vec<(bool, String)> {
        let mut out = self.lines.clone();
        for (i, p) in self.partial.iter().enumerate() {
            if !p.is_empty() {
                out.push((i == 1, p.clone()));
            }
        }
        out
    }

    /// Line `i` (unfinished lines after the complete ones): (stderr, text).
    pub fn line(&self, i: usize) -> Option<(bool, &str)> {
        if let Some((e, l)) = self.lines.get(i) {
            return Some((*e, l.as_str()));
        }
        let mut k = i - self.lines.len();
        for (slot, p) in self.partial.iter().enumerate() {
            if p.is_empty() {
                continue;
            }
            if k == 0 {
                return Some((slot == 1, p.as_str()));
            }
            k -= 1;
        }
        None
    }

    /// Number of lines, unfinished ones included.
    pub fn len(&self) -> usize {
        self.lines.len() + self.partial.iter().filter(|p| !p.is_empty()).count()
    }

    /// Whether nothing was printed.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Plain text.
    pub fn text(&self) -> String {
        let mut s = String::new();
        for (_, l) in self.all() {
            s.push_str(&l);
            s.push('\n');
        }
        s
    }
}

/// One Host of a Fleet tab.
#[derive(Clone, Debug, PartialEq)]
pub struct HostRow {
    /// Host id.
    pub id: ProfileId,
    /// Name.
    pub name: String,
    /// Environment.
    pub env: EnvironmentLabel,
    /// Where it is (`None` before the first run).
    pub status: Option<HostStatus>,
    /// Exit status of a command.
    pub exit: Option<u32>,
    /// Job time.
    pub duration_ms: Option<u64>,
    /// First output line, or the result.
    pub first_line: String,
    /// Error.
    pub error: Option<String>,
    /// Output digest (majority filter).
    pub digest: Option<u64>,
    /// Output.
    pub output: OutputLog,
    /// Push progress.
    pub progress: Option<(u64, Option<u64>)>,
    /// Last probe.
    pub probe: Option<Result<ProbeReport, String>>,
    /// Last key report.
    pub keys: Option<Result<KeyReport, String>>,
}

impl HostRow {
    /// A row for `h`, not run yet.
    pub fn new(h: &Host) -> Self {
        Self {
            id: h.id.clone(),
            name: h.name.clone(),
            env: h.environment,
            status: None,
            exit: None,
            duration_ms: None,
            first_line: String::new(),
            error: None,
            digest: None,
            output: OutputLog::default(),
            progress: None,
            probe: None,
            keys: None,
        }
    }

    /// Forget the previous run (keeps the probe, which refreshes in place).
    pub fn reset(&mut self) {
        self.status = None;
        self.exit = None;
        self.duration_ms = None;
        self.first_line.clear();
        self.error = None;
        self.digest = None;
        self.output = OutputLog::default();
        self.progress = None;
        self.keys = None;
    }

    /// The probe's flags, and "unreachable" when it failed.
    pub fn flags(&self) -> Vec<String> {
        match &self.probe {
            Some(Ok(p)) => p.flags().iter().map(ProbeFlag::label).collect(),
            Some(Err(_)) => vec!["unreachable".into()],
            None => Vec::new(),
        }
    }

    /// Keys of the last key report.
    pub fn key_list(&self) -> &[AuthorizedKey] {
        match &self.keys {
            Some(Ok(r)) => &r.keys,
            _ => &[],
        }
    }
}

/// The Hosts of a tab and their results.
#[derive(Clone, Debug, Default)]
pub struct Rows {
    /// Rows in selection order.
    pub rows: Vec<HostRow>,
    /// The run in progress (or last run) of the tab's job.
    pub run: Option<FleetRunId>,
    /// The dashboard's probe round in progress.
    pub probe_run: Option<FleetRunId>,
    /// The job is running.
    pub running: bool,
    /// Why the last run was refused.
    pub refused: Option<String>,
}

impl Rows {
    /// Rows for `hosts`, keeping results of Hosts already listed.
    pub fn set_hosts(&mut self, hosts: &[&Host]) {
        let mut old = std::mem::take(&mut self.rows);
        self.rows = hosts
            .iter()
            .map(|h| match old.iter().position(|r| r.id == h.id) {
                Some(i) => {
                    let mut r = old.swap_remove(i);
                    r.name = h.name.clone();
                    r.env = h.environment;
                    r
                }
                None => HostRow::new(h),
            })
            .collect();
    }

    fn row(&mut self, id: &ProfileId) -> Option<&mut HostRow> {
        self.rows.iter_mut().find(|r| &r.id == id)
    }

    /// Start a run: clear the previous results.
    pub fn start(&mut self, run: FleetRunId) {
        self.run = Some(run);
        self.running = true;
        self.refused = None;
        for r in &mut self.rows {
            r.reset();
        }
    }

    /// Apply an event of this tab's runs; returns whether it was one.
    pub fn apply(&mut self, ev: &FleetEvent) -> bool {
        let run = match ev {
            FleetEvent::Started { run, .. }
            | FleetEvent::Refused { run, .. }
            | FleetEvent::Status { run, .. }
            | FleetEvent::Output { run, .. }
            | FleetEvent::Progress { run, .. }
            | FleetEvent::Done { run, .. }
            | FleetEvent::Probed { run, .. }
            | FleetEvent::Keys { run, .. }
            | FleetEvent::Finished { run, .. } => *run,
            FleetEvent::InventoryPreview { .. } => return false,
        };
        let probe = self.probe_run == Some(run);
        if !probe && self.run != Some(run) {
            return false;
        }
        match ev {
            FleetEvent::Refused { message, .. } => {
                if probe {
                    self.probe_run = None;
                } else {
                    self.running = false;
                    self.refused = Some(message.clone());
                }
            }
            FleetEvent::Status { host, status, .. } if !probe => {
                if let Some(r) = self.row(host) {
                    r.status = Some(*status);
                }
            }
            FleetEvent::Output {
                host, stderr, text, ..
            } => {
                if let Some(r) = self.row(host) {
                    r.output.push(*stderr, text);
                }
            }
            FleetEvent::Progress {
                host, done, total, ..
            } => {
                if let Some(r) = self.row(host) {
                    r.progress = Some((*done, *total));
                }
            }
            FleetEvent::Done { host, outcome, .. } => {
                if let Some(r) = self.row(host) {
                    r.status = Some(outcome.status);
                    r.exit = outcome.exit_status;
                    r.duration_ms = Some(outcome.duration_ms);
                    r.first_line = outcome.first_line.clone();
                    r.error = outcome.error.clone();
                    r.digest = outcome.digest;
                    if r.output.is_empty()
                        && let Some(e) = &outcome.error
                    {
                        r.output.push(true, &format!("{e}\n"));
                    }
                }
            }
            FleetEvent::Probed { host, result, .. } => {
                if let Some(r) = self.row(host) {
                    r.probe = Some(result.clone());
                }
            }
            FleetEvent::Keys { host, result, .. } => {
                if let Some(r) = self.row(host) {
                    r.keys = Some(result.clone());
                }
            }
            FleetEvent::Finished { .. } => {
                if probe {
                    self.probe_run = None;
                } else {
                    self.running = false;
                }
            }
            _ => {}
        }
        true
    }

    /// Indexes of the rows to show.
    pub fn visible(&self, failed_only: bool, differs_only: bool) -> Vec<usize> {
        let differ: Vec<ProfileId> = if differs_only {
            let keys: Vec<(ProfileId, Option<(Option<HostStatus>, Option<u64>)>)> = self
                .rows
                .iter()
                .map(|r| {
                    let done = r.status.is_some_and(HostStatus::is_final);
                    (r.id.clone(), done.then_some((r.status, r.digest)))
                })
                .collect();
            outliers(&keys)
        } else {
            Vec::new()
        };
        (0..self.rows.len())
            .filter(|&i| {
                let r = &self.rows[i];
                (!failed_only || r.status.is_some_and(HostStatus::is_failure))
                    && (!differs_only || differ.contains(&r.id))
            })
            .collect()
    }

    /// Counts by outcome: (ok, failed, other finished, still running or queued).
    pub fn tally(&self) -> (usize, usize, usize, usize) {
        let mut t = (0, 0, 0, 0);
        for r in &self.rows {
            match r.status {
                Some(HostStatus::Ok) => t.0 += 1,
                Some(s) if s.is_failure() => t.1 += 1,
                Some(s) if s.is_final() => t.2 += 1,
                Some(_) => t.3 += 1,
                None => {}
            }
        }
        t
    }

    /// Every shown Host's result and output, for the clipboard.
    pub fn copy_all(&self, shown: &[usize]) -> String {
        let mut s = String::new();
        for &i in shown {
            let r = &self.rows[i];
            let status = r.status.map_or("not run", HostStatus::label);
            s.push_str(&format!("=== {} · {status}", r.name));
            if let Some(c) = r.exit {
                s.push_str(&format!(" · exit {c}"));
            }
            if let Some(d) = r.duration_ms {
                s.push_str(&format!(" · {d} ms"));
            }
            s.push_str(" ===\n");
            let out = r.output.text();
            if out.is_empty() {
                if !r.first_line.is_empty() {
                    s.push_str(&r.first_line);
                    s.push('\n');
                }
            } else {
                s.push_str(&out);
            }
            s.push('\n');
        }
        s
    }
}

/// Dashboard columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DashCol {
    /// Host name.
    Host,
    /// Flags first.
    Health,
    /// Operating system.
    Os,
    /// Uptime.
    Uptime,
    /// 1-minute load per CPU.
    Load,
    /// Fullest disk.
    Disk,
    /// Memory use.
    Memory,
}

impl DashCol {
    /// Every column with its header and width (0 = rest).
    pub const ALL: [(DashCol, &'static str, f32); 7] = [
        (DashCol::Host, "HOST", 170.),
        (DashCol::Health, "HEALTH", 0.),
        (DashCol::Os, "OS", 190.),
        (DashCol::Uptime, "UPTIME", 80.),
        (DashCol::Load, "LOAD", 110.),
        (DashCol::Disk, "DISK", 120.),
        (DashCol::Memory, "MEMORY", 80.),
    ];
}

/// One dashboard cell's text.
pub fn dash_cell(r: &HostRow, col: DashCol) -> String {
    let p = match &r.probe {
        Some(Ok(p)) => p,
        Some(Err(e)) if col == DashCol::Health => return format!("unreachable · {e}"),
        _ if col == DashCol::Host => return r.name.clone(),
        _ => return String::new(),
    };
    match col {
        DashCol::Host => r.name.clone(),
        DashCol::Health => {
            let f = r.flags();
            if f.is_empty() {
                "ok".into()
            } else {
                f.join(" · ")
            }
        }
        DashCol::Os => p.os.clone().unwrap_or_default(),
        DashCol::Uptime => p.uptime_secs.map(format_uptime).unwrap_or_default(),
        DashCol::Load => match (p.load, p.cpus) {
            (Some(l), Some(c)) => format!("{:.2} · {c} cpu", l[0]),
            (Some(l), None) => format!("{:.2} {:.2} {:.2}", l[0], l[1], l[2]),
            _ => String::new(),
        },
        DashCol::Disk => p
            .fullest_disk()
            .map(|d| format!("{:.0}% {}", d.use_percent, d.mount))
            .unwrap_or_default(),
        DashCol::Memory => p
            .memory
            .map(|m| format!("{:.0}%", m.used_percent()))
            .unwrap_or_default(),
    }
}

/// Numeric sort key of a cell (`None` sorts last).
fn dash_number(r: &HostRow, col: DashCol) -> Option<f64> {
    let p = match &r.probe {
        Some(Ok(p)) => p,
        // Unreachable Hosts are the least healthy.
        Some(Err(_)) if col == DashCol::Health => return Some(1000.0),
        _ => return None,
    };
    match col {
        DashCol::Health => Some(p.flags().len() as f64),
        DashCol::Uptime => p.uptime_secs.map(|s| s as f64),
        DashCol::Load => p.load.map(|l| l[0] / f64::from(p.cpus.unwrap_or(1).max(1))),
        DashCol::Disk => p.fullest_disk().map(|d| d.use_percent),
        DashCol::Memory => p.memory.map(|m| m.used_percent()),
        DashCol::Host | DashCol::Os => None,
    }
}

/// Row order for the dashboard sorted by `col`.
pub fn dash_order(rows: &[HostRow], col: DashCol, desc: bool) -> Vec<usize> {
    let mut ix: Vec<usize> = (0..rows.len()).collect();
    match col {
        DashCol::Host | DashCol::Os => ix.sort_by(|&a, &b| {
            let (x, y) = (
                dash_cell(&rows[a], col).to_lowercase(),
                dash_cell(&rows[b], col).to_lowercase(),
            );
            if desc { y.cmp(&x) } else { x.cmp(&y) }
        }),
        _ => ix.sort_by(
            |&a, &b| match (dash_number(&rows[a], col), dash_number(&rows[b], col)) {
                (Some(x), Some(y)) if desc => y.total_cmp(&x),
                (Some(x), Some(y)) => x.total_cmp(&y),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            },
        ),
    }
    ix
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use switchyard_core::fleet::HostOutcome;
    use switchyard_core::fleet::probe::{Disk, Memory};

    fn host(name: &str, env: EnvironmentLabel, folder: Option<&str>, tags: &[&str]) -> Host {
        let mut h = Host::new(name, "10.0.0.1", "ops");
        h.environment = env;
        h.folder = folder.map(str::to_owned);
        h.tags = tags.iter().map(|t| (*t).to_owned()).collect();
        h
    }

    fn sample() -> Vec<Host> {
        vec![
            host(
                "web-1",
                EnvironmentLabel::Production,
                Some("Prod"),
                &["web"],
            ),
            host(
                "web-2",
                EnvironmentLabel::Production,
                Some("Prod"),
                &["web"],
            ),
            host("db-1", EnvironmentLabel::Staging, None, &["role:db"]),
        ]
    }

    #[test]
    fn picks_and_confirmation() {
        let all = sample();
        let names = |p: &Pick| -> Vec<String> {
            p.hosts(all.iter()).iter().map(|h| h.name.clone()).collect()
        };
        assert_eq!(names(&Pick::All).len(), 3);
        assert_eq!(names(&Pick::Folder("Prod".into())), ["web-1", "web-2"]);
        assert_eq!(names(&Pick::Tag("ROLE:db".into())), ["db-1"]);
        assert_eq!(
            names(&Pick::Hosts(vec![all[2].id.clone(), all[0].id.clone()])),
            ["web-1", "db-1"]
        );
        assert_eq!(Pick::Tag("web".into()).label(2), "#web");
        assert_eq!(Pick::Hosts(vec![]).label(1), "1 Host");
        let picked = Pick::Folder("Prod".into()).hosts(all.iter());
        assert_eq!(production_count(&picked), 2);
        assert!(confirmation_ok(" PRODUCTION "));
        assert!(!confirmation_ok("production"));
    }

    #[test]
    fn output_lines_stream_in_pieces() {
        let mut o = OutputLog::default();
        o.push(false, "hel");
        o.push(true, "warn\n");
        o.push(false, "lo\r\nwor");
        assert_eq!(o.lines, [(true, "warn".into()), (false, "hello".into())]);
        assert_eq!(o.len(), 3);
        assert_eq!(o.line(1), Some((false, "hello")));
        assert_eq!(o.line(2), Some((false, "wor")));
        assert_eq!(o.line(3), None);
        assert_eq!(o.text(), "warn\nhello\nwor\n");
    }

    fn done(run: u64, id: &ProfileId, status: HostStatus, digest: u64) -> FleetEvent {
        FleetEvent::Done {
            run,
            host: id.clone(),
            outcome: HostOutcome {
                status,
                exit_status: Some(u32::from(status != HostStatus::Ok)),
                duration_ms: 12,
                first_line: "out".into(),
                error: None,
                digest: Some(digest),
                truncated: false,
            },
        }
    }

    #[test]
    fn events_filters_and_copy() {
        let all = sample();
        let refs: Vec<&Host> = all.iter().collect();
        let mut rows = Rows::default();
        rows.set_hosts(&refs);
        rows.start(7);
        // Another run's events are ignored.
        assert!(!rows.apply(&done(8, &all[0].id, HostStatus::Ok, 1)));
        assert!(rows.apply(&FleetEvent::Output {
            run: 7,
            host: all[0].id.clone(),
            stderr: false,
            text: "same\n".into(),
        }));
        rows.apply(&done(7, &all[0].id, HostStatus::Ok, 1));
        rows.apply(&done(7, &all[1].id, HostStatus::Ok, 1));
        rows.apply(&done(7, &all[2].id, HostStatus::Failed, 2));
        assert_eq!(rows.visible(false, false), [0, 1, 2]);
        assert_eq!(rows.visible(true, false), [2]);
        assert_eq!(rows.visible(false, true), [2]);
        assert_eq!(rows.tally(), (2, 1, 0, 0));
        assert!(rows.running);
        rows.apply(&FleetEvent::Finished {
            run: 7,
            cancelled: false,
        });
        assert!(!rows.running);
        let text = rows.copy_all(&[0, 2]);
        assert_eq!(
            text,
            "=== web-1 · ok · exit 0 · 12 ms ===\nsame\n\n=== db-1 · failed · exit 1 · 12 ms ===\nout\n\n"
        );
        // A new selection keeps the results of Hosts still in it.
        rows.set_hosts(&refs[..1]);
        assert_eq!(rows.rows.len(), 1);
        assert_eq!(rows.rows[0].status, Some(HostStatus::Ok));
    }

    fn probed(rows: &mut Rows, i: usize, disk: f64, mem_avail: u64) {
        let r = ProbeReport {
            disks: vec![Disk {
                filesystem: "/dev/sda1".into(),
                mount: "/".into(),
                size_kb: 100,
                used_kb: disk as u64,
                use_percent: disk,
            }],
            memory: Some(Memory {
                total: 100,
                available: mem_avail,
            }),
            failed_units: Some(Vec::new()),
            load: Some([0.5, 0.4, 0.3]),
            cpus: Some(2),
            ..ProbeReport::default()
        };
        rows.rows[i].probe = Some(Ok(r));
    }

    #[test]
    fn dashboard_cells_and_sorting() {
        let all = sample();
        let refs: Vec<&Host> = all.iter().collect();
        let mut rows = Rows::default();
        rows.set_hosts(&refs);
        probed(&mut rows, 0, 95.0, 50);
        probed(&mut rows, 1, 40.0, 5);
        rows.rows[2].probe = Some(Err("connection refused".into()));
        assert_eq!(dash_cell(&rows.rows[0], DashCol::Disk), "95% /");
        assert_eq!(dash_cell(&rows.rows[0], DashCol::Health), "/ 95% full");
        assert_eq!(dash_cell(&rows.rows[1], DashCol::Memory), "95%");
        assert_eq!(dash_cell(&rows.rows[1], DashCol::Load), "0.50 · 2 cpu");
        assert_eq!(rows.rows[2].flags(), ["unreachable"]);
        assert_eq!(
            dash_cell(&rows.rows[2], DashCol::Health),
            "unreachable · connection refused"
        );
        assert_eq!(dash_order(&rows.rows, DashCol::Disk, true), [0, 1, 2]);
        assert_eq!(dash_order(&rows.rows, DashCol::Disk, false), [1, 0, 2]);
        assert_eq!(dash_order(&rows.rows, DashCol::Health, true), [2, 0, 1]);
        assert_eq!(dash_order(&rows.rows, DashCol::Host, false), [2, 0, 1]);
    }
}
