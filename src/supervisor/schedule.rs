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

impl Core {
    /// Start one Process: mark it and its required enabled Dependencies,
    /// transitively, as Desired State Running, then evaluate what can start.
    pub(super) fn start_at(&mut self, index: usize) {
        if !self.is_enabled(index) {
            return;
        }
        self.require_running(index);
        self.evaluate();
    }

    /// Mark one Process and its enabled Dependencies, transitively, as
    /// Desired State Running. Disabled Dependencies stay untouched.
    fn require_running(&mut self, index: usize) {
        if !self.is_enabled(index) {
            return;
        }
        let entry = &mut self.entries[index];
        if entry.desired == DesiredState::Running {
            return;
        }
        entry.desired = DesiredState::Running;
        let dependencies = self.dependency_indices(index);
        for dependency in dependencies {
            self.require_running(dependency);
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
        let dependencies = &self.project.processes()[index].dependencies;
        for dependency in dependencies {
            let dependency_index = self
                .named_index(&dependency.name)
                .expect("configuration validation resolved every dependency");
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

    /// `completed_successfully` holds once the dependency's latest completed
    /// Run exited with code zero. The satisfaction survives later evaluation
    /// passes and stays while a new Run is active; only that Run reaching a
    /// completion replaces it (rerun semantics are Issue #32's work).
    fn completed_condition_satisfied(&self, index: usize) -> bool {
        let entry = &self.entries[index];
        entry.lifecycle == Lifecycle::Done || entry.completed
    }

    fn is_enabled(&self, index: usize) -> bool {
        matches!(self.project.processes()[index].enabled, Enabled::Yes)
    }

    /// The session positions of one Process's Dependencies. Configuration
    /// validation resolved every name before startup.
    fn dependency_indices(&self, index: usize) -> Vec<usize> {
        self.project.processes()[index]
            .dependencies
            .iter()
            .map(|dependency| {
                self.named_index(&dependency.name)
                    .expect("configuration validation resolved every dependency")
            })
            .collect()
    }
}
