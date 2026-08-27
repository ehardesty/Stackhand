//! The authoritative Project lifecycle state machine.
//!
//! [`Core`] owns the only mutable lifecycle truth for the Project. Callers
//! reach it through semantic commands, typed seam events, and immutable
//! snapshots — the same surface the serializing task wrapper drives.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::geometry::TerminalGeometry;
use crate::model::{Autostart, EffectiveProject, Enabled, ProcessKind};
use crate::runtime::{ProcessId, RunId};
use crate::supervisor::clock::Clock;
use crate::supervisor::command::{Command, shell_program};
use crate::supervisor::seam::{
    ProbeIntent, ProbeSeam, RunSeam, SeamEvent, SeamSender, StartIntent,
};
use crate::supervisor::snapshot::{ProcessSnapshot, ProjectSnapshot, ReadinessStatus};

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
    /// No current Run remains after a stop or exit.
    Stopped,
    /// A One-shot Run completed with exit code zero. Done survives like a
    /// satisfied Dependency condition until a new Run completes.
    Done,
}

/// One bounded metrics sample from the current Run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MetricsMetadata {
    /// The Run this sample belongs to; a sample never crosses Run boundaries.
    pub run_id: u64,
    pub cpu_percent: f64,
    pub rss_kib: u64,
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
    /// The Run's output path failed.
    Output,
    /// The bounded cleanup did not fully confirm.
    Shutdown,
}

/// One bounded failure summary: a structured kind and a bounded detail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailureSummary {
    pub kind: FailureKind,
    pub detail: String,
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

/// Live readiness bookkeeping for one probed Service's current Run.
#[derive(Debug)]
pub(super) struct ReadinessTracking {
    /// Attempts dispatched for this Run so far.
    pub(super) attempts: u32,
    pub(super) last_error: Option<String>,
    /// One bounded attempt is out with the probe adapter; attempts for one
    /// Run never overlap.
    pub(super) in_flight: bool,
    /// Earliest time [`Core::poll_timers`] may dispatch the next attempt.
    pub(super) next_attempt_at: Instant,
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
    /// Whether this Process's latest completed Run exited with code zero.
    /// The latch persists across evaluations and while a new Run is active;
    /// only a later Run completion replaces it (rerun semantics are Issue
    /// #32's work).
    pub(super) completed: bool,
    /// Present only while the current Run of a probed Service is alive and
    /// still awaiting its first passing attempt.
    pub(super) readiness: Option<ReadinessTracking>,
    /// The previous Run's cleanup finished unconfirmed: its Run identity
    /// stays held so a manual Stop can retry the bounded cleanup, and no
    /// new Run may replace it until that retry confirms.
    pub(super) cleanup_unconfirmed: bool,
    /// The bounded recent finished-Run summaries, oldest first.
    pub(super) runs: VecDeque<RunSummary>,
    /// When the current Run began, in session milliseconds.
    pub(super) run_started_at_ms: Option<u64>,
    /// Whether the user asked the current Run to stop before it completed.
    pub(super) stop_intended: bool,
    /// The natural exit code of the current Run, when it exited on its own.
    pub(super) natural_exit_code: Option<i32>,
    /// What will start the next Run: the latest command that marked the
    /// Desired State Running, or a restart/rerun request pending cleanup.
    pub(super) pending_trigger: RunTrigger,
    /// What started the current Run.
    pub(super) run_trigger: RunTrigger,
    /// The spawned root PID of the current Run, when observed.
    pub(super) root_pid: Option<u32>,
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
            completed: false,
            readiness: None,
            cleanup_unconfirmed: false,
            runs: VecDeque::new(),
            run_started_at_ms: None,
            stop_intended: false,
            natural_exit_code: None,
            pending_trigger: RunTrigger::Dependency,
            run_trigger: RunTrigger::Dependency,
            root_pid: None,
        }
    }

    /// Record the bounded finished-Run summary for the Run that just ended
    /// through a confirmed cleanup. The caller must invoke it before
    /// clearing the Run's identity; the summary window keeps its older
    /// entries.
    fn record_finished_run(&mut self, now_ms: u64) {
        let Some(started_at_ms) = self.run_started_at_ms.take() else {
            return;
        };
        let Some(run_id) = self.current_run else {
            return;
        };
        let exit = if self.lifecycle == Lifecycle::Done {
            RunExitDisposition::Success
        } else if self.stop_intended {
            RunExitDisposition::Stopped
        } else {
            RunExitDisposition::Failed {
                code: self.natural_exit_code,
            }
        };
        let failure = if self.lifecycle == Lifecycle::Done {
            None
        } else {
            self.failure
                .as_ref()
                .map(|failure| failure.detail.clone())
                .or_else(|| {
                    self.natural_exit_code
                        .map(|code| format!("exited with code {code}"))
                })
        };
        self.runs.push_back(RunSummary {
            run_id: run_id.get(),
            started_at_ms,
            ended_at_ms: now_ms,
            exit,
            intentional_stop: self.stop_intended,
            failure,
            trigger: self.run_trigger,
        });
        if self.runs.len() > RECENT_RUNS {
            self.runs.pop_front();
        }
        self.stop_intended = false;
        self.natural_exit_code = None;
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
                if let Some(index) = self.named_index(&name) {
                    self.start_at(index, RunTrigger::Manual);
                }
            }
            Command::Stop(name) => {
                if let Some(index) = self.named_index(&name) {
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
                if let Some(index) = self.named_index(&name) {
                    self.restart_at(index, RunTrigger::Restart);
                }
            }
            Command::Rerun(name) => {
                if let Some(index) = self.named_index(&name) {
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

    /// Resolve one configured Process name to its stable session position.
    pub(super) fn named_index(&self, name: &str) -> Option<usize> {
        self.project
            .processes()
            .iter()
            .position(|spec| spec.name == name)
    }

    pub(super) fn build_intent(&self, index: usize, run_id: RunId) -> StartIntent {
        let spec = &self.project.processes()[index];
        // Shell command text reaches the child through the user's own shell
        // so its syntax means what configuration promised; direct commands
        // never gain shell parsing.
        let (program, args) = match &spec.command {
            crate::model::CommandForm::Direct { program, args } => (program.clone(), args.clone()),
            crate::model::CommandForm::Shell { text } => (
                shell_program(std::env::var_os("SHELL").as_deref()),
                vec![OsString::from("-c"), OsString::from(text)],
            ),
        };
        StartIntent {
            process_id: self.entries[index].process_id,
            run_id,
            program,
            args,
            working_dir: spec.working_dir.clone(),
            env: spec.env.clone(),
            initial_geometry: self.initial_geometry,
            pty: matches!(spec.terminal_mode, crate::model::TerminalMode::Pty),
        }
    }

    fn stop_at(&mut self, index: usize) {
        let entry = &mut self.entries[index];
        // An unconfirmed cleanup holds its Run identity: Stop retries the
        // bounded cleanup for that same Run, and only a confirmed
        // completion releases it.
        if entry.cleanup_unconfirmed {
            let run_id = entry
                .current_run
                .expect("an unconfirmed cleanup holds its Run identity");
            entry.lifecycle = Lifecycle::Stopping;
            entry.readiness = None;
            self.seam.stop(entry.process_id, run_id, None, &self.events);
            return;
        }
        if entry.desired != DesiredState::Running {
            return;
        }
        // A Process without a Run (idle or Waiting on a Dependency) just
        // loses its desire to run.
        let Some(run_id) = entry.current_run else {
            entry.desired = DesiredState::Stopped;
            entry.lifecycle = Lifecycle::Stopped;
            entry.blocked = None;
            return;
        };
        // Record the intentional desired state before cleanup begins so a
        // later exit reads as an intended stop.
        entry.desired = DesiredState::Stopped;
        entry.lifecycle = Lifecycle::Stopping;
        entry.blocked = None;
        entry.stop_intended = true;
        // Pending readiness belongs to the ending Run; its tracking ends
        // here and any in-flight attempt's result is rejected by the gate.
        entry.readiness = None;
        self.seam.stop(entry.process_id, run_id, None, &self.events);
    }

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
        if self.entries[index].cleanup_unconfirmed
            && !matches!(event, SeamEvent::ShutdownComplete { .. })
        {
            return;
        }
        match &event {
            SeamEvent::ShutdownComplete {
                confirmed,
                detail,
                remaining_pids,
                ..
            } => self.finish_shutdown_run(
                index,
                *confirmed,
                detail.clone(),
                remaining_pids.iter().map(|pid| pid.get()).collect(),
            ),
            SeamEvent::Failed { detail, .. } if self.shutdown_in_progress() => {
                self.finish_shutdown_run(index, false, Some(detail.clone()), Vec::new());
            }
            _ => {}
        }
        match event {
            SeamEvent::Spawned { root_pid, .. } => {
                let probed = self.project.processes()[index].readiness.is_some();
                let entry = &mut self.entries[index];
                entry.root_pid = root_pid.map(|pid| pid.get());
                if entry.lifecycle == Lifecycle::Starting {
                    if probed {
                        // The Run exists but is not available yet; its first
                        // attempt is due immediately once timers are polled.
                        entry.readiness = Some(ReadinessTracking {
                            attempts: 0,
                            last_error: None,
                            in_flight: false,
                            next_attempt_at: self.clock.now(),
                        });
                    } else {
                        // A Service without readiness becomes Running at
                        // spawn; its label projects as Ready.
                        entry.lifecycle = Lifecycle::Running;
                    }
                }
                self.evaluate();
            }
            SeamEvent::Readiness {
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
                tracking.in_flight = false;
                if passing {
                    // Passing releases dependents through the Running
                    // transition exactly once per Run; per-Run readiness
                    // bookkeeping ends here, which also makes any further
                    // result for this Run land on no tracking at all.
                    entry.lifecycle = Lifecycle::Running;
                    entry.readiness = None;
                } else if !passing {
                    tracking.last_error = diagnostic;
                    if let Some(interval) = interval {
                        tracking.next_attempt_at = self.clock.now() + interval;
                    }
                }
                self.evaluate();
            }
            SeamEvent::Exited { code, .. } => {
                // An in-flight intentional stop finalizes through its own
                // ShutdownComplete; the observed code never becomes a
                // completion or a failure there.
                let stopping_intentionally = self.entries[index].lifecycle == Lifecycle::Stopping;
                if stopping_intentionally {
                    // The intentional stop's own completion report finalizes
                    // the Run; the observed code is not a result there.
                } else {
                    // Record the natural exit before projecting it: the code
                    // becomes the finished-Run summary's disposition.
                    self.entries[index].natural_exit_code = code;
                    match self.project.processes()[index].kind {
                        ProcessKind::OneShot => self.complete_one_shot(index, code),
                        ProcessKind::Service => self.observe_service_exit(index, code),
                    }
                }
                self.evaluate();
            }
            SeamEvent::ShutdownComplete {
                confirmed, detail, ..
            } => {
                let now_ms = self.now_ms();
                let entry = &mut self.entries[index];
                if !confirmed {
                    // Hold the Run identity: the cleanup is unconfirmed, so
                    // a manual Stop can retry it and no replacement Run
                    // may start until the retry confirms.
                    entry.cleanup_unconfirmed = true;
                    entry.failure = Some(FailureSummary {
                        kind: FailureKind::Shutdown,
                        detail: detail
                            .unwrap_or_else(|| "Run cleanup did not fully confirm".to_string()),
                    });
                    if entry.lifecycle != Lifecycle::Done {
                        entry.lifecycle = Lifecycle::Stopped;
                    }
                    entry.metrics = None;
                    entry.readiness = None;
                    self.evaluate();
                    return;
                }
                entry.record_finished_run(now_ms);
                entry.current_run = None;
                entry.root_pid = None;
                entry.metrics = None;
                entry.readiness = None;
                entry.cleanup_unconfirmed = false;
                // A One-shot that completed successfully stays Done instead
                // of falling back to Stopped with its cleanup result.
                if entry.lifecycle != Lifecycle::Done {
                    entry.lifecycle = Lifecycle::Stopped;
                }
                self.evaluate();
            }
            SeamEvent::Failed { kind, detail, .. } => {
                let now_ms = self.now_ms();
                let entry = &mut self.entries[index];
                entry.failure = Some(FailureSummary { kind, detail });
                // A failed adapter report ends the Run identity and reverts
                // the Process to stopped so it can be started again.
                entry.record_finished_run(now_ms);
                entry.current_run = None;
                entry.root_pid = None;
                entry.desired = DesiredState::Stopped;
                entry.lifecycle = Lifecycle::Stopped;
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

    /// Project one One-shot Run exit into its terminal lifecycle state.
    /// Exit code zero completes the One-shot; every other exit fails it.
    /// Either way Desired State reverts to Stopped: restarting is manual
    /// until automatic restart policy work lands.
    fn complete_one_shot(&mut self, index: usize, code: Option<i32>) {
        let entry = &mut self.entries[index];
        match code {
            Some(0) => {
                entry.lifecycle = Lifecycle::Done;
                entry.completed = true;
                entry.failure = None;
            }
            other => {
                entry.lifecycle = Lifecycle::Running;
                entry.completed = false;
                entry.failure = Some(FailureSummary {
                    kind: FailureKind::ProcessExit,
                    detail: match other {
                        Some(exit_code) => format!("exited with code {exit_code}"),
                        None => "exited without an exit code".to_string(),
                    },
                });
            }
        }
        entry.desired = DesiredState::Stopped;
        entry.blocked = None;
    }

    /// Record a Service's unexpected natural exit. The Run identity stays
    /// occupied until bounded shutdown reports ShutdownComplete, which then
    /// shows the Process as Stopped with this failure. Desired State reverts
    /// to Stopped so the Supervisor never silently crash-loops; automatic
    /// restart policy is later milestone work.
    fn observe_service_exit(&mut self, index: usize, code: Option<i32>) {
        let entry = &mut self.entries[index];
        if entry.desired == DesiredState::Running {
            entry.failure = Some(FailureSummary {
                kind: FailureKind::ProcessExit,
                detail: match code {
                    Some(code) => format!("exited unexpectedly with code {code}"),
                    None => "exited unexpectedly".to_string(),
                },
            });
            entry.desired = DesiredState::Stopped;
            entry.blocked = None;
        }
    }

    /// A Process identity is its stable position in the Project.
    fn index_of(&self, process_id: ProcessId) -> Option<usize> {
        let index = process_id.get() as usize;
        (index < self.entries.len()).then_some(index)
    }

    /// Dispatch one bounded readiness attempt for every probed Service
    /// whose attempt is due. Tests drive this directly with a fake clock;
    /// the threaded wrapper drives it on tick timeouts.
    pub(crate) fn poll_timers(&mut self, now: Instant) {
        for index in self.due_probe_indices(now) {
            self.dispatch_probe(index);
        }
        self.expire_shutdown(now);
    }

    fn due_probe_indices(&self, now: Instant) -> Vec<usize> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                let Some(tracking) = entry.readiness.as_ref() else {
                    return false;
                };
                entry.desired == DesiredState::Running
                    && entry.current_run.is_some()
                    && !tracking.in_flight
                    && tracking.next_attempt_at <= now
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// Hand exactly one bounded attempt to the probe adapter. The adapter
    /// answers with one `Readiness` event; until it arrives the Run has no
    /// second attempt out.
    fn dispatch_probe(&mut self, index: usize) {
        let Some(config) = &self.project.processes()[index].readiness else {
            return;
        };
        let probe = config.probe.clone();
        let timeout = config.timeout;
        let intent = {
            let entry = &mut self.entries[index];
            let Some(run_id) = entry.current_run else {
                return;
            };
            let Some(tracking) = entry.readiness.as_mut() else {
                return;
            };
            if tracking.in_flight {
                return;
            }
            tracking.in_flight = true;
            tracking.attempts += 1;
            ProbeIntent {
                process_id: entry.process_id,
                run_id,
                probe,
                timeout,
            }
        };
        self.probes.probe(intent, &self.events);
    }

    /// How long the caller may wait before some readiness attempt becomes
    /// due, or `None` when no probe work is pending.
    pub(crate) fn time_until_next_timer(&self) -> Option<Duration> {
        let now = self.clock.now();
        let probe_wait = self
            .entries
            .iter()
            .filter_map(|entry| {
                let tracking = entry.readiness.as_ref()?;
                (entry.desired == DesiredState::Running
                    && entry.current_run.is_some()
                    && !tracking.in_flight)
                    .then(|| tracking.next_attempt_at.saturating_duration_since(now))
            })
            .min();
        match (probe_wait, self.time_until_shutdown_deadline()) {
            (Some(probe), Some(shutdown)) => Some(probe.min(shutdown)),
            (Some(probe), None) => Some(probe),
            (None, Some(shutdown)) => Some(shutdown),
            (None, None) => None,
        }
    }

    pub(crate) fn snapshot(&self) -> ProjectSnapshot {
        let now_ms = self.now_ms();
        let processes = self
            .project
            .processes()
            .iter()
            .zip(&self.entries)
            .map(|(spec, entry)| ProcessSnapshot {
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
                readiness: entry.readiness.as_ref().map(|tracking| ReadinessStatus {
                    attempts: tracking.attempts,
                    last_error: tracking.last_error.clone(),
                }),
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
