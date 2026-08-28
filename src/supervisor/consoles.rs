//! Data-plane access to terminal sessions owned by active Runs.
//!
//! Console access shares the runtime adapter's Run registry, but it never
//! stores lifecycle policy or sends control-plane events.

use std::sync::Arc;

use crate::geometry::TerminalGeometry;
use crate::runtime::{ProcessId, TerminalHandle};
use crate::terminal::{OwnedTerminalSnapshot, TerminalEvent};

use super::run_record::{RunRecord, RunRegistry};

/// Data-plane access to the terminal session each PTY Run owns. Output bytes
/// and terminal interaction stay outside the Supervisor control queue.
pub struct Consoles {
    pub(super) runs: RunRegistry,
}

impl Consoles {
    /// The live console view for one Process's current Run, when one is
    /// active.
    pub fn view_process(&self, process_id: ProcessId, run_id: u64) -> Option<ConsoleView> {
        let record = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&(process_id.get(), run_id))
            .cloned()?;
        record.is_active().then_some(ConsoleView { record })
    }

    /// The live console view by scalar Process identity.
    /// Kept for caller compatibility; internal callers use [`Self::view_process`].
    pub fn view(&self, process_id: u32, run_id: u64) -> Option<ConsoleView> {
        self.view_process(ProcessId::new(process_id), run_id)
    }
}

/// A shared handle to one active PTY Run's terminal. Every operation locks
/// one Run coordinator briefly and never performs process work.
pub struct ConsoleView {
    record: Arc<RunRecord>,
}

impl ConsoleView {
    pub(crate) fn with<R>(&self, f: impl FnOnce(&TerminalHandle<'_>) -> R) -> Option<R> {
        self.record.with_terminal(f)
    }

    pub fn snapshot(&self) -> Option<OwnedTerminalSnapshot> {
        self.with(|handle| handle.snapshot())
    }

    pub fn is_dirty(&self) -> bool {
        self.with(|handle| handle.is_dirty()).unwrap_or(false)
    }

    pub fn mouse_tracking(&self) -> bool {
        self.with(|handle| handle.mouse_tracking()).unwrap_or(false)
    }

    pub fn poll_event(&self) -> Option<TerminalEvent> {
        self.with(|handle| handle.poll_event())?
    }

    /// Resize to the selected console geometry. Returns false when the Run
    /// rejected the request (stopping or backpressure); non-fatal either way.
    pub fn resize(&self, geometry: TerminalGeometry) -> bool {
        let result = self.with(|handle| handle.resize(geometry));
        !matches!(result, Some(Err(_)))
    }
}
