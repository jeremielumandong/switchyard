//! The run engine: which step runs next, when a run waits for the user and when it stops.
//!
//! The engine is a pure state machine. Its owner (the runbook tab) executes the step it
//! names, reports the outcome with [`RunEngine::completed`] and answers pauses with
//! [`RunEngine::proceed`] or [`RunEngine::skip`]. Tests drive it with fake executors.
//!
//! Rules: a Note step completes at once; a Confirm step and a Shell step pause for the
//! user (a Shell step runs only after the user clicks Run on it), except when the user
//! started exactly that step ([`RunMode::One`]); a failure stops the run and marks the
//! steps that did not run as skipped.

use switchyard_store::StepKind;

/// Status of a step in the current run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepStatus {
    /// Not run (yet).
    Pending,
    /// Waiting for the user (Confirm, Shell).
    Waiting,
    /// Executing.
    Running,
    /// Done.
    Ok,
    /// Failed or cancelled.
    Failed,
    /// Not run: skipped by the user, or after a failure.
    Skipped,
}

impl StepStatus {
    /// Label for the UI.
    pub fn label(self) -> &'static str {
        match self {
            StepStatus::Pending => "pending",
            StepStatus::Waiting => "waiting",
            StepStatus::Running => "running",
            StepStatus::Ok => "ok",
            StepStatus::Failed => "failed",
            StepStatus::Skipped => "skipped",
        }
    }
}

/// How a step behaves in the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    /// Completes without running anything (Note).
    Instant,
    /// Runs at once (SQL, HTTP).
    Auto,
    /// Pauses for the user, then completes (Confirm).
    Checkpoint,
    /// Pauses for the user, then runs (Shell).
    Manual,
}

impl Gate {
    /// The gate of a step kind.
    pub fn of(kind: &StepKind) -> Self {
        match kind {
            StepKind::Note { .. } => Gate::Instant,
            StepKind::Sql { .. } | StepKind::Http(_) => Gate::Auto,
            StepKind::Confirm { .. } => Gate::Checkpoint,
            StepKind::Shell { .. } => Gate::Manual,
        }
    }
}

/// Which steps a run covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunMode {
    /// Only this step (the user clicked Run on it, so it does not pause).
    One(usize),
    /// This step and the ones after it.
    FromHere(usize),
    /// Every step.
    All,
}

/// What the owner does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// Execute step `i` and report with [`RunEngine::completed`].
    Execute(usize),
    /// Step `i` waits for the user ([`RunEngine::proceed`] / [`RunEngine::skip`]).
    Wait(usize),
    /// The run ended.
    Finished(Outcome),
    /// No run is active (or the call did not apply).
    Idle,
}

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Every covered step is ok or skipped by the user.
    Completed,
    /// Step `i` failed; the rest did not run.
    Failed(usize),
    /// The user stopped the run.
    Stopped,
}

/// The run state of one runbook.
#[derive(Clone, Debug)]
pub struct RunEngine {
    gates: Vec<Gate>,
    status: Vec<StepStatus>,
    /// Next step to look at; `None` when no run is active.
    cursor: Option<usize>,
    /// Last step the run covers (inclusive).
    last: usize,
    mode: Option<RunMode>,
}

impl RunEngine {
    /// An idle engine for steps with these gates (all pending).
    pub fn new(gates: Vec<Gate>) -> Self {
        Self {
            status: vec![StepStatus::Pending; gates.len()],
            gates,
            cursor: None,
            last: 0,
            mode: None,
        }
    }

    /// An idle engine for `steps`.
    pub fn for_steps<'a>(steps: impl IntoIterator<Item = &'a StepKind>) -> Self {
        Self::new(steps.into_iter().map(Gate::of).collect())
    }

    /// Status of every step.
    pub fn statuses(&self) -> &[StepStatus] {
        &self.status
    }

    /// Status of step `i`.
    pub fn status(&self, i: usize) -> StepStatus {
        self.status.get(i).copied().unwrap_or(StepStatus::Pending)
    }

    /// Whether a run is active.
    pub fn is_active(&self) -> bool {
        self.cursor.is_some()
    }

    /// The run's mode, while active.
    pub fn mode(&self) -> Option<RunMode> {
        self.mode
    }

    /// Start a run (refused while one is active). Steps it covers go back to pending.
    pub fn start(&mut self, mode: RunMode) -> Next {
        if self.is_active() || self.gates.is_empty() {
            return Next::Idle;
        }
        let (first, last) = match mode {
            RunMode::One(i) => (i, i),
            RunMode::FromHere(i) => (i, self.gates.len() - 1),
            RunMode::All => (0, self.gates.len() - 1),
        };
        if first >= self.gates.len() {
            return Next::Idle;
        }
        for s in &mut self.status[first..=last] {
            *s = StepStatus::Pending;
        }
        self.cursor = Some(first);
        self.last = last;
        self.mode = Some(mode);
        self.advance()
    }

    /// Look at the step under the cursor.
    fn advance(&mut self) -> Next {
        loop {
            let Some(i) = self.cursor else {
                return Next::Idle;
            };
            if i > self.last {
                return self.finish(Outcome::Completed);
            }
            let started_here = self.mode == Some(RunMode::One(i));
            match self.gates[i] {
                Gate::Instant => {
                    self.status[i] = StepStatus::Ok;
                    self.cursor = Some(i + 1);
                }
                Gate::Auto => {
                    self.status[i] = StepStatus::Running;
                    return Next::Execute(i);
                }
                Gate::Checkpoint if started_here => {
                    self.status[i] = StepStatus::Ok;
                    self.cursor = Some(i + 1);
                }
                Gate::Manual if started_here => {
                    self.status[i] = StepStatus::Running;
                    return Next::Execute(i);
                }
                Gate::Checkpoint | Gate::Manual => {
                    self.status[i] = StepStatus::Waiting;
                    return Next::Wait(i);
                }
            }
        }
    }

    fn finish(&mut self, outcome: Outcome) -> Next {
        self.cursor = None;
        self.mode = None;
        Next::Finished(outcome)
    }

    fn skip_rest(&mut self, from: usize) {
        let last = self.last.min(self.status.len().saturating_sub(1));
        for s in self.status.iter_mut().take(last + 1).skip(from) {
            if matches!(s, StepStatus::Pending | StepStatus::Waiting) {
                *s = StepStatus::Skipped;
            }
        }
    }

    /// The step the run is executing finished. A failure ends the run.
    pub fn completed(&mut self, i: usize, ok: bool) -> Next {
        if self.cursor != Some(i) || self.status(i) != StepStatus::Running {
            return Next::Idle;
        }
        if ok {
            self.status[i] = StepStatus::Ok;
            self.cursor = Some(i + 1);
            self.advance()
        } else {
            self.status[i] = StepStatus::Failed;
            self.skip_rest(i + 1);
            self.finish(Outcome::Failed(i))
        }
    }

    /// The user answered a waiting step with Run (Shell) or Continue (Confirm).
    pub fn proceed(&mut self, i: usize) -> Next {
        if self.cursor != Some(i) || self.status(i) != StepStatus::Waiting {
            return Next::Idle;
        }
        match self.gates[i] {
            Gate::Manual => {
                self.status[i] = StepStatus::Running;
                Next::Execute(i)
            }
            _ => {
                self.status[i] = StepStatus::Ok;
                self.cursor = Some(i + 1);
                self.advance()
            }
        }
    }

    /// The user skipped a waiting step; the run goes on.
    pub fn skip(&mut self, i: usize) -> Next {
        if self.cursor != Some(i) || self.status(i) != StepStatus::Waiting {
            return Next::Idle;
        }
        self.status[i] = StepStatus::Skipped;
        self.cursor = Some(i + 1);
        self.advance()
    }

    /// The user stopped the run. A running step is marked failed (its cancel is the
    /// owner's to send); the rest are skipped.
    pub fn stop(&mut self) -> Next {
        let Some(i) = self.cursor else {
            return Next::Idle;
        };
        if let Some(s) = self.status.get_mut(i)
            && *s == StepStatus::Running
        {
            *s = StepStatus::Failed;
        }
        self.skip_rest(i);
        self.finish(Outcome::Stopped)
    }

    /// The step list changed (edit mode): every status resets and any run ends.
    pub fn reset(&mut self, gates: Vec<Gate>) {
        *self = Self::new(gates);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use Gate::*;
    use StepStatus as S;

    /// Drive a run to its end: `exec` is the fake executor (step → success), `user`
    /// answers pauses (step → Some(true) run/continue, Some(false) skip, None stop).
    fn drive(
        e: &mut RunEngine,
        mode: RunMode,
        mut exec: impl FnMut(usize) -> bool,
        mut user: impl FnMut(usize) -> Option<bool>,
    ) -> (Outcome, Vec<usize>, Vec<usize>) {
        let mut ran = Vec::new();
        let mut waited = Vec::new();
        let mut next = e.start(mode);
        loop {
            next = match next {
                Next::Execute(i) => {
                    assert_eq!(e.status(i), S::Running);
                    ran.push(i);
                    let ok = exec(i);
                    e.completed(i, ok)
                }
                Next::Wait(i) => {
                    assert_eq!(e.status(i), S::Waiting);
                    waited.push(i);
                    match user(i) {
                        Some(true) => e.proceed(i),
                        Some(false) => e.skip(i),
                        None => e.stop(),
                    }
                }
                Next::Finished(o) => {
                    assert!(!e.is_active());
                    return (o, ran, waited);
                }
                Next::Idle => panic!("engine went idle mid-run"),
            }
        }
    }

    #[test]
    fn run_all_pauses_on_confirm_and_shell() {
        let mut e = RunEngine::new(vec![Instant, Auto, Checkpoint, Manual, Auto]);
        let (o, ran, waited) = drive(&mut e, RunMode::All, |_| true, |_| Some(true));
        assert_eq!(o, Outcome::Completed);
        assert_eq!(ran, [1, 3, 4], "the shell step runs after the user's Run");
        assert_eq!(waited, [2, 3]);
        assert!(e.statuses().iter().all(|s| *s == S::Ok));
    }

    #[test]
    fn stops_on_first_failure() {
        let mut e = RunEngine::new(vec![Auto, Auto, Auto, Checkpoint]);
        let (o, ran, waited) = drive(&mut e, RunMode::All, |i| i != 1, |_| Some(true));
        assert_eq!(o, Outcome::Failed(1));
        assert_eq!(ran, [0, 1]);
        assert!(waited.is_empty());
        assert_eq!(e.statuses(), [S::Ok, S::Failed, S::Skipped, S::Skipped]);
    }

    #[test]
    fn skipping_a_pause_continues_and_stop_ends() {
        let mut e = RunEngine::new(vec![Manual, Auto, Checkpoint, Auto]);
        let (o, ran, _) = drive(
            &mut e,
            RunMode::All,
            |_| true,
            |i| if i == 0 { Some(false) } else { None },
        );
        assert_eq!(o, Outcome::Stopped);
        assert_eq!(ran, [1]);
        assert_eq!(e.statuses(), [S::Skipped, S::Ok, S::Skipped, S::Skipped]);
    }

    #[test]
    fn run_one_and_from_here() {
        let mut e = RunEngine::new(vec![Auto, Manual, Checkpoint, Auto]);
        // Run on the shell step itself: no pause.
        let (o, ran, waited) = drive(&mut e, RunMode::One(1), |_| true, |_| None);
        assert_eq!((o, ran, waited), (Outcome::Completed, vec![1], vec![]));
        assert_eq!(e.statuses(), [S::Pending, S::Ok, S::Pending, S::Pending]);
        // Run on a confirm step: done at once.
        let (o, ran, _) = drive(&mut e, RunMode::One(2), |_| true, |_| None);
        assert_eq!((o, ran.len()), (Outcome::Completed, 0));
        assert_eq!(e.status(2), S::Ok);
        // From step 1: the shell step pauses this time.
        let (o, ran, waited) = drive(&mut e, RunMode::FromHere(1), |_| true, |_| Some(true));
        assert_eq!(o, Outcome::Completed);
        assert_eq!(ran, [1, 3]);
        assert_eq!(waited, [1, 2]);
        assert_eq!(e.status(0), S::Pending, "steps before are untouched");
    }

    #[test]
    fn stray_answers_are_ignored() {
        let mut e = RunEngine::new(vec![Auto, Checkpoint]);
        assert_eq!(e.completed(0, true), Next::Idle, "nothing running");
        assert_eq!(e.start(RunMode::All), Next::Execute(0));
        assert_eq!(e.start(RunMode::All), Next::Idle, "one run at a time");
        assert_eq!(e.proceed(0), Next::Idle, "not waiting");
        assert_eq!(e.completed(1, true), Next::Idle, "not the running step");
        assert_eq!(e.completed(0, true), Next::Wait(1));
        assert_eq!(e.stop(), Next::Finished(Outcome::Stopped));
        assert_eq!(e.statuses(), [S::Ok, S::Skipped]);
        assert_eq!(e.start(RunMode::One(5)), Next::Idle);
        let mut empty = RunEngine::new(vec![]);
        assert_eq!(empty.start(RunMode::All), Next::Idle);
    }

    #[test]
    fn stop_while_running_fails_the_step() {
        let mut e = RunEngine::new(vec![Auto, Auto]);
        assert_eq!(e.start(RunMode::All), Next::Execute(0));
        assert_eq!(e.stop(), Next::Finished(Outcome::Stopped));
        assert_eq!(e.statuses(), [S::Failed, S::Skipped]);
        // A late completion of the stopped step changes nothing.
        assert_eq!(e.completed(0, true), Next::Idle);
        assert_eq!(e.status(0), S::Failed);
    }

    #[test]
    fn gates_of_kinds() {
        use switchyard_store::StepKind;
        let kinds = ["MD", "SQL", "API", "SH", "OK?"].map(|b| StepKind::empty(b).unwrap());
        let e = RunEngine::for_steps(kinds.iter());
        assert_eq!(e.gates, [Instant, Auto, Auto, Manual, Checkpoint]);
    }
}
