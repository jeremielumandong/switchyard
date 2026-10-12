//! The Fleet runner: one job on many Hosts at once, at most `concurrency` at a time, each
//! with a timeout, all stoppable with one cancel.
//!
//! The runner knows nothing about SSH: a [`FleetExecutor`] opens each Host's session and
//! runs the job on it, so tests drive it with a fake. The timeout covers the job, not the
//! connection: a login may wait on the user (host key, password, MFA), which has its own
//! limits, and cancel-all stops it too.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use switchyard_store::ProfileId;
use tokio_util::sync::CancellationToken;

/// Most Hosts worked on at once, whatever the setting says.
pub const MAX_CONCURRENCY: usize = 64;

/// One Host of a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FleetHost {
    /// Host id.
    pub id: ProfileId,
    /// Display name.
    pub name: String,
}

/// Where a Host is in a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HostStatus {
    /// Waiting for a free slot.
    Queued,
    /// Opening (or reusing) its SSH session.
    Connecting,
    /// The job is running.
    Running,
    /// Finished and succeeded.
    Ok,
    /// Finished and failed (non-zero exit, refused change, error).
    Failed,
    /// The session could not be opened.
    Unreachable,
    /// Stopped at its timeout.
    TimedOut,
    /// Stopped by cancel-all (or never started).
    Cancelled,
    /// Nothing to do on this Host (exists, already present).
    Skipped,
}

impl HostStatus {
    /// Whether the Host is done.
    pub fn is_final(self) -> bool {
        !matches!(
            self,
            HostStatus::Queued | HostStatus::Connecting | HostStatus::Running
        )
    }

    /// Whether it counts as a failure ("failed only" filter).
    pub fn is_failure(self) -> bool {
        matches!(
            self,
            HostStatus::Failed | HostStatus::Unreachable | HostStatus::TimedOut
        )
    }

    /// Short label.
    pub fn label(self) -> &'static str {
        match self {
            HostStatus::Queued => "queued",
            HostStatus::Connecting => "connecting",
            HostStatus::Running => "running",
            HostStatus::Ok => "ok",
            HostStatus::Failed => "failed",
            HostStatus::Unreachable => "unreachable",
            HostStatus::TimedOut => "timed out",
            HostStatus::Cancelled => "cancelled",
            HostStatus::Skipped => "skipped",
        }
    }
}

/// How a run is spread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunLimits {
    /// Hosts worked on at once (clamped to 1..=[`MAX_CONCURRENCY`]).
    pub concurrency: usize,
    /// Longest a job may run on one Host.
    pub timeout: Duration,
}

/// Output as it arrives: `(stderr, bytes)`.
pub type OutputSink<'a> = dyn Fn(bool, &[u8]) + Send + Sync + 'a;

/// Opens Hosts and runs one job on them.
pub trait FleetExecutor: Send + Sync {
    /// An open session (shared SSH connection, SFTP, …).
    type Session: Send;
    /// What the job returns.
    type Output: Send;

    /// Open (or reuse) the Host's session.
    fn connect<'a>(&'a self, host: &'a FleetHost) -> BoxFuture<'a, Result<Self::Session, String>>;

    /// Run the job; stop promptly once `stop` is cancelled (timeout or cancel-all).
    fn run<'a>(
        &'a self,
        session: Self::Session,
        host: &'a FleetHost,
        stop: CancellationToken,
        output: &'a OutputSink<'a>,
    ) -> BoxFuture<'a, Result<Self::Output, String>>;

    /// The status a finished job reports (default: [`HostStatus::Ok`]).
    fn status(&self, _output: &Self::Output) -> HostStatus {
        HostStatus::Ok
    }
}

/// One Host's result.
#[derive(Debug)]
pub struct HostResult<O> {
    /// The Host.
    pub host: FleetHost,
    /// Final status.
    pub status: HostStatus,
    /// What the job returned (also for timed out and cancelled jobs that returned).
    pub output: Option<O>,
    /// Why it failed.
    pub error: Option<String>,
    /// Time from the start of the job (not counting the login).
    pub elapsed: Duration,
}

/// Progress of a run, as it happens.
pub enum Update<'a, O> {
    /// A Host changed status (before it is final).
    Status(&'a FleetHost, HostStatus),
    /// A Host printed something.
    Output(&'a FleetHost, bool, &'a [u8]),
    /// A Host is done.
    Done(HostResult<O>),
}

/// Run the job on every Host; `report` sees each step. Returns when every Host is done.
pub async fn run_hosts<E: FleetExecutor>(
    exec: &E,
    hosts: Vec<FleetHost>,
    limits: RunLimits,
    cancel: CancellationToken,
    report: &(dyn Fn(Update<'_, E::Output>) + Send + Sync),
) {
    let concurrency = limits.concurrency.clamp(1, MAX_CONCURRENCY);
    for h in &hosts {
        report(Update::Status(h, HostStatus::Queued));
    }
    futures::stream::iter(hosts)
        .for_each_concurrent(concurrency, |host| {
            let cancel = cancel.clone();
            async move {
                let result = run_one(exec, &host, limits.timeout, &cancel, report).await;
                report(Update::Done(HostResult { host, ..result }));
            }
        })
        .await;
}

async fn run_one<E: FleetExecutor>(
    exec: &E,
    host: &FleetHost,
    timeout: Duration,
    cancel: &CancellationToken,
    report: &(dyn Fn(Update<'_, E::Output>) + Send + Sync),
) -> HostResult<E::Output> {
    let result = |status, output, error, elapsed| HostResult {
        host: host.clone(),
        status,
        output,
        error,
        elapsed,
    };
    if cancel.is_cancelled() {
        return result(HostStatus::Cancelled, None, None, Duration::ZERO);
    }
    report(Update::Status(host, HostStatus::Connecting));
    let session = tokio::select! {
        s = exec.connect(host) => s,
        () = cancel.cancelled() => return result(HostStatus::Cancelled, None, None, Duration::ZERO),
    };
    let session = match session {
        Ok(s) => s,
        Err(e) => return result(HostStatus::Unreachable, None, Some(e), Duration::ZERO),
    };
    report(Update::Status(host, HostStatus::Running));
    let stop = cancel.child_token();
    let timed_out = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let timer = {
        let (stop, timed_out) = (stop.clone(), timed_out.clone());
        tokio::spawn(async move {
            tokio::select! {
                () = tokio::time::sleep(timeout) => {
                    timed_out.store(true, std::sync::atomic::Ordering::SeqCst);
                    stop.cancel();
                }
                () = stop.cancelled() => {}
            }
        })
    };
    let sink = |stderr: bool, bytes: &[u8]| report(Update::Output(host, stderr, bytes));
    let started = Instant::now();
    let out = exec.run(session, host, stop.clone(), &sink).await;
    let elapsed = started.elapsed();
    timer.abort();
    let timed_out = timed_out.load(std::sync::atomic::Ordering::SeqCst);
    let cancelled = cancel.is_cancelled();
    match out {
        _ if timed_out => {
            let o = out.ok();
            result(
                HostStatus::TimedOut,
                o,
                Some(format!("stopped after {} s", timeout.as_secs())),
                elapsed,
            )
        }
        _ if cancelled => result(HostStatus::Cancelled, out.ok(), None, elapsed),
        Ok(o) => {
            let status = exec.status(&o);
            result(status, Some(o), None, elapsed)
        }
        Err(e) => result(HostStatus::Failed, None, Some(e), elapsed),
    }
}

/// The Hosts whose `key` differs from the most common one ("differs from majority").
/// With no clear majority (a tie for most common) every Host differs from it unless all
/// agree; Hosts without a key (not finished) are left out.
pub fn outliers<K: Eq + std::hash::Hash + Clone>(
    keys: &[(ProfileId, Option<K>)],
) -> Vec<ProfileId> {
    let mut counts: Vec<(K, usize)> = Vec::new();
    for k in keys.iter().filter_map(|(_, k)| k.as_ref()) {
        match counts.iter_mut().find(|(c, _)| c == k) {
            Some((_, n)) => *n += 1,
            None => counts.push((k.clone(), 1)),
        }
    }
    let Some(top) = counts.iter().map(|(_, n)| *n).max() else {
        return Vec::new();
    };
    let tied = counts.iter().filter(|(_, n)| *n == top).count() > 1;
    let majority = counts
        .iter()
        .find(|(_, n)| *n == top)
        .map(|(k, _)| k.clone());
    keys.iter()
        .filter(|(_, k)| match (k, &majority) {
            (Some(k), Some(m)) => tied || k != m,
            _ => false,
        })
        .map(|(id, _)| id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Hosts named `down*` do not connect; `slow*` run until stopped; `fail*` exit 1.
    struct Fake {
        running: AtomicUsize,
        peak: AtomicUsize,
    }

    impl FleetExecutor for Fake {
        type Session = ();
        type Output = u32;

        fn connect<'a>(&'a self, host: &'a FleetHost) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async move {
                if host.name.starts_with("down") {
                    Err("connection refused".into())
                } else {
                    Ok(())
                }
            })
        }

        fn run<'a>(
            &'a self,
            _: (),
            host: &'a FleetHost,
            stop: CancellationToken,
            output: &'a OutputSink<'a>,
        ) -> BoxFuture<'a, Result<u32, String>> {
            Box::pin(async move {
                let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(now, Ordering::SeqCst);
                output(false, host.name.as_bytes());
                if host.name.starts_with("slow") {
                    stop.cancelled().await;
                } else {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                self.running.fetch_sub(1, Ordering::SeqCst);
                Ok(u32::from(host.name.starts_with("fail")))
            })
        }

        fn status(&self, code: &u32) -> HostStatus {
            if *code == 0 {
                HostStatus::Ok
            } else {
                HostStatus::Failed
            }
        }
    }

    fn hosts(names: &[&str]) -> Vec<FleetHost> {
        names
            .iter()
            .map(|n| FleetHost {
                id: ProfileId((*n).to_owned()),
                name: (*n).to_owned(),
            })
            .collect()
    }

    type Log = Arc<Mutex<Vec<(String, HostStatus, Option<String>)>>>;

    fn collector(
        log: &Log,
        output: &Arc<Mutex<Vec<String>>>,
    ) -> impl Fn(Update<'_, u32>) + Send + Sync + use<> {
        let (log, output) = (log.clone(), output.clone());
        move |u| match u {
            Update::Done(r) => log.lock().unwrap().push((r.host.name, r.status, r.error)),
            Update::Output(h, _, b) => {
                output
                    .lock()
                    .unwrap()
                    .push(format!("{}:{}", h.name, String::from_utf8_lossy(b)))
            }
            Update::Status(..) => {}
        }
    }

    fn status_of(log: &Log, name: &str) -> HostStatus {
        log.lock()
            .unwrap()
            .iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, s, _)| *s)
            .unwrap()
    }

    #[tokio::test]
    async fn bounded_concurrency_and_statuses() {
        let fake = Arc::new(Fake {
            running: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        });
        let names = ["a", "b", "c", "d", "e", "f", "fail-1", "down-1"];
        let (log, out): (Log, _) = Default::default();
        run_hosts(
            fake.as_ref(),
            hosts(&names),
            RunLimits {
                concurrency: 3,
                timeout: Duration::from_secs(5),
            },
            CancellationToken::new(),
            &collector(&log, &out),
        )
        .await;
        assert_eq!(log.lock().unwrap().len(), names.len());
        assert!(fake.peak.load(Ordering::SeqCst) <= 3);
        assert!(fake.peak.load(Ordering::SeqCst) >= 2);
        assert_eq!(status_of(&log, "a"), HostStatus::Ok);
        assert_eq!(status_of(&log, "fail-1"), HostStatus::Failed);
        assert_eq!(status_of(&log, "down-1"), HostStatus::Unreachable);
        assert!(out.lock().unwrap().contains(&"a:a".to_owned()));
    }

    #[tokio::test]
    async fn timeout_stops_one_host() {
        let fake = Arc::new(Fake {
            running: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        });
        let (log, out): (Log, _) = Default::default();
        let started = Instant::now();
        run_hosts(
            fake.as_ref(),
            hosts(&["slow-1", "quick"]),
            RunLimits {
                concurrency: 8,
                timeout: Duration::from_millis(150),
            },
            CancellationToken::new(),
            &collector(&log, &out),
        )
        .await;
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(status_of(&log, "slow-1"), HostStatus::TimedOut);
        assert_eq!(status_of(&log, "quick"), HostStatus::Ok);
        let err = log.lock().unwrap()[1].2.clone();
        assert_eq!(err.as_deref(), Some("stopped after 0 s"));
    }

    #[tokio::test]
    async fn cancel_all_stops_running_and_queued_hosts() {
        let fake = Arc::new(Fake {
            running: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        });
        let (log, out): (Log, _) = Default::default();
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            c2.cancel();
        });
        run_hosts(
            fake.as_ref(),
            hosts(&["slow-1", "slow-2", "queued-1", "queued-2"]),
            RunLimits {
                concurrency: 2,
                timeout: Duration::from_secs(60),
            },
            cancel,
            &collector(&log, &out),
        )
        .await;
        for n in ["slow-1", "slow-2", "queued-1", "queued-2"] {
            assert_eq!(status_of(&log, n), HostStatus::Cancelled, "{n}");
        }
        // The queued Hosts never ran.
        assert!(!out.lock().unwrap().iter().any(|o| o.starts_with("queued")));
    }

    #[test]
    fn majority_outliers() {
        let id = |s: &str| ProfileId(s.into());
        let keys = vec![
            (id("a"), Some(1)),
            (id("b"), Some(1)),
            (id("c"), Some(2)),
            (id("d"), None),
        ];
        assert_eq!(outliers(&keys), [id("c")]);
        // A tie has no majority: everyone differs.
        let tie = vec![(id("a"), Some(1)), (id("b"), Some(2))];
        assert_eq!(outliers(&tie), [id("a"), id("b")]);
        let same = vec![(id("a"), Some(1)), (id("b"), Some(1))];
        assert!(outliers(&same).is_empty());
        assert!(outliers::<u8>(&[]).is_empty());
    }
}
