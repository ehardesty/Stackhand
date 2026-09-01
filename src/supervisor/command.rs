//! Command resolution used when the Supervisor starts a Run.

use std::time::Instant;

/// Semantic commands. Callers never mutate Supervisor state directly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Start(String),
    /// Start one Waiting Process without requiring its Dependencies. This
    /// affects only the next Run and does not change the Project.
    StartAnyway(String),
    Stop(String),
    Restart(String),
    /// Rerun one enabled One-shot; a no-op for every other Process.
    Rerun(String),
    StartAutostart,
    /// Select the next Project Profile without changing current Runs.
    SelectNextProcessProfile,
    /// Select one exact Project Profile without changing current Runs. `None`
    /// selects the base Project Profile.
    SelectProjectProfile(Option<String>),
    /// Apply pending profile changes to active Processes. Stop Processes that
    /// become disabled and restart affected enabled autostart Processes.
    RestartProfiledAutostart,
    StopAll,
    /// Stop the Project once, with no new lifecycle admission after this.
    Shutdown {
        deadline: Instant,
    },
}
