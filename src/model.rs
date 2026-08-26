//! The effective Project model.
//!
//! Configuration (Milestone 1: YAML version 1) parses and validates into one
//! [`EffectiveProject`]. The Supervisor consumes that validated Project and
//! never sees YAML text or diagnostics.

use std::ffi::OsString;
use std::path::PathBuf;

/// How a Process expects to run. A Process is exactly one of these kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessKind {
    /// A Process that stays active until it is stopped.
    Service,
    /// A Process that runs to completion and exits.
    OneShot,
}

/// What the Supervisor should do with a Process when the Project starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Autostart {
    /// Start the Process when the Project starts.
    Yes,
    /// Leave the Process stopped; it remains available for a manual start.
    No,
}

/// Whether configuration enables a Process at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Enabled {
    Yes,
    /// The Process stays visible but cannot start.
    No,
}

/// Exactly one command form for one Process. The two forms are mutually
/// exclusive.
#[derive(Clone, Debug)]
pub enum CommandForm {
    /// Run `program` with `args` directly, without shell parsing.
    Direct {
        program: OsString,
        args: Vec<OsString>,
    },
    /// Run `text` through the user's shell.
    Shell { text: String },
}

/// The terminal transport for one Process's Runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalMode {
    /// Separate stdout and stderr pipes without terminal semantics.
    Pipe,
    /// A pseudo-terminal owned by each Run.
    Pty,
}

/// Whether a Process may receive keyboard input from Stackhand.
///
/// Terminal allocation and input policy stay separate: a PTY-mode Process
/// can have colors without receiving keys.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InputPolicy {
    /// Never deliver child input to this Process.
    #[default]
    Disabled,
    /// Deliver input only while this Process is selected and focused.
    Focused,
}

/// One configured Process as it will actually run.
#[derive(Clone, Debug)]
pub struct ProcessSpec {
    pub name: String,
    pub kind: ProcessKind,
    pub enabled: Enabled,
    pub autostart: Autostart,
    pub command: CommandForm,
    /// Absolute working directory. Relative paths are resolved by
    /// configuration before this model exists.
    pub working_dir: PathBuf,
    pub env: Vec<(String, String)>,
    pub terminal_mode: TerminalMode,
    /// Consumed by TUI input routing (Issue #30); configuration validation
    /// covers it in Issue #23.
    #[allow(dead_code)]
    pub input_policy: InputPolicy,
}

/// Why one effective Project could not be built.
#[derive(Debug, PartialEq, Eq)]
pub enum ProjectError {
    DuplicateName(String),
}

/// The validated set of Processes for one Stackhand session. Process order
/// is configuration order and stays stable for the session.
#[derive(Clone, Debug, Default)]
pub struct EffectiveProject {
    processes: Vec<ProcessSpec>,
}

impl EffectiveProject {
    /// Build one Project, rejecting duplicate Process names before any
    /// Process can start.
    pub fn new(processes: Vec<ProcessSpec>) -> Result<Self, ProjectError> {
        let mut seen = std::collections::HashSet::new();
        for spec in &processes {
            if !seen.insert(spec.name.as_str()) {
                return Err(ProjectError::DuplicateName(spec.name.clone()));
            }
        }
        Ok(Self { processes })
    }

    pub fn processes(&self) -> &[ProcessSpec] {
        &self.processes
    }
}
