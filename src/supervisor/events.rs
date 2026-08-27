//! Control-plane event application for the Supervisor.
//!
//! Adapters report facts only. This module applies those facts to the one
//! authoritative lifecycle owner and keeps the Process/Run identity gate in
//! one place.

use std::time::Duration;

use crate::model::ProcessKind;
use crate::supervisor::seam::{FinishedRun, SeamEvent};

use super::core::{Core, FailureKind, FailureSummary, MetricsMetadata, ReadinessTracking};

impl Core {
    pub(crate) fn event(&mut self, event: SeamEvent) {
        // The single stale-event gate: every Run-scoped event must match the
        // receiving Process's current Run or it cannot change state.
        let Some(index) = self.index_of(event.process_id()) else {
            return;
        };
        if Some(event.run_id()) != self.entries[index].current_run {
            return;
        }
        // While a cleanup retry is held, only the confirming completion of
        // the held Run is meaningful; stale reports for it stay stale.
        if self.entries[index].cleanup_unconfirmed && !matches!(event, SeamEvent::Finished(_)) {
            return;
        }
        // Stop/restart/shutdown cancellation closes every observational
        // result. A cleanup failure still has authority to finish shutdown;
        // all other late observations are discarded.
        if self.entries[index].run_cancelled
            && !matches!(event, SeamEvent::Finished(_))
            && !(self.shutdown_in_progress() && matches!(event, SeamEvent::Failed { .. }))
        {
            return;
        }
        match &event {
            SeamEvent::Finished(finished) => self.finish_shutdown_run(
                index,
                finished.cleanup_confirmed,
                finished.detail.clone(),
                finished
                    .remaining_pids
                    .iter()
                    .map(|pid| pid.get())
                    .collect(),
            ),
            SeamEvent::Failed { detail, .. } if self.shutdown_in_progress() => {
                self.finish_shutdown_run(index, false, Some(detail.clone()), Vec::new());
            }
            _ => {}
        }
        match event {
            SeamEvent::Spawned { root_pid, .. } => {
                let initial_delay = self.project.processes()[index]
                    .readiness
                    .as_ref()
                    .map_or(Duration::ZERO, |config| config.initial_delay);
                let probed = self.project.processes()[index].readiness.is_some();
                let work_id = probed.then(|| self.allocate_work_id(index));
                let now = self.clock.now();
                let entry = &mut self.entries[index];
                entry.root_pid = root_pid.map(|pid| pid.get());
                if entry.lifecycle == super::core::Lifecycle::Starting {
                    if let Some(work_id) = work_id {
                        // The Run exists but is not available yet; its first
                        // attempt is due after the configured initial delay.
                        entry.readiness = Some(ReadinessTracking {
                            work_id,
                            attempts: 0,
                            next_attempt_id: 1,
                            last_error: None,
                            in_flight: None,
                            next_attempt_at: now + initial_delay,
                        });
                    } else {
                        // A Service without readiness becomes Running at
                        // spawn; its label projects as Ready.
                        entry.lifecycle = super::core::Lifecycle::Running;
                    }
                }
                self.evaluate();
            }
            SeamEvent::Readiness {
                work_id,
                attempt_id,
                passing,
                diagnostic,
                ..
            } => {
                let interval = self.project.processes()[index]
                    .readiness
                    .as_ref()
                    .map(|config| config.interval);
                let entry = &mut self.entries[index];
                let Some(tracking) = entry.readiness.as_mut() else {
                    return;
                };
                if tracking.work_id != work_id || tracking.in_flight != Some(attempt_id) {
                    return;
                }
                tracking.in_flight = None;
                if passing {
                    // Passing releases dependents through the Running
                    // transition exactly once per Run; per-Run readiness
                    // bookkeeping ends here, which also makes any further
                    // result for this Run land on no tracking at all.
                    entry.lifecycle = super::core::Lifecycle::Running;
                    entry.readiness = None;
                } else {
                    tracking.last_error = diagnostic;
                    if let Some(interval) = interval {
                        tracking.next_attempt_at = self.clock.now() + interval;
                    }
                }
                self.evaluate();
            }
            SeamEvent::Finished(finished) => self.apply_finished_run(index, finished),
            SeamEvent::Failed { kind, detail, .. } => {
                self.cancel_run_work(index);
                let now_ms = self.now_ms();
                let entry = &mut self.entries[index];
                entry.failure = Some(FailureSummary { kind, detail });
                // A failed adapter report ends the Run identity and reverts
                // the Process to stopped so it can be started again.
                entry.record_finished_run(now_ms, None, false);
                entry.current_run = None;
                entry.root_pid = None;
                entry.desired = super::core::DesiredState::Stopped;
                entry.lifecycle = super::core::Lifecycle::Stopped;
                entry.metrics = None;
                entry.readiness = None;
                self.evaluate();
            }
            SeamEvent::Metrics {
                run_id,
                cpu_percent,
                rss_kib,
                ..
            } => {
                // The stale-event gate already matched the Run; the stamp
                // keeps the sample attributable to exactly that Run.
                self.entries[index].metrics = Some(MetricsMetadata {
                    run_id: run_id.get(),
                    cpu_percent,
                    rss_kib,
                });
            }
            SeamEvent::OutputFailure { detail, .. } => {
                // Output-path failure: record it without flipping a healthy
                // Run's lifecycle, and never clobber a real failure.
                let entry = &mut self.entries[index];
                if entry.failure.is_none() {
                    entry.failure = Some(FailureSummary {
                        kind: FailureKind::Output,
                        detail,
                    });
                }
            }
        }
    }

    /// Apply one authoritative finished-Run fact. Natural results are
    /// classified before cleanup releases the identity, so Done, failure,
    /// held cleanup, recent history, and replacement scheduling change in
    /// one serialized Supervisor turn.
    fn apply_finished_run(&mut self, index: usize, finished: FinishedRun) {
        // Natural exit also ends any probe that is still in flight. The
        // identity gate below makes a result released after this point
        // harmless.
        self.cancel_run_work(index);
        if !finished.intentional_stop {
            match self.project.processes()[index].kind {
                ProcessKind::OneShot => self.complete_one_shot(index, finished.exit_code),
                ProcessKind::Service => self.observe_service_exit(index, finished.exit_code),
            }
        }

        let now_ms = self.now_ms();
        let entry = &mut self.entries[index];
        if !finished.cleanup_confirmed {
            // Hold the Run identity. A later Stop retries the same adapter
            // owner; no replacement Run can start before confirmation.
            entry.cleanup_unconfirmed = true;
            entry.failure = Some(FailureSummary {
                kind: FailureKind::Shutdown,
                detail: finished
                    .detail
                    .unwrap_or_else(|| "Run cleanup did not fully confirm".to_string()),
            });
            if entry.lifecycle != super::core::Lifecycle::Done {
                entry.lifecycle = super::core::Lifecycle::Stopped;
            }
            entry.metrics = None;
            entry.readiness = None;
            self.evaluate();
            return;
        }

        entry.record_finished_run(now_ms, finished.exit_code, finished.intentional_stop);
        entry.current_run = None;
        entry.root_pid = None;
        entry.metrics = None;
        entry.readiness = None;
        entry.cleanup_unconfirmed = false;
        // A successful One-shot stays Done. Every other finished Run settles
        // at Stopped after its result has been recorded.
        if entry.lifecycle != super::core::Lifecycle::Done {
            entry.lifecycle = super::core::Lifecycle::Stopped;
        }
        self.evaluate();
    }

    /// Project one One-shot Run result into its terminal lifecycle state.
    /// Exit code zero completes the One-shot; every other exit fails it.
    /// Either way Desired State reverts to Stopped: restarting is manual
    /// until automatic restart policy work lands.
    fn complete_one_shot(&mut self, index: usize, code: Option<i32>) {
        let entry = &mut self.entries[index];
        match code {
            Some(0) => {
                entry.lifecycle = super::core::Lifecycle::Done;
                entry.failure = None;
            }
            other => {
                entry.lifecycle = super::core::Lifecycle::Running;
                entry.failure = Some(FailureSummary {
                    kind: FailureKind::ProcessExit,
                    detail: match other {
                        Some(exit_code) => format!("exited with code {exit_code}"),
                        None => "exited without an exit code".to_string(),
                    },
                });
            }
        }
        entry.desired = super::core::DesiredState::Stopped;
        entry.blocked = None;
    }

    /// Record a Service's unexpected natural result. The Run identity stays
    /// occupied until the finished fact confirms cleanup. Desired State reverts
    /// to Stopped so the Supervisor never silently crash-loops; automatic
    /// restart policy is later milestone work.
    fn observe_service_exit(&mut self, index: usize, code: Option<i32>) {
        let entry = &mut self.entries[index];
        if entry.desired == super::core::DesiredState::Running {
            entry.failure = Some(FailureSummary {
                kind: FailureKind::ProcessExit,
                detail: match code {
                    Some(code) => format!("exited unexpectedly with code {code}"),
                    None => "exited unexpectedly".to_string(),
                },
            });
            entry.desired = super::core::DesiredState::Stopped;
            entry.blocked = None;
        }
    }
}
