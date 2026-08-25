use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender as CompletionSender;
use std::sync::{Arc, Mutex};

use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError};
use crossterm::event::KeyEvent;

use crate::geometry::TerminalGeometry;

pub const COMMAND_QUEUE_SLOTS: usize = 256;
pub const COMMAND_QUEUE_BYTES: usize = 256 * 1_024;
const COMMAND_EVENT_SLOTS: usize = 64;

#[derive(Debug)]
pub enum TerminalCommand {
    Key(KeyEvent),
    Focus(bool),
    Raw(Vec<u8>),
    Paste {
        data: Vec<u8>,
        completion: CompletionSender<Result<(), String>>,
    },
    Resize(TerminalGeometry),
    Scroll(isize),
}

impl TerminalCommand {
    pub fn estimated_bytes(&self) -> usize {
        match self {
            // Reserve an upper bound for the encoded item, not only the Rust
            // command value. This keeps the byte bound true after encoding.
            Self::Key(_) => 64,
            Self::Focus(_) => input_focus_bytes(),
            Self::Raw(data) => data.len(),
            Self::Paste { data, .. } => data.len().saturating_add(12),
            Self::Resize(_) => 4,
            Self::Scroll(_) => std::mem::size_of::<isize>(),
        }
    }
}

const fn input_focus_bytes() -> usize {
    b"\x1b[I".len()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandBackpressure {
    pub attempted_bytes: usize,
    pub pending_bytes: usize,
    pub limit_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandEvent {
    Backpressure(CommandBackpressure),
    Failed(String),
}

#[derive(Default)]
struct CommandStatus {
    events: Mutex<VecDeque<CommandEvent>>,
}

impl CommandStatus {
    fn record(&self, event: CommandEvent) {
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if events.len() < COMMAND_EVENT_SLOTS {
            events.push_back(event);
        } else if matches!(event, CommandEvent::Failed(_))
            && !events
                .iter()
                .any(|existing| matches!(existing, CommandEvent::Failed(_)))
        {
            events.pop_front();
            events.push_back(event);
        }
    }

    fn take(&self) -> Option<CommandEvent> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop_front()
    }
}

pub struct CommandGate {
    sender: Mutex<Option<Sender<TerminalCommand>>>,
    pending_bytes: Arc<AtomicUsize>,
    status: Arc<CommandStatus>,
}

pub struct CommandReceiver {
    receiver: Receiver<TerminalCommand>,
    pending_bytes: Arc<AtomicUsize>,
}

impl CommandGate {
    pub fn new() -> (Self, CommandReceiver) {
        let (sender, receiver) = crossbeam_channel::bounded(COMMAND_QUEUE_SLOTS);
        let pending_bytes = Arc::new(AtomicUsize::new(0));
        (
            Self {
                sender: Mutex::new(Some(sender)),
                pending_bytes: Arc::clone(&pending_bytes),
                status: Arc::new(CommandStatus::default()),
            },
            CommandReceiver {
                receiver,
                pending_bytes,
            },
        )
    }

    pub fn try_send(&self, command: TerminalCommand) -> Result<(), CommandBackpressure> {
        let attempted_bytes = command.estimated_bytes();
        let reservation =
            reserve_bytes(&self.pending_bytes, attempted_bytes).map_err(|pending| {
                let error = CommandBackpressure {
                    attempted_bytes,
                    pending_bytes: pending,
                    limit_bytes: COMMAND_QUEUE_BYTES,
                };
                self.status.record(CommandEvent::Backpressure(error));
                error
            })?;

        let result = self
            .sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map(|sender| sender.try_send(command));
        match result {
            Some(Ok(())) => Ok(()),
            Some(Err(TrySendError::Full(command))) => {
                self.pending_bytes
                    .fetch_sub(command.estimated_bytes(), Ordering::AcqRel);
                let error = CommandBackpressure {
                    attempted_bytes,
                    pending_bytes: reservation,
                    limit_bytes: COMMAND_QUEUE_BYTES,
                };
                self.status.record(CommandEvent::Backpressure(error));
                Err(error)
            }
            Some(Err(TrySendError::Disconnected(command))) => {
                self.pending_bytes
                    .fetch_sub(command.estimated_bytes(), Ordering::AcqRel);
                self.status.record(CommandEvent::Failed(
                    "terminal command owner is not available".to_string(),
                ));
                Err(CommandBackpressure {
                    attempted_bytes,
                    pending_bytes: reservation,
                    limit_bytes: COMMAND_QUEUE_BYTES,
                })
            }
            None => {
                self.pending_bytes
                    .fetch_sub(attempted_bytes, Ordering::AcqRel);
                self.status.record(CommandEvent::Failed(
                    "terminal command gate is shut down".to_string(),
                ));
                Err(CommandBackpressure {
                    attempted_bytes,
                    pending_bytes: reservation,
                    limit_bytes: COMMAND_QUEUE_BYTES,
                })
            }
        }
    }

    pub fn poll_event(&self) -> Option<CommandEvent> {
        self.status.take()
    }

    pub fn close(&self) {
        self.sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
    }
}

impl CommandReceiver {
    pub fn try_recv(&self) -> Result<TerminalCommand, TryRecvError> {
        self.receiver.try_recv()
    }

    pub fn complete(&self, command_bytes: usize) {
        self.pending_bytes
            .fetch_sub(command_bytes, Ordering::AcqRel);
    }
}

fn reserve_bytes(pending: &AtomicUsize, amount: usize) -> Result<usize, usize> {
    let mut current = pending.load(Ordering::Acquire);
    loop {
        let Some(next) = current.checked_add(amount) else {
            return Err(current);
        };
        if next > COMMAND_QUEUE_BYTES {
            return Err(current);
        }
        match pending.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Ok(current),
            Err(actual) => current = actual,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservation_stays_live_until_the_owner_completes_the_command() {
        let (gate, receiver) = CommandGate::new();
        gate.try_send(TerminalCommand::Raw(vec![0; COMMAND_QUEUE_BYTES]))
            .unwrap();
        let command = receiver.try_recv().unwrap();

        assert!(gate.try_send(TerminalCommand::Raw(vec![1])).is_err());
        receiver.complete(command.estimated_bytes());
        assert!(gate.try_send(TerminalCommand::Raw(vec![1])).is_ok());
    }

    #[test]
    fn oversized_command_is_rejected_without_partial_admission() {
        let (gate, receiver) = CommandGate::new();
        assert!(
            gate.try_send(TerminalCommand::Raw(vec![0; COMMAND_QUEUE_BYTES + 1]))
                .is_err()
        );
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    }
}
