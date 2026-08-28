//! The private readiness module: one leaf policy record per child, one
//! aggregate state, and one place for scheduling, threshold, cancellation,
//! and snapshot rules.

use std::time::{Duration, Instant};

use crate::model::{ReadinessConfig, ReadinessProbe};
use crate::runtime::{ProcessId, RunId};
use crate::supervisor::seam::{
    AttemptId, ExecContext, LogMatcherIntent, ProbeIntent, ProbeSeam, WorkId,
};

use super::core::{Core, FailureKind, FailureSummary, Lifecycle};
use super::snapshot::{ReadinessCheckKind, ReadinessChildStatus, ReadinessStatus};

/// The current state of one Service readiness check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadinessState {
    /// No success threshold has been reached for this Run.
    Pending,
    /// The success threshold currently holds.
    Passing,
    /// The failure threshold has been reached after readiness.
    Failing,
}

/// Live readiness bookkeeping for one probed Service's current Run.
#[derive(Debug)]
pub(super) struct ReadinessTracking {
    /// Session time when readiness evaluation began after spawn.
    started_at_ms: u64,
    /// One state record for every leaf check. The enum keeps scheduled probe
    /// state out of live log children.
    checks: Vec<ReadinessCheckTracking>,
    /// Deadline for the complete readiness policy to pass, when configured.
    startup_deadline: Option<Instant>,
    /// False while a log matcher is waiting for the Run's Spawned fact. Work
    /// identities exist before spawn, but timers must start at spawn.
    started: bool,
}

/// One readiness child. Live log children only carry the state needed for a
/// latched observation; scheduled probes carry the additional attempt state.
#[derive(Debug)]
enum ReadinessCheckTracking {
    Probe(ProbeCheckTracking),
    Log(LogCheckTracking),
}

#[derive(Debug)]
struct CheckProgress {
    /// Stable identity of this check within its Run.
    work_id: WorkId,
    /// Observations or attempts recorded for this check.
    attempts: u32,
    state: ReadinessState,
    consecutive_successes: u32,
    consecutive_failures: u32,
    last_error: Option<String>,
}

#[derive(Debug)]
struct ProbeCheckTracking {
    progress: CheckProgress,
    /// The next attempt identity. Attempt identities are never reused.
    next_attempt_id: u64,
    /// One bounded attempt is out with the probe adapter; attempts for one
    /// check never overlap.
    in_flight: Option<AttemptId>,
    /// Earliest time the next attempt may be dispatched.
    next_attempt_at: Instant,
}

#[derive(Debug)]
struct LogCheckTracking {
    progress: CheckProgress,
}

impl ReadinessCheckTracking {
    fn progress(&self) -> &CheckProgress {
        match self {
            Self::Probe(check) => &check.progress,
            Self::Log(check) => &check.progress,
        }
    }
}

impl ReadinessTracking {
    /// Aggregate state for the policy. A failing child is visible after it
    /// has reached its own failure threshold; otherwise an incomplete policy
    /// stays pending until every child passes.
    pub(super) fn state(&self) -> ReadinessState {
        if !self.checks.is_empty()
            && self
                .checks
                .iter()
                .all(|check| check.progress().state == ReadinessState::Passing)
        {
            ReadinessState::Passing
        } else if self
            .checks
            .iter()
            .any(|check| check.progress().state == ReadinessState::Failing)
        {
            ReadinessState::Failing
        } else {
            ReadinessState::Pending
        }
    }

    /// Cancel every scheduled probe operation for this Run. Log observation
    /// has no adapter operation to cancel; its Run observer is canceled by the
    /// runtime seam.
    pub(super) fn cancel(&self, probes: &dyn ProbeSeam, process_id: ProcessId, run_id: RunId) {
        for check in &self.checks {
            if let ReadinessCheckTracking::Probe(check) = check {
                probes.cancel(process_id, run_id, check.progress.work_id);
            }
        }
    }

    /// Apply one result to its matching child. `None` means that the child or
    /// attempt identity is stale and the result must be ignored.
    pub(super) fn apply_result(
        &mut self,
        config: &ReadinessConfig,
        work_id: WorkId,
        attempt_id: AttemptId,
        now: Instant,
        passing: bool,
        diagnostic: Option<String>,
    ) -> Option<bool> {
        let check_index = self
            .checks
            .iter()
            .position(|check| check.progress().work_id == work_id)?;
        let check_config = config.checks.get(check_index)?;
        let ReadinessCheckTracking::Probe(check) = self.checks.get_mut(check_index)? else {
            return None;
        };
        if check.in_flight != Some(attempt_id) {
            return None;
        }

        check.in_flight = None;
        check.next_attempt_at = now + check_config.interval;
        let progress = &mut check.progress;
        if passing {
            progress.consecutive_successes = progress.consecutive_successes.saturating_add(1);
            progress.consecutive_failures = 0;
            if progress.consecutive_successes >= check_config.success_threshold {
                progress.state = ReadinessState::Passing;
            }
        } else {
            progress.consecutive_failures = progress.consecutive_failures.saturating_add(1);
            progress.consecutive_successes = 0;
            progress.last_error = diagnostic;
            if progress.state == ReadinessState::Passing
                && progress.consecutive_failures >= check_config.failure_threshold
            {
                progress.state = ReadinessState::Failing;
            }
        }
        Some(self.state() == ReadinessState::Passing)
    }

    /// Apply the first live match for one log child. A log match is one
    /// observation, not a scheduled attempt, but it passes immediately and
    /// stays latched until this Run ends.
    pub(super) fn apply_log_match(&mut self, work_id: WorkId) -> Option<bool> {
        let check_index = self
            .checks
            .iter()
            .position(|check| check.progress().work_id == work_id)?;
        let ReadinessCheckTracking::Log(check) = self.checks.get_mut(check_index)? else {
            return None;
        };
        let progress = &mut check.progress;
        if progress.state == ReadinessState::Passing {
            return None;
        }
        progress.attempts = progress.attempts.saturating_add(1);
        progress.consecutive_successes = progress.consecutive_successes.saturating_add(1);
        progress.consecutive_failures = 0;
        progress.state = ReadinessState::Passing;
        Some(self.state() == ReadinessState::Passing)
    }

    pub(super) fn clear_startup_deadline(&mut self) {
        self.startup_deadline = None;
    }

    /// Return the live log checks that must be attached to the Run's output
    /// observer. The configured literal remains the source of truth.
    pub(super) fn log_matchers(&self, config: &ReadinessConfig) -> Vec<LogMatcherIntent> {
        self.checks
            .iter()
            .zip(&config.checks)
            .filter_map(|(tracking, check_config)| {
                let ReadinessCheckTracking::Log(_) = tracking else {
                    return None;
                };
                let ReadinessProbe::Log { contains } = &check_config.probe else {
                    return None;
                };
                Some(LogMatcherIntent {
                    work_id: tracking.progress().work_id,
                    contains: contains.clone(),
                })
            })
            .collect()
    }

    /// Return the scheduled probe children that are due now. Live log
    /// children have no timer and therefore never enter this iterator.
    pub(super) fn due_probe_indices(&self, now: Instant) -> impl Iterator<Item = usize> + '_ {
        self.checks
            .iter()
            .enumerate()
            .filter_map(move |(index, check)| {
                let ReadinessCheckTracking::Probe(check) = check else {
                    return None;
                };
                (check.in_flight.is_none() && check.next_attempt_at <= now).then_some(index)
            })
    }

    /// Start one scheduled probe and reserve its attempt identity.
    pub(super) fn begin_probe(
        &mut self,
        check_index: usize,
        process_id: ProcessId,
        run_id: RunId,
        probe: ReadinessProbe,
        timeout: Duration,
        exec_context: Option<ExecContext>,
    ) -> Option<ProbeIntent> {
        let ReadinessCheckTracking::Probe(check) = self.checks.get_mut(check_index)? else {
            return None;
        };
        if check.in_flight.is_some() {
            return None;
        }
        let attempt_id = AttemptId::new(check.next_attempt_id);
        check.next_attempt_id += 1;
        check.in_flight = Some(attempt_id);
        check.progress.attempts += 1;
        Some(ProbeIntent {
            process_id,
            run_id,
            work_id: check.progress.work_id,
            attempt_id,
            probe,
            timeout,
            exec_context,
        })
    }

    /// How long until the next scheduled probe becomes due.
    pub(super) fn next_probe_wait(&self, now: Instant) -> Option<Duration> {
        self.checks
            .iter()
            .filter_map(|check| {
                let ReadinessCheckTracking::Probe(check) = check else {
                    return None;
                };
                check
                    .in_flight
                    .is_none()
                    .then(|| check.next_attempt_at.saturating_duration_since(now))
            })
            .min()
    }

    /// Start timing and scheduled checks when the Run reports Spawned. A
    /// duplicate Spawned fact does not reset the current Run's progress.
    pub(super) fn activate(&mut self, config: &ReadinessConfig, now: Instant, now_ms: u64) {
        if self.started {
            return;
        }
        self.started = true;
        self.started_at_ms = now_ms;
        self.startup_deadline = (self.state() != ReadinessState::Passing)
            .then_some(config.startup_timeout)
            .flatten()
            .map(|timeout| now + timeout);
        for (check, check_config) in self.checks.iter_mut().zip(&config.checks) {
            if let ReadinessCheckTracking::Probe(check) = check {
                check.next_attempt_at = now + check_config.initial_delay;
            }
        }
    }

    /// Project the authoritative child state into the public snapshot.
    pub(super) fn snapshot(&self, config: &ReadinessConfig, now_ms: u64) -> ReadinessStatus {
        let is_composite = config.checks.len() > 1;
        let kind = if is_composite {
            ReadinessCheckKind::All
        } else {
            ReadinessCheckKind::from(
                &config
                    .checks
                    .first()
                    .expect("a readiness config has at least one check")
                    .probe,
            )
        };
        let attempts = self.checks.iter().fold(0_u32, |total, check| {
            total.saturating_add(check.progress().attempts)
        });
        let consecutive_successes = self.checks.iter().fold(0_u32, |total, check| {
            total.saturating_add(check.progress().consecutive_successes)
        });
        let consecutive_failures = self.checks.iter().fold(0_u32, |total, check| {
            total.saturating_add(check.progress().consecutive_failures)
        });
        let children = if is_composite {
            self.checks
                .iter()
                .zip(&config.checks)
                .enumerate()
                .map(|(index, (tracking, config))| ReadinessChildStatus {
                    index: index + 1,
                    kind: ReadinessCheckKind::from(&config.probe),
                    state: tracking.progress().state,
                    attempts: tracking.progress().attempts,
                    consecutive_successes: tracking.progress().consecutive_successes,
                    consecutive_failures: tracking.progress().consecutive_failures,
                    last_error: tracking.progress().last_error.clone(),
                })
                .collect()
        } else {
            Vec::new()
        };
        let error = self
            .checks
            .iter()
            .enumerate()
            .filter(|(_, check)| !is_composite || check.progress().state != ReadinessState::Passing)
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
        ReadinessStatus {
            kind,
            state: self.state(),
            attempts,
            consecutive_successes,
            consecutive_failures,
            last_error,
            startup_elapsed_ms: now_ms.saturating_sub(self.started_at_ms),
            children,
        }
    }
}

impl Core {
    pub(super) fn new_readiness_tracking(
        &mut self,
        index: usize,
        now: Instant,
        started: bool,
    ) -> Option<ReadinessTracking> {
        let config = self.project.processes()[index].readiness.as_ref()?;
        if config.checks.is_empty() {
            return None;
        }
        let check_specs = config
            .checks
            .iter()
            .map(|check| (check.probe.clone(), check.initial_delay))
            .collect::<Vec<_>>();
        let startup_deadline = started
            .then_some(config.startup_timeout)
            .flatten()
            .map(|timeout| now + timeout);
        let checks = check_specs
            .into_iter()
            .map(|(probe, initial_delay)| {
                let progress = CheckProgress {
                    work_id: self.allocate_work_id(index),
                    attempts: 0,
                    state: ReadinessState::Pending,
                    consecutive_successes: 0,
                    consecutive_failures: 0,
                    last_error: None,
                };
                match probe {
                    ReadinessProbe::Log { .. } => {
                        ReadinessCheckTracking::Log(LogCheckTracking { progress })
                    }
                    _ => ReadinessCheckTracking::Probe(ProbeCheckTracking {
                        progress,
                        next_attempt_id: 1,
                        in_flight: None,
                        next_attempt_at: now + initial_delay,
                    }),
                }
            })
            .collect();
        Some(ReadinessTracking {
            started_at_ms: self.now_ms(),
            checks,
            startup_deadline,
            started,
        })
    }

    pub(super) fn poll_readiness_timers(&mut self, now: Instant) {
        self.expire_startup_timeouts(now);
        for (index, check_index) in self.due_probe_indices(now) {
            self.dispatch_probe(index, check_index);
        }
    }

    fn expire_startup_timeouts(&mut self, now: Instant) {
        let expired = self
            .active_readiness()
            .filter_map(|(index, run_id, tracking)| {
                tracking
                    .startup_deadline
                    .filter(|deadline| *deadline <= now)
                    .map(|_| (index, run_id))
            })
            .collect::<Vec<_>>();
        for (index, run_id) in expired {
            self.timeout_startup(index, run_id);
        }
    }

    /// The active readiness checks whose Run still desires Running and has
    /// reported Spawned. Log work may exist before that fact so early output
    /// has stable identities, but its timer must still start at Spawned.
    fn active_readiness(&self) -> impl Iterator<Item = (usize, RunId, &ReadinessTracking)> + '_ {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let run_id = entry.current_run?;
                let tracking = entry.readiness.as_ref()?;
                (entry.desired == super::core::DesiredState::Running && tracking.started)
                    .then_some((index, run_id, tracking))
            })
    }

    /// Turn an expired first-readiness deadline into a normal Run cleanup.
    /// Readiness cancellation happens before the Process Tree stop request.
    fn timeout_startup(&mut self, index: usize, run_id: RunId) {
        let timeout = self.project.processes()[index]
            .readiness
            .as_ref()
            .and_then(|config| config.startup_timeout)
            .expect("a startup deadline has a configured timeout");
        let process_id = self.entries[index].process_id;
        self.cancel_run_work(index);
        let entry = &mut self.entries[index];
        entry.startup_timeout_pending = true;
        entry.failure = Some(FailureSummary {
            kind: FailureKind::Readiness,
            detail: format!("readiness startup timeout after {} ms", timeout.as_millis()),
        });
        entry.desired = super::core::DesiredState::Stopped;
        entry.lifecycle = Lifecycle::Stopping;
        entry.blocked = None;
        self.seam.stop(process_id, run_id, None, &self.events);
    }

    fn due_probe_indices(&self, now: Instant) -> Vec<(usize, usize)> {
        self.active_readiness()
            .flat_map(|(index, _, tracking)| {
                tracking
                    .due_probe_indices(now)
                    .map(move |check_index| (index, check_index))
            })
            .collect()
    }

    /// Hand one bounded attempt for one due leaf to the probe adapter. Each
    /// child owns its in-flight gate, so independent children may run at the
    /// same time while one child never overlaps itself.
    fn dispatch_probe(&mut self, index: usize, check_index: usize) {
        let Some(config) = &self.project.processes()[index].readiness else {
            return;
        };
        let Some(check_config) = config.checks.get(check_index) else {
            return;
        };
        let probe = check_config.probe.clone();
        let timeout = check_config.timeout;
        let exec_context = matches!(&probe, ReadinessProbe::Exec { .. }).then(|| ExecContext {
            working_dir: self.project.processes()[index].working_dir.clone(),
            env: self.project.processes()[index].env.clone(),
            shell: self.project.shell().clone(),
        });
        let process_id = self.entries[index].process_id;
        let Some(run_id) = self.entries[index].current_run else {
            return;
        };
        let Some(tracking) = self.entries[index].readiness.as_mut() else {
            return;
        };
        let Some(intent) = tracking.begin_probe(
            check_index,
            process_id,
            run_id,
            probe,
            timeout,
            exec_context,
        ) else {
            return;
        };
        self.probes.probe(intent, &self.events);
    }

    /// How long until some readiness attempt or startup deadline becomes due.
    pub(super) fn readiness_time_until_next_timer(&self) -> Option<Duration> {
        let now = self.clock.now();
        let probe_wait = self
            .active_readiness()
            .filter_map(|(_, _, tracking)| tracking.next_probe_wait(now))
            .min();
        let startup_wait = self
            .active_readiness()
            .filter_map(|(_, _, tracking)| tracking.startup_deadline)
            .map(|deadline| deadline.saturating_duration_since(now))
            .min();
        probe_wait.into_iter().chain(startup_wait).min()
    }
}
