//! Immutable Supervisor views for rendering and other callers.

use crate::model::{ProcessKind, ReadinessProbe};
use crate::runtime::ProcessId;

use super::core::{DesiredState, FailureSummary, Lifecycle, MetricsMetadata, RunSummary};
use super::readiness::ReadinessState;

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

/// The check kinds in the readiness snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadinessCheckKind {
    Tcp,
    Http,
    Exec,
    Log,
    All,
}

impl From<&ReadinessProbe> for ReadinessCheckKind {
    fn from(probe: &ReadinessProbe) -> Self {
        match probe {
            ReadinessProbe::Tcp { .. } => Self::Tcp,
            ReadinessProbe::Http { .. } => Self::Http,
            ReadinessProbe::Exec { .. } => Self::Exec,
            ReadinessProbe::Log { .. } => Self::Log,
        }
    }
}

/// One bounded readiness progress view for one child of an `all` check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadinessChildStatus {
    /// The one-based position of this child in the configured `all` list.
    pub index: usize,
    pub kind: ReadinessCheckKind,
    pub state: ReadinessState,
    pub attempts: u32,
    pub consecutive_successes: u32,
    pub consecutive_failures: u32,
    pub last_error: Option<String>,
}

/// One bounded readiness progress view of the current Run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadinessStatus {
    /// The kind of check configured for this Service. `All` identifies a
    /// composite whose child states are in `children`.
    pub kind: ReadinessCheckKind,
    /// The aggregate threshold state of the check.
    pub state: ReadinessState,
    /// Total attempts dispatched for the current Run. For `All`, this is the
    /// sum of the child attempts.
    pub attempts: u32,
    /// Consecutive passing attempts currently counted. For `All`, this is the
    /// sum of the child counters.
    pub consecutive_successes: u32,
    /// Consecutive failing attempts currently counted. For `All`, this is the
    /// sum of the child counters.
    pub consecutive_failures: u32,
    /// A retained bounded diagnostic from a child that has reported a
    /// failure, when any.
    pub last_error: Option<String>,
    /// Milliseconds elapsed since readiness evaluation began after spawn.
    pub startup_elapsed_ms: u64,
    /// Per-child progress for an `all` check. Empty for a direct leaf check.
    pub children: Vec<ReadinessChildStatus>,
}

/// The visible state of one pending automatic restart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestartBackoffStatus {
    /// Why the current Run will be started again.
    pub reason: String,
    /// Session milliseconds when the next automatic Run may start.
    pub next_attempt_at_ms: u64,
}

/// The visible automatic-retry budget for one Process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestartBudgetStatus {
    /// Automatic retries started after the initial Run in this session.
    pub automatic_retries_used: u32,
    /// The configured maximum automatic retries after the initial Run.
    pub max_restarts: u32,
    /// Whether a requested automatic retry was refused because no budget
    /// remained.
    pub exhausted: bool,
}

/// The immutable lifecycle and diagnostic view of one Process.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessSnapshot {
    /// The stable identity of this Process for the Supervisor session.
    /// Callers use this value instead of reconstructing it from Project order.
    pub process_id: ProcessId,
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
    /// Readiness progress for the current Run of a probed Service, including
    /// Passing and Failing recovery states; `None` without a probe or after it
    /// ended.
    pub readiness: Option<ReadinessStatus>,
    /// The pending automatic retry while the Process is in RestartBackoff.
    pub restart_backoff: Option<RestartBackoffStatus>,
    /// Automatic retry use and exhaustion for this Project session.
    pub automatic_restart_budget: RestartBudgetStatus,
    /// The bounded recent finished-Run summaries, newest first.
    pub recent_runs: Vec<RunSummary>,
}
