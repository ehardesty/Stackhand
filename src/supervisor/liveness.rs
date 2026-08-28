//! Liveness owns ongoing health state and unhealthy-run effects. Leaf
//! scheduling, thresholds, cancellation, and snapshot projection are shared
//! with readiness in [`super::checks`].

use std::time::{Duration, Instant};

use crate::model::{LivenessConfig, ReadinessProbe};
use crate::runtime::{ProcessId, RunId};
use crate::supervisor::checks::{CheckMode, CheckResult, CheckSet};
use crate::supervisor::seam::{
    AttemptId, ExecContext, LogMatcherIntent, ProbeIntent, ProbeScope, ProbeSeam, WorkId,
};

use super::checks::LivenessState;
use super::core::Core;
use super::snapshot::{LivenessChildStatus, LivenessStatus};

/// Live liveness bookkeeping for one Service's current Run.
#[derive(Debug)]
pub(super) struct LivenessTracking {
    started_at_ms: u64,
    checks: CheckSet,
    started: bool,
}

impl LivenessTracking {
    pub(super) fn state(&self) -> LivenessState {
        if self.started {
            self.checks.state()
        } else {
            LivenessState::Inactive
        }
    }

    pub(super) fn is_started(&self) -> bool {
        self.started
    }

    pub(super) fn cancel(&self, probes: &dyn ProbeSeam, process_id: ProcessId, run_id: RunId) {
        self.checks.cancel(probes, process_id, run_id);
    }

    pub(super) fn apply_result(
        &mut self,
        config: &LivenessConfig,
        work_id: WorkId,
        attempt_id: AttemptId,
        now: Instant,
        passing: bool,
        diagnostic: Option<String>,
    ) -> Option<LivenessState> {
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
                CheckMode::Liveness,
            )
            .map(|_| self.state())
    }

    pub(super) fn complete_log_match(
        &mut self,
        config: &LivenessConfig,
        work_id: WorkId,
        attempt_id: AttemptId,
        now: Instant,
    ) -> Option<LivenessState> {
        self.checks
            .complete_log_match(&config.checks, work_id, attempt_id, now)
            .map(|_| self.state())
    }

    pub(super) fn due_indices(&self, now: Instant) -> impl Iterator<Item = usize> + '_ {
        self.checks.due_indices(now, CheckMode::Liveness)
    }

    pub(super) fn next_wait(&self, now: Instant) -> Option<Duration> {
        self.checks.next_wait(now, CheckMode::Liveness)
    }

    pub(super) fn expired_log_attempts(
        &self,
        now: Instant,
    ) -> impl Iterator<Item = (WorkId, AttemptId)> + '_ {
        self.checks.expired_log_attempts(now)
    }

    pub(super) fn begin_probe(
        &mut self,
        check_index: usize,
        process_id: ProcessId,
        run_id: RunId,
        config: &LivenessConfig,
        exec_context: Option<ExecContext>,
    ) -> Option<ProbeIntent> {
        self.checks.begin_probe(
            check_index,
            process_id,
            run_id,
            &config.checks,
            exec_context,
            ProbeScope::Liveness,
        )
    }

    pub(super) fn begin_log(
        &mut self,
        check_index: usize,
        config: &LivenessConfig,
        now: Instant,
    ) -> Option<LogMatcherIntent> {
        self.checks.begin_log(check_index, &config.checks, now)
    }

    pub(super) fn log_matchers(&self, config: &LivenessConfig) -> Vec<LogMatcherIntent> {
        self.checks.log_matchers(&config.checks, None)
    }

    pub(super) fn activate(&mut self, config: &LivenessConfig, now: Instant, now_ms: u64) {
        if self.started {
            return;
        }
        self.started = true;
        self.started_at_ms = now_ms;
        self.checks
            .activate(&config.checks, now, CheckMode::Liveness);
    }

    pub(super) fn snapshot(&self, config: &LivenessConfig, now_ms: u64) -> LivenessStatus {
        let snapshot = self.checks.snapshot(&config.checks);
        LivenessStatus {
            kind: snapshot.kind,
            state: self.state(),
            attempts: snapshot.attempts,
            consecutive_successes: snapshot.consecutive_successes,
            consecutive_failures: snapshot.consecutive_failures,
            last_error: snapshot.last_error,
            elapsed_ms: if self.started {
                now_ms.saturating_sub(self.started_at_ms)
            } else {
                0
            },
            children: snapshot
                .children
                .into_iter()
                .map(|child| LivenessChildStatus {
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
    pub(super) fn new_liveness_tracking(
        &mut self,
        index: usize,
        now: Instant,
    ) -> Option<LivenessTracking> {
        let config = self.project.processes()[index].liveness.clone()?;
        if config.checks.is_empty() {
            return None;
        }
        let work_ids = (0..config.checks.len())
            .map(|_| self.allocate_work_id(index))
            .collect::<Vec<_>>();
        let started_at_ms = self.now_ms();
        Some(LivenessTracking {
            started_at_ms,
            checks: CheckSet::new(&config.checks, work_ids, now),
            started: false,
        })
    }

    pub(super) fn poll_liveness_timers(&mut self, now: Instant) {
        let expired = self
            .active_liveness()
            .flat_map(|(index, _, tracking)| {
                tracking
                    .expired_log_attempts(now)
                    .map(move |(work_id, attempt_id)| (index, work_id, attempt_id))
            })
            .collect::<Vec<_>>();
        for (index, work_id, attempt_id) in expired {
            self.apply_liveness_result(
                index,
                work_id,
                attempt_id,
                false,
                Some("liveness log check timed out".to_string()),
                now,
            );
        }
        for (index, check_index) in self.due_liveness_indices(now) {
            self.dispatch_liveness_check(index, check_index, now);
        }
    }

    fn active_liveness(&self) -> impl Iterator<Item = (usize, RunId, &LivenessTracking)> + '_ {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let run_id = entry.current_run?;
                let tracking = entry.liveness.as_ref()?;
                (entry.desired == super::core::DesiredState::Running && tracking.is_started())
                    .then_some((index, run_id, tracking))
            })
    }

    fn due_liveness_indices(&self, now: Instant) -> Vec<(usize, usize)> {
        self.active_liveness()
            .flat_map(|(index, _, tracking)| {
                tracking
                    .due_indices(now)
                    .map(move |check_index| (index, check_index))
            })
            .collect()
    }

    fn dispatch_liveness_check(&mut self, index: usize, check_index: usize, now: Instant) {
        let Some(config) = self.project.processes()[index].liveness.clone() else {
            return;
        };
        let Some(check_config) = config.checks.get(check_index) else {
            return;
        };
        let process_id = self.entries[index].process_id;
        let Some(run_id) = self.entries[index].current_run else {
            return;
        };
        let exec_context =
            matches!(&check_config.probe, ReadinessProbe::Exec { .. }).then(|| ExecContext {
                working_dir: self.project.processes()[index].working_dir.clone(),
                env: self.project.processes()[index].env.clone(),
                shell: self.project.shell().clone(),
            });
        if matches!(check_config.probe, ReadinessProbe::Log { .. }) {
            let Some(intent) = self.entries[index]
                .liveness
                .as_mut()
                .and_then(|tracking| tracking.begin_log(check_index, &config, now))
            else {
                return;
            };
            self.seam.arm_log_matcher(process_id, run_id, intent);
            return;
        }
        let Some(intent) = self.entries[index].liveness.as_mut().and_then(|tracking| {
            tracking.begin_probe(check_index, process_id, run_id, &config, exec_context)
        }) else {
            return;
        };
        self.probes.probe(intent, &self.events);
    }

    pub(super) fn apply_liveness_result(
        &mut self,
        index: usize,
        work_id: WorkId,
        attempt_id: AttemptId,
        passing: bool,
        diagnostic: Option<String>,
        now: Instant,
    ) {
        let Some(config) = self.project.processes()[index].liveness.as_ref() else {
            return;
        };
        let before = self.entries[index]
            .liveness
            .as_ref()
            .map(LivenessTracking::state);
        let after = self.entries[index].liveness.as_mut().and_then(|tracking| {
            tracking.apply_result(config, work_id, attempt_id, now, passing, diagnostic)
        });
        let Some(after) = after else {
            return;
        };
        if before != Some(LivenessState::Failing) && after == LivenessState::Failing {
            self.handle_liveness_failure(index);
        } else if before == Some(LivenessState::Failing) && after == LivenessState::Passing {
            self.recover_liveness(index);
        }
        self.evaluate();
    }

    pub(super) fn complete_liveness_log(
        &mut self,
        index: usize,
        work_id: WorkId,
        attempt_id: AttemptId,
        now: Instant,
    ) {
        let Some(config) = self.project.processes()[index].liveness.as_ref() else {
            return;
        };
        let before = self.entries[index]
            .liveness
            .as_ref()
            .map(LivenessTracking::state);
        let after = self.entries[index]
            .liveness
            .as_mut()
            .and_then(|tracking| tracking.complete_log_match(config, work_id, attempt_id, now));
        let Some(after) = after else {
            return;
        };
        if before != Some(LivenessState::Failing) && after == LivenessState::Failing {
            self.handle_liveness_failure(index);
        } else if before == Some(LivenessState::Failing) && after == LivenessState::Passing {
            self.recover_liveness(index);
        }
        self.evaluate();
    }

    fn handle_liveness_failure(&mut self, index: usize) {
        let detail = self.entries[index]
            .liveness
            .as_ref()
            .and_then(|tracking| {
                tracking
                    .snapshot(
                        self.project.processes()[index]
                            .liveness
                            .as_ref()
                            .expect("liveness tracking has a configured policy"),
                        self.now_ms(),
                    )
                    .last_error
            })
            .unwrap_or_else(|| "liveness failure threshold reached".to_string());
        self.entries[index].failure = Some(super::core::FailureSummary {
            kind: super::core::FailureKind::Liveness,
            detail,
        });
        if !self.project.processes()[index].restart.on_unhealthy {
            return;
        }
        let Some(run_id) = self.entries[index].current_run else {
            return;
        };
        let process_id = self.entries[index].process_id;
        {
            let entry = &mut self.entries[index];
            entry.unhealthy_restart_pending = true;
            entry.desired = super::core::DesiredState::Stopped;
            entry.lifecycle = super::core::Lifecycle::Stopping;
            entry.blocked = None;
        }
        self.cancel_run_work(index);
        self.seam.stop(process_id, run_id, None, &self.events);
    }

    fn recover_liveness(&mut self, index: usize) {
        if self.entries[index]
            .failure
            .as_ref()
            .is_some_and(|failure| failure.kind == super::core::FailureKind::Liveness)
        {
            self.entries[index].failure = None;
        }
    }

    pub(super) fn liveness_time_until_next_timer(&self) -> Option<Duration> {
        let now = self.clock.now();
        self.active_liveness()
            .filter_map(|(_, _, tracking)| tracking.next_wait(now))
            .min()
    }
}
