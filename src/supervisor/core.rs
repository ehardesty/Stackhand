//! The authoritative Project lifecycle state machine.
//!
//! [`Core`] owns the only mutable lifecycle truth for the Project. Callers
//! reach it through semantic commands, typed seam events, and immutable
//! snapshots — the same surface the serializing task wrapper drives.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::geometry::TerminalGeometry;
use crate::model::{Autostart, EffectiveProject, Enabled, ProcessKind};
use crate::runtime::{ProcessId, RunId};
use crate::supervisor::clock::Clock;
use crate::supervisor::command::Command;
use crate::supervisor::liveness::LivenessTracking;
use crate::supervisor::readiness::ReadinessTracking;
use crate::supervisor::seam::{ProbeSeam, RunSeam, SeamSender, StartIntent, WorkId};
use crate::supervisor::snapshot::{
    ProcessSnapshot, ProjectSnapshot, RestartBackoffStatus, RestartBudgetStatus,
};

/// The user's current intent for a Process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesiredState {
    Running,
    Stopped,
}

/// Where one Process stands in its lifecycle. This is structured state; the
/// TUI projects it into labels and never stores authoritative strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lifecycle {
    /// Never started.
    Idle,
    /// A Run was requested but has not spawned yet.
    Starting,
    /// The current Run is active.
    Running,
    /// Desired State is Running but an unsatisfied Dependency blocks the
    /// start. Scheduling starts the Process automatically once its
    /// Dependencies are satisfied.
    Waiting,
    /// Bounded shutdown is in progress for the current Run.
    Stopping,
    /// Desired State is Running while a failed Run waits before retrying.
    RestartBackoff,
    /// No current Run remains after a stop or exit.
    Stopped,
    /// A One-shot Run completed with an accepted success exit code. Done
    /// survives like a satisfied Dependency condition until a new Run starts.
    Done,
}

/// One bounded metrics sample from the current Run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MetricsMetadata {
    /// The Run this sample belongs to; a sample never crosses Run boundaries.
    pub run_id: u64,
    pub cpu_percent: f64,
    pub rss_kib: u64,
    /// True when the platform could not prove complete Process Tree coverage.
    pub best_effort: bool,
}

/// The disposition of a finished Run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunExitDisposition {
    /// The user stopped the Run before it completed.
    Stopped,
    /// A One-shot Run ended with its configured success exit code.
    Success,
    /// The Run ended in a failed result.
    Failed { code: Option<i32> },
}

/// One bounded summary of a finished Run. The Supervisor retains a small
/// recent window per Process; it is metadata, not an audit log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunSummary {
    pub run_id: u64,
    /// Milliseconds after session start when the Run began.
    pub started_at_ms: u64,
    /// Milliseconds after session start when the Run ended.
    pub ended_at_ms: u64,
    pub exit: RunExitDisposition,
    /// The raw operating-system exit code, when one was reported.
    pub exit_code: Option<i32>,
    /// Whether a user stop intentionally ended the Run.
    pub intentional_stop: bool,
    /// The bounded failure reason, present when the Run did not complete.
    pub failure: Option<String>,
    /// What started this Run.
    pub trigger: RunTrigger,
}

/// What started a Run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunTrigger {
    /// The Supervisor started the Process at session start.
    Autostart,
    /// The user started the selected Process.
    Manual,
    /// The user restarted the selected Service.
    Restart,
    /// The user reran the selected One-shot.
    Rerun,
    /// The scheduler restarted a failed Run automatically.
    AutomaticRestart,
    /// The scheduler started a Process because the user marked a
    /// dependent Process Running.
    Dependency,
}

/// The bounded size of every Process's retained Run summary window.
pub const RECENT_RUNS: usize = 8;

/// The structured class of a bounded failure summary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    /// The configured command could not be started.
    Configuration,
    /// The Run's spawn worker failed after the Run was admitted.
    Spawn,
    /// The Process ended with a failed exit.
    ProcessExit,
    /// A readiness probe failure ended the Run.
    Readiness,
    /// A liveness failure marked the Run unhealthy.
    Liveness,
    /// The Run's output path failed.
    Output,
    /// The bounded cleanup did not fully confirm.
    Shutdown,
    /// No automatic retries remain for the Process.
    RestartLimit,
}

/// One bounded failure summary: a structured kind and a bounded detail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailureSummary {
    pub kind: FailureKind,
    pub detail: String,
}

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

pub(crate) struct Core {
    pub(super) project: EffectiveProject,
    pub(super) entries: Vec<Entry>,
    /// The console pane geometry at startup time; each Run's PTY opens with
    /// it so children never see a stale default size.
    initial_geometry: TerminalGeometry,
    pub(super) seam: Box<dyn RunSeam>,
    pub(super) probes: Box<dyn ProbeSeam>,
    pub(super) clock: Arc<dyn Clock>,
    /// The session start point every Run summary's millisecond stamps use.
    epoch: Instant,
    pub(super) events: SeamSender,
    pub(super) shutdown: Option<crate::supervisor::shutdown::ShutdownState>,
}

pub(super) struct Entry {
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
    /// True while an unhealthy Run is being shut down for an automatic
    /// replacement, including while cleanup confirmation is pending.
    pub(super) unhealthy_restart_pending: bool,
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
    /// True while cleanup for a readiness startup timeout is still pending.
    pub(super) startup_timeout_pending: bool,
    /// A naturally ended Service stays desired-running but waits for an
    /// explicit start or restart when automatic restart is disabled.
    pub(super) awaiting_manual_restart: bool,
    /// The pending automatic retry, if one is waiting for its fixed delay.
    pub(super) restart_backoff: Option<RestartBackoff>,
    /// The per-Process automatic retry budget for this Project session.
    pub(super) restart_budget: RestartBudget,
    /// An explicit action can suppress automatic restart for its cleanup.
    pub(super) restart_suppressed: bool,
    /// Work identities allocated for this Process. They are monotonic across
    /// Runs so a late result cannot become current through reuse.
    pub(super) next_work_id: u64,
    /// True after Run-scoped work has been canceled. Cleanup facts are still
    /// accepted, but late observations cannot change the current snapshot.
    pub(super) run_cancelled: bool,
}

impl Entry {
    fn new(process_id: ProcessId) -> Self {
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
            unhealthy_restart_pending: false,
            cleanup_unconfirmed: false,
            runs: VecDeque::new(),
            run_started_at_ms: None,
            pending_trigger: RunTrigger::Dependency,
            run_trigger: RunTrigger::Dependency,
            root_pid: None,
            exited: false,
            startup_timeout_pending: false,
            awaiting_manual_restart: false,
            restart_backoff: None,
            restart_budget: RestartBudget::default(),
            restart_suppressed: false,
            next_work_id: 1,
            run_cancelled: false,
        }
    }

    /// Remove transient automatic-restart gates before a new explicit or
    /// admitted Run takes ownership. The pending trigger remains unchanged.
    pub(super) fn clear_restart_state(&mut self) {
        self.awaiting_manual_restart = false;
        self.restart_backoff = None;
        self.restart_suppressed = false;
        self.unhealthy_restart_pending = false;
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

impl Core {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        project: EffectiveProject,
        seam: Box<dyn RunSeam>,
        probes: Box<dyn ProbeSeam>,
        clock: Arc<dyn Clock>,
        events: SeamSender,
        initial_geometry: TerminalGeometry,
    ) -> Self {
        let entries = project
            .processes()
            .iter()
            .enumerate()
            .map(|(index, _)| Entry::new(ProcessId::new(index as u32)))
            .collect();
        let epoch = clock.now();
        Self {
            project,
            entries,
            initial_geometry,
            seam,
            probes,
            clock,
            epoch,
            events,
            shutdown: None,
        }
    }

    /// The current clock reading in session milliseconds.
    pub(super) fn now_ms(&self) -> u64 {
        self.clock
            .now()
            .saturating_duration_since(self.epoch)
            .as_millis() as u64
    }

    pub(crate) fn command(&mut self, command: Command) {
        if self.shutdown_in_progress()
            && matches!(
                command,
                Command::Start(_)
                    | Command::Restart(_)
                    | Command::Rerun(_)
                    | Command::StartAutostart
            )
        {
            return;
        }
        match command {
            Command::Start(name) => {
                if let Some(index) = self.project.process_index(&name) {
                    self.start_at(index, RunTrigger::Manual);
                }
            }
            Command::Stop(name) => {
                if let Some(index) = self.project.process_index(&name) {
                    self.stop_at(index);
                }
            }
            Command::StartAutostart => {
                for index in 0..self.entries.len() {
                    if matches!(self.project.processes()[index].autostart, Autostart::Yes) {
                        self.start_at(index, RunTrigger::Autostart);
                    }
                }
            }
            Command::StopAll => {
                for index in 0..self.entries.len() {
                    self.stop_at(index);
                }
            }
            Command::Shutdown { deadline } => self.begin_shutdown(deadline),
            Command::Restart(name) => {
                if let Some(index) = self.project.process_index(&name) {
                    self.restart_at(index, RunTrigger::Restart);
                }
            }
            Command::Rerun(name) => {
                if let Some(index) = self.project.process_index(&name) {
                    self.rerun_at(index);
                }
            }
        }
    }

    /// Rerun one enabled One-shot: invalidate its prior completion, stop
    /// the active Run when one exists, and let the scheduler open the next
    /// Run after cleanup.
    pub(super) fn rerun_at(&mut self, index: usize) {
        let spec = &self.project.processes()[index];
        if !matches!(spec.kind, ProcessKind::OneShot) || !self.is_enabled(index) {
            return;
        }
        self.restart_at(index, RunTrigger::Rerun);
    }

    pub(super) fn allocate_work_id(&mut self, index: usize) -> WorkId {
        let entry = &mut self.entries[index];
        let work_id = WorkId::new(entry.next_work_id);
        entry.next_work_id += 1;
        work_id
    }

    /// Cancel every Run-scoped adapter operation currently known to the
    /// Supervisor before the Run is stopped or replaced. Removing the local
    /// tracking below makes any result released later harmless as well.
    pub(super) fn cancel_run_work(&mut self, index: usize) {
        let Some(run_id) = self.entries[index].current_run else {
            return;
        };
        let process_id = self.entries[index].process_id;
        if let Some(tracking) = self.entries[index].readiness.as_ref() {
            tracking.cancel(self.probes.as_ref(), process_id, run_id);
        }
        if let Some(tracking) = self.entries[index].liveness.as_ref() {
            tracking.cancel(self.probes.as_ref(), process_id, run_id);
        }
        self.seam.cancel(process_id, run_id);
        self.entries[index].run_cancelled = true;
        self.entries[index].readiness = None;
        self.entries[index].liveness = None;
        self.entries[index].spawned = false;
    }

    pub(super) fn build_intent(&self, index: usize, run_id: RunId) -> StartIntent {
        let spec = &self.project.processes()[index];
        // Shell command text reaches the child through the Project's
        // configured launcher; direct commands never gain shell parsing.
        let (program, args) = spec.command.resolve(self.project.shell());
        let mut log_matchers = self.entries[index]
            .readiness
            .as_ref()
            .zip(spec.readiness.as_ref())
            .map(|(tracking, config)| tracking.log_matchers(config))
            .unwrap_or_default();
        if let Some((tracking, config)) = self.entries[index]
            .liveness
            .as_ref()
            .zip(spec.liveness.as_ref())
        {
            log_matchers.extend(tracking.log_matchers(config));
        }
        StartIntent {
            process_id: self.entries[index].process_id,
            run_id,
            program,
            args,
            working_dir: spec.working_dir.clone(),
            env: spec.env.clone(),
            env_remove: spec.env_remove.clone(),
            initial_geometry: self.initial_geometry,
            pty: matches!(spec.terminal_mode, crate::model::TerminalMode::Pty),
            log_matchers,
        }
    }

    fn stop_at(&mut self, index: usize) {
        // An unconfirmed cleanup holds its Run identity: Stop retries the
        // bounded cleanup for that same Run, and only a confirmed completion
        // releases it. The manual stop also suppresses any automatic retry
        // that the held Run would otherwise have scheduled.
        if self.entries[index].cleanup_unconfirmed {
            let run_id = self.entries[index]
                .current_run
                .expect("an unconfirmed cleanup holds its Run identity");
            let process_id = self.entries[index].process_id;
            let entry = &mut self.entries[index];
            // A Restart/Rerun already requested this cleanup as the first
            // half of a replacement. Stop is the retry operation for that
            // unconfirmed cleanup; preserve the pending desire so the
            // replacement can start after confirmation. A plain Stop keeps
            // its normal suppressing meaning.
            let replacement_pending = entry.unhealthy_restart_pending
                || matches!(
                    entry.pending_trigger,
                    crate::supervisor::RunTrigger::Restart | crate::supervisor::RunTrigger::Rerun
                );
            entry.restart_suppressed = if replacement_pending {
                entry.startup_timeout_pending
            } else {
                true
            };
            if !replacement_pending {
                entry.desired = DesiredState::Stopped;
            }
            entry.lifecycle = Lifecycle::Stopping;
            self.cancel_run_work(index);
            self.seam.stop(process_id, run_id, None, &self.events);
            return;
        }
        // A pending automatic retry has no Run to stop. Removing the state
        // invalidates the timer, so an expiry already queued elsewhere can
        // never authorize a new Run.
        if self.entries[index].restart_backoff.is_some() {
            let entry = &mut self.entries[index];
            entry.clear_restart_state();
            entry.desired = DesiredState::Stopped;
            entry.lifecycle = Lifecycle::Stopped;
            entry.blocked = None;
            return;
        }
        // A cleanup already dispatched by a timeout or restart remains the
        // same Run. Change the desired state and suppress its replacement,
        // but do not issue a second stop request.
        if self.entries[index].current_run.is_some()
            && self.entries[index].lifecycle == Lifecycle::Stopping
        {
            let entry = &mut self.entries[index];
            entry.desired = DesiredState::Stopped;
            entry.restart_suppressed = true;
            entry.blocked = None;
            return;
        }
        if self.entries[index].desired != DesiredState::Running {
            return;
        }
        // A Process without a Run (idle or Waiting on a Dependency) just
        // loses its desire to run.
        let Some(run_id) = self.entries[index].current_run else {
            let entry = &mut self.entries[index];
            entry.desired = DesiredState::Stopped;
            entry.lifecycle = Lifecycle::Stopped;
            entry.blocked = None;
            return;
        };
        // Record the intentional desired state before cleanup begins so a
        // later exit reads as an intended stop.
        let process_id = self.entries[index].process_id;
        let entry = &mut self.entries[index];
        entry.desired = DesiredState::Stopped;
        entry.lifecycle = Lifecycle::Stopping;
        entry.restart_suppressed = true;
        entry.blocked = None;
        // Pending readiness belongs to the ending Run. The work cancellation
        // also rejects any result released after this command.
        self.cancel_run_work(index);
        self.seam.stop(process_id, run_id, None, &self.events);
    }

    /// A Process identity is its stable position in the Project.
    pub(super) fn index_of(&self, process_id: ProcessId) -> Option<usize> {
        let index = process_id.get() as usize;
        (index < self.entries.len()).then_some(index)
    }

    /// Dispatch one bounded readiness attempt for every probed Service
    /// whose attempt is due. Tests drive this directly with a fake clock;
    /// the threaded wrapper drives it on tick timeouts.
    pub(crate) fn poll_timers(&mut self, now: Instant) {
        self.poll_readiness_timers(now);
        self.poll_liveness_timers(now);
        self.poll_restart_timers(now);
        self.expire_shutdown(now);
    }

    /// How long the caller may wait before readiness, restart, or shutdown
    /// work is due.
    pub(crate) fn time_until_next_timer(&self) -> Option<Duration> {
        self.readiness_time_until_next_timer()
            .into_iter()
            .chain(self.liveness_time_until_next_timer())
            .chain(self.restart_time_until_next_timer())
            .chain(self.time_until_shutdown_deadline())
            .min()
    }

    pub(crate) fn snapshot(&self) -> ProjectSnapshot {
        let now_ms = self.now_ms();
        let processes = self
            .project
            .processes()
            .iter()
            .zip(&self.entries)
            .map(|(spec, entry)| ProcessSnapshot {
                process_id: entry.process_id,
                name: spec.name.clone(),
                kind: spec.kind,
                enabled: matches!(spec.enabled, Enabled::Yes),
                autostart: matches!(spec.autostart, Autostart::Yes),
                input_focused: matches!(spec.input_policy, crate::model::InputPolicy::Focused),
                desired: entry.desired,
                lifecycle: entry.lifecycle,
                terminal_mode: spec.terminal_mode,
                current_run: entry.current_run.map(RunId::get),
                root_pid: entry.root_pid,
                run_started_at_ms: entry.run_started_at_ms,
                failure: entry.failure.clone(),
                metrics: entry.metrics,
                blocked_reason: entry.blocked.clone(),
                readiness: entry.readiness.as_ref().map(|tracking| {
                    let config = spec
                        .readiness
                        .as_ref()
                        .expect("tracking exists only for a configured readiness check");
                    tracking.snapshot(config, now_ms)
                }),
                liveness: entry.liveness.as_ref().map(|tracking| {
                    let config = spec
                        .liveness
                        .as_ref()
                        .expect("tracking exists only for a configured liveness check");
                    tracking.snapshot(config, now_ms)
                }),
                restart_backoff: entry.restart_backoff.map(|backoff| RestartBackoffStatus {
                    reason: backoff.reason.label().to_string(),
                    next_attempt_at_ms: backoff
                        .deadline
                        .saturating_duration_since(self.epoch)
                        .as_millis() as u64,
                }),
                automatic_restart_budget: entry.restart_budget.snapshot(spec.restart.max_restarts),
                recent_runs: entry.runs.iter().rev().cloned().collect(),
            })
            .collect();
        ProjectSnapshot {
            processes,
            now_ms,
            shutdown: self.shutdown_snapshot(),
        }
    }
}
