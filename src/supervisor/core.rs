//! The authoritative Project lifecycle state machine.
//!
//! [`Core`] owns the only mutable lifecycle truth for the Project. Callers
//! reach it through semantic commands, typed seam events, and immutable
//! snapshots — the same surface the serializing task wrapper drives.

use std::ffi::{OsStr, OsString};

use crate::geometry::TerminalGeometry;
use crate::model::{Autostart, EffectiveProject, Enabled, ProcessKind};
use crate::runtime::{ProcessId, RunId};
use crate::supervisor::clock::Clock;
use crate::supervisor::seam::{RunSeam, SeamEvent, SeamSender, StartIntent};

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
    /// Bounded shutdown is in progress for the current Run.
    Stopping,
    /// No current Run remains after a stop or exit.
    Stopped,
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
    project: EffectiveProject,
    entries: Vec<Entry>,
    /// The console pane geometry at startup time; each Run's PTY opens with
    /// it so children never see a stale default size.
    initial_geometry: TerminalGeometry,
    seam: Box<dyn RunSeam>,
    #[allow(dead_code)] // Readiness intervals consume this from Issue #27 on.
    clock: Box<dyn Clock>,
    events: SeamSender,
}

struct Entry {
    process_id: ProcessId,
    next_run: u64,
    desired: DesiredState,
    lifecycle: Lifecycle,
    current_run: Option<RunId>,
    failure: Option<FailureSummary>,
    metrics: Option<MetricsMetadata>,
}

impl Core {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        project: EffectiveProject,
        seam: Box<dyn RunSeam>,
        clock: Box<dyn Clock>,
        events: SeamSender,
        initial_geometry: TerminalGeometry,
    ) -> Self {
        let entries = project
            .processes()
            .iter()
            .enumerate()
            .map(|(index, _)| Entry {
                process_id: ProcessId::new(index as u32),
                next_run: 1,
                desired: DesiredState::Stopped,
                lifecycle: Lifecycle::Idle,
                current_run: None,
                failure: None,
                metrics: None,
            })
            .collect();
        Self {
            project,
            entries,
            initial_geometry,
            seam,
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
    fn named_index(&self, name: &str) -> Option<usize> {
        self.project
            .processes()
            .iter()
            .position(|spec| spec.name == name)
    }

    fn start_at(&mut self, index: usize) {
        if !matches!(self.project.processes()[index].enabled, Enabled::Yes) {
            return;
        }
        let entry = &mut self.entries[index];
        if entry.current_run.is_some() || entry.desired == DesiredState::Running {
            return;
        }
        let run_id = RunId::new(entry.next_run);
        entry.next_run += 1;
        entry.current_run = Some(run_id);
        entry.desired = DesiredState::Running;
        entry.lifecycle = Lifecycle::Starting;
        entry.failure = None;
        entry.metrics = None;
        let intent = self.build_intent(index, run_id);
        self.seam.start(intent, &self.events);
    }

    fn build_intent(&self, index: usize, run_id: RunId) -> StartIntent {
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
        let Some(run_id) = entry.current_run else {
            return;
        };
        if entry.desired != DesiredState::Running {
            return;
        }
        // Record the intentional desired state before cleanup begins so a
        // later exit reads as an intended stop.
        entry.desired = DesiredState::Stopped;
        entry.lifecycle = Lifecycle::Stopping;
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
                let entry = &mut self.entries[index];
                if entry.lifecycle == Lifecycle::Starting {
                    // A Service without readiness becomes Running at spawn;
                    // probe-gated readiness arrives with Issues #27/#28.
                    entry.lifecycle = Lifecycle::Running;
                }
            }
            SeamEvent::Exited { code, .. } => {
                let entry = &mut self.entries[index];
                // Milestone 1 boundary: a Run identity stays occupied until
                // bounded shutdown reports completion, so an unexpected exit
                // is recovered by a manual stop then start. Automatic restart
                // policies are later milestone work.
                if entry.desired == DesiredState::Running {
                    entry.failure = Some(FailureSummary {
                        detail: match code {
                            Some(code) => format!("exited unexpectedly with code {code}"),
                            None => "exited unexpectedly".to_string(),
                        },
                    });
                }
            }
            SeamEvent::ShutdownComplete {
                confirmed, detail, ..
            } => {
                let entry = &mut self.entries[index];
                entry.current_run = None;
                entry.metrics = None;
                entry.lifecycle = Lifecycle::Stopped;
                if !confirmed {
                    entry.failure = Some(FailureSummary {
                        detail: detail
                            .unwrap_or_else(|| "Run cleanup did not fully confirm".to_string()),
                    });
                }
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

    /// A Process identity is its stable position in the Project.
    fn index_of(&self, process_id: ProcessId) -> Option<usize> {
        let index = process_id.get() as usize;
        (index < self.entries.len()).then_some(index)
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
                current_run: entry.current_run.map(RunId::get),
                failure: entry.failure.clone(),
                metrics: entry.metrics,
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

#[derive(Clone, Debug, PartialEq)]
pub struct ProcessSnapshot {
    pub name: String,
    pub kind: ProcessKind,
    pub enabled: bool,
    pub autostart: bool,
    /// Whether this Process accepts focused child input when selected.
    /// Consumed by input routing across selection (Issue #30).
    #[allow(dead_code)]
    pub input_focused: bool,
    pub desired: DesiredState,
    pub lifecycle: Lifecycle,
    /// The numeric identity of the current Run, when one exists.
    pub current_run: Option<u64>,
    pub failure: Option<FailureSummary>,
    pub metrics: Option<MetricsMetadata>,
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
