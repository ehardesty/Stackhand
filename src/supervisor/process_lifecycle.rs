//! Authoritative mutable lifecycle state for one Process.
//!
//! Scheduling and runtime facts reach this module as semantic transitions.
//! The implementation keeps Run identity, lifecycle, restart, and cleanup
//! fields together so callers do not create partial lifecycle changes.

use std::collections::VecDeque;
use std::time::Instant;

use crate::model::ReadinessConfig;
use crate::runtime::{ProcessId, RunId};
use crate::supervisor::liveness::LivenessTracking;
use crate::supervisor::readiness::ReadinessTracking;
use crate::supervisor::snapshot::RestartBudgetStatus;

use super::core::{
    DesiredState, FailureKind, FailureSummary, Lifecycle, MetricsMetadata, RECENT_RUNS,
    RunExitDisposition, RunSummary, RunTrigger,
};

/// Why the Supervisor is waiting before an automatic restart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RestartReason {
    SpawnFailure,
    FailedRun,
    StartupTimeout,
    Unhealthy,
    UnexpectedSuccessfulExit,
}

impl RestartReason {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::SpawnFailure => "spawn failure",
            Self::FailedRun => "failed Run",
            Self::StartupTimeout => "startup timeout",
            Self::Unhealthy => "unhealthy",
            Self::UnexpectedSuccessfulExit => "unexpected successful exit",
        }
    }
}

/// Why the current Run is ending. This remains authoritative until cleanup
/// confirms or the Run identity stays held for a cleanup retry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CleanupPurpose {
    #[default]
    Ordinary,
    ManualStop,
    ExplicitReplacement,
    StartupTimeout,
    StartupTimeoutSuppressed,
    UnhealthyReplacement,
    UnhealthySuppressed,
    ProjectShutdown,
    ProjectShutdownAfterStartupTimeout,
    ProjectShutdownAfterUnhealthy,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct CleanupDecision {
    pub(super) timed_out: bool,
    pub(super) unhealthy: bool,
    pub(super) automatic_restart_suppressed: bool,
}

impl CleanupPurpose {
    fn decision(self) -> CleanupDecision {
        CleanupDecision {
            timed_out: matches!(
                self,
                Self::StartupTimeout
                    | Self::StartupTimeoutSuppressed
                    | Self::ProjectShutdownAfterStartupTimeout
            ),
            unhealthy: matches!(
                self,
                Self::UnhealthyReplacement
                    | Self::UnhealthySuppressed
                    | Self::ProjectShutdownAfterUnhealthy
            ),
            automatic_restart_suppressed: matches!(
                self,
                Self::ManualStop
                    | Self::StartupTimeoutSuppressed
                    | Self::UnhealthySuppressed
                    | Self::ProjectShutdown
                    | Self::ProjectShutdownAfterStartupTimeout
                    | Self::ProjectShutdownAfterUnhealthy
            ),
        }
    }

    fn suppress_automatic_restart(self) -> Self {
        match self {
            Self::StartupTimeout | Self::StartupTimeoutSuppressed => Self::StartupTimeoutSuppressed,
            Self::UnhealthyReplacement | Self::UnhealthySuppressed => Self::UnhealthySuppressed,
            Self::ProjectShutdown
            | Self::ProjectShutdownAfterStartupTimeout
            | Self::ProjectShutdownAfterUnhealthy => self,
            Self::Ordinary | Self::ManualStop | Self::ExplicitReplacement => Self::ManualStop,
        }
    }

    fn begin_project_shutdown(self) -> Self {
        let decision = self.decision();
        if decision.timed_out {
            Self::ProjectShutdownAfterStartupTimeout
        } else if decision.unhealthy {
            Self::ProjectShutdownAfterUnhealthy
        } else {
            Self::ProjectShutdown
        }
    }
}

/// One pending automatic restart. The failed Run identity makes the timer
/// specific to the Run that created it even though no Run is current during
/// the backoff.
#[derive(Clone, Copy, Debug)]
pub(super) struct RestartBackoff {
    pub(super) failed_run_id: RunId,
    pub(super) deadline: Instant,
    pub(super) reason: RestartReason,
}

/// Mutable retry state for one Process. The configured maximum remains in
/// the immutable Process specification; this value tracks only this session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct RestartBudget {
    automatic_retries_used: u32,
    exhausted: bool,
}

impl RestartBudget {
    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(super) fn has_remaining(&self, max_restarts: u32) -> bool {
        !self.exhausted && self.automatic_retries_used < max_restarts
    }

    pub(super) fn consume(&mut self, max_restarts: u32) -> bool {
        if !self.has_remaining(max_restarts) {
            self.exhausted = true;
            return false;
        }
        self.automatic_retries_used += 1;
        true
    }

    pub(super) fn exhaust(&mut self) {
        self.exhausted = true;
    }

    pub(super) fn snapshot(&self, max_restarts: u32) -> RestartBudgetStatus {
        RestartBudgetStatus {
            automatic_retries_used: self.automatic_retries_used,
            max_restarts,
            exhausted: self.exhausted,
        }
    }
}

pub(super) struct ProcessLifecycle {
    pub(super) process_id: ProcessId,
    pub(super) next_run: u64,
    pub(super) desired: DesiredState,
    pub(super) lifecycle: Lifecycle,
    pub(super) current_run: Option<RunId>,
    pub(super) failure: Option<FailureSummary>,
    pub(super) metrics: Option<MetricsMetadata>,
    /// Why Desired State Running has not produced a Run yet, as a bounded
    /// "dependency: condition" reason.
    pub(super) blocked: Option<String>,
    /// Present while the current Run of a probed Service has an active
    /// readiness check, including after the first pass for recovery tracking.
    pub(super) readiness: Option<ReadinessTracking>,
    /// Present while the current Run has an ongoing liveness policy. It is
    /// inactive until this Run first becomes effectively ready.
    pub(super) liveness: Option<LivenessTracking>,
    /// True after the adapter reports that the current Run has spawned.
    pub(super) spawned: bool,
    /// Why the current Run is ending and how its terminal fact is classified.
    cleanup_purpose: CleanupPurpose,
    /// The previous Run's cleanup finished unconfirmed: its Run identity
    /// stays held so a manual Stop can retry the bounded cleanup, and no
    /// new Run may replace it until that retry confirms.
    pub(super) cleanup_unconfirmed: bool,
    /// The bounded recent finished-Run summaries, oldest first.
    pub(super) runs: VecDeque<RunSummary>,
    /// When the current Run began, in session milliseconds.
    pub(super) run_started_at_ms: Option<u64>,
    /// What will start the next Run: the latest command that marked the
    /// Desired State Running, or a restart/rerun request pending cleanup.
    pub(super) pending_trigger: RunTrigger,
    /// What started the current Run.
    pub(super) run_trigger: RunTrigger,
    /// The spawned root PID of the current Run, when observed.
    pub(super) root_pid: Option<u32>,
    /// True after a One-shot Run has ended and its cleanup is confirmed.
    pub(super) exited: bool,
    /// A naturally ended Service stays desired-running but waits for an
    /// explicit start or restart when automatic restart is disabled.
    pub(super) awaiting_manual_restart: bool,
    /// The pending automatic retry, if one is waiting for its fixed delay.
    pub(super) restart_backoff: Option<RestartBackoff>,
    /// The per-Process automatic retry budget for this Project session.
    pub(super) restart_budget: RestartBudget,
    /// Work identities allocated for this Process. They are monotonic across
    /// Runs so a late result cannot become current through reuse.
    pub(super) next_work_id: u64,
    /// True after Run-scoped work has been canceled. Cleanup facts are still
    /// accepted, but late observations cannot change the current snapshot.
    pub(super) run_cancelled: bool,
}

impl ProcessLifecycle {
    pub(super) fn new(process_id: ProcessId) -> Self {
        Self {
            process_id,
            next_run: 1,
            desired: DesiredState::Stopped,
            lifecycle: Lifecycle::Idle,
            current_run: None,
            failure: None,
            metrics: None,
            blocked: None,
            readiness: None,
            liveness: None,
            spawned: false,
            cleanup_purpose: CleanupPurpose::Ordinary,
            cleanup_unconfirmed: false,
            runs: VecDeque::new(),
            run_started_at_ms: None,
            pending_trigger: RunTrigger::Dependency,
            run_trigger: RunTrigger::Dependency,
            root_pid: None,
            exited: false,
            awaiting_manual_restart: false,
            restart_backoff: None,
            restart_budget: RestartBudget::default(),
            next_work_id: 1,
            run_cancelled: false,
        }
    }

    /// Apply one Start request before Project Dependency propagation.
    pub(super) fn prepare_start_request(&mut self, trigger: RunTrigger) {
        if trigger != RunTrigger::Manual {
            return;
        }
        let timed_out = self.cleanup_purpose.decision().timed_out;
        self.clear_restart_state();
        self.restart_budget.reset();
        self.pending_trigger = trigger;
        self.cleanup_purpose = if timed_out {
            CleanupPurpose::StartupTimeoutSuppressed
        } else {
            CleanupPurpose::Ordinary
        };
        self.exited = false;
    }

    /// Apply one restart or rerun request before cleanup and scheduling.
    pub(super) fn prepare_restart_request(&mut self, trigger: RunTrigger) {
        let timed_out = self.cleanup_purpose.decision().timed_out;
        self.pending_trigger = trigger;
        self.clear_restart_state();
        if matches!(trigger, RunTrigger::Restart | RunTrigger::Rerun) {
            self.restart_budget.reset();
        }
        self.cleanup_purpose = if timed_out {
            CleanupPurpose::StartupTimeoutSuppressed
        } else {
            CleanupPurpose::ExplicitReplacement
        };
        self.exited = false;
    }

    /// Change Desired State to Running once. The return value tells the
    /// scheduler whether it must propagate the request to Dependencies.
    pub(super) fn require_running(&mut self, trigger: RunTrigger) -> bool {
        if self.desired == DesiredState::Running {
            return false;
        }
        self.desired = DesiredState::Running;
        self.pending_trigger = trigger;
        true
    }

    /// Move the current Run into cleanup for an explicit replacement.
    pub(super) fn begin_replacement_cleanup(&mut self) -> Option<(ProcessId, RunId)> {
        let run_id = self.current_run.filter(|_| !self.cleanup_unconfirmed)?;
        self.lifecycle = Lifecycle::Stopping;
        self.blocked = None;
        Some((self.process_id, run_id))
    }

    /// Return the deadline only while every fact still identifies the failed
    /// Run that created this automatic-restart wait.
    pub(super) fn valid_restart_deadline(&self) -> Option<Instant> {
        let backoff = self.restart_backoff?;
        (self.current_run.is_none()
            && self.desired == DesiredState::Running
            && self.lifecycle == Lifecycle::RestartBackoff
            && self.next_run == backoff.failed_run_id.get().saturating_add(1))
        .then_some(backoff.deadline)
    }

    /// Release an authoritative automatic-restart timer into scheduling.
    pub(super) fn release_restart_backoff(&mut self) {
        self.restart_backoff = None;
        self.pending_trigger = RunTrigger::AutomaticRestart;
    }

    /// Admit one Run and reset every prior Run-scoped fact together.
    pub(super) fn begin_run(
        &mut self,
        now_ms: u64,
        readiness: Option<ReadinessTracking>,
        liveness: Option<LivenessTracking>,
    ) -> RunId {
        let run_id = RunId::new(self.next_run);
        self.next_run += 1;
        self.current_run = Some(run_id);
        self.lifecycle = Lifecycle::Starting;
        self.failure = None;
        self.metrics = None;
        self.exited = false;
        self.cleanup_purpose = CleanupPurpose::Ordinary;
        self.clear_restart_state();
        self.run_cancelled = false;
        self.blocked = None;
        self.run_started_at_ms = Some(now_ms);
        self.run_trigger = self.pending_trigger;
        self.readiness = readiness;
        self.liveness = liveness;
        self.spawned = false;
        run_id
    }

    /// Admit one automatic retry from this Process's session budget.
    pub(super) fn admit_automatic_retry(&mut self, max_restarts: u32) -> bool {
        self.restart_budget.consume(max_restarts)
    }

    /// Apply the first authoritative spawn fact for the current Run.
    pub(super) fn record_spawn(
        &mut self,
        root_pid: Option<u32>,
        readiness: Option<ReadinessTracking>,
        config: Option<&ReadinessConfig>,
        now: Instant,
        now_ms: u64,
    ) {
        self.root_pid = root_pid;
        self.spawned = true;
        if let Some(readiness) = readiness {
            self.readiness = Some(readiness);
        }
        if let Some(config) = config
            && let Some(tracking) = self.readiness.as_mut()
        {
            tracking.activate(config, now, now_ms);
        } else if self.lifecycle == Lifecycle::Starting && self.readiness.is_none() {
            self.lifecycle = Lifecycle::Running;
        }
    }

    /// Project aggregate readiness into the Process lifecycle.
    pub(super) fn promote_ready(&mut self) -> bool {
        if let Some(tracking) = self.readiness.as_mut() {
            tracking.clear_startup_deadline();
        }
        if self.lifecycle == Lifecycle::Starting {
            self.lifecycle = Lifecycle::Running;
        }
        self.spawned
    }

    /// Keep a desired Process waiting on one unsatisfied Dependency.
    pub(super) fn wait_for_dependency(&mut self, reason: String) {
        if self.current_run.is_some()
            || self.desired != DesiredState::Running
            || self.restart_backoff.is_some()
        {
            return;
        }
        self.lifecycle = Lifecycle::Waiting;
        self.failure = None;
        self.blocked = Some(reason);
    }

    /// Apply a manual Stop and return the Run that needs adapter cleanup.
    /// State-only stops return no Run, and repeated cleanup requests retain
    /// the same Run identity.
    pub(super) fn request_stop(&mut self) -> Option<(ProcessId, RunId)> {
        if self.cleanup_unconfirmed {
            let run_id = self
                .current_run
                .expect("an unconfirmed cleanup holds its Run identity");
            let decision = self.cleanup_purpose.decision();
            let replacement_pending = decision.unhealthy
                || matches!(
                    self.pending_trigger,
                    RunTrigger::Restart | RunTrigger::Rerun
                );
            if !replacement_pending || decision.timed_out {
                self.cleanup_purpose = self.cleanup_purpose.suppress_automatic_restart();
            }
            if !replacement_pending {
                self.desired = DesiredState::Stopped;
            }
            self.lifecycle = Lifecycle::Stopping;
            return Some((self.process_id, run_id));
        }
        if self.restart_backoff.is_some() {
            self.clear_restart_state();
            self.desired = DesiredState::Stopped;
            self.lifecycle = Lifecycle::Stopped;
            self.blocked = None;
            return None;
        }
        if self.current_run.is_some() && self.lifecycle == Lifecycle::Stopping {
            self.desired = DesiredState::Stopped;
            self.cleanup_purpose = self.cleanup_purpose.suppress_automatic_restart();
            self.blocked = None;
            return None;
        }
        if self.desired != DesiredState::Running {
            return None;
        }
        let Some(run_id) = self.current_run else {
            self.desired = DesiredState::Stopped;
            self.lifecycle = Lifecycle::Stopped;
            self.blocked = None;
            return None;
        };
        self.desired = DesiredState::Stopped;
        self.lifecycle = Lifecycle::Stopping;
        self.cleanup_purpose = CleanupPurpose::ManualStop;
        self.blocked = None;
        Some((self.process_id, run_id))
    }

    /// Close all observational state for the current Run.
    pub(super) fn cancel_run_work(&mut self) {
        self.run_cancelled = true;
        self.readiness = None;
        self.liveness = None;
        self.spawned = false;
    }

    /// Start cleanup after readiness did not pass before its deadline.
    pub(super) fn timeout_startup(&mut self, detail: String) {
        self.cleanup_purpose = CleanupPurpose::StartupTimeout;
        self.failure = Some(FailureSummary {
            kind: FailureKind::Readiness,
            detail,
        });
        self.desired = DesiredState::Stopped;
        self.lifecycle = Lifecycle::Stopping;
        self.blocked = None;
    }

    /// Record a failed liveness decision and, when configured, move the Run
    /// into cleanup for an automatic replacement.
    pub(super) fn fail_liveness(
        &mut self,
        detail: String,
        restart_on_unhealthy: bool,
    ) -> Option<(ProcessId, RunId)> {
        self.failure = Some(FailureSummary {
            kind: FailureKind::Liveness,
            detail,
        });
        if !restart_on_unhealthy {
            return None;
        }
        let run_id = self.current_run?;
        self.cleanup_purpose = CleanupPurpose::UnhealthyReplacement;
        self.desired = DesiredState::Stopped;
        self.lifecycle = Lifecycle::Stopping;
        self.blocked = None;
        Some((self.process_id, run_id))
    }

    /// Retain one current-Run metrics sample.
    pub(super) fn record_metrics(&mut self, metrics: MetricsMetadata) {
        self.metrics = Some(metrics);
    }

    /// Retain an output-path failure without hiding a lifecycle failure.
    pub(super) fn record_output_failure(&mut self, detail: String) {
        if self.failure.is_none() {
            self.failure = Some(FailureSummary {
                kind: FailureKind::Output,
                detail,
            });
        }
    }

    /// Remove only the liveness failure after the checks recover.
    pub(super) fn recover_liveness(&mut self) {
        if self
            .failure
            .as_ref()
            .is_some_and(|failure| failure.kind == FailureKind::Liveness)
        {
            self.failure = None;
        }
    }

    /// Apply this Process's part of Project shutdown in one transition.
    pub(super) fn begin_project_shutdown(&mut self) {
        self.desired = DesiredState::Stopped;
        self.blocked = None;
        self.readiness = None;
        self.restart_backoff = None;
        self.cleanup_purpose = self.cleanup_purpose.begin_project_shutdown();
        if self.current_run.is_none()
            && matches!(
                self.lifecycle,
                Lifecycle::Waiting | Lifecycle::RestartBackoff
            )
        {
            self.lifecycle = Lifecycle::Stopped;
        }
    }

    /// Mark an admitted Run as being cleaned up by Project shutdown.
    pub(super) fn begin_shutdown_cleanup(&mut self) {
        debug_assert!(self.current_run.is_some());
        self.lifecycle = Lifecycle::Stopping;
    }

    pub(super) fn cleanup_decision(&self) -> CleanupDecision {
        self.cleanup_purpose.decision()
    }

    /// Hold a Run identity when adapter cleanup did not confirm.
    pub(super) fn hold_unconfirmed_cleanup(
        &mut self,
        timed_out: bool,
        unhealthy: bool,
        cleanup_detail: String,
    ) {
        self.cleanup_unconfirmed = true;
        if timed_out || unhealthy {
            let failure = self.failure.get_or_insert(FailureSummary {
                kind: if unhealthy {
                    FailureKind::Liveness
                } else {
                    FailureKind::Readiness
                },
                detail: if unhealthy {
                    "liveness failure threshold reached".to_string()
                } else {
                    "readiness startup timeout".to_string()
                },
            });
            failure.detail = format!("{}; cleanup failed: {cleanup_detail}", failure.detail);
        } else {
            self.failure = Some(FailureSummary {
                kind: FailureKind::Shutdown,
                detail: cleanup_detail,
            });
        }
        if self.lifecycle != Lifecycle::Done {
            self.lifecycle = if timed_out || unhealthy {
                Lifecycle::Stopping
            } else {
                Lifecycle::Stopped
            };
        }
        self.metrics = None;
        self.readiness = None;
        self.liveness = None;
        self.spawned = false;
    }

    /// Record facts that are known before a confirmed cleanup releases a Run.
    pub(super) fn confirm_cleanup(&mut self, one_shot: bool, timed_out: bool) {
        if one_shot {
            self.exited = true;
        }
        if timed_out {
            let failure = self
                .failure
                .as_mut()
                .expect("a pending startup timeout has a recorded failure");
            failure.detail.push_str("; cleanup confirmed");
        }
    }

    /// A failed adapter fact ends the admitted Run without a second cleanup.
    pub(super) fn fail_run(&mut self, one_shot: bool, kind: FailureKind, detail: String) {
        if one_shot {
            self.exited = true;
        }
        self.failure = Some(FailureSummary { kind, detail });
        self.desired = DesiredState::Stopped;
        self.lifecycle = Lifecycle::Stopped;
    }

    /// Project one One-shot exit through its configured success decision.
    pub(super) fn complete_one_shot(&mut self, success: bool, code: Option<i32>) {
        if success {
            self.lifecycle = Lifecycle::Done;
            self.failure = None;
        } else {
            self.lifecycle = Lifecycle::Stopped;
            self.failure = Some(FailureSummary {
                kind: FailureKind::ProcessExit,
                detail: match code {
                    Some(exit_code) => format!("exited with code {exit_code}"),
                    None => "exited without an exit code".to_string(),
                },
            });
        }
        self.desired = DesiredState::Stopped;
        self.blocked = None;
    }

    /// Record an unexpected natural Service exit while preserving desire.
    pub(super) fn observe_service_exit(&mut self, code: Option<i32>) {
        if self.desired != DesiredState::Running {
            return;
        }
        self.failure = Some(FailureSummary {
            kind: FailureKind::ProcessExit,
            detail: match code {
                Some(code) => format!("exited unexpectedly with code {code}"),
                None => "exited unexpectedly".to_string(),
            },
        });
        self.awaiting_manual_restart = true;
        self.blocked = None;
    }

    /// Release one confirmed Run identity after recording its summary.
    pub(super) fn finish_confirmed_run(
        &mut self,
        run_id: RunId,
        now_ms: u64,
        exit_code: Option<i32>,
        intentional_stop: bool,
    ) {
        debug_assert_eq!(self.current_run, Some(run_id));
        self.record_finished_run(now_ms, exit_code, intentional_stop);
        self.current_run = None;
        self.root_pid = None;
        self.metrics = None;
        self.readiness = None;
        self.liveness = None;
        self.spawned = false;
        self.cleanup_purpose = CleanupPurpose::Ordinary;
        self.cleanup_unconfirmed = false;
        if self.lifecycle != Lifecycle::Done {
            self.lifecycle = Lifecycle::Stopped;
        }
    }

    /// Hold the next automatic attempt behind its fixed delay.
    pub(super) fn wait_for_automatic_restart(
        &mut self,
        failed_run_id: RunId,
        deadline: Instant,
        reason: RestartReason,
    ) {
        self.desired = DesiredState::Running;
        self.lifecycle = Lifecycle::RestartBackoff;
        self.blocked = None;
        self.clear_restart_state();
        self.restart_backoff = Some(RestartBackoff {
            failed_run_id,
            deadline,
            reason,
        });
    }

    /// Stop automatic admission after the configured restart budget ends.
    pub(super) fn exhaust_restart_budget(&mut self, max_restarts: u32) {
        self.restart_budget.exhaust();
        self.desired = DesiredState::Running;
        self.lifecycle = Lifecycle::Stopped;
        self.blocked = None;
        self.awaiting_manual_restart = true;
        self.restart_backoff = None;
        self.cleanup_purpose = CleanupPurpose::Ordinary;
        self.failure = Some(FailureSummary {
            kind: FailureKind::RestartLimit,
            detail: format!("Restart limit reached after {max_restarts} automatic attempts"),
        });
    }

    /// Remove transient automatic-restart gates before a new explicit or
    /// admitted Run takes ownership. The pending trigger remains unchanged.
    pub(super) fn clear_restart_state(&mut self) {
        self.awaiting_manual_restart = false;
        self.restart_backoff = None;
    }

    /// Record the bounded finished-Run summary for the Run that just ended
    /// through a confirmed cleanup. The caller must invoke it before
    /// clearing the Run's identity; the summary window keeps its older
    /// entries.
    pub(super) fn record_finished_run(
        &mut self,
        now_ms: u64,
        exit_code: Option<i32>,
        intentional_stop: bool,
    ) {
        let Some(started_at_ms) = self.run_started_at_ms.take() else {
            return;
        };
        let Some(run_id) = self.current_run else {
            return;
        };
        let exit = if self.lifecycle == Lifecycle::Done {
            RunExitDisposition::Success
        } else if intentional_stop {
            RunExitDisposition::Stopped
        } else {
            RunExitDisposition::Failed { code: exit_code }
        };
        let failure = if self.lifecycle == Lifecycle::Done || intentional_stop {
            None
        } else {
            self.failure
                .as_ref()
                .map(|failure| failure.detail.clone())
                .or_else(|| exit_code.map(|code| format!("exited with code {code}")))
        };
        self.runs.push_back(RunSummary {
            run_id: run_id.get(),
            started_at_ms,
            ended_at_ms: now_ms,
            exit,
            exit_code,
            intentional_stop,
            failure,
            trigger: self.run_trigger,
        });
        if self.runs.len() > RECENT_RUNS {
            self.runs.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn lifecycle() -> ProcessLifecycle {
        ProcessLifecycle::new(ProcessId::new(7))
    }

    #[test]
    fn stopping_without_a_run_closes_desire_without_cleanup() {
        let mut lifecycle = lifecycle();
        assert!(lifecycle.require_running(RunTrigger::Manual));

        assert_eq!(lifecycle.request_stop(), None);
        assert_eq!(lifecycle.desired, DesiredState::Stopped);
        assert_eq!(lifecycle.lifecycle, Lifecycle::Stopped);
        assert_eq!(lifecycle.current_run, None);
        assert_eq!(lifecycle.blocked, None);
    }

    #[test]
    fn restart_deadline_requires_the_failed_run_identity_and_wait_state() {
        let mut lifecycle = lifecycle();
        lifecycle.next_run = 2;
        let deadline = Instant::now() + Duration::from_secs(1);
        lifecycle.wait_for_automatic_restart(RunId::new(1), deadline, RestartReason::FailedRun);
        assert_eq!(lifecycle.valid_restart_deadline(), Some(deadline));

        lifecycle.next_run = 3;
        assert_eq!(lifecycle.valid_restart_deadline(), None);
    }

    #[test]
    fn confirmed_finish_releases_all_run_scoped_facts_together() {
        let mut lifecycle = lifecycle();
        lifecycle.require_running(RunTrigger::Manual);
        let run_id = lifecycle.begin_run(10, None, None);
        lifecycle.record_spawn(None, None, None, Instant::now(), 10);
        lifecycle.failure = Some(FailureSummary {
            kind: FailureKind::ProcessExit,
            detail: "failed".to_string(),
        });

        lifecycle.finish_confirmed_run(run_id, 20, Some(1), false);

        assert_eq!(lifecycle.current_run, None);
        assert_eq!(lifecycle.root_pid, None);
        assert_eq!(lifecycle.metrics, None);
        assert!(lifecycle.readiness.is_none());
        assert!(lifecycle.liveness.is_none());
        assert!(!lifecycle.spawned);
        assert!(!lifecycle.cleanup_unconfirmed);
        assert_eq!(lifecycle.lifecycle, Lifecycle::Stopped);
        assert_eq!(lifecycle.runs.len(), 1);
    }
}
