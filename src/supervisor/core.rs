//! The authoritative Project lifecycle state machine.
//!
//! [`Core`] owns the only mutable lifecycle truth for the Project. Callers
//! reach it through semantic commands, typed seam events, and immutable
//! snapshots — the same surface the serializing task wrapper drives.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::geometry::TerminalGeometry;
use crate::model::{Autostart, EffectiveProject, Enabled, ProcessKind};
use crate::runtime::{ProcessId, RunId};
use crate::supervisor::clock::Clock;
use crate::supervisor::command::Command;
use crate::supervisor::process_lifecycle::ProcessLifecycle;
use crate::supervisor::seam::{
    ProbeSeam, RunSeam, SeamSender, StartIntent, StartTransport, WorkId,
};
use crate::supervisor::snapshot::{ProcessSnapshot, ProjectSnapshot, RestartBackoffStatus};

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

/// One bounded listening-port observation for the current Run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListeningPortsMetadata {
    pub ports: Vec<u16>,
    pub omitted: u16,
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
    /// The user started a Waiting Process without requiring its Dependencies.
    StartAnyway,
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

pub(crate) struct Core {
    pub(super) project: EffectiveProject,
    pub(super) lifecycles: Vec<ProcessLifecycle>,
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
        let port_discovery = project.port_discovery();
        let lifecycles = project
            .processes()
            .iter()
            .enumerate()
            .map(|(index, _)| ProcessLifecycle::new(ProcessId::new(index as u32), port_discovery))
            .collect();
        let epoch = clock.now();
        Self {
            project,
            lifecycles,
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
                    | Command::StartAnyway(_)
                    | Command::Restart(_)
                    | Command::Rerun(_)
                    | Command::StartAutostart
                    | Command::RestartProfiledAutostart
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
            Command::StartAnyway(name) => {
                if let Some(index) = self.project.process_index(&name) {
                    self.start_anyway_at(index);
                }
            }
            Command::Stop(name) => {
                if let Some(index) = self.project.process_index(&name) {
                    self.stop_at(index);
                }
            }
            Command::StartAutostart => {
                for index in 0..self.lifecycles.len() {
                    if matches!(self.project.processes()[index].autostart, Autostart::Yes) {
                        self.start_at(index, RunTrigger::Autostart);
                    }
                }
            }
            Command::SelectNextProcessProfile => self.select_next_process_profile(),
            Command::SelectProjectProfile(profile) => {
                self.select_project_profile(profile.as_deref());
            }
            Command::RestartProfiledAutostart => self.restart_profiled_autostart(),
            Command::StopAll => {
                for index in 0..self.lifecycles.len() {
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

    fn select_next_process_profile(&mut self) {
        let names = self.project.process_profile_names();
        if names.is_empty() {
            return;
        }
        let next = match self.project.selected_process_profile() {
            None => names.first().cloned(),
            Some(selected) => names
                .iter()
                .position(|name| name == selected)
                .and_then(|index| names.get(index + 1).cloned()),
        };
        self.select_project_profile(next.as_deref());
    }

    fn select_project_profile(&mut self, profile: Option<&str>) {
        // Invalid names are ignored because the Supervisor has no error
        // channel. The Project API keeps the current selection unchanged.
        let _ = self.project.select_process_profile(profile);
    }

    fn restart_profiled_autostart(&mut self) {
        let affected = (0..self.lifecycles.len())
            .filter(|&index| {
                let lifecycle = &self.lifecycles[index];
                lifecycle.current_run.is_some()
                    && lifecycle.current_profile.as_deref() != self.project.process_profile(index)
            })
            .collect::<Vec<_>>();
        for index in affected {
            let spec = &self.project.processes()[index];
            if matches!(spec.enabled, Enabled::No) {
                self.stop_at(index);
            } else if matches!(spec.autostart, Autostart::Yes) {
                let trigger = if spec.kind == ProcessKind::OneShot {
                    RunTrigger::Rerun
                } else {
                    RunTrigger::Restart
                };
                self.restart_at(index, trigger);
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
        let entry = &mut self.lifecycles[index];
        let work_id = WorkId::new(entry.next_work_id);
        entry.next_work_id += 1;
        work_id
    }

    /// Cancel every Run-scoped adapter operation currently known to the
    /// Supervisor before the Run is stopped or replaced. Removing the local
    /// tracking below makes any result released later harmless as well.
    pub(super) fn cancel_run_work(&mut self, index: usize) {
        let Some(run_id) = self.lifecycles[index].current_run else {
            return;
        };
        let process_id = self.lifecycles[index].process_id;
        if let Some(tracking) = self.lifecycles[index].readiness.as_ref() {
            tracking.cancel(self.probes.as_ref(), process_id, run_id);
        }
        if let Some(tracking) = self.lifecycles[index].liveness.as_ref() {
            tracking.cancel(self.probes.as_ref(), process_id, run_id);
        }
        self.seam.cancel(process_id, run_id);
        self.lifecycles[index].cancel_run_work();
    }

    pub(super) fn build_intent(&self, index: usize, run_id: RunId) -> StartIntent {
        let spec = &self.project.processes()[index];
        // Shell command text reaches the child through the Project's
        // configured launcher; direct commands never gain shell parsing.
        let (program, args) = spec.command.resolve(self.project.shell());
        let mut log_matchers = self.lifecycles[index]
            .readiness
            .as_ref()
            .zip(spec.readiness.as_ref())
            .map(|(tracking, config)| tracking.log_matchers(config))
            .unwrap_or_default();
        if let Some((tracking, config)) = self.lifecycles[index]
            .liveness
            .as_ref()
            .zip(spec.liveness.as_ref())
        {
            log_matchers.extend(tracking.log_matchers(config));
        }
        StartIntent {
            process_id: self.lifecycles[index].process_id,
            run_id,
            program,
            args,
            working_dir: spec.working_dir.clone(),
            env: spec.env.clone(),
            env_remove: spec.env_remove.clone(),
            transport: match spec.terminal_mode {
                crate::model::TerminalMode::Pipe => StartTransport::Pipe,
                crate::model::TerminalMode::Pty => StartTransport::Pty {
                    initial_geometry: self.initial_geometry,
                },
            },
            log_matchers,
        }
    }

    fn stop_at(&mut self, index: usize) {
        let Some((process_id, run_id)) = self.lifecycles[index].request_stop() else {
            return;
        };
        // Adapter work is canceled only when the lifecycle owner requests
        // cleanup for an active or held Run.
        self.cancel_run_work(index);
        self.seam.stop(process_id, run_id, None, &self.events);
    }

    /// A Process identity is its stable position in the Project.
    pub(super) fn index_of(&self, process_id: ProcessId) -> Option<usize> {
        let index = process_id.get() as usize;
        (index < self.lifecycles.len()).then_some(index)
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
            .zip(&self.lifecycles)
            .enumerate()
            .map(|(index, (spec, entry))| ProcessSnapshot {
                process_id: entry.process_id,
                name: spec.name.clone(),
                group: self.project.process_group(index).map(str::to_owned),
                kind: spec.kind,
                enabled: matches!(spec.enabled, Enabled::Yes),
                autostart: matches!(spec.autostart, Autostart::Yes),
                input_focused: matches!(spec.input_policy, crate::model::InputPolicy::Focused),
                desired: entry.desired,
                lifecycle: entry.lifecycle,
                terminal_mode: spec.terminal_mode,
                current_run: entry.current_run.map(RunId::get),
                current_profile: entry.current_profile.clone(),
                next_profile: self
                    .project
                    .process_profile(entry.process_id.get() as usize)
                    .map(str::to_owned),
                root_pid: entry.root_pid,
                run_started_at_ms: entry.run_started_at_ms,
                failure: entry.failure.clone(),
                metrics: entry.metrics,
                listening_ports: entry.listening_ports.clone(),
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
            base_profile_name: self.project.base_profile_name().to_string(),
            selected_profile: self.project.selected_process_profile().map(str::to_owned),
            available_profiles: self.project.process_profile_names().to_vec(),
            now_ms,
            shutdown: self.shutdown_snapshot(),
        }
    }
}
