//! Load tests: one compiled request sent many times by a pool of workers,
//! summarized as latency percentiles, throughput, status codes, errors and a
//! per-second timeline.
//!
//! The request is compiled once (pre-request scripts, auth and variables run a
//! single time), then every worker sends that same [`PreparedRequest`] through
//! the [`WorkbenchTransport`], each send in its own operation so a cancel
//! reaches the sockets in flight. Post-response scripts do not run: a load test
//! measures the server, and thresholds ([`LoadThresholds`]) replace per-request
//! tests.
//!
//! [`run_load_test`] blocks until the test ends; callers run it on a blocking
//! thread and read [`LoadProgress::report`] meanwhile for live numbers.

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::PreparedRequest;

use super::transport::{FileCapabilities, OperationPhase, WorkbenchTransport};

/// Most requests one load test may send.
pub const MAX_LOAD_REQUESTS: u32 = 100_000;
/// Most requests in flight at once.
pub const MAX_LOAD_CONCURRENCY: u32 = 64;
/// Longest a load test may run.
pub const MAX_LOAD_DURATION: Duration = Duration::from_secs(60 * 60);
/// Bars in [`LoadReport::histogram`].
const HISTOGRAM_BUCKETS: u64 = 12;
/// Distinct error messages kept in [`LoadReport::errors`].
const TOP_ERRORS: usize = 5;
/// Longest error message kept per sample.
const MAX_ERROR_CHARS: usize = 240;

/// What a load test sends and when it stops.
#[derive(Clone, Debug, PartialEq)]
pub struct LoadTestConfig {
    /// Requests to send in total; `0` means no count limit (a duration is then required).
    pub requests: u32,
    /// Workers sending at once.
    pub concurrency: u32,
    /// Stop issuing new requests after this long; requests in flight finish.
    pub duration: Option<Duration>,
    pub thresholds: LoadThresholds,
}

/// Pass / fail budgets checked against the final numbers.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LoadThresholds {
    /// The 95th percentile latency must stay at or under this.
    pub p95_ms: Option<u64>,
    /// Failed requests (transport errors and HTTP 4xx/5xx) must stay at or under this share.
    pub error_rate_percent: Option<f64>,
}

impl LoadTestConfig {
    /// Why this configuration cannot run, if it cannot.
    pub fn validate(&self) -> Result<(), String> {
        if self.concurrency == 0 || self.concurrency > MAX_LOAD_CONCURRENCY {
            return Err(format!(
                "Concurrency must be between 1 and {MAX_LOAD_CONCURRENCY}."
            ));
        }
        if self.requests > MAX_LOAD_REQUESTS {
            return Err(format!("Send at most {MAX_LOAD_REQUESTS} requests."));
        }
        match self.duration {
            Some(duration) if duration.is_zero() || duration > MAX_LOAD_DURATION => Err(format!(
                "Duration must be between 1 and {} seconds.",
                MAX_LOAD_DURATION.as_secs()
            )),
            None if self.requests == 0 => {
                Err("Set a number of requests, a duration, or both.".into())
            }
            _ => Ok(()),
        }
    }
}

/// One request's outcome.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LoadSample {
    /// When the request started, from the start of the test.
    pub started_ms: u64,
    pub latency_ms: u64,
    /// `None` when the request failed before a response arrived.
    pub status: Option<u16>,
    pub bytes: u64,
    pub error: Option<String>,
    pub connect_ms: Option<u64>,
    pub tls_ms: Option<u64>,
    pub first_byte_ms: Option<u64>,
}

impl LoadSample {
    /// Failed: no response, or an HTTP 4xx / 5xx.
    pub fn failed(&self) -> bool {
        self.error.is_some() || self.status.is_none_or(|status| status >= 400)
    }
}

/// Live state shared between the workers and whoever watches them.
#[derive(Default)]
pub struct LoadProgress {
    samples: Mutex<Vec<LoadSample>>,
    started: Mutex<Option<Instant>>,
    ended: Mutex<Option<Duration>>,
    in_flight: Mutex<HashSet<String>>,
    issued: AtomicU32,
    cancelled: AtomicBool,
    finished: AtomicBool,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl LoadProgress {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Stop issuing requests; the ones in flight finish on their own.
    pub fn stop(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Stop issuing requests and cancel the ones in flight.
    pub fn cancel(&self, transport: &dyn WorkbenchTransport) {
        self.stop();
        let in_flight: Vec<String> = lock(&self.in_flight).iter().cloned().collect();
        for operation in in_flight {
            let _ = transport.cancel(&operation);
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }

    /// Requests completed so far.
    pub fn completed(&self) -> usize {
        lock(&self.samples).len()
    }

    fn elapsed(&self) -> Duration {
        if let Some(ended) = *lock(&self.ended) {
            return ended;
        }
        lock(&self.started)
            .map(|started| started.elapsed())
            .unwrap_or_default()
    }

    /// The numbers so far (or final, once [`Self::is_finished`]).
    pub fn report(&self, config: &LoadTestConfig) -> LoadReport {
        let samples = lock(&self.samples).clone();
        let mut report = LoadReport::from_samples(&samples, self.elapsed(), &config.thresholds);
        report.finished = self.is_finished();
        report.cancelled = self.is_cancelled();
        report.planned = config.requests;
        report
    }
}

/// Send `request` the way `config` says and return the final report.
///
/// Blocks until every worker stops. A configuration [`LoadTestConfig::validate`]
/// rejects returns that error without sending anything.
pub fn run_load_test(
    request: &PreparedRequest,
    files: &FileCapabilities,
    transport: Arc<dyn WorkbenchTransport>,
    config: &LoadTestConfig,
    progress: &LoadProgress,
) -> Result<LoadReport, String> {
    config.validate()?;
    let started = Instant::now();
    *lock(&progress.started) = Some(started);
    let run = uuid::Uuid::new_v4().simple().to_string();
    let workers = if config.requests == 0 {
        config.concurrency
    } else {
        config.concurrency.min(config.requests)
    };
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let transport = transport.clone();
            let run = run.as_str();
            scope.spawn(move || {
                worker(
                    request,
                    files,
                    transport.as_ref(),
                    config,
                    progress,
                    started,
                    run,
                )
            });
        }
    });
    *lock(&progress.ended) = Some(started.elapsed());
    progress.finished.store(true, Ordering::SeqCst);
    Ok(progress.report(config))
}

fn worker(
    request: &PreparedRequest,
    files: &FileCapabilities,
    transport: &dyn WorkbenchTransport,
    config: &LoadTestConfig,
    progress: &LoadProgress,
    started: Instant,
    run: &str,
) {
    loop {
        if progress.is_cancelled()
            || config
                .duration
                .is_some_and(|duration| started.elapsed() >= duration)
        {
            return;
        }
        let index = progress.issued.fetch_add(1, Ordering::SeqCst);
        if config.requests != 0 && index >= config.requests {
            return;
        }
        let operation_id = format!("load-{run}-{index}");
        let phase = OperationPhase {
            operation_id: operation_id.clone(),
            starts_operation: true,
            final_phase: true,
        };
        lock(&progress.in_flight).insert(operation_id.clone());
        // A cancel that read the in-flight set just before the insert above
        // missed this request: check again now that it is registered.
        if progress.is_cancelled() {
            lock(&progress.in_flight).remove(&operation_id);
            return;
        }
        let sent_at = Instant::now();
        let started_ms = millis(sent_at.saturating_duration_since(started));
        let result = transport.send(request, files, &phase);
        let wall_ms = millis(sent_at.elapsed());
        lock(&progress.in_flight).remove(&operation_id);
        let sample = match result {
            Ok(response) => LoadSample {
                started_ms,
                // The service times the HTTP exchange itself; the hop to it is
                // not the server's latency.
                latency_ms: if response.duration_ms > 0 {
                    response.duration_ms
                } else {
                    wall_ms
                },
                status: Some(response.status),
                bytes: response.received_bytes,
                error: None,
                connect_ms: response.timings.connect_ms,
                tls_ms: response.timings.tls_ms,
                first_byte_ms: response.timings.first_byte_ms,
            },
            Err(_) if progress.is_cancelled() => return,
            Err(error) => LoadSample {
                started_ms,
                latency_ms: wall_ms,
                error: Some(error.chars().take(MAX_ERROR_CHARS).collect()),
                ..LoadSample::default()
            },
        };
        lock(&progress.samples).push(sample);
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Latency percentiles, in milliseconds.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LatencySummary {
    pub min: u64,
    pub mean: u64,
    pub p50: u64,
    pub p90: u64,
    pub p95: u64,
    pub p99: u64,
    pub max: u64,
}

/// Mean connection phases over the requests that reported them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PhaseMeans {
    pub connect_ms: Option<u64>,
    pub tls_ms: Option<u64>,
    pub first_byte_ms: Option<u64>,
}

/// Requests whose latency fell at or under `upper_ms` (and above the previous bucket).
#[derive(Clone, Debug, PartialEq)]
pub struct HistogramBucket {
    pub upper_ms: u64,
    pub count: u32,
}

/// Requests that completed during one second of the test.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TimelinePoint {
    pub second: u32,
    pub requests: u32,
    pub failed: u32,
    pub mean_ms: u64,
}

/// One threshold and whether the run met it.
#[derive(Clone, Debug, PartialEq)]
pub struct ThresholdCheck {
    pub label: String,
    pub actual: String,
    pub passed: bool,
}

/// A load test's numbers.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LoadReport {
    /// Requests the test was asked to send (`0` when it ran on a duration only).
    pub planned: u32,
    pub completed: u32,
    pub failed: u32,
    pub elapsed_ms: u64,
    pub requests_per_second: f64,
    pub error_rate_percent: f64,
    pub received_bytes: u64,
    pub latency: LatencySummary,
    pub phases: PhaseMeans,
    /// `("200", n)`, `("503", n)`, `("error", n)`; most frequent first.
    pub status_counts: Vec<(String, u32)>,
    /// The most frequent transport errors, most frequent first.
    pub errors: Vec<(String, u32)>,
    pub histogram: Vec<HistogramBucket>,
    pub timeline: Vec<TimelinePoint>,
    pub thresholds: Vec<ThresholdCheck>,
    pub finished: bool,
    pub cancelled: bool,
}

impl LoadReport {
    /// Summarize `samples` collected over `elapsed`.
    pub fn from_samples(
        samples: &[LoadSample],
        elapsed: Duration,
        thresholds: &LoadThresholds,
    ) -> Self {
        let completed = u32::try_from(samples.len()).unwrap_or(u32::MAX);
        let failed = u32::try_from(samples.iter().filter(|s| s.failed()).count()).unwrap_or(0);
        let elapsed_ms = millis(elapsed);
        let requests_per_second = if elapsed_ms == 0 {
            0.0
        } else {
            f64::from(completed) * 1000.0 / elapsed_ms as f64
        };
        let error_rate_percent = if completed == 0 {
            0.0
        } else {
            f64::from(failed) * 100.0 / f64::from(completed)
        };
        let mut latencies: Vec<u64> = samples.iter().map(|s| s.latency_ms).collect();
        latencies.sort_unstable();
        let latency = summarize(&latencies);
        let mut report = Self {
            completed,
            failed,
            elapsed_ms,
            requests_per_second,
            error_rate_percent,
            received_bytes: samples.iter().map(|s| s.bytes).sum(),
            phases: PhaseMeans {
                connect_ms: mean_of(samples.iter().filter_map(|s| s.connect_ms)),
                tls_ms: mean_of(samples.iter().filter_map(|s| s.tls_ms)),
                first_byte_ms: mean_of(samples.iter().filter_map(|s| s.first_byte_ms)),
            },
            status_counts: status_counts(samples),
            errors: top_errors(samples),
            histogram: histogram(&latencies),
            timeline: timeline(samples),
            ..Self::default()
        };
        report.thresholds = check_thresholds(&report, &latency, thresholds);
        report.latency = latency;
        report
    }

    /// Every threshold passed (true when none were set).
    pub fn passed(&self) -> bool {
        self.thresholds.iter().all(|check| check.passed)
    }
}

/// Nearest-rank percentile of sorted values.
fn percentile(sorted: &[u64], percent: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (percent / 100.0 * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn summarize(sorted: &[u64]) -> LatencySummary {
    if sorted.is_empty() {
        return LatencySummary::default();
    }
    let total: u128 = sorted.iter().map(|&v| u128::from(v)).sum();
    LatencySummary {
        min: sorted[0],
        mean: u64::try_from(total / sorted.len() as u128).unwrap_or(u64::MAX),
        p50: percentile(sorted, 50.0),
        p90: percentile(sorted, 90.0),
        p95: percentile(sorted, 95.0),
        p99: percentile(sorted, 99.0),
        max: sorted[sorted.len() - 1],
    }
}

fn mean_of(values: impl Iterator<Item = u64>) -> Option<u64> {
    let (sum, count) = values.fold((0u128, 0u128), |(sum, count), value| {
        (sum + u128::from(value), count + 1)
    });
    (count > 0).then(|| u64::try_from(sum / count).unwrap_or(u64::MAX))
}

fn status_counts(samples: &[LoadSample]) -> Vec<(String, u32)> {
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    for sample in samples {
        let key = match (sample.status, &sample.error) {
            (Some(status), None) => status.to_string(),
            _ => "error".to_string(),
        };
        *counts.entry(key).or_default() += 1;
    }
    let mut counts: Vec<_> = counts.into_iter().collect();
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    counts
}

fn top_errors(samples: &[LoadSample]) -> Vec<(String, u32)> {
    let mut counts: BTreeMap<&str, u32> = BTreeMap::new();
    for error in samples.iter().filter_map(|s| s.error.as_deref()) {
        *counts.entry(error).or_default() += 1;
    }
    let mut counts: Vec<_> = counts
        .into_iter()
        .map(|(error, count)| (error.to_string(), count))
        .collect();
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    counts.truncate(TOP_ERRORS);
    counts
}

/// Equal-width buckets from the fastest to the slowest request.
fn histogram(sorted: &[u64]) -> Vec<HistogramBucket> {
    let (Some(&min), Some(&max)) = (sorted.first(), sorted.last()) else {
        return Vec::new();
    };
    let width = ((max - min) / HISTOGRAM_BUCKETS + 1).max(1);
    let buckets = ((max - min) / width + 1).min(HISTOGRAM_BUCKETS);
    let mut out: Vec<HistogramBucket> = (0..buckets)
        .map(|index| HistogramBucket {
            upper_ms: min + width * (index + 1) - 1,
            count: 0,
        })
        .collect();
    for &value in sorted {
        let index = usize::try_from(((value - min) / width).min(buckets - 1)).unwrap_or(0);
        out[index].count += 1;
    }
    out
}

/// Requests per second of completion time.
fn timeline(samples: &[LoadSample]) -> Vec<TimelinePoint> {
    let mut seconds: BTreeMap<u32, (u32, u32, u128)> = BTreeMap::new();
    for sample in samples {
        let second = u32::try_from((sample.started_ms + sample.latency_ms) / 1000).unwrap_or(0);
        let entry = seconds.entry(second).or_default();
        entry.0 += 1;
        entry.1 += u32::from(sample.failed());
        entry.2 += u128::from(sample.latency_ms);
    }
    let Some(&last) = seconds.keys().next_back() else {
        return Vec::new();
    };
    (0..=last)
        .map(|second| {
            let (requests, failed, total) = seconds.get(&second).copied().unwrap_or_default();
            TimelinePoint {
                second,
                requests,
                failed,
                mean_ms: if requests == 0 {
                    0
                } else {
                    u64::try_from(total / u128::from(requests)).unwrap_or(u64::MAX)
                },
            }
        })
        .collect()
}

fn check_thresholds(
    report: &LoadReport,
    latency: &LatencySummary,
    thresholds: &LoadThresholds,
) -> Vec<ThresholdCheck> {
    let mut checks = Vec::new();
    if let Some(budget) = thresholds.p95_ms {
        checks.push(ThresholdCheck {
            label: format!("p95 ≤ {budget} ms"),
            actual: format!("{} ms", latency.p95),
            passed: report.completed > 0 && latency.p95 <= budget,
        });
    }
    if let Some(budget) = thresholds.error_rate_percent {
        checks.push(ThresholdCheck {
            label: format!("errors ≤ {budget}%"),
            actual: format!("{:.2}%", report.error_rate_percent),
            passed: report.completed > 0 && report.error_rate_percent <= budget,
        });
    }
    checks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::transport::{
        Response, ScriptRequestView, ScriptResponseView, ScriptResult, ScriptScopes,
    };
    use crate::{HttpMethod, PreparedBody, RequestId, RequestSettings};
    use std::sync::atomic::AtomicUsize;

    fn sample(latency_ms: u64, status: Option<u16>) -> LoadSample {
        LoadSample {
            latency_ms,
            status,
            error: status.is_none().then(|| "connection refused".to_string()),
            ..LoadSample::default()
        }
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let sorted: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&sorted, 50.0), 50);
        assert_eq!(percentile(&sorted, 95.0), 95);
        assert_eq!(percentile(&sorted, 99.0), 99);
        assert_eq!(percentile(&[7], 99.0), 7);
        assert_eq!(percentile(&[], 50.0), 0);
        let summary = summarize(&sorted);
        assert_eq!((summary.min, summary.max, summary.mean), (1, 100, 50));
    }

    #[test]
    fn report_counts_failures_statuses_and_thresholds() {
        let samples = vec![
            sample(10, Some(200)),
            sample(20, Some(200)),
            sample(30, Some(503)),
            sample(40, None),
        ];
        let report = LoadReport::from_samples(
            &samples,
            Duration::from_secs(2),
            &LoadThresholds {
                p95_ms: Some(50),
                error_rate_percent: Some(10.0),
            },
        );
        assert_eq!(report.completed, 4);
        assert_eq!(report.failed, 2);
        assert_eq!(report.error_rate_percent, 50.0);
        assert_eq!(report.requests_per_second, 2.0);
        assert_eq!(report.status_counts[0], ("200".to_string(), 2));
        assert!(report.status_counts.contains(&("error".to_string(), 1)));
        assert_eq!(report.errors, vec![("connection refused".to_string(), 1)]);
        assert_eq!(report.thresholds.len(), 2);
        assert!(report.thresholds[0].passed, "p95 40 ms is within 50 ms");
        assert!(!report.thresholds[1].passed, "50% errors is over 10%");
        assert!(!report.passed());
    }

    #[test]
    fn histogram_covers_every_sample_once() {
        let sorted: Vec<u64> = (0..1000).map(|v| v * 3).collect();
        let buckets = histogram(&sorted);
        assert!(buckets.len() <= HISTOGRAM_BUCKETS as usize);
        assert_eq!(buckets.iter().map(|b| b.count).sum::<u32>(), 1000);
        assert!(buckets.last().is_some_and(|b| b.upper_ms >= 2997));
        let flat = histogram(&[5, 5, 5]);
        assert_eq!(flat.len(), 1);
        assert_eq!(flat[0].count, 3);
    }

    #[test]
    fn timeline_fills_quiet_seconds() {
        let samples = vec![
            LoadSample {
                started_ms: 100,
                latency_ms: 100,
                status: Some(200),
                ..LoadSample::default()
            },
            LoadSample {
                started_ms: 2500,
                latency_ms: 100,
                status: Some(500),
                ..LoadSample::default()
            },
        ];
        let points = timeline(&samples);
        assert_eq!(points.len(), 3);
        assert_eq!(points[1].requests, 0);
        assert_eq!((points[2].requests, points[2].failed), (1, 1));
    }

    #[test]
    fn config_needs_a_stop_condition_and_sane_limits() {
        let mut config = LoadTestConfig {
            requests: 0,
            concurrency: 4,
            duration: None,
            thresholds: LoadThresholds::default(),
        };
        assert!(config.validate().is_err());
        config.duration = Some(Duration::from_secs(5));
        assert!(config.validate().is_ok());
        config.concurrency = MAX_LOAD_CONCURRENCY + 1;
        assert!(config.validate().is_err());
        config.concurrency = 1;
        config.requests = MAX_LOAD_REQUESTS + 1;
        assert!(config.validate().is_err());
    }

    /// Answers every send after a short pause, counting the most sends at once.
    #[derive(Default)]
    struct CountingTransport {
        active: AtomicUsize,
        peak: AtomicUsize,
        operations: Mutex<HashSet<String>>,
        cancels: AtomicUsize,
        pause: Duration,
    }

    impl WorkbenchTransport for CountingTransport {
        fn send(
            &self,
            _: &PreparedRequest,
            _: &FileCapabilities,
            phase: &OperationPhase,
        ) -> Result<Response, String> {
            assert!(lock(&self.operations).insert(phase.operation_id.clone()));
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(self.pause);
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(Response {
                status: 200,
                duration_ms: 3,
                received_bytes: 10,
                ..Response::default()
            })
        }

        fn run_script(
            &self,
            _: &str,
            _: ScriptScopes,
            _: ScriptRequestView,
            _: Option<ScriptResponseView>,
            _: Option<&str>,
            _: Option<&OperationPhase>,
        ) -> Result<ScriptResult, String> {
            Err("no scripts in load tests".into())
        }

        fn cancel(&self, _: &str) -> Result<bool, String> {
            self.cancels.fetch_add(1, Ordering::SeqCst);
            Ok(true)
        }
    }

    fn request() -> PreparedRequest {
        PreparedRequest {
            request_id: RequestId::new(),
            method: HttpMethod::get(),
            url: "https://example.test/".into(),
            headers: Vec::new(),
            body: PreparedBody::None,
            settings: RequestSettings::default(),
            aws_sigv4: None,
            redactions: Vec::new(),
        }
    }

    #[test]
    fn runs_the_planned_requests_within_the_concurrency() {
        let transport = Arc::new(CountingTransport {
            pause: Duration::from_millis(5),
            ..CountingTransport::default()
        });
        let config = LoadTestConfig {
            requests: 40,
            concurrency: 4,
            duration: None,
            thresholds: LoadThresholds::default(),
        };
        let progress = LoadProgress::new();
        let report = run_load_test(
            &request(),
            &FileCapabilities::default(),
            transport.clone(),
            &config,
            &progress,
        )
        .unwrap();
        assert_eq!(report.completed, 40);
        assert_eq!(report.failed, 0);
        assert_eq!(report.planned, 40);
        assert!(report.finished);
        assert_eq!(report.latency.p50, 3, "the service's own timing wins");
        assert_eq!(report.received_bytes, 400);
        assert_eq!(
            lock(&transport.operations).len(),
            40,
            "one operation per send"
        );
        let peak = transport.peak.load(Ordering::SeqCst);
        assert!((1..=4).contains(&peak), "peak {peak}");
    }

    #[test]
    fn cancel_stops_issuing_and_cancels_in_flight_sends() {
        let transport = Arc::new(CountingTransport {
            pause: Duration::from_millis(20),
            ..CountingTransport::default()
        });
        let config = LoadTestConfig {
            requests: 0,
            concurrency: 2,
            duration: Some(Duration::from_secs(30)),
            thresholds: LoadThresholds::default(),
        };
        let progress = LoadProgress::new();
        let started = Instant::now();
        std::thread::scope(|scope| {
            let worker_transport = transport.clone();
            let progress_ref = &progress;
            let config_ref = &config;
            let handle = scope.spawn(move || {
                run_load_test(
                    &request(),
                    &FileCapabilities::default(),
                    worker_transport,
                    config_ref,
                    progress_ref,
                )
            });
            std::thread::sleep(Duration::from_millis(100));
            progress.cancel(transport.as_ref());
            let report = handle.join().unwrap().unwrap();
            assert!(report.cancelled);
            assert!(report.finished);
            assert!(report.completed > 0);
        });
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn load_tests_a_local_mock_through_the_native_transport() {
        use crate::runtime::transport::NativeWorkbenchTransport;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        let saved = crate::SavedRequest {
            id: RequestId::new(),
            collection_id: crate::CollectionId::new(),
            folder_id: None,
            name: "Health".into(),
            method: HttpMethod::get(),
            url: "{{base}}/health".into(),
            params: Vec::new(),
            headers: Vec::new(),
            auth: Default::default(),
            body: Default::default(),
            variables: Vec::new(),
            scripts: Default::default(),
            settings: Default::default(),
            extensions: Default::default(),
            sort_key: 0,
        };
        let example = crate::Example {
            id: crate::ExampleId::new(),
            request_id: saved.id.clone(),
            name: "Healthy".into(),
            request: None,
            response: crate::ResponseSnapshot {
                status: 200,
                body_base64: "b2s=".into(),
                ..Default::default()
            },
            extensions: Default::default(),
            sort_key: 0,
        };
        let routes = crate::mock::mock_routes(&[saved], &[example]);
        let server = crate::mock::MockServer::start(runtime.handle(), routes, 0).unwrap();
        let mut prepared = request();
        prepared.url = format!("{}/health", server.url());
        prepared.settings.allow_private_network = true;
        let transport: Arc<dyn WorkbenchTransport> =
            Arc::new(NativeWorkbenchTransport::from_env().with_runtime(runtime.handle().clone()));
        // More sends than the service tracks at once: finished operations
        // must make room for new ones.
        let config = LoadTestConfig {
            requests: 1_200,
            concurrency: 16,
            duration: None,
            thresholds: LoadThresholds {
                p95_ms: Some(5_000),
                error_rate_percent: Some(0.0),
            },
        };
        let progress = LoadProgress::new();
        let report = run_load_test(
            &prepared,
            &FileCapabilities::default(),
            transport,
            &config,
            &progress,
        )
        .unwrap();
        assert_eq!(report.completed, 1_200);
        assert_eq!(report.errors, Vec::<(String, u32)>::new());
        assert_eq!(report.status_counts, vec![("200".to_string(), 1_200)]);
        assert!(report.passed(), "{:?}", report.thresholds);
        assert_eq!(server.total_hits(), 1_200);
    }
}
