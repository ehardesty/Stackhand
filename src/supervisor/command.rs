//! Command resolution used when the Supervisor starts a Run.

use std::ffi::{OsStr, OsString};
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

/// Use `$SHELL` when present and `/bin/sh` otherwise.
pub(super) fn shell_program(shell_env: Option<&OsStr>) -> OsString {
    shell_env.map_or_else(|| OsString::from("/bin/sh"), OsString::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_resolution_prefers_the_environment_and_falls_back() {
        assert_eq!(
            shell_program(Some(OsStr::new("/bin/zsh"))),
            OsString::from("/bin/zsh")
        );
        assert_eq!(shell_program(None), OsString::from("/bin/sh"));
    }
}
