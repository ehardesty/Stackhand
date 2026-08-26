//! The authoritative Project lifecycle state machine.
//!
//! [`Core`] owns the only mutable lifecycle truth for the Project. Callers
//! reach it through semantic commands, typed seam events, and immutable
//! snapshots — the same surface the serializing task wrapper drives.

use std::ffi::{OsStr, OsString};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::geometry::TerminalGeometry;
use crate::model::{Autostart, EffectiveProject, Enabled, ProcessKind};
use crate::runtime::{ProcessId, RunId};
use crate::supervisor::clock::Clock;
use crate::supervisor::seam::{
    ProbeIntent, ProbeSeam, RunSeam, SeamEvent, SeamSender, StartIntent,
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
    /// No current Run remains after a stop or exit.
    Stopped,
    /// A One-shot Run completed with exit code zero. Done survives like a
    /// satisfied Dependency condition until a new Run completes.
    Done,
}

/// One bounded metrics sample from the current Run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MetricsMetadata {
    pub cpu_percent: f64,
    pub rss_kib: u64,
}

/// One bounded failure summary. Structured kinds arrive with One-shot and
/// shutdown work; Milestone 1 starts with a bounded diagnostic string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailureSummary {
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
    clock: Arc<dyn Clock>,
    pub(super) events: SeamSender,
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
        Self {
            project,
            entries,
            initial_geometry,
            seam,
            probes,
            clock,
            events,
        }
    }

    pub(crate) fn command(&mut self, command: Command) {
        match command {
            Command::Start(name) => {
                if let Some(index) = self.named_index(&name) {
                    self.start_at(index);
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
                        self.start_at(index);
                    }
                }
            }
            Command::StopAll => {
                for index in 0..self.entries.len() {
                    self.stop_at(index);
                }
            }
        }
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
        // Pending readiness belongs to the ending Run; its tracking ends
        // here and any in-flight attempt's result is rejected by the gate.
        entry.readiness = None;
        self.seam.stop(entry.process_id, run_id, &self.events);
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
        match event {
            SeamEvent::Spawned { .. } => {
                let probed = self.project.processes()[index].readiness.is_some();
                let entry = &mut self.entries[index];
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
                if !stopping_intentionally {
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
                let entry = &mut self.entries[index];
                entry.current_run = None;
                entry.metrics = None;
                entry.readiness = None;
                // A One-shot that completed successfully stays Done instead
                // of falling back to Stopped with its cleanup result.
                if entry.lifecycle != Lifecycle::Done {
                    entry.lifecycle = Lifecycle::Stopped;
                }
                if !confirmed {
                    entry.failure = Some(FailureSummary {
                        detail: detail
                            .unwrap_or_else(|| "Run cleanup did not fully confirm".to_string()),
                    });
                }
                self.evaluate();
            }
            SeamEvent::Failed { detail, .. } => {
                let entry = &mut self.entries[index];
                entry.failure = Some(FailureSummary { detail });
                // A failed adapter report ends the Run identity and reverts
                // the Process to stopped so it can be started again.
                entry.current_run = None;
                entry.desired = DesiredState::Stopped;
                entry.lifecycle = Lifecycle::Stopped;
                entry.metrics = None;
                entry.readiness = None;
                self.evaluate();
            }
            SeamEvent::Metrics {
                cpu_percent,
                rss_kib,
                ..
            } => {
                self.entries[index].metrics = Some(MetricsMetadata {
                    cpu_percent,
                    rss_kib,
                });
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
    pub(crate) fn time_until_next_probe(&self) -> Option<Duration> {
        let now = self.clock.now();
        self.entries
            .iter()
            .filter_map(|entry| {
                let tracking = entry.readiness.as_ref()?;
                (entry.desired == DesiredState::Running
                    && entry.current_run.is_some()
                    && !tracking.in_flight)
                    .then(|| tracking.next_attempt_at.saturating_duration_since(now))
            })
            .min()
    }

    pub(crate) fn snapshot(&self) -> ProjectSnapshot {
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
                failure: entry.failure.clone(),
                metrics: entry.metrics,
                blocked_reason: entry.blocked.clone(),
                readiness: entry.readiness.as_ref().map(|tracking| ReadinessStatus {
                    attempts: tracking.attempts,
                    last_error: tracking.last_error.clone(),
                }),
            })
            .collect();
        ProjectSnapshot { processes }
    }
}

/// The program that interprets shell command text: `$SHELL` from the
/// environment when present, otherwise `/bin/sh`.
fn shell_program(shell_env: Option<&OsStr>) -> OsString {
    shell_env.map_or_else(|| OsString::from("/bin/sh"), OsString::from)
}

/// Semantic commands. Callers never mutate Supervisor state directly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Start(String),
    Stop(String),
    StartAutostart,
    StopAll,
}

/// An immutable view of the whole Project at one moment. Rendering and
/// callers can hold and inspect this freely; it cannot mutate lifecycle
/// state.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectSnapshot {
    pub processes: Vec<ProcessSnapshot>,
}

impl ProjectSnapshot {
    pub fn named(&self, name: &str) -> Option<&ProcessSnapshot> {
        self.processes.iter().find(|p| p.name == name)
    }
}

/// One bounded readiness progress view of the current Run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadinessStatus {
    /// Attempts dispatched for the current Run so far.
    pub attempts: u32,
    /// The most recent failing attempt's bounded diagnostic, when any.
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProcessSnapshot {
    pub name: String,
    pub kind: ProcessKind,
    pub enabled: bool,
    pub autostart: bool,
    /// Whether this Process accepts focused child input when selected.
    pub input_focused: bool,
    pub desired: DesiredState,
    pub lifecycle: Lifecycle,
    /// The terminal transport of the Process; the TUI routes the selected
    /// view to the terminal session or the retained output accordingly.
    pub terminal_mode: crate::model::TerminalMode,
    /// The numeric identity of the current Run, when one exists.
    pub current_run: Option<u64>,
    pub failure: Option<FailureSummary>,
    pub metrics: Option<MetricsMetadata>,
    /// Why this Process has not started although Desired State is Running:
    /// a bounded "dependency: condition" (or "dependency: disabled") reason.
    pub blocked_reason: Option<String>,
    /// Readiness progress while the current Run of a probed Service is still
    /// becoming available; `None` without a probe or once it passed or ended.
    pub readiness: Option<ReadinessStatus>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_resolution_prefers_the_environment_and_falls_back() {
        assert_eq!(
            shell_program(Some(OsStr::new("/bin/zsh"))),
            OsString::from("/bin/zsh")
        );
        assert_eq!(shell_program(None), OsString::from("/bin/sh"));
    }
}
