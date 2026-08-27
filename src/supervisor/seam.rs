//! Private seams between the Supervisor control task and the work that must
//! never run on it: process execution, bounded shutdown, readiness probes,
//! and time.
//!
//! These types are `pub(crate)`. Production adapters wrap the existing Run
//! interface and real sockets; tests install scripted fakes. Neither adapter
//! widens the external Supervisor interface.

use crossbeam_channel::Sender;
use std::time::Duration;

use crate::geometry::TerminalGeometry;
use crate::model::ReadinessProbe;
use crate::runtime::{OsPid, ProcessId, RunId};
use crate::supervisor::FailureKind;

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
    /// The root process exited with the reported code, when known.
    Exited {
        process_id: ProcessId,
        run_id: RunId,
        code: Option<i32>,
    },
    /// Bounded cleanup for this Run finished. The Run is over either way.
    ShutdownComplete {
        process_id: ProcessId,
        run_id: RunId,
        confirmed: bool,
        detail: Option<String>,
        remaining_pids: Vec<OsPid>,
    },
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
        passing: bool,
        diagnostic: Option<String>,
    },
}

impl SeamEvent {
    pub(crate) fn process_id(&self) -> ProcessId {
        match self {
            Self::Spawned { process_id, .. }
            | Self::Exited { process_id, .. }
            | Self::ShutdownComplete { process_id, .. }
            | Self::Failed { process_id, .. }
            | Self::OutputFailure { process_id, .. }
            | Self::Metrics { process_id, .. }
            | Self::Readiness { process_id, .. } => *process_id,
        }
    }

    pub(crate) fn run_id(&self) -> RunId {
        match self {
            Self::Spawned { run_id, .. }
            | Self::Exited { run_id, .. }
            | Self::ShutdownComplete { run_id, .. }
            | Self::Failed { run_id, .. }
            | Self::OutputFailure { run_id, .. }
            | Self::Metrics { run_id, .. }
            | Self::Readiness { run_id, .. } => *run_id,
        }
    }
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
}

/// The runtime seam. Implementations own every Run they start until stop.
pub(crate) trait RunSeam: Send {
    /// Apply the Project's shared remaining deadline to cleanup work that a
    /// natural-exit owner can already be completing.
    fn begin_shutdown(&self, _remaining: Duration) {}

    /// Begin one Run. Report progress only as [`SeamEvent`]s on `events`;
    /// never block the caller on process work beyond cheap bookkeeping.
    fn start(&self, intent: StartIntent, events: &SeamSender);

    /// Perform the complete bounded shutdown for one active Run off the
    /// control task, then report one [`SeamEvent::ShutdownComplete`].
    fn stop(
        &self,
        process_id: ProcessId,
        run_id: RunId,
        remaining: Option<Duration>,
        events: &SeamSender,
    );
}

/// One request for exactly one bounded readiness attempt. The Supervisor
/// dispatches attempts one at a time per Run; the adapter performs each one
/// off the control task and reports exactly one
/// [`SeamEvent::Readiness`] for these identities.
#[derive(Clone, Debug)]
pub struct ProbeIntent {
    pub process_id: ProcessId,
    pub run_id: RunId,
    pub probe: ReadinessProbe,
    pub timeout: Duration,
}

/// The readiness seam. Implementations own network waits so they never run
/// on the Supervisor control task.
pub(crate) trait ProbeSeam: Send {
    fn probe(&self, intent: ProbeIntent, events: &SeamSender);
}
