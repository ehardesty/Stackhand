//! The private readiness module: one leaf policy record per child, one
//! aggregate state, and one place for scheduling, threshold, cancellation,
//! and snapshot rules.

use std::time::{Duration, Instant};

use crate::model::ReadinessConfig;
use crate::runtime::{ProcessId, RunId};
use crate::supervisor::seam::{AttemptId, ProbeIntent, ProbeSeam, WorkId};

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
    pub(super) started_at_ms: u64,
    /// One independently scheduled state record for every leaf check.
    pub(super) checks: Vec<ReadinessCheckTracking>,
    /// Deadline for the complete readiness policy to pass, when configured.
    pub(super) startup_deadline: Option<Instant>,
}

/// Mutable scheduling and threshold state for one leaf readiness check.
#[derive(Debug)]
pub(super) struct ReadinessCheckTracking {
    /// Stable identity of this check within its Run.
    pub(super) work_id: WorkId,
    /// Attempts dispatched for this check.
    pub(super) attempts: u32,
    pub(super) state: ReadinessState,
    pub(super) consecutive_successes: u32,
    pub(super) consecutive_failures: u32,
    /// The next attempt identity. Attempt identities are never reused.
    pub(super) next_attempt_id: u64,
    pub(super) last_error: Option<String>,
    /// One bounded attempt is out with the probe adapter; attempts for one
    /// check never overlap.
    pub(super) in_flight: Option<AttemptId>,
    /// Earliest time the next attempt may be dispatched.
    pub(super) next_attempt_at: Instant,
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
                .all(|check| check.state == ReadinessState::Passing)
        {
            ReadinessState::Passing
        } else if self
            .checks
            .iter()
            .any(|check| check.state == ReadinessState::Failing)
        {
            ReadinessState::Failing
        } else {
            ReadinessState::Pending
        }
    }

    /// Cancel every child operation for this Run.
    pub(super) fn cancel(&self, probes: &dyn ProbeSeam, process_id: ProcessId, run_id: RunId) {
        for check in &self.checks {
            probes.cancel(process_id, run_id, check.work_id);
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
            .position(|check| check.work_id == work_id)?;
        let check_config = config.checks.get(check_index)?;
        let check = self.checks.get_mut(check_index)?;
        if check.in_flight != Some(attempt_id) {
            return None;
        }

        check.in_flight = None;
        check.next_attempt_at = now + check_config.interval;
        if passing {
            check.consecutive_successes = check.consecutive_successes.saturating_add(1);
            check.consecutive_failures = 0;
            if check.consecutive_successes >= check_config.success_threshold {
                check.state = ReadinessState::Passing;
            }
        } else {
            check.consecutive_failures = check.consecutive_failures.saturating_add(1);
            check.consecutive_successes = 0;
            check.last_error = diagnostic;
            if check.state == ReadinessState::Passing
                && check.consecutive_failures >= check_config.failure_threshold
            {
                check.state = ReadinessState::Failing;
            }
        }
        Some(self.state() == ReadinessState::Passing)
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
        let attempts = self
            .checks
            .iter()
            .fold(0_u32, |total, check| total.saturating_add(check.attempts));
        let consecutive_successes = self.checks.iter().fold(0_u32, |total, check| {
            total.saturating_add(check.consecutive_successes)
        });
        let consecutive_failures = self.checks.iter().fold(0_u32, |total, check| {
            total.saturating_add(check.consecutive_failures)
        });
        let children = if is_composite {
            self.checks
                .iter()
                .zip(&config.checks)
                .enumerate()
                .map(|(index, (tracking, config))| ReadinessChildStatus {
                    index: index + 1,
                    kind: ReadinessCheckKind::from(&config.probe),
                    state: tracking.state,
                    attempts: tracking.attempts,
                    consecutive_successes: tracking.consecutive_successes,
                    consecutive_failures: tracking.consecutive_failures,
                    last_error: tracking.last_error.clone(),
                })
                .collect()
        } else {
            Vec::new()
        };
        let error = self
            .checks
            .iter()
            .enumerate()
            .filter(|(_, check)| !is_composite || check.state != ReadinessState::Passing)
            .find_map(|(index, check)| check.last_error.as_ref().map(|error| (index, error)))
            .or_else(|| {
                self.checks.iter().enumerate().find_map(|(index, check)| {
                    check.last_error.as_ref().map(|error| (index, error))
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
    ) -> Option<ReadinessTracking> {
        let config = self.project.processes()[index].readiness.as_ref()?;
        if config.checks.is_empty() {
            return None;
        }
        let initial_delays = config
            .checks
            .iter()
            .map(|check| check.initial_delay)
            .collect::<Vec<_>>();
        let startup_deadline = config.startup_timeout.map(|timeout| now + timeout);
        let checks = initial_delays
            .into_iter()
            .map(|initial_delay| ReadinessCheckTracking {
                work_id: self.allocate_work_id(index),
                attempts: 0,
                state: ReadinessState::Pending,
                consecutive_successes: 0,
                consecutive_failures: 0,
                next_attempt_id: 1,
                last_error: None,
                in_flight: None,
                next_attempt_at: now + initial_delay,
            })
            .collect();
        Some(ReadinessTracking {
            started_at_ms: self.now_ms(),
            checks,
            startup_deadline,
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

    /// The active readiness checks whose Run still desires Running.
    fn active_readiness(&self) -> impl Iterator<Item = (usize, RunId, &ReadinessTracking)> + '_ {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let run_id = entry.current_run?;
                let tracking = entry.readiness.as_ref()?;
                (entry.desired == super::core::DesiredState::Running)
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
                    .checks
                    .iter()
                    .enumerate()
                    .filter(|(_, check)| check.in_flight.is_none() && check.next_attempt_at <= now)
                    .map(move |(check_index, _)| (index, check_index))
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
        let intent = {
            let entry = &mut self.entries[index];
            let Some(run_id) = entry.current_run else {
                return;
            };
            let Some(tracking) = entry.readiness.as_mut() else {
                return;
            };
            let Some(check) = tracking.checks.get_mut(check_index) else {
                return;
            };
            if check.in_flight.is_some() {
                return;
            }
            let attempt_id = AttemptId::new(check.next_attempt_id);
            check.next_attempt_id += 1;
            check.in_flight = Some(attempt_id);
            check.attempts += 1;
            ProbeIntent {
                process_id: entry.process_id,
                run_id,
                work_id: check.work_id,
                attempt_id,
                probe,
                timeout,
            }
        };
        self.probes.probe(intent, &self.events);
    }

    /// How long until some readiness attempt or startup deadline becomes due.
    pub(super) fn readiness_time_until_next_timer(&self) -> Option<Duration> {
        let now = self.clock.now();
        let probe_wait = self
            .active_readiness()
            .flat_map(|(_, _, tracking)| {
                tracking
                    .checks
                    .iter()
                    .filter(|check| check.in_flight.is_none())
                    .map(|check| check.next_attempt_at.saturating_duration_since(now))
            })
            .min();
        let startup_wait = self
            .active_readiness()
            .filter_map(|(_, _, tracking)| tracking.startup_deadline)
            .map(|deadline| deadline.saturating_duration_since(now))
            .min();
        probe_wait.into_iter().chain(startup_wait).min()
    }
}
