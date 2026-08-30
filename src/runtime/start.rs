//! The small request interface for starting one owned Run.

use std::sync::{Arc, mpsc};
use std::time::Duration;

use crate::geometry::TerminalGeometry;
use crate::runtime::outcome::ShutdownLadder;
use crate::runtime::pipe::RunOutputSender;
use crate::runtime::pty::SpawnCommand;

use super::run::{ProcessId, RunEvent, RunId};

/// A low-volume observer attached to the first live output point of a Run.
/// Observers receive raw output bytes and never own the output history or
/// terminal state.
pub trait RunOutputObserver: Send + Sync {
    fn observe(&self, data: &[u8]);
}

/// Transport-specific resources for one Run.
pub enum RunTransport {
    /// Non-interactive transport with separate stdout and stderr drains.
    Pipe { output: RunOutputSender },
    /// Interactive transport with terminal semantics.
    Pty {
        initial_geometry: TerminalGeometry,
        /// Optional redraw notification. It never carries output bytes.
        on_output_wake: Option<Box<dyn Fn() + Send + 'static>>,
    },
}

/// Everything needed to start one Run.
pub struct RunStartRequest {
    pub process_id: ProcessId,
    pub run_id: RunId,
    pub command: SpawnCommand,
    pub transport: RunTransport,
    /// Low-volume sink for Run state events. High-volume output never enters
    /// this path.
    pub events: mpsc::Sender<RunEvent>,
    /// The configured semantic shutdown ladder timeouts for this Run.
    pub ladder: ShutdownLadder,
    /// Aggregate Process Tree sampling interval. `None` disables sampling.
    pub metrics_interval: Option<Duration>,
    /// Optional observer attached before the child can emit output. It is
    /// used for bounded live facts such as log readiness; output bytes never
    /// enter the Run event sink through this path.
    pub output_observer: Option<Arc<dyn RunOutputObserver>>,
}
