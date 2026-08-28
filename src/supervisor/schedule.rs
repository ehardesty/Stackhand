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

use super::core::{
    Core, DesiredState, FailureKind, FailureSummary, Lifecycle, RestartBackoff, RestartReason,
};
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
        if trigger == RunTrigger::Manual {
            let entry = &mut self.entries[index];
            let timeout_cleanup = entry.startup_timeout_pending;
            entry.clear_restart_state();
            entry.restart_budget.reset();
            entry.pending_trigger = trigger;
            entry.restart_suppressed = timeout_cleanup;
            entry.exited = false;
        }
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
        {
            let entry = &mut self.entries[index];
            let timeout_cleanup = entry.startup_timeout_pending;
            entry.pending_trigger = trigger;
            entry.clear_restart_state();
            if matches!(trigger, RunTrigger::Restart | RunTrigger::Rerun) {
                entry.restart_budget.reset();
            }
            entry.restart_suppressed = timeout_cleanup;
            entry.exited = false;
        }
        self.require_running(index, trigger);
        let entry = &mut self.entries[index];
        if let Some(run_id) = entry.current_run.filter(|_| !entry.cleanup_unconfirmed) {
            // A stopping Run releases its identity on the finished report;
            // that pass starts the replacement through the scheduler. The
            // stop is intentional: its summary must not read as a failure.
            let process_id = entry.process_id;
            entry.lifecycle = Lifecycle::Stopping;
            entry.blocked = None;
            let _ = entry;
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
        let entry = &mut self.entries[index];
        if entry.desired == DesiredState::Running {
            return;
        }
        entry.desired = DesiredState::Running;
        entry.pending_trigger = trigger;
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
        for index in 0..self.entries.len() {
            match self.blocked_reason(index) {
                None => self.begin_desired_run(index),
                Some(reason) => self.mark_blocked(index, reason),
            }
        }
    }

    fn begin_desired_run(&mut self, index: usize) {
        let can_start = {
            let entry = &self.entries[index];
            entry.current_run.is_none()
                && entry.desired == DesiredState::Running
                && !entry.awaiting_manual_restart
                && entry.restart_backoff.is_none()
        };
        if !can_start {
            return;
        }
        let automatic_retry = self.entries[index].pending_trigger == RunTrigger::AutomaticRestart;
        if automatic_retry {
            let max_restarts = self.project.processes()[index].restart.max_restarts;
            if !self.entries[index].restart_budget.consume(max_restarts) {
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
        let entry = &mut self.entries[index];
        let run_id = RunId::new(entry.next_run);
        entry.next_run += 1;
        entry.current_run = Some(run_id);
        entry.lifecycle = Lifecycle::Starting;
        entry.failure = None;
        entry.metrics = None;
        entry.exited = false;
        entry.startup_timeout_pending = false;
        entry.clear_restart_state();
        entry.run_cancelled = false;
        entry.blocked = None;
        // Starting is the immediate invalidation of an earlier successful
        // One-shot completion represented by Done.
        entry.run_started_at_ms = Some(now_ms);
        entry.run_trigger = entry.pending_trigger;
        entry.readiness = readiness;
        entry.liveness = liveness;
        entry.spawned = false;
        entry.unhealthy_restart_pending = false;
        let intent = self.build_intent(index, run_id);
        self.seam.start(intent, &self.events);
    }

    fn mark_blocked(&mut self, index: usize, reason: String) {
        let entry = &mut self.entries[index];
        if entry.current_run.is_some()
            || entry.desired != DesiredState::Running
            || entry.restart_backoff.is_some()
        {
            return;
        }
        entry.lifecycle = Lifecycle::Waiting;
        // A previous Run's failure must not mask the current waiting state.
        entry.failure = None;
        entry.blocked = Some(reason);
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
                if let Some(failure) = &self.entries[dependency_index].failure {
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
        let entry = &self.entries[index];
        entry.current_run.is_some()
            && matches!(entry.lifecycle, Lifecycle::Starting | Lifecycle::Running)
    }

    /// `ready` holds only while the dependency has an active Running Run. A
    /// probed Service reaches Running only when its readiness probe passed,
    /// and later readiness loss removes `ready` for any new dependent without
    /// stopping an already-running dependent.
    fn ready_condition_satisfied(&self, index: usize) -> bool {
        let entry = &self.entries[index];
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
        self.entries[index].exited
    }

    /// `completed_successfully` holds while the dependency's authoritative
    /// lifecycle is Done. Starting a later Run immediately replaces Done.
    fn completed_condition_satisfied(&self, index: usize) -> bool {
        self.entries[index].lifecycle == Lifecycle::Done
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
        if !self.entries[index]
            .restart_budget
            .has_remaining(max_restarts)
        {
            self.mark_restart_limit(index);
            return;
        }
        let deadline = self.clock.now() + self.project.processes()[index].restart.backoff;
        let entry = &mut self.entries[index];
        entry.desired = DesiredState::Running;
        entry.lifecycle = Lifecycle::RestartBackoff;
        entry.blocked = None;
        entry.clear_restart_state();
        entry.restart_backoff = Some(RestartBackoff {
            failed_run_id,
            deadline,
            reason,
        });
    }

    fn mark_restart_limit(&mut self, index: usize) {
        let max_restarts = self.project.processes()[index].restart.max_restarts;
        let entry = &mut self.entries[index];
        entry.restart_budget.exhaust();
        entry.desired = DesiredState::Running;
        entry.lifecycle = Lifecycle::Stopped;
        entry.blocked = None;
        entry.awaiting_manual_restart = true;
        entry.restart_backoff = None;
        entry.unhealthy_restart_pending = false;
        entry.failure = Some(FailureSummary {
            kind: FailureKind::RestartLimit,
            detail: format!("Restart limit reached after {max_restarts} automatic attempts"),
        });
    }

    /// Release automatic retries whose fixed delay has expired. Only the
    /// still-current backoff can produce the next Run; an old state is left
    /// inert rather than being allowed to start a newer or stopped Run.
    pub(super) fn poll_restart_timers(&mut self, now: Instant) {
        let expired = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let deadline = valid_restart_deadline(entry)?;
                (deadline <= now).then_some(index)
            })
            .collect::<Vec<_>>();
        for index in expired.iter().copied() {
            let entry = &mut self.entries[index];
            entry.restart_backoff = None;
            entry.pending_trigger = RunTrigger::AutomaticRestart;
        }
        if !expired.is_empty() {
            self.evaluate();
        }
    }

    /// How long until a still-authoritative automatic retry is due.
    pub(super) fn restart_time_until_next_timer(&self) -> Option<Duration> {
        let now = self.clock.now();
        self.entries
            .iter()
            .filter_map(valid_restart_deadline)
            .map(|deadline| deadline.saturating_duration_since(now))
            .min()
    }

    pub(super) fn is_enabled(&self, index: usize) -> bool {
        matches!(self.project.processes()[index].enabled, Enabled::Yes)
    }
}

fn valid_restart_deadline(entry: &super::core::Entry) -> Option<Instant> {
    let backoff = entry.restart_backoff?;
    (entry.current_run.is_none()
        && entry.desired == DesiredState::Running
        && entry.lifecycle == Lifecycle::RestartBackoff
        && entry.next_run == backoff.failed_run_id.get().saturating_add(1))
    .then_some(backoff.deadline)
}
