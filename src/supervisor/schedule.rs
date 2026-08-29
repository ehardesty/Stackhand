//! Dependency-aware scheduling for the Supervisor core.
//!
//! Starting a Process recursively sets it and its required enabled
//! Dependencies to Desired State Running. A Process starts only when every
//! Dependency's `started` condition holds; until then it waits with a
//! bounded blocked reason. One evaluation pass over the Project starts
//! every eligible Process; each Run state change and each Start command
//! triggers a new pass, so waiting Processes start automatically once
//! their Dependencies are satisfied.
//!
//! A Dependency is a startup relationship only: nothing here stops an
//! already-running dependent when a dependency later stops.

use std::time::{Duration, Instant};

use super::core::{Core, DesiredState, Lifecycle};
use super::process_lifecycle::RestartReason;
use super::readiness::ReadinessState;
use crate::model::{DependencyCondition, Enabled};
use crate::runtime::RunId;
use crate::supervisor::RunTrigger;

impl Core {
    /// Start one Process: mark it and its required enabled Dependencies,
    /// transitively, as Desired State Running, then evaluate what can start.
    pub(super) fn start_at(&mut self, index: usize, trigger: RunTrigger) {
        if !self.is_enabled(index) {
            return;
        }
        self.lifecycles[index].prepare_start_request(trigger);
        self.require_running(index, trigger);
        self.evaluate();
    }

    /// Restart one Process: keep Desired State Running, stop the active
    /// Run without touching that desire, and let the scheduler start the
    /// next Run only after the bounded cleanup reports completion. While
    /// the previous cleanup is unconfirmed the request stays pending; its
    /// visible failure names the held Run and Stop retries the cleanup.
    pub(super) fn restart_at(&mut self, index: usize, trigger: RunTrigger) {
        if !self.is_enabled(index) {
            return;
        }
        self.lifecycles[index].prepare_restart_request(trigger);
        self.require_running(index, trigger);
        if let Some((process_id, run_id)) = self.lifecycles[index].begin_replacement_cleanup() {
            // A stopping Run releases its identity on the finished report;
            // that pass starts the replacement through the scheduler. The
            // stop is intentional: its summary must not read as a failure.
            self.cancel_run_work(index);
            self.seam.stop(process_id, run_id, None, &self.events);
        }
        self.evaluate();
    }

    /// Mark one Process and its enabled Dependencies, transitively, as
    /// Desired State Running. Disabled Dependencies stay untouched. The
    /// target keeps the command's trigger; a Dependency started on its
    /// behalf is recorded as started by the user's dependent.
    fn require_running(&mut self, index: usize, trigger: RunTrigger) {
        if !self.is_enabled(index) {
            return;
        }
        if !self.lifecycles[index].require_running(trigger) {
            return;
        }
        let dependency_indices = self
            .project
            .resolved_dependencies(index)
            .map(|(dependency_index, _)| dependency_index)
            .collect::<Vec<_>>();
        for dependency_index in dependency_indices {
            self.require_running(dependency_index, RunTrigger::Dependency);
        }
    }

    /// One scheduling pass in configuration order: start every Process that
    /// desires Running with no current Run and satisfied Dependencies, and
    /// give the rest a visible Waiting reason.
    pub(super) fn evaluate(&mut self) {
        for index in 0..self.lifecycles.len() {
            match self.blocked_reason(index) {
                None => self.begin_desired_run(index),
                Some(reason) => self.mark_blocked(index, reason),
            }
        }
    }

    fn begin_desired_run(&mut self, index: usize) {
        let can_start = {
            let entry = &self.lifecycles[index];
            entry.current_run.is_none()
                && entry.desired == DesiredState::Running
                && !entry.awaiting_manual_restart
                && entry.restart_backoff.is_none()
        };
        if !can_start {
            return;
        }
        let automatic_retry =
            self.lifecycles[index].pending_trigger == RunTrigger::AutomaticRestart;
        if automatic_retry {
            let max_restarts = self.project.processes()[index].restart.max_restarts;
            if !self.lifecycles[index].admit_automatic_retry(max_restarts) {
                self.mark_restart_limit(index);
                return;
            }
        }
        let has_log_readiness = self.project.processes()[index]
            .readiness
            .as_ref()
            .is_some_and(|config| {
                config
                    .checks
                    .iter()
                    .any(|check| matches!(check.probe, crate::model::ReadinessProbe::Log { .. }))
            });
        // Allocate readiness identities before the adapter can spawn only
        // when a live log match needs them. Other checks start at Spawned.
        let readiness = has_log_readiness
            .then(|| self.new_readiness_tracking(index, self.clock.now(), false))
            .flatten();
        let liveness = self.new_liveness_tracking(index, self.clock.now());
        let now_ms = self.now_ms();
        // Starting immediately invalidates an earlier successful One-shot
        // completion represented by Done.
        let run_id = self.lifecycles[index].begin_run(now_ms, readiness, liveness);
        let intent = self.build_intent(index, run_id);
        self.seam.start(intent, &self.events);
    }

    fn mark_blocked(&mut self, index: usize, reason: String) {
        self.lifecycles[index].wait_for_dependency(reason);
    }

    /// Why this Process cannot start yet, or `None` when every Dependency
    /// condition is satisfied.
    fn blocked_reason(&self, index: usize) -> Option<String> {
        for (dependency_index, dependency) in self.project.resolved_dependencies(index) {
            if !self.is_enabled(dependency_index) {
                return Some(format!("{}: disabled", dependency.name));
            }
            if !self.condition_satisfied(dependency_index, dependency.condition) {
                // A visible reason names the Dependency and its condition;
                // a failed dependency adds its bounded failure summary.
                let mut reason = format!("{}: {}", dependency.name, dependency.condition.label());
                if let Some(failure) = &self.lifecycles[dependency_index].failure {
                    reason.push_str(&format!(" ({})", failure.detail));
                }
                return Some(reason);
            }
        }
        None
    }

    fn condition_satisfied(&self, index: usize, condition: DependencyCondition) -> bool {
        match condition {
            DependencyCondition::Started => self.started_condition_satisfied(index),
            DependencyCondition::Ready => self.ready_condition_satisfied(index),
            DependencyCondition::Exited => self.exited_condition_satisfied(index),
            DependencyCondition::CompletedSuccessfully => self.completed_condition_satisfied(index),
        }
    }

    /// `started` holds only while the dependency has an active Run that is
    /// Starting or Running. A Stopping Run or an ended Run never satisfies.
    fn started_condition_satisfied(&self, index: usize) -> bool {
        let entry = &self.lifecycles[index];
        entry.current_run.is_some()
            && matches!(entry.lifecycle, Lifecycle::Starting | Lifecycle::Running)
    }

    /// `ready` holds only while the dependency has an active Running Run. A
    /// probed Service reaches Running only when its readiness probe passed,
    /// and later readiness loss removes `ready` for any new dependent without
    /// stopping an already-running dependent.
    fn ready_condition_satisfied(&self, index: usize) -> bool {
        let entry = &self.lifecycles[index];
        if entry.current_run.is_none() || entry.lifecycle != Lifecycle::Running {
            return false;
        }
        entry
            .readiness
            .as_ref()
            .is_none_or(|tracking| tracking.state() == ReadinessState::Passing)
    }

    /// `exited` holds after the latest scheduled One-shot Run completes its
    /// cleanup, whether its exit succeeded or failed. Starting a later Run
    /// immediately clears the condition.
    fn exited_condition_satisfied(&self, index: usize) -> bool {
        self.lifecycles[index].exited
    }

    /// `completed_successfully` holds while the dependency's authoritative
    /// lifecycle is Done. Starting a later Run immediately replaces Done.
    fn completed_condition_satisfied(&self, index: usize) -> bool {
        self.lifecycles[index].lifecycle == Lifecycle::Done
    }

    /// Hold one automatic retry behind the configured fixed delay. The
    /// failed Run identity is retained so a stale expiry cannot authorize a
    /// later Run.
    pub(super) fn schedule_automatic_restart(
        &mut self,
        index: usize,
        failed_run_id: RunId,
        reason: RestartReason,
    ) {
        let max_restarts = self.project.processes()[index].restart.max_restarts;
        if !self.lifecycles[index]
            .restart_budget
            .has_remaining(max_restarts)
        {
            self.mark_restart_limit(index);
            return;
        }
        let deadline = self.clock.now() + self.project.processes()[index].restart.backoff;
        self.lifecycles[index].wait_for_automatic_restart(failed_run_id, deadline, reason);
    }

    fn mark_restart_limit(&mut self, index: usize) {
        let max_restarts = self.project.processes()[index].restart.max_restarts;
        self.lifecycles[index].exhaust_restart_budget(max_restarts);
    }

    /// Release automatic retries whose fixed delay has expired. Only the
    /// still-current backoff can produce the next Run; an old state is left
    /// inert rather than being allowed to start a newer or stopped Run.
    pub(super) fn poll_restart_timers(&mut self, now: Instant) {
        let expired = self
            .lifecycles
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let deadline = entry.valid_restart_deadline()?;
                (deadline <= now).then_some(index)
            })
            .collect::<Vec<_>>();
        for index in expired.iter().copied() {
            self.lifecycles[index].release_restart_backoff();
        }
        if !expired.is_empty() {
            self.evaluate();
        }
    }

    /// How long until a still-authoritative automatic retry is due.
    pub(super) fn restart_time_until_next_timer(&self) -> Option<Duration> {
        let now = self.clock.now();
        self.lifecycles
            .iter()
            .filter_map(|lifecycle| lifecycle.valid_restart_deadline())
            .map(|deadline| deadline.saturating_duration_since(now))
            .min()
    }

    pub(super) fn is_enabled(&self, index: usize) -> bool {
        matches!(self.project.processes()[index].enabled, Enabled::Yes)
    }
}
