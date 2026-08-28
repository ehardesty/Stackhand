//! Dependency-safe Project shutdown under one shared deadline.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use crate::runtime::RunId;

use super::core::{Core, DesiredState, Lifecycle};

/// One Process cleanup that did not finish cleanly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessShutdownFailure {
    pub process: String,
    pub detail: String,
    pub remaining_pids: Vec<u32>,
}

/// Observable progress and final result of the one Project shutdown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectShutdownSnapshot {
    pub complete: bool,
    pub timed_out: bool,
    pub failures: Vec<ProcessShutdownFailure>,
}

pub(super) struct ShutdownState {
    pub(super) deadline: Instant,
    remaining: BTreeSet<usize>,
    dispatched: BTreeSet<usize>,
    failures: Vec<ProcessShutdownFailure>,
    timed_out: bool,
}

impl Core {
    pub(super) fn begin_shutdown(&mut self, deadline: Instant) {
        if self.shutdown.is_some() {
            return;
        }
        self.seam
            .begin_shutdown(deadline.saturating_duration_since(self.clock.now()));
        let remaining: BTreeSet<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| entry.current_run.map(|_| index))
            .collect();
        for index in remaining.iter().copied() {
            self.cancel_run_work(index);
        }
        self.shutdown = Some(ShutdownState {
            deadline,
            remaining,
            dispatched: BTreeSet::new(),
            failures: Vec::new(),
            timed_out: false,
        });

        for entry in &mut self.entries {
            entry.desired = DesiredState::Stopped;
            entry.blocked = None;
            entry.readiness = None;
            entry.restart_backoff = None;
            entry.restart_suppressed = true;
            if entry.current_run.is_none()
                && matches!(
                    entry.lifecycle,
                    Lifecycle::Waiting | Lifecycle::RestartBackoff
                )
            {
                entry.lifecycle = Lifecycle::Stopped;
            }
        }
        self.dispatch_shutdown_wave();
    }

    pub(super) fn shutdown_in_progress(&self) -> bool {
        self.shutdown.is_some()
    }

    pub(super) fn shutdown_snapshot(&self) -> Option<ProjectShutdownSnapshot> {
        self.shutdown.as_ref().map(|state| ProjectShutdownSnapshot {
            complete: state.remaining.is_empty(),
            timed_out: state.timed_out,
            failures: state.failures.clone(),
        })
    }

    pub(super) fn time_until_shutdown_deadline(&self) -> Option<Duration> {
        self.shutdown
            .as_ref()
            .filter(|state| !state.remaining.is_empty())
            .map(|state| state.deadline.saturating_duration_since(self.clock.now()))
    }

    pub(super) fn finish_shutdown_run(
        &mut self,
        index: usize,
        confirmed: bool,
        detail: Option<String>,
        remaining_pids: Vec<u32>,
    ) {
        let Some(state) = self.shutdown.as_mut() else {
            return;
        };
        if !state.remaining.remove(&index) {
            return;
        }
        if !confirmed {
            state.failures.push(ProcessShutdownFailure {
                process: self.project.processes()[index].name.clone(),
                detail: detail.unwrap_or_else(|| "Run cleanup did not fully confirm".to_string()),
                remaining_pids,
            });
        }
        self.dispatch_shutdown_wave();
    }

    pub(super) fn expire_shutdown(&mut self, now: Instant) {
        let Some(state) = self.shutdown.as_ref() else {
            return;
        };
        if state.remaining.is_empty() || now < state.deadline {
            return;
        }
        let unfinished: Vec<usize> = state.remaining.iter().copied().collect();
        let unattempted: Vec<usize> = unfinished
            .iter()
            .copied()
            .filter(|index| !state.dispatched.contains(index))
            .collect();

        // Even at the deadline, every Run receives one zero-wait cleanup
        // attempt. The shared bound must not cause deeper Dependencies to be
        // skipped when an earlier dependent did not finish.
        for index in unattempted {
            let entry = &mut self.entries[index];
            entry.lifecycle = Lifecycle::Stopping;
            self.seam.stop(
                entry.process_id,
                entry.current_run.expect("shutdown tracks active Runs"),
                Some(Duration::ZERO),
                &self.events,
            );
        }

        let state = self.shutdown.as_mut().expect("shutdown remains active");
        state.timed_out = true;
        state.remaining.clear();
        for index in unfinished {
            let entry = &self.entries[index];
            state.failures.push(ProcessShutdownFailure {
                process: self.project.processes()[index].name.clone(),
                detail: "Project shutdown deadline expired".to_string(),
                remaining_pids: entry.root_pid.into_iter().collect(),
            });
        }
    }

    fn dispatch_shutdown_wave(&mut self) {
        let Some(state) = self.shutdown.as_ref() else {
            return;
        };
        if state.remaining.is_empty() {
            return;
        }
        let candidates: Vec<usize> = state
            .remaining
            .iter()
            .copied()
            .filter(|index| !state.dispatched.contains(index))
            .filter(|dependency| {
                !state.remaining.iter().any(|dependent| {
                    self.project
                        .resolved_dependencies(*dependent)
                        .any(|(dependency_index, _)| dependency_index == *dependency)
                })
            })
            .collect();
        let remaining = state.deadline.saturating_duration_since(self.clock.now());

        for index in candidates {
            let run_id: RunId = self.entries[index]
                .current_run
                .expect("shutdown tracks only active Runs");
            self.shutdown
                .as_mut()
                .expect("shutdown remains active")
                .dispatched
                .insert(index);
            let entry = &mut self.entries[index];
            entry.lifecycle = Lifecycle::Stopping;
            self.seam
                .stop(entry.process_id, run_id, Some(remaining), &self.events);
        }
    }
}
