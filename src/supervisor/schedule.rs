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

use super::core::{Core, DesiredState, Lifecycle};
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
        self.entries[index].pending_trigger = trigger;
        self.require_running(index, trigger);
        let entry = &mut self.entries[index];
        if let Some(run_id) = entry.current_run.filter(|_| !entry.cleanup_unconfirmed) {
            // A stopping Run releases its identity on the finished report;
            // that pass starts the replacement through the scheduler. The
            // stop is intentional: its summary must not read as a failure.
            entry.lifecycle = Lifecycle::Stopping;
            entry.blocked = None;
            entry.readiness = None;
            self.seam.stop(entry.process_id, run_id, None, &self.events);
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
        let now_ms = self.now_ms();
        let entry = &mut self.entries[index];
        if entry.current_run.is_some() || entry.desired != DesiredState::Running {
            return;
        }
        let run_id = RunId::new(entry.next_run);
        entry.next_run += 1;
        entry.current_run = Some(run_id);
        entry.lifecycle = Lifecycle::Starting;
        entry.failure = None;
        entry.metrics = None;
        entry.blocked = None;
        // Starting is the immediate invalidation of an earlier successful
        // One-shot completion represented by Done.
        entry.run_started_at_ms = Some(now_ms);
        entry.run_trigger = entry.pending_trigger;
        let intent = self.build_intent(index, run_id);
        self.seam.start(intent, &self.events);
    }

    fn mark_blocked(&mut self, index: usize, reason: String) {
        let entry = &mut self.entries[index];
        if entry.current_run.is_some() || entry.desired != DesiredState::Running {
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
    /// so Running implies availability; readiness passes at most once per
    /// Run, so each Run releases its dependents exactly once.
    fn ready_condition_satisfied(&self, index: usize) -> bool {
        let entry = &self.entries[index];
        entry.current_run.is_some() && entry.lifecycle == Lifecycle::Running
    }

    /// `completed_successfully` holds while the dependency's authoritative
    /// lifecycle is Done. Starting a later Run immediately replaces Done.
    fn completed_condition_satisfied(&self, index: usize) -> bool {
        self.entries[index].lifecycle == Lifecycle::Done
    }

    pub(super) fn is_enabled(&self, index: usize) -> bool {
        matches!(self.project.processes()[index].enabled, Enabled::Yes)
    }
}
