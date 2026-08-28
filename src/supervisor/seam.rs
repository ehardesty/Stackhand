//! Private seams between the Supervisor control task and the work that must
//! never run on it: process execution, bounded shutdown, readiness probes,
//! and time.
//!
//! These types are `pub(crate)`. Production adapters wrap the existing Run
//! interface and real sockets; tests install scripted fakes. Neither adapter
//! widens the external Supervisor interface.

use crossbeam_channel::Sender;
use std::path::PathBuf;
use std::time::Duration;

use crate::geometry::TerminalGeometry;
use crate::model::{ReadinessProbe, ShellConfig};
use crate::runtime::{OsPid, ProcessId, RunId};
use crate::supervisor::FailureKind;

/// Identity of one long-lived piece of work within a Run, such as a
/// readiness or liveness check. Work identities are never reused for a later
/// Run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct WorkId(u64);

impl WorkId {
    pub(crate) fn new(value: u64) -> Self {
        Self(value)
    }

    #[allow(dead_code)]
    pub(crate) fn get(self) -> u64 {
        self.0
    }
}

/// Identity of one attempt made by a Run-scoped piece of work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct AttemptId(u64);

impl AttemptId {
    pub(crate) fn new(value: u64) -> Self {
        Self(value)
    }

    #[allow(dead_code)]
    pub(crate) fn get(self) -> u64 {
        self.0
    }
}

/// Where adapters deliver typed events back to the Supervisor. Every
/// Run-scoped event carries both identities so the Supervisor can reject
/// stale events in one place.
#[derive(Clone)]
pub struct SeamSender {
    tx: Sender<SeamEvent>,
}

impl SeamSender {
    pub(crate) fn new(tx: Sender<SeamEvent>) -> Self {
        Self { tx }
    }

    pub(crate) fn send(&self, event: SeamEvent) {
        // A closed inbox means the Supervisor task has stopped; dropping the
        // event is then correct.
        let _ = self.tx.send(event);
    }
}

/// The one bounded fact produced when a Run and its cleanup finish.
/// `exit_code` and `intentional_stop` come from the authoritative
/// [`crate::runtime::RunOutcome`] when one was produced.
#[derive(Clone, Debug, PartialEq)]
pub struct FinishedRun {
    pub process_id: ProcessId,
    pub run_id: RunId,
    pub exit_code: Option<i32>,
    pub intentional_stop: bool,
    pub cleanup_confirmed: bool,
    pub detail: Option<String>,
    pub remaining_pids: Vec<OsPid>,
}

/// One typed event from a runtime or probe adapter. Output bytes never
/// travel through this type.
#[derive(Clone, Debug, PartialEq)]
pub enum SeamEvent {
    /// The root process spawned for this Run.
    Spawned {
        process_id: ProcessId,
        run_id: RunId,
        root_pid: Option<OsPid>,
    },
    /// The Run and its bounded cleanup finished.
    Finished(FinishedRun),
    /// The Run could not start or an owned worker failed. Carries no output
    /// bytes.
    Failed {
        process_id: ProcessId,
        run_id: RunId,
        kind: FailureKind,
        detail: String,
    },
    /// The Run's output path failed. Bounded: the reader reports the first
    /// failure of each stream only.
    OutputFailure {
        process_id: ProcessId,
        run_id: RunId,
        detail: String,
    },
    /// One aggregate Process Tree sample for the current Run.
    Metrics {
        process_id: ProcessId,
        run_id: RunId,
        cpu_percent: f64,
        rss_kib: u64,
    },
    /// One bounded readiness attempt finished for the current Run. `passing`
    /// is true only when the probe succeeded; a failure carries one bounded
    /// diagnostic.
    Readiness {
        process_id: ProcessId,
        run_id: RunId,
        work_id: WorkId,
        attempt_id: AttemptId,
        passing: bool,
        diagnostic: Option<String>,
    },
    /// One bounded liveness attempt finished for the current Run.
    Liveness {
        process_id: ProcessId,
        run_id: RunId,
        work_id: WorkId,
        attempt_id: AttemptId,
        passing: bool,
        diagnostic: Option<String>,
    },
    /// A configured literal appeared in the current Run's live output.
    /// Output bytes never travel through this event.
    LogMatched {
        process_id: ProcessId,
        run_id: RunId,
        work_id: WorkId,
        /// Readiness matches have no scheduled attempt. Liveness matches
        /// carry the attempt that armed the fresh output window.
        attempt_id: Option<AttemptId>,
    },
}

impl SeamEvent {
    pub(crate) fn process_id(&self) -> ProcessId {
        match self {
            Self::Spawned { process_id, .. }
            | Self::Failed { process_id, .. }
            | Self::OutputFailure { process_id, .. }
            | Self::Metrics { process_id, .. }
            | Self::Readiness { process_id, .. }
            | Self::Liveness { process_id, .. }
            | Self::LogMatched { process_id, .. } => *process_id,
            Self::Finished(finished) => finished.process_id,
        }
    }

    pub(crate) fn run_id(&self) -> RunId {
        match self {
            Self::Spawned { run_id, .. }
            | Self::Failed { run_id, .. }
            | Self::OutputFailure { run_id, .. }
            | Self::Metrics { run_id, .. }
            | Self::Readiness { run_id, .. }
            | Self::Liveness { run_id, .. }
            | Self::LogMatched { run_id, .. } => *run_id,
            Self::Finished(finished) => finished.run_id,
        }
    }
}

/// One live log matcher attached to a Run before it can emit output.
#[derive(Clone, Debug)]
pub(crate) struct LogMatcherIntent {
    pub(crate) work_id: WorkId,
    /// `None` is a latched readiness match. `Some` identifies one fresh
    /// liveness attempt window.
    pub(crate) attempt_id: Option<AttemptId>,
    pub(crate) contains: String,
}

/// One request to begin a Run. The Supervisor allocates the Run identity;
/// adapters perform the actual spawn off the control task.
#[derive(Clone, Debug)]
pub struct StartIntent {
    pub process_id: ProcessId,
    pub run_id: RunId,
    pub program: std::ffi::OsString,
    pub args: Vec<std::ffi::OsString>,
    /// Absolute working directory. Consumed by the production adapter once
    /// the Run interface accepts it (Issue #22).
    pub working_dir: std::path::PathBuf,
    pub env: Vec<(String, String)>,
    /// The PTY geometry of the rendered console pane at request time.
    pub initial_geometry: TerminalGeometry,
    pub pty: bool,
    /// Literal log checks to attach before this Run is spawned. Liveness
    /// entries are placeholders until the first attempt is armed.
    pub(crate) log_matchers: Vec<LogMatcherIntent>,
}

/// The runtime seam. Implementations own every Run they start until stop.
pub(crate) trait RunSeam: Send {
    /// Cancel Run-scoped work that is not part of the Process Tree owner,
    /// such as output observations or future hooks. Cancellation is
    /// idempotent and never changes Supervisor state by itself.
    fn cancel(&self, _process_id: ProcessId, _run_id: RunId) {}

    /// Replace one live log matcher with a fresh attempt window. The default
    /// is a no-op for adapters that do not observe output.
    fn arm_log_matcher(&self, _process_id: ProcessId, _run_id: RunId, _matcher: LogMatcherIntent) {}

    /// Apply the Project's shared remaining deadline to cleanup work that a
    /// natural-exit owner can already be completing.
    fn begin_shutdown(&self, _remaining: Duration) {}

    /// Begin one Run. Report progress only as [`SeamEvent`]s on `events`;
    /// never block the caller on process work beyond cheap bookkeeping.
    fn start(&self, intent: StartIntent, events: &SeamSender);

    /// Request the complete bounded shutdown for one active Run. The
    /// adapter performs it off the control task and reports one finished-Run
    /// fact.
    fn stop(
        &self,
        process_id: ProcessId,
        run_id: RunId,
        remaining: Option<Duration>,
        events: &SeamSender,
    );
}

/// Process context needed to resolve one exec readiness command. The
/// readiness configuration stores only validated overrides; the Supervisor
/// supplies the current Process context when it dispatches an attempt.
#[derive(Clone, Debug)]
pub(crate) struct ExecContext {
    pub(crate) working_dir: PathBuf,
    pub(crate) env: Vec<(String, String)>,
    pub(crate) shell: ShellConfig,
}

/// Which health policy owns one probe request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProbeScope {
    Readiness,
    Liveness,
}

/// One request for exactly one bounded health-check attempt. The Supervisor
/// dispatches at most one attempt at a time per child; the adapter performs
/// each one off the control task and reports exactly one matching seam event.
#[derive(Clone, Debug)]
pub struct ProbeIntent {
    pub process_id: ProcessId,
    pub run_id: RunId,
    pub work_id: WorkId,
    pub attempt_id: AttemptId,
    pub probe: ReadinessProbe,
    pub timeout: Duration,
    pub(crate) exec_context: Option<ExecContext>,
    pub(crate) scope: ProbeScope,
}

/// The readiness seam. Implementations own network waits so they never run
/// on the Supervisor control task. Cancellation is logical and idempotent:
/// an adapter may still finish a bounded operation, but its result must not
/// be used after the matching work is canceled.
pub(crate) trait ProbeSeam: Send {
    fn probe(&self, intent: ProbeIntent, events: &SeamSender);

    fn cancel(&self, _process_id: ProcessId, _run_id: RunId, _work_id: WorkId) {}
}
