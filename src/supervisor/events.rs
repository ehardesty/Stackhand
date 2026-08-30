//! Control-plane event application for the Supervisor.
//!
//! Adapters report facts only. This module applies those facts to the one
//! authoritative lifecycle owner and keeps the Process/Run identity gate in
//! one place.

use crate::model::{ProcessKind, RestartPolicy};
use crate::runtime::RunId;
use crate::supervisor::seam::{FinishedRun, SeamEvent};

use super::core::{Core, FailureKind, MetricsMetadata};
use super::process_lifecycle::RestartReason;

#[derive(Clone, Copy)]
enum TerminalOutcome {
    Failed {
        kind: FailureKind,
    },
    Finished {
        exit_code: Option<i32>,
        intentional_stop: bool,
        timed_out: bool,
    },
}

impl Core {
    pub(crate) fn event(&mut self, event: SeamEvent) {
        // The single stale-event gate: every Run-scoped event must match the
        // receiving Process's current Run or it cannot change state.
        let Some(index) = self.index_of(event.process_id()) else {
            return;
        };
        if Some(event.run_id()) != self.lifecycles[index].current_run {
            return;
        }
        // While a cleanup retry is held, only the confirming completion of
        // the held Run is meaningful; stale reports for it stay stale.
        if self.lifecycles[index].cleanup_unconfirmed && !matches!(event, SeamEvent::Finished(_)) {
            return;
        }
        // Stop/restart/shutdown cancellation closes every observational
        // result. A cleanup failure still has authority to finish shutdown;
        // all other late observations are discarded.
        if self.lifecycles[index].run_cancelled
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
                if self.lifecycles[index].spawned {
                    // A Run has one authoritative spawn fact. Repeated
                    // callbacks must not overwrite its PID or reinitialize
                    // Run-scoped work.
                    return;
                }
                let now = self.clock.now();
                let initialize_readiness = self.lifecycles[index].lifecycle
                    == super::core::Lifecycle::Starting
                    && self.lifecycles[index].readiness.is_none();
                let readiness = initialize_readiness
                    .then(|| self.new_readiness_tracking(index, now, true))
                    .flatten();
                let spawn_now_ms = self.now_ms();
                let readiness_config = self.project.processes()[index].readiness.clone();
                self.lifecycles[index].record_spawn(
                    root_pid.map(|pid| pid.get()),
                    readiness,
                    readiness_config.as_ref(),
                    now,
                    spawn_now_ms,
                );
                if self.lifecycles[index].lifecycle == super::core::Lifecycle::Running
                    && self.lifecycles[index]
                        .readiness
                        .as_ref()
                        .is_none_or(|tracking| {
                            tracking.state() == super::readiness::ReadinessState::Passing
                        })
                {
                    self.activate_liveness(index, now);
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
                let Some(config) = self.project.processes()[index].readiness.as_ref() else {
                    return;
                };
                let now = self.clock.now();
                let became_ready = {
                    let entry = &mut self.lifecycles[index];
                    let Some(tracking) = entry.readiness.as_mut() else {
                        return;
                    };
                    tracking.apply_result(config, work_id, attempt_id, now, passing, diagnostic)
                };
                let Some(became_ready) = became_ready else {
                    return;
                };
                if became_ready {
                    self.promote_ready(index);
                }
                self.evaluate();
            }
            SeamEvent::Liveness {
                work_id,
                attempt_id,
                passing,
                diagnostic,
                ..
            } => {
                self.apply_liveness_result(
                    index,
                    work_id,
                    attempt_id,
                    passing,
                    diagnostic,
                    self.clock.now(),
                );
            }
            SeamEvent::LogMatched {
                work_id,
                attempt_id,
                ..
            } => {
                if let Some(attempt_id) = attempt_id {
                    self.complete_liveness_log(index, work_id, attempt_id, self.clock.now());
                    return;
                }
                let became_ready = {
                    let entry = &mut self.lifecycles[index];
                    let Some(tracking) = entry.readiness.as_mut() else {
                        return;
                    };
                    tracking.apply_log_match(work_id)
                };
                let Some(became_ready) = became_ready else {
                    return;
                };
                if became_ready {
                    self.promote_ready(index);
                }
                self.evaluate();
            }
            SeamEvent::Finished(finished) => self.apply_finished_run(index, finished),
            SeamEvent::Failed { kind, detail, .. } => {
                self.apply_failed_run(index, kind, detail);
            }
            SeamEvent::Metrics {
                run_id,
                cpu_percent,
                rss_kib,
                best_effort,
                ..
            } => {
                // The stale-event gate already matched the Run; the stamp
                // keeps the sample attributable to exactly that Run.
                self.lifecycles[index].record_metrics(MetricsMetadata {
                    run_id: run_id.get(),
                    cpu_percent,
                    rss_kib,
                    best_effort,
                });
            }
            SeamEvent::OutputFailure { detail, .. } => {
                // Output-path failure: record it without flipping a healthy
                // Run's lifecycle, and never clobber a real failure.
                self.lifecycles[index].record_output_failure(detail);
            }
        }
    }

    /// Project an aggregate readiness pass into lifecycle state. Event
    /// identity and child state are handled by each event arm before this
    /// shared effect runs.
    fn promote_ready(&mut self, index: usize) {
        if self.lifecycles[index].promote_ready() {
            self.activate_liveness(index, self.clock.now());
        }
    }

    fn activate_liveness(&mut self, index: usize, now: std::time::Instant) {
        let Some(config) = self.project.processes()[index].liveness.clone() else {
            return;
        };
        let now_ms = self.now_ms();
        if let Some(tracking) = self.lifecycles[index].liveness.as_mut() {
            tracking.activate(&config, now, now_ms);
        }
    }

    /// Apply one authoritative finished-Run fact. Natural results are
    /// classified before cleanup releases the identity, so Done, failure,
    /// held cleanup, recent history, and replacement scheduling change in
    /// one serialized Supervisor turn.
    fn apply_finished_run(&mut self, index: usize, finished: FinishedRun) {
        let cleanup = self.lifecycles[index].cleanup_decision();
        let restart_reason = self.restart_reason_for_terminal(
            index,
            TerminalOutcome::Finished {
                exit_code: finished.exit_code,
                intentional_stop: finished.intentional_stop,
                timed_out: cleanup.timed_out,
            },
        );
        // Natural exit also ends any probe that is still in flight. The
        // identity gate below makes a result released after this point
        // harmless.
        self.cancel_run_work(index);
        if !finished.intentional_stop && !cleanup.timed_out {
            match self.project.processes()[index].kind {
                ProcessKind::OneShot => self.complete_one_shot(index, finished.exit_code),
                ProcessKind::Service => self.observe_service_exit(index, finished.exit_code),
            }
        }

        if !finished.cleanup_confirmed {
            let cleanup_detail = finished
                .detail
                .unwrap_or_else(|| "Run cleanup did not fully confirm".to_string());
            self.lifecycles[index].hold_unconfirmed_cleanup(
                cleanup.timed_out,
                cleanup.unhealthy,
                cleanup_detail,
            );
            self.evaluate();
            return;
        }

        let one_shot = self.project.processes()[index].kind == ProcessKind::OneShot;
        self.lifecycles[index].confirm_cleanup(one_shot, cleanup.timed_out);
        self.finalize_confirmed_run(
            index,
            finished.run_id,
            finished.exit_code,
            finished.intentional_stop && !cleanup.timed_out && !cleanup.unhealthy,
            restart_reason,
        );
    }

    /// Apply a failed adapter fact as a confirmed failed Run. Spawn failures
    /// have no separate Process Tree cleanup fact, so this event itself
    /// releases the Run identity and enters the same restart policy path.
    fn apply_failed_run(&mut self, index: usize, kind: FailureKind, detail: String) {
        let restart_reason =
            self.restart_reason_for_terminal(index, TerminalOutcome::Failed { kind });
        let failed_run_id = self.lifecycles[index]
            .current_run
            .expect("a failed event belongs to the current Run");
        self.cancel_run_work(index);
        let one_shot = self.project.processes()[index].kind == ProcessKind::OneShot;
        self.lifecycles[index].fail_run(one_shot, kind, detail);
        self.finalize_confirmed_run(index, failed_run_id, None, false, restart_reason);
    }

    fn restart_reason_for_terminal(
        &self,
        index: usize,
        outcome: TerminalOutcome,
    ) -> Option<RestartReason> {
        let cleanup = self.lifecycles[index].cleanup_decision();
        if self.shutdown_in_progress() || cleanup.automatic_restart_suppressed {
            return None;
        }
        if cleanup.unhealthy {
            return Some(RestartReason::Unhealthy);
        }
        let policy = self.project.processes()[index].restart.policy;
        match outcome {
            TerminalOutcome::Failed { kind } => match policy {
                RestartPolicy::Never => None,
                RestartPolicy::OnFailure | RestartPolicy::Always => match kind {
                    FailureKind::Configuration | FailureKind::Spawn => {
                        Some(RestartReason::SpawnFailure)
                    }
                    _ => Some(RestartReason::FailedRun),
                },
            },
            TerminalOutcome::Finished {
                exit_code,
                intentional_stop,
                timed_out,
            } => {
                if timed_out {
                    // The timeout initiates the cleanup, so the runtime may
                    // report that cleanup as intentional. The timeout
                    // failure still owns the automatic restart decision.
                    return (policy != RestartPolicy::Never)
                        .then_some(RestartReason::StartupTimeout);
                }
                if intentional_stop {
                    return None;
                }
                let failed = !self.exit_code_succeeds(index, exit_code);
                match policy {
                    RestartPolicy::Never => None,
                    RestartPolicy::OnFailure => failed.then_some(RestartReason::FailedRun),
                    RestartPolicy::Always if failed => Some(RestartReason::FailedRun),
                    RestartPolicy::Always => Some(RestartReason::UnexpectedSuccessfulExit),
                }
            }
        }
    }

    fn finalize_confirmed_run(
        &mut self,
        index: usize,
        run_id: RunId,
        exit_code: Option<i32>,
        intentional_stop: bool,
        restart_reason: Option<RestartReason>,
    ) {
        let now_ms = self.now_ms();
        self.lifecycles[index].finish_confirmed_run(run_id, now_ms, exit_code, intentional_stop);
        if let Some(reason) = restart_reason {
            // The terminal Run identity is the timer's guard. The Run has
            // been released before the replacement timer is installed.
            self.schedule_automatic_restart(index, run_id, reason);
        }
        self.evaluate();
    }

    /// Apply the Process's configured exit-code policy consistently to every
    /// terminal path. A missing exit code never counts as success.
    fn exit_code_succeeds(&self, index: usize, code: Option<i32>) -> bool {
        code.is_some_and(|code| {
            self.project.processes()[index]
                .success_exit_codes
                .contains(&code)
        })
    }

    /// Project one One-shot Run result into its terminal lifecycle state.
    /// Only a configured exit code completes the One-shot; a missing code or
    /// any other code fails it. Desired State reverts to Stopped either way.
    fn complete_one_shot(&mut self, index: usize, code: Option<i32>) {
        let success = self.exit_code_succeeds(index, code);
        self.lifecycles[index].complete_one_shot(success, code);
    }

    /// Record a Service's unexpected natural result. The Run identity stays
    /// occupied until the finished fact confirms cleanup. Desired State stays
    /// Running so later restart policy can make an explicit decision; until
    /// then the scheduler waits for a manual start or restart.
    fn observe_service_exit(&mut self, index: usize, code: Option<i32>) {
        self.lifecycles[index].observe_service_exit(code);
    }
}
