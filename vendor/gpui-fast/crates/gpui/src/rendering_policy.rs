//! Switchyard's Windows rendering policy for software adapters and remote
//! sessions (ported from AgentOps' GPUI patch). All other platforms keep
//! upstream behavior, including the high-rate input keepalive on macOS.

use std::time::{Duration, Instant};

#[cfg(target_os = "windows")]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(target_os = "windows")]
static LOW_POWER_RENDERING: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "windows")]
static SOFTWARE_RENDERING_EFFECTS: AtomicBool = AtomicBool::new(false);

/// Omit decorative blurred shadows on Windows software renderers. Callers
/// should enable it only after identifying a software adapter; zero-blur
/// focus rings and outlines remain. Configure before the first paint.
/// A no-op on other platforms.
pub fn set_software_rendering_effects(enabled: bool) {
    #[cfg(target_os = "windows")]
    SOFTWARE_RENDERING_EFFECTS.store(enabled, Ordering::Relaxed);
    #[cfg(not(target_os = "windows"))]
    let _ = enabled;
}

pub(crate) fn software_rendering_effects() -> bool {
    #[cfg(target_os = "windows")]
    {
        SOFTWARE_RENDERING_EFFECTS.load(Ordering::Relaxed)
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

/// Enable economical rendering for Windows software adapters and remote sessions.
///
/// While enabled, the Windows frame scheduler runs at most 30 times per second,
/// expensive frames leave the UI thread recovery time before the next scheduled
/// draw, and moving the window without resizing it reuses the current scene.
/// Scene changes, explicit presentation requests and frame callbacks are kept.
/// This process-wide setting may be changed after startup. It is a no-op on
/// other platforms, and defaults to disabled.
pub fn set_low_power_rendering(enabled: bool) {
    #[cfg(target_os = "windows")]
    LOW_POWER_RENDERING.store(enabled, Ordering::Relaxed);
    #[cfg(not(target_os = "windows"))]
    let _ = enabled;
}

/// Whether [`set_low_power_rendering`] is enabled. Read by the platform crates.
#[doc(hidden)]
pub fn low_power_rendering() -> bool {
    #[cfg(target_os = "windows")]
    {
        LOW_POWER_RENDERING.load(Ordering::Relaxed)
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

/// The Windows frame scheduler's sleep between frame requests while low-power
/// rendering is enabled; `None` keeps the display's vsync.
#[doc(hidden)]
pub fn low_power_frame_interval(low_power: bool) -> Option<Duration> {
    // Round up so timer-driven frame requests cannot exceed 30 Hz.
    low_power.then_some(Duration::from_nanos(33_333_334))
}

/// Whether an unchanged scene must be presented again.
///
/// Windows' compositor keeps the last presented frame, and presenting there
/// renders the whole scene again: on a software adapter each one costs a full
/// frame of CPU. Windows therefore only presents when asked to, while other
/// platforms keep upstream's keepalive during high-rate input (which stops
/// ProMotion displays from lowering their refresh rate).
pub(crate) fn requires_presentation(
    explicitly_required: bool,
    pending: bool,
    high_rate_input: bool,
) -> bool {
    explicitly_required || pending || (!cfg!(target_os = "windows") && high_rate_input)
}

/// A timer frequency alone cannot leave CPU headroom when a frame takes longer
/// than its interval. In low-power mode, wait the last frame's cost (capped at
/// 100 ms so a cold font or device stall does not cause another long visual
/// stall) before drawing a scheduled frame. Never sleeps on the UI thread or
/// clears dirty state while waiting.
#[derive(Default)]
pub(crate) struct FramePacer {
    last_frame: Option<(Instant, Duration)>,
}

impl FramePacer {
    pub(crate) fn ready(&self, low_power: bool, required: bool, now: Instant) -> bool {
        if !low_power || required {
            return true;
        }
        self.last_frame.is_none_or(|(completed, cost)| {
            // The periodic scheduler already limits the maximum frequency.
            now.saturating_duration_since(completed) >= cost.min(Duration::from_millis(100))
        })
    }

    pub(crate) fn presented(&mut self, completed: Instant, cost: Duration) {
        self.last_frame = Some((completed, cost));
    }
}

/// Opt-in aggregate frame diagnostics in the log (`GPUI_RENDER_DIAGNOSTICS=1`),
/// every five seconds while frames run. No coordinates or contents are logged.
static DIAGNOSTICS: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
    std::env::var("GPUI_RENDER_DIAGNOSTICS").is_ok_and(|value| value != "0")
});

pub(crate) struct FrameDiagnostics {
    since: Instant,
    frames: u64,
    deferred: u64,
    total: Duration,
    longest: Duration,
}

impl Default for FrameDiagnostics {
    fn default() -> Self {
        Self {
            since: Instant::now(),
            frames: 0,
            deferred: 0,
            total: Duration::ZERO,
            longest: Duration::ZERO,
        }
    }
}

impl FrameDiagnostics {
    pub(crate) fn record(&mut self, low_power: bool, cost: Option<Duration>, deferred: bool) {
        if !*DIAGNOSTICS {
            return;
        }
        if let Some(cost) = cost {
            self.frames += 1;
            self.total += cost;
            self.longest = self.longest.max(cost);
        }
        self.deferred += u64::from(deferred);
        let elapsed = self.since.elapsed();
        if elapsed >= Duration::from_secs(5) {
            log::info!(
                "gpui rendering: low_power={low_power} software_effects={} seconds={:.1} frames={} fps={:.1} frame_ms_avg={:.1} frame_ms_max={:.1} deferred={}",
                software_rendering_effects(),
                elapsed.as_secs_f64(),
                self.frames,
                self.frames as f64 / elapsed.as_secs_f64(),
                self.total.as_secs_f64() * 1000.0 / self.frames.max(1) as f64,
                self.longest.as_secs_f64() * 1000.0,
                self.deferred,
            );
            *self = Self::default();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expensive_frames_leave_recovery_time_instead_of_running_back_to_back() {
        let completed = Instant::now();
        let mut pacer = FramePacer::default();
        assert!(pacer.ready(true, false, completed));
        pacer.presented(completed, Duration::from_millis(80));
        assert!(!pacer.ready(true, false, completed + Duration::from_millis(33)));
        assert!(!pacer.ready(true, false, completed + Duration::from_millis(79)));
        assert!(pacer.ready(true, false, completed + Duration::from_millis(80)));
    }

    #[test]
    fn cheap_frames_keep_thirty_hz_and_idle_resumes_immediately() {
        let completed = Instant::now();
        let mut pacer = FramePacer::default();
        pacer.presented(completed, Duration::from_millis(5));
        assert!(!pacer.ready(true, false, completed + Duration::from_millis(4)));
        assert!(pacer.ready(true, false, completed + Duration::from_millis(5)));
        assert!(pacer.ready(true, false, completed + Duration::from_secs(1)));
    }

    #[test]
    fn required_presentations_and_normal_mode_bypass_recovery() {
        let now = Instant::now();
        let mut pacer = FramePacer::default();
        pacer.presented(now, Duration::from_millis(80));
        assert!(pacer.ready(true, true, now));
        assert!(pacer.ready(false, false, now));
    }

    #[test]
    fn cold_frame_does_not_cause_another_multi_second_visual_stall() {
        let now = Instant::now();
        let mut pacer = FramePacer::default();
        pacer.presented(now, Duration::from_secs(5));
        assert!(!pacer.ready(true, false, now + Duration::from_millis(99)));
        assert!(pacer.ready(true, false, now + Duration::from_millis(100)));
    }

    #[test]
    fn explicit_and_pending_presentations_are_kept() {
        for high_rate_input in [false, true] {
            assert!(requires_presentation(true, false, high_rate_input));
            assert!(requires_presentation(false, true, high_rate_input));
        }
        assert!(!requires_presentation(false, false, false));
    }

    #[test]
    fn only_windows_stops_presenting_unchanged_scenes_during_high_rate_input() {
        assert_eq!(
            requires_presentation(false, false, true),
            !cfg!(target_os = "windows")
        );
    }

    #[test]
    fn only_low_power_mode_uses_a_thirty_hz_timer() {
        assert_eq!(low_power_frame_interval(false), None);
        let interval = low_power_frame_interval(true).unwrap();
        assert!(interval.as_nanos() * 30 >= Duration::from_secs(1).as_nanos());
        assert!(interval < Duration::from_millis(34));
    }
}
