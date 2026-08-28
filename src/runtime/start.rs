//! The small request interface for starting one owned Run.

use std::sync::{Arc, mpsc};
use std::time::Duration;

use crate::runtime::outcome::ShutdownLadder;
use crate::runtime::pipe::RunOutputSender;
use crate::runtime::pty::SpawnCommand;

use super::run::{ProcessId, RunEvent, RunId, RunMode};

/// A low-volume observer attached to the first live output point of a Run.
/// Observers receive raw output bytes and never own the output history or
/// terminal state.
pub trait RunOutputObserver: Send + Sync {
    fn observe(&self, data: &[u8]);
}

/// Everything needed to start one Run.
pub struct RunStartRequest {
    pub process_id: ProcessId,
    pub run_id: RunId,
    pub command: SpawnCommand,
    pub mode: RunMode,
    /// Low-volume sink for Run state events. High-volume output never enters
    /// this path.
    pub events: mpsc::Sender<RunEvent>,
    /// High-volume sink for pipe-mode process output. PTY-mode Runs deliver
    /// output into their TerminalSession instead.
    pub output: RunOutputSender,
    /// The configured semantic shutdown ladder timeouts for this Run.
    pub ladder: ShutdownLadder,
    /// Aggregate Process Tree sampling interval. `None` disables sampling.
    pub metrics_interval: Option<Duration>,
    /// Optional wake called when terminal output arrives. This is the
    /// redraw-notification path for interactive hosts; it never carries
    /// output bytes.
    pub on_output_wake: Option<Box<dyn Fn() + Send + 'static>>,
    /// Optional observer attached before the child can emit output. It is
    /// used for bounded live facts such as log readiness; output bytes never
    /// enter the Run event sink through this path.
    pub output_observer: Option<Arc<dyn RunOutputObserver>>,
}
