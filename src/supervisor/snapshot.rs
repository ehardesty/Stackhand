//! Immutable Supervisor views for rendering and other callers.

use crate::model::ProcessKind;

use super::core::{DesiredState, FailureSummary, Lifecycle, MetricsMetadata, RunSummary};

/// An immutable view of the whole Project at one moment. Rendering and
/// callers can hold and inspect this freely; it cannot mutate lifecycle
/// state.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectSnapshot {
    pub processes: Vec<ProcessSnapshot>,
    /// The one controlled Project shutdown, once requested.
    pub shutdown: Option<super::shutdown::ProjectShutdownSnapshot>,
    /// The Supervisor session's elapsed milliseconds when this snapshot
    /// was projected; active-Run ages are measured against it.
    pub now_ms: u64,
}

impl ProjectSnapshot {
    pub fn named(&self, name: &str) -> Option<&ProcessSnapshot> {
        self.processes.iter().find(|process| process.name == name)
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

/// The immutable lifecycle and diagnostic view of one Process.
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
    /// The spawned root PID of the current Run, when observed.
    pub root_pid: Option<u32>,
    /// When the current Run began, in session milliseconds, when one
    /// exists; the age of an active Run is this stamp's distance from the
    /// snapshot's session time.
    pub run_started_at_ms: Option<u64>,
    pub failure: Option<FailureSummary>,
    pub metrics: Option<MetricsMetadata>,
    /// Why this Process has not started although Desired State is Running:
    /// a bounded "dependency: condition" (or "dependency: disabled") reason.
    pub blocked_reason: Option<String>,
    /// Readiness progress while the current Run of a probed Service is still
    /// becoming available; `None` without a probe or once it passed or ended.
    pub readiness: Option<ReadinessStatus>,
    /// The bounded recent finished-Run summaries, newest first.
    pub recent_runs: Vec<RunSummary>,
}
