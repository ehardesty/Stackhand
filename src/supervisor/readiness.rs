//! Readiness owns startup gating and deadline effects. Leaf scheduling,
//! threshold, cancellation, and projection mechanics live in the shared
//! check module so liveness uses the same policy without a second model.

use std::time::{Duration, Instant};

use crate::model::{ReadinessConfig, ReadinessProbe};
use crate::runtime::{ProcessId, RunId};
use crate::supervisor::checks::{CheckMode, CheckResult, CheckSet};
use crate::supervisor::seam::{
    AttemptId, ExecContext, LogMatcherIntent, ProbeIntent, ProbeScope, ProbeSeam, WorkId,
};

use super::core::{Core, FailureKind, FailureSummary, Lifecycle};
use super::snapshot::{ReadinessChildStatus, ReadinessStatus};

pub use super::checks::ReadinessState;

/// Live readiness bookkeeping for one probed Service's current Run.
#[derive(Debug)]
pub(super) struct ReadinessTracking {
    /// Session time when readiness evaluation began after spawn.
    started_at_ms: u64,
    checks: CheckSet,
    /// Deadline for the complete readiness policy to pass, when configured.
    startup_deadline: Option<Instant>,
    /// False while a log matcher is waiting for the Run's Spawned fact. Work
    /// identities exist before spawn, but timers must start at spawn.
    started: bool,
}

impl ReadinessTracking {
    pub(super) fn state(&self) -> ReadinessState {
        self.checks.state()
    }

    pub(super) fn cancel(&self, probes: &dyn ProbeSeam, process_id: ProcessId, run_id: RunId) {
        self.checks.cancel(probes, process_id, run_id);
    }

    pub(super) fn apply_result(
        &mut self,
        config: &ReadinessConfig,
        work_id: WorkId,
        attempt_id: AttemptId,
        now: Instant,
        passing: bool,
        diagnostic: Option<String>,
    ) -> Option<bool> {
        self.checks
            .apply_result(
                &config.checks,
                CheckResult {
                    work_id,
                    attempt_id,
                    now,
                    passing,
                    diagnostic,
                },
                CheckMode::Readiness,
            )
            .map(|_| self.state() == ReadinessState::Passing)
    }

    pub(super) fn apply_log_match(&mut self, work_id: WorkId) -> Option<bool> {
        self.checks
            .apply_log_match(work_id)
            .map(|_| self.state() == ReadinessState::Passing)
    }

    pub(super) fn clear_startup_deadline(&mut self) {
        self.startup_deadline = None;
    }

    pub(super) fn log_matchers(&self, config: &ReadinessConfig) -> Vec<LogMatcherIntent> {
        self.checks.log_matchers(&config.checks, None)
    }

    pub(super) fn due_probe_indices(&self, now: Instant) -> impl Iterator<Item = usize> + '_ {
        self.checks.due_indices(now, CheckMode::Readiness)
    }

    pub(super) fn next_wait(&self, now: Instant) -> Option<Duration> {
        self.checks.next_wait(now, CheckMode::Readiness)
    }

    pub(super) fn begin_probe(
        &mut self,
        check_index: usize,
        process_id: ProcessId,
        run_id: RunId,
        config: &ReadinessConfig,
        exec_context: Option<ExecContext>,
    ) -> Option<ProbeIntent> {
        self.checks.begin_probe(
            check_index,
            process_id,
            run_id,
            &config.checks,
            exec_context,
            ProbeScope::Readiness,
        )
    }

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
        self.checks
            .activate(&config.checks, now, CheckMode::Readiness);
    }

    pub(super) fn snapshot(&self, config: &ReadinessConfig, now_ms: u64) -> ReadinessStatus {
        let snapshot = self.checks.snapshot(&config.checks);
        ReadinessStatus {
            kind: snapshot.kind,
            state: snapshot.state,
            attempts: snapshot.attempts,
            consecutive_successes: snapshot.consecutive_successes,
            consecutive_failures: snapshot.consecutive_failures,
            last_error: snapshot.last_error,
            startup_elapsed_ms: now_ms.saturating_sub(self.started_at_ms),
            children: snapshot
                .children
                .into_iter()
                .map(|child| ReadinessChildStatus {
                    index: child.index,
                    kind: child.kind,
                    state: child.state,
                    attempts: child.attempts,
                    consecutive_successes: child.consecutive_successes,
                    consecutive_failures: child.consecutive_failures,
                    last_error: child.last_error,
                })
                .collect(),
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
        let config = self.project.processes()[index].readiness.clone()?;
        if config.checks.is_empty() {
            return None;
        }
        let work_ids = (0..config.checks.len())
            .map(|_| self.allocate_work_id(index))
            .collect::<Vec<_>>();
        let startup_deadline = started
            .then_some(config.startup_timeout)
            .flatten()
            .map(|timeout| now + timeout);
        let started_at_ms = self.now_ms();
        let mut tracking = ReadinessTracking {
            started_at_ms,
            checks: CheckSet::new(&config.checks, work_ids, now),
            startup_deadline,
            started: false,
        };
        if started {
            tracking.activate(&config, now, started_at_ms);
        }
        Some(tracking)
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

    fn dispatch_probe(&mut self, index: usize, check_index: usize) {
        let Some(config) = &self.project.processes()[index].readiness else {
            return;
        };
        let Some(check_config) = config.checks.get(check_index) else {
            return;
        };
        let probe = check_config.probe.clone();
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
        let Some(intent) =
            tracking.begin_probe(check_index, process_id, run_id, config, exec_context)
        else {
            return;
        };
        self.probes.probe(intent, &self.events);
    }

    pub(super) fn readiness_time_until_next_timer(&self) -> Option<Duration> {
        let now = self.clock.now();
        let probe_wait = self
            .active_readiness()
            .filter_map(|(_, _, tracking)| tracking.next_wait(now))
            .min();
        let startup_wait = self
            .active_readiness()
            .filter_map(|(_, _, tracking)| tracking.startup_deadline)
            .map(|deadline| deadline.saturating_duration_since(now))
            .min();
        probe_wait.into_iter().chain(startup_wait).min()
    }
}
