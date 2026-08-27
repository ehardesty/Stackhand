//! Command resolution used when the Supervisor starts a Run.

use std::time::Instant;

/// Semantic commands. Callers never mutate Supervisor state directly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Start(String),
    Stop(String),
    Restart(String),
    /// Rerun one enabled One-shot; a no-op for every other Process.
    Rerun(String),
    StartAutostart,
    StopAll,
    /// Stop the Project once, with no new lifecycle admission after this.
    Shutdown {
        deadline: Instant,
    },
}
