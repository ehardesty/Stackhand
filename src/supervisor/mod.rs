//! One deep Supervisor module that owns the authoritative mutable Project
//! state: Desired State, lifecycle state, current Run identity, failure
//! summaries, and metrics metadata.
//!
//! Callers send semantic [`Command`]s and read immutable
//! [`ProjectSnapshot`]s. They never mutate lifecycle state directly. One
//! Supervisor task serializes commands and control-plane events; process
//! work, network readiness work, output bytes, and rendering never run on
//! that task.
//!
//! The primary test seam is this interface plus the private scripted fake
//! runtime and fake clock (`support`), never internal state-machine fields.

use std::sync::{Arc, mpsc};
use std::time::Duration;

use anyhow::Result;
use crossbeam_channel::{Receiver, Select, SelectTimeoutError, Sender};

use crate::model::EffectiveProject;
use crate::supervisor::clock::{Clock, SystemClock};
use crate::supervisor::core::Core;
use crate::supervisor::probe::RealProbes;
use crate::supervisor::runtime::RealRunSeam;
use crate::supervisor::seam::{ProbeSeam, RunSeam, SeamEvent, SeamSender};

mod clock;
mod command;
mod core;
mod probe;
mod runtime;
mod schedule;
mod seam;
mod shutdown;
mod snapshot;
#[cfg(test)]
mod support;
#[cfg(test)]
mod tests;

pub use crate::runtime::ProcessId;
pub use command::Command;
pub use core::{
    DesiredState, FailureKind, FailureSummary, Lifecycle, MetricsMetadata, RECENT_RUNS,
    RunExitDisposition, RunSummary, RunTrigger,
};
pub use runtime::{ConsoleView, Consoles};
pub use shutdown::{ProcessShutdownFailure, ProjectShutdownSnapshot};
pub use snapshot::{ProcessSnapshot, ProjectSnapshot, ReadinessStatus};
// The data-plane retained-output view is built alongside the Supervisor, so
// its public types are reachable through this entry point.
pub use crate::output::{
    OutputViews, ProcessOutput, RETAINED_BYTES, RETAINED_CHUNKS, RetainedChunk, RetainedOutput,
};

/// How long a snapshot request waits for the control task before giving up.
const SNAPSHOT_WAIT: Duration = Duration::from_secs(1);

enum Inbox {
    Command(Command),
    Snapshot(mpsc::Sender<ProjectSnapshot>),
}

/// Starts a Supervisor for one validated effective Project using the real
/// Run interface and system clock. The returned [`Consoles`] registry is the
/// data-plane view of each Process's current terminal, and the returned
/// [`OutputViews`] registry is the data-plane view of each Process's
/// retained output; neither carries lifecycle authority.
pub fn start(project: EffectiveProject) -> Result<(SupervisorHandle, Consoles, Arc<OutputViews>)> {
    // Size each first PTY like the pane that will render it, so children
    // never observe a stale default size.
    let initial_geometry = crate::tui::project_console_geometry(project.processes().len());
    let outputs = Arc::new(OutputViews::new(project.processes().len()));
    let seam = RealRunSeam::new(Arc::clone(&outputs));
    let consoles = seam.consoles();
    Ok((
        start_with(
            project,
            Box::new(seam),
            Box::new(RealProbes),
            Arc::new(SystemClock),
            initial_geometry,
        ),
        consoles,
        outputs,
    ))
}

/// The full seam, shared by production and tests. Tests install the private
/// scripted fake runtime, fake probes, and fake clock here; neither adapter
/// widens the external interface.
pub(crate) fn start_with(
    project: EffectiveProject,
    seam: Box<dyn RunSeam>,
    probes: Box<dyn ProbeSeam>,
    clock: Arc<dyn Clock>,
    initial_geometry: crate::geometry::TerminalGeometry,
) -> SupervisorHandle {
    let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded::<Inbox>();
    let (event_tx, event_rx) = crossbeam_channel::unbounded::<SeamEvent>();
    let mut core = Core::new(
        project,
        seam,
        probes,
        Arc::clone(&clock),
        SeamSender::new(event_tx.clone()),
        initial_geometry,
    );
    let worker = std::thread::Builder::new()
        .name("stackhand-supervisor".to_string())
        .spawn(move || run_task(&mut core, &inbox_rx, &event_rx, clock))
        .expect("supervisor task spawns");
    SupervisorHandle {
        inbox: inbox_tx,
        events: event_tx,
        worker: Some(worker),
    }
}

/// Serialize commands and control-plane events onto one task. The core owns
/// a `SeamSender`, so the event channel stays connected for the whole loop;
/// the task ends when every caller drops its handle.
///
/// Between messages the loop waits at most until the next readiness attempt
/// becomes due, then polls the core's timers. Readiness work itself always
/// runs on probe-adapter threads, never here.
fn run_task(
    core: &mut Core,
    inbox: &Receiver<Inbox>,
    events: &Receiver<SeamEvent>,
    clock: Arc<dyn Clock>,
) {
    loop {
        let mut select = Select::new();
        let inbox_index = select.recv(inbox);
        let _event_index = select.recv(events);
        let oper = match core.time_until_next_timer() {
            Some(wait) => match select.select_timeout(wait) {
                Ok(oper) => oper,
                Err(SelectTimeoutError) => {
                    core.poll_timers(clock.now());
                    continue;
                }
            },
            None => select.select(),
        };
        if oper.index() == inbox_index {
            match oper.recv(inbox) {
                Ok(Inbox::Command(command)) => core.command(command),
                Ok(Inbox::Snapshot(reply)) => {
                    let _ = reply.send(core.snapshot());
                }
                Err(_) => return,
            }
        } else if let Ok(event) = oper.recv(events) {
            core.event(event);
        }
    }
}

/// The caller-facing Supervisor handle. Commands are fire-and-forget;
/// snapshots are bounded requests against the serialized task.
pub struct SupervisorHandle {
    inbox: Sender<Inbox>,
    events: Sender<SeamEvent>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl SupervisorHandle {
    /// Send one semantic command without waiting for its effect. Observe
    /// results through [`Self::snapshot`].
    pub fn command(&self, command: Command) {
        let _ = self.inbox.send(Inbox::Command(command));
    }

    /// Receive one immutable Project snapshot. Returns `None` only when the
    /// Supervisor task has stopped or cannot service the request in time.
    pub fn snapshot(&self) -> Option<ProjectSnapshot> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox.send(Inbox::Snapshot(reply_tx)).ok()?;
        reply_rx.recv_timeout(SNAPSHOT_WAIT).ok()
    }

    /// Deliver one typed seam event from an adapter thread; exercised by
    /// the threaded wrapper test.
    #[allow(dead_code)]
    pub(crate) fn deliver_event(&self, event: SeamEvent) {
        let _ = self.events.send(event);
    }

    /// Stop the control task and wait for it to finish. Active Runs fall
    /// back to their own best-effort drop cleanup; the full controlled
    /// Project shutdown arrives with Issue #34.
    pub fn stop_task(self) {
        let SupervisorHandle {
            inbox,
            events,
            worker,
        } = self;
        drop(inbox);
        drop(events);
        if let Some(worker) = worker {
            let _ = worker.join();
        }
    }
}
