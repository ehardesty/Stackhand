//! Lifecycle command requests queued by the console keymap. One requested
//! lifecycle command targets the currently selected Process; command modes
//! carry only the request, and the app event loop owns the selection and
//! dispatches it through the Supervisor.

/// One requested lifecycle command for the currently selected Process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleCommand {
    Start,
    Stop,
    Restart,
}

/// The lifecycle command a character key requests, if any. Keys outside
/// the lifecycle set leave the decision to the surrounding match.
pub(crate) fn lifecycle_request_for(c: char) -> Option<LifecycleCommand> {
    match c {
        's' => Some(LifecycleCommand::Start),
        'x' => Some(LifecycleCommand::Stop),
        'r' => Some(LifecycleCommand::Restart),
        _ => None,
    }
}

/// The pending lifecycle request queue shared by every key-routing mode.
pub(crate) struct LifecycleQueue {
    commands: Vec<LifecycleCommand>,
}

impl LifecycleQueue {
    pub(crate) fn new() -> Self {
        Self {
            commands: Vec::new(),
        }
    }

    pub(crate) fn queue(&mut self, command: LifecycleCommand) {
        self.commands.push(command);
    }

    /// Drain every queued request. The app event loop dispatches each one
    /// for the currently selected Process.
    pub(crate) fn take(&mut self) -> Vec<LifecycleCommand> {
        std::mem::take(&mut self.commands)
    }
}

impl Default for LifecycleQueue {
    fn default() -> Self {
        Self::new()
    }
}
