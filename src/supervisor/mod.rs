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

use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
use crossbeam_channel::{Receiver, Select, Sender};

use crate::model::EffectiveProject;
use crate::supervisor::clock::{Clock, SystemClock};
use crate::supervisor::core::Core;
use crate::supervisor::runtime::RealRunSeam;
use crate::supervisor::seam::{RunSeam, SeamEvent, SeamSender};

mod clock;
mod core;
mod runtime;
mod seam;
#[cfg(test)]
mod support;
#[cfg(test)]
mod tests;

pub use core::{
    Command, DesiredState, FailureSummary, Lifecycle, MetricsMetadata, ProcessSnapshot,
    ProjectSnapshot,
};
pub use runtime::{ConsoleView, Consoles};

/// How long a snapshot request waits for the control task before giving up.
const SNAPSHOT_WAIT: Duration = Duration::from_secs(1);

enum Inbox {
    Command(Command),
    Snapshot(mpsc::Sender<ProjectSnapshot>),
}

/// Starts a Supervisor for one validated effective Project using the real
/// Run interface and system clock. The returned [`Consoles`] registry is
/// the data-plane view of each Process's current terminal; it carries no
/// lifecycle authority.
pub fn start(project: EffectiveProject) -> Result<(SupervisorHandle, Consoles)> {
    // Size each first PTY like the pane that will render it, so children
    // never observe a stale default size.
    let initial_geometry = crate::tui::project_console_geometry(project.processes().len());
    let seam = RealRunSeam::default();
    let consoles = seam.consoles();
    Ok((
        start_with(
            project,
            Box::new(seam),
            Box::new(SystemClock),
            initial_geometry,
        ),
        consoles,
    ))
}

/// The full seam, shared by production and tests. Tests install the private
/// scripted fake runtime and fake clock here; neither widens the external
/// interface.
pub(crate) fn start_with(
    project: EffectiveProject,
    seam: Box<dyn RunSeam>,
    clock: Box<dyn Clock>,
    initial_geometry: crate::geometry::TerminalGeometry,
) -> SupervisorHandle {
    let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded::<Inbox>();
    let (event_tx, event_rx) = crossbeam_channel::unbounded::<SeamEvent>();
    let mut core = Core::new(
        project,
        seam,
        clock,
        SeamSender::new(event_tx.clone()),
        initial_geometry,
    );
    let worker = std::thread::Builder::new()
        .name("stackhand-supervisor".to_string())
        .spawn(move || run_task(&mut core, &inbox_rx, &event_rx))
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
fn run_task(core: &mut Core, inbox: &Receiver<Inbox>, events: &Receiver<SeamEvent>) {
    loop {
        let mut select = Select::new();
        let inbox_index = select.recv(inbox);
        let _event_index = select.recv(events);
        let oper = select.select();
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

    #[allow(dead_code)] // Probe and runtime adapters deliver events from Issue #27 on.
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
