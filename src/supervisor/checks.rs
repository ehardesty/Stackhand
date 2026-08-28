//! Shared scheduling and threshold bookkeeping for readiness and liveness.
//!
//! The two policies have different lifecycle effects, but their leaf checks
//! have one implementation: each child owns its work identity, attempt
//! identity, cadence, thresholds, and bounded diagnostic.

use std::time::{Duration, Instant};

use crate::model::{ReadinessCheck, ReadinessProbe};
use crate::runtime::{ProcessId, RunId};
use crate::supervisor::seam::{
    AttemptId, ExecContext, LogMatcherIntent, ProbeIntent, ProbeScope, ProbeSeam, WorkId,
};

use super::snapshot::ReadinessCheckKind;

/// The threshold state shared by readiness and liveness checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckState {
    /// The policy has not been activated for this Run.
    Inactive,
    /// No success threshold has been reached for this policy.
    Pending,
    /// The success threshold currently holds.
    Passing,
    /// The failure threshold currently holds.
    Failing,
}

/// Compatibility names that keep the public state vocabulary aligned with
/// the two user-facing policies while both use the same representation.
pub use CheckState as LivenessState;
pub use CheckState as ReadinessState;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CheckMode {
    Readiness,
    Liveness,
}

#[derive(Debug)]
pub(super) struct CheckResult {
    pub(super) work_id: WorkId,
    pub(super) attempt_id: AttemptId,
    pub(super) now: Instant,
    pub(super) passing: bool,
    pub(super) diagnostic: Option<String>,
}

/// One aggregate set of independently scheduled leaf checks.
#[derive(Debug)]
pub(super) struct CheckSet {
    checks: Vec<CheckTracking>,
}

#[derive(Debug)]
enum CheckTracking {
    Probe(ProbeCheckTracking),
    Log(LogCheckTracking),
}

#[derive(Debug)]
struct CheckProgress {
    work_id: WorkId,
    attempts: u32,
    state: CheckState,
    consecutive_successes: u32,
    consecutive_failures: u32,
    last_error: Option<String>,
}

#[derive(Debug)]
struct ProbeCheckTracking {
    progress: CheckProgress,
    next_attempt_id: u64,
    in_flight: Option<AttemptId>,
    next_attempt_at: Instant,
}

#[derive(Debug)]
struct LogCheckTracking {
    progress: CheckProgress,
    next_attempt_id: u64,
    in_flight: Option<AttemptId>,
    next_attempt_at: Instant,
    attempt_deadline: Option<Instant>,
}

impl CheckTracking {
    fn progress(&self) -> &CheckProgress {
        match self {
            Self::Probe(check) => &check.progress,
            Self::Log(check) => &check.progress,
        }
    }

    fn progress_mut(&mut self) -> &mut CheckProgress {
        match self {
            Self::Probe(check) => &mut check.progress,
            Self::Log(check) => &mut check.progress,
        }
    }

    fn in_flight(&self) -> Option<AttemptId> {
        match self {
            Self::Probe(check) => check.in_flight,
            Self::Log(check) => check.in_flight,
        }
    }

    fn set_in_flight(&mut self, attempt_id: Option<AttemptId>) {
        match self {
            Self::Probe(check) => check.in_flight = attempt_id,
            Self::Log(check) => check.in_flight = attempt_id,
        }
    }

    fn set_next_attempt_at(&mut self, next_attempt_at: Instant) {
        match self {
            Self::Probe(check) => check.next_attempt_at = next_attempt_at,
            Self::Log(check) => check.next_attempt_at = next_attempt_at,
        }
    }
}

impl CheckSet {
    pub(super) fn new(
        config: &[ReadinessCheck],
        work_ids: impl IntoIterator<Item = WorkId>,
        now: Instant,
    ) -> Self {
        let checks = config
            .iter()
            .zip(work_ids)
            .map(|(check, work_id)| {
                let progress = CheckProgress {
                    work_id,
                    attempts: 0,
                    state: CheckState::Pending,
                    consecutive_successes: 0,
                    consecutive_failures: 0,
                    last_error: None,
                };
                match check.probe {
                    ReadinessProbe::Log { .. } => CheckTracking::Log(LogCheckTracking {
                        progress,
                        next_attempt_id: 1,
                        in_flight: None,
                        next_attempt_at: now,
                        attempt_deadline: None,
                    }),
                    _ => CheckTracking::Probe(ProbeCheckTracking {
                        progress,
                        next_attempt_id: 1,
                        in_flight: None,
                        next_attempt_at: now,
                    }),
                }
            })
            .collect();
        Self { checks }
    }

    pub(super) fn state(&self) -> CheckState {
        if !self.checks.is_empty()
            && self
                .checks
                .iter()
                .all(|check| check.progress().state == CheckState::Passing)
        {
            CheckState::Passing
        } else if self
            .checks
            .iter()
            .any(|check| check.progress().state == CheckState::Failing)
        {
            CheckState::Failing
        } else {
            CheckState::Pending
        }
    }

    pub(super) fn cancel(&self, probes: &dyn ProbeSeam, process_id: ProcessId, run_id: RunId) {
        for check in &self.checks {
            if let CheckTracking::Probe(check) = check {
                probes.cancel(process_id, run_id, check.progress.work_id);
            }
        }
    }

    /// Apply one result from a scheduled probe attempt. Liveness may also
    /// use this path for a timed-out log window.
    pub(super) fn apply_result(
        &mut self,
        configs: &[ReadinessCheck],
        result: CheckResult,
        mode: CheckMode,
    ) -> Option<CheckState> {
        let check_index = self
            .checks
            .iter()
            .position(|check| check.progress().work_id == result.work_id)?;
        let check_config = configs.get(check_index)?;
        let check = self.checks.get_mut(check_index)?;
        if check.in_flight() != Some(result.attempt_id) {
            return None;
        }
        check.set_in_flight(None);
        check.set_next_attempt_at(result.now + check_config.interval);
        if let CheckTracking::Log(check) = check {
            check.attempt_deadline = None;
        }
        record_result(
            check.progress_mut(),
            check_config,
            result.passing,
            result.diagnostic,
            mode == CheckMode::Liveness,
        );
        Some(self.state())
    }

    /// Apply one latched readiness log observation. Liveness logs use
    /// `complete_log_match` because each match belongs to an armed attempt.
    pub(super) fn apply_log_match(&mut self, work_id: WorkId) -> Option<CheckState> {
        let check_index = self
            .checks
            .iter()
            .position(|check| check.progress().work_id == work_id)?;
        let CheckTracking::Log(check) = self.checks.get_mut(check_index)? else {
            return None;
        };
        if check.in_flight.is_some() || check.progress.state == CheckState::Passing {
            return None;
        }
        check.progress.attempts = check.progress.attempts.saturating_add(1);
        check.progress.consecutive_successes =
            check.progress.consecutive_successes.saturating_add(1);
        check.progress.consecutive_failures = 0;
        check.progress.state = CheckState::Passing;
        Some(self.state())
    }

    /// The liveness log path needs the configured cadence and thresholds
    /// while keeping the attempt identity gate above.
    pub(super) fn complete_log_match(
        &mut self,
        configs: &[ReadinessCheck],
        work_id: WorkId,
        attempt_id: AttemptId,
        now: Instant,
    ) -> Option<CheckState> {
        let check_index = self
            .checks
            .iter()
            .position(|check| check.progress().work_id == work_id)?;
        let check_config = configs.get(check_index)?;
        let CheckTracking::Log(check) = self.checks.get_mut(check_index)? else {
            return None;
        };
        if check.in_flight != Some(attempt_id)
            || check
                .attempt_deadline
                .is_none_or(|deadline| deadline <= now)
        {
            return None;
        }
        check.in_flight = None;
        check.attempt_deadline = None;
        check.next_attempt_at = now + check_config.interval;
        record_result(&mut check.progress, check_config, true, None, true);
        Some(self.state())
    }

    pub(super) fn begin_probe(
        &mut self,
        check_index: usize,
        process_id: ProcessId,
        run_id: RunId,
        configs: &[ReadinessCheck],
        exec_context: Option<ExecContext>,
        scope: ProbeScope,
    ) -> Option<ProbeIntent> {
        let check_config = configs.get(check_index)?;
        let probe = check_config.probe.clone();
        let CheckTracking::Probe(check) = self.checks.get_mut(check_index)? else {
            return None;
        };
        if check.in_flight.is_some() {
            return None;
        }
        let attempt_id = AttemptId::new(check.next_attempt_id);
        check.next_attempt_id += 1;
        check.in_flight = Some(attempt_id);
        check.progress.attempts = check.progress.attempts.saturating_add(1);
        Some(ProbeIntent {
            process_id,
            run_id,
            work_id: check.progress.work_id,
            attempt_id,
            probe,
            timeout: check_config.timeout,
            exec_context,
            scope,
        })
    }

    pub(super) fn begin_log(
        &mut self,
        check_index: usize,
        configs: &[ReadinessCheck],
        now: Instant,
    ) -> Option<LogMatcherIntent> {
        let check_config = configs.get(check_index)?;
        let ReadinessProbe::Log { contains } = &check_config.probe else {
            return None;
        };
        let CheckTracking::Log(check) = self.checks.get_mut(check_index)? else {
            return None;
        };
        if check.in_flight.is_some() {
            return None;
        }
        let attempt_id = AttemptId::new(check.next_attempt_id);
        check.next_attempt_id += 1;
        check.in_flight = Some(attempt_id);
        check.attempt_deadline = Some(now + check_config.timeout);
        check.progress.attempts = check.progress.attempts.saturating_add(1);
        Some(LogMatcherIntent {
            work_id: check.progress.work_id,
            attempt_id: Some(attempt_id),
            contains: contains.clone(),
        })
    }

    pub(super) fn expired_log_attempts(
        &self,
        now: Instant,
    ) -> impl Iterator<Item = (WorkId, AttemptId)> + '_ {
        self.checks.iter().filter_map(move |check| {
            let CheckTracking::Log(check) = check else {
                return None;
            };
            let attempt_id = check.in_flight?;
            (check.attempt_deadline? <= now).then_some((check.progress.work_id, attempt_id))
        })
    }

    pub(super) fn due_indices(
        &self,
        now: Instant,
        mode: CheckMode,
    ) -> impl Iterator<Item = usize> + '_ {
        self.checks
            .iter()
            .enumerate()
            .filter_map(move |(index, check)| {
                let due = match check {
                    CheckTracking::Probe(check) => {
                        check.in_flight.is_none() && check.next_attempt_at <= now
                    }
                    CheckTracking::Log(check) => {
                        mode == CheckMode::Liveness
                            && check.in_flight.is_none()
                            && check.next_attempt_at <= now
                    }
                };
                due.then_some(index)
            })
    }

    pub(super) fn next_wait(&self, now: Instant, mode: CheckMode) -> Option<Duration> {
        self.checks
            .iter()
            .filter_map(|check| match check {
                CheckTracking::Probe(check) => check
                    .in_flight
                    .is_none()
                    .then(|| check.next_attempt_at.saturating_duration_since(now)),
                CheckTracking::Log(check) if mode == CheckMode::Liveness => check
                    .attempt_deadline
                    .or_else(|| check.in_flight.is_none().then_some(check.next_attempt_at))
                    .map(|deadline| deadline.saturating_duration_since(now)),
                CheckTracking::Log(_) => None,
            })
            .min()
    }

    pub(super) fn activate(&mut self, configs: &[ReadinessCheck], now: Instant, mode: CheckMode) {
        for (check, config) in self.checks.iter_mut().zip(configs) {
            match check {
                CheckTracking::Probe(check) => {
                    check.next_attempt_at = now + config.initial_delay;
                }
                CheckTracking::Log(check) if mode == CheckMode::Liveness => {
                    check.next_attempt_at = now + config.initial_delay;
                }
                CheckTracking::Log(_) => {}
            }
        }
    }

    pub(super) fn log_matchers(
        &self,
        configs: &[ReadinessCheck],
        attempt_id: Option<AttemptId>,
    ) -> Vec<LogMatcherIntent> {
        self.checks
            .iter()
            .zip(configs)
            .filter_map(|(check, config)| {
                let CheckTracking::Log(_) = check else {
                    return None;
                };
                let ReadinessProbe::Log { contains } = &config.probe else {
                    return None;
                };
                Some(LogMatcherIntent {
                    work_id: check.progress().work_id,
                    attempt_id,
                    contains: contains.clone(),
                })
            })
            .collect()
    }

    pub(super) fn snapshot(&self, configs: &[ReadinessCheck]) -> CheckSetSnapshot {
        let is_composite = configs.len() > 1;
        let kind = if is_composite {
            ReadinessCheckKind::All
        } else {
            ReadinessCheckKind::from(
                &configs
                    .first()
                    .expect("a check config has at least one check")
                    .probe,
            )
        };
        let (attempts, consecutive_successes, consecutive_failures) = self.checks.iter().fold(
            (0_u32, 0_u32, 0_u32),
            |(attempts, successes, failures), check| {
                (
                    attempts.saturating_add(check.progress().attempts),
                    successes.saturating_add(check.progress().consecutive_successes),
                    failures.saturating_add(check.progress().consecutive_failures),
                )
            },
        );
        let children = if is_composite {
            self.checks
                .iter()
                .zip(configs)
                .enumerate()
                .map(|(index, (check, config))| CheckChildSnapshot {
                    index: index + 1,
                    kind: ReadinessCheckKind::from(&config.probe),
                    state: check.progress().state,
                    attempts: check.progress().attempts,
                    consecutive_successes: check.progress().consecutive_successes,
                    consecutive_failures: check.progress().consecutive_failures,
                    last_error: check.progress().last_error.clone(),
                })
                .collect()
        } else {
            Vec::new()
        };
        let error = self
            .checks
            .iter()
            .enumerate()
            .filter(|(_, check)| !is_composite || check.progress().state != CheckState::Passing)
            .find_map(|(index, check)| {
                check
                    .progress()
                    .last_error
                    .as_ref()
                    .map(|error| (index, error))
            })
            .or_else(|| {
                self.checks.iter().enumerate().find_map(|(index, check)| {
                    check
                        .progress()
                        .last_error
                        .as_ref()
                        .map(|error| (index, error))
                })
            });
        let last_error = error.map(|(index, error)| {
            if is_composite {
                format!("all child {}: {error}", index + 1)
            } else {
                error.clone()
            }
        });
        CheckSetSnapshot {
            kind,
            state: self.state(),
            attempts,
            consecutive_successes,
            consecutive_failures,
            last_error,
            children,
        }
    }
}

fn record_result(
    progress: &mut CheckProgress,
    config: &ReadinessCheck,
    passing: bool,
    diagnostic: Option<String>,
    fail_before_passing: bool,
) {
    if passing {
        progress.consecutive_successes = progress.consecutive_successes.saturating_add(1);
        progress.consecutive_failures = 0;
        if progress.consecutive_successes >= config.success_threshold {
            progress.state = CheckState::Passing;
        }
    } else {
        progress.consecutive_failures = progress.consecutive_failures.saturating_add(1);
        progress.consecutive_successes = 0;
        progress.last_error = diagnostic;
        if (fail_before_passing || progress.state == CheckState::Passing)
            && progress.consecutive_failures >= config.failure_threshold
        {
            progress.state = CheckState::Failing;
        }
    }
}

#[derive(Debug)]
pub(super) struct CheckSetSnapshot {
    pub(super) kind: ReadinessCheckKind,
    pub(super) state: CheckState,
    pub(super) attempts: u32,
    pub(super) consecutive_successes: u32,
    pub(super) consecutive_failures: u32,
    pub(super) last_error: Option<String>,
    pub(super) children: Vec<CheckChildSnapshot>,
}

#[derive(Debug)]
pub(super) struct CheckChildSnapshot {
    pub(super) index: usize,
    pub(super) kind: ReadinessCheckKind,
    pub(super) state: CheckState,
    pub(super) attempts: u32,
    pub(super) consecutive_successes: u32,
    pub(super) consecutive_failures: u32,
    pub(super) last_error: Option<String>,
}
