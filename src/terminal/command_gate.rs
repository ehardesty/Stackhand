use std::collections::VecDeque;
use std::sync::mpsc::Sender as CompletionSender;
use std::sync::{Arc, Mutex};

use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError};
use crossterm::event::KeyEvent;

use super::mouse::TerminalMouseEvent;
use super::selection::SelectionDirection;
use crate::byte_budget::ByteBudget;
use crate::geometry::TerminalGeometry;

pub const COMMAND_QUEUE_SLOTS: usize = 256;
pub const COMMAND_QUEUE_BYTES: usize = 256 * 1_024;
const COMMAND_EVENT_SLOTS: usize = 64;

#[derive(Debug)]
pub enum TerminalCommand {
    Key(KeyEvent),
    Focus(bool),
    Mouse(TerminalMouseEvent),
    Raw(Vec<u8>),
    Paste {
        data: Vec<u8>,
        completion: CompletionSender<Result<(), String>>,
    },
    Resize(TerminalGeometry),
    Scroll(isize),
    ScrollTo(usize),
    ScrollBatch(Arc<CoalescedScroll>),
    SelectionAll,
    SelectionClear,
    SelectionKeyboardStart,
    SelectionKeyboardToggle,
    SelectionKeyboardMove(SelectionDirection),
    SelectionText(CompletionSender<Result<Option<String>, String>>),
}

impl TerminalCommand {
    pub fn estimated_bytes(&self) -> usize {
        match self {
            // Reserve an upper bound for the encoded item, not only the Rust
            // command value. This keeps the byte bound true after encoding.
            Self::Key(_) => 64,
            Self::Focus(_) => input_focus_bytes(),
            Self::Mouse(_) => 64,
            Self::Raw(data) => data.len(),
            Self::Paste { data, .. } => data.len().saturating_add(12),
            Self::Resize(_) => 4,
            Self::Scroll(_) | Self::ScrollTo(_) | Self::ScrollBatch(_) => {
                std::mem::size_of::<isize>()
            }
            Self::SelectionAll
            | Self::SelectionClear
            | Self::SelectionKeyboardStart
            | Self::SelectionKeyboardToggle => 1,
            Self::SelectionKeyboardMove(_) => 2,
            Self::SelectionText(_) => 64,
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

/// Why a command was not admitted to the terminal owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandRejection {
    /// The terminal owner is stopping or has already stopped.
    Stopping,
    /// The bounded command queue cannot admit this complete command.
    Backpressure(CommandBackpressure),
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

#[derive(Debug)]
pub struct CoalescedScroll {
    state: Mutex<CoalescedScrollState>,
}

#[derive(Debug)]
struct CoalescedScrollState {
    delta: isize,
    open: bool,
}

impl CoalescedScroll {
    fn new(delta: isize) -> Self {
        Self {
            state: Mutex::new(CoalescedScrollState { delta, open: true }),
        }
    }

    fn add_if_open(&self, delta: isize) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.open || (state.delta != 0 && delta != 0 && state.delta.signum() != delta.signum())
        {
            return false;
        }
        state.delta = state.delta.saturating_add(delta);
        true
    }

    fn close(&self) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .open = false;
    }

    pub(super) fn take(&self) -> isize {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.open = false;
        std::mem::take(&mut state.delta)
    }
}

struct CommandSender {
    sender: Sender<TerminalCommand>,
    open_scroll: Option<Arc<CoalescedScroll>>,
}

pub struct CommandGate {
    sender: Mutex<Option<CommandSender>>,
    budget: ByteBudget,
    status: Arc<CommandStatus>,
}

pub struct CommandReceiver {
    receiver: Receiver<TerminalCommand>,
    budget: ByteBudget,
}

impl CommandGate {
    pub fn new() -> (Self, CommandReceiver) {
        let (sender, receiver) = crossbeam_channel::bounded(COMMAND_QUEUE_SLOTS);
        let budget = ByteBudget::new(COMMAND_QUEUE_BYTES);
        (
            Self {
                sender: Mutex::new(Some(CommandSender {
                    sender,
                    open_scroll: None,
                })),
                budget: budget.clone(),
                status: Arc::new(CommandStatus::default()),
            },
            CommandReceiver { receiver, budget },
        )
    }

    pub fn try_send(&self, command: TerminalCommand) -> Result<(), CommandRejection> {
        let mut guard = self
            .sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(sender) = guard.as_mut() else {
            self.status.record(CommandEvent::Failed(
                "terminal command gate is shut down".to_string(),
            ));
            return Err(CommandRejection::Stopping);
        };

        if let TerminalCommand::Scroll(delta) = command {
            if sender
                .open_scroll
                .as_ref()
                .is_some_and(|scroll| scroll.add_if_open(delta))
            {
                return Ok(());
            }
            let scroll = Arc::new(CoalescedScroll::new(delta));
            self.try_send_locked(
                &sender.sender,
                TerminalCommand::ScrollBatch(Arc::clone(&scroll)),
            )?;
            sender.open_scroll = Some(scroll);
            return Ok(());
        }

        if let Some(scroll) = sender.open_scroll.take() {
            scroll.close();
        }
        self.try_send_locked(&sender.sender, command)
    }

    fn try_send_locked(
        &self,
        sender: &Sender<TerminalCommand>,
        command: TerminalCommand,
    ) -> Result<(), CommandRejection> {
        let attempted_bytes = command.estimated_bytes();
        let reservation = self.budget.reserve(attempted_bytes).map_err(|error| {
            let error = CommandBackpressure {
                attempted_bytes,
                pending_bytes: error.pending_bytes,
                limit_bytes: COMMAND_QUEUE_BYTES,
            };
            self.status.record(CommandEvent::Backpressure(error));
            CommandRejection::Backpressure(error)
        })?;
        let pending_before = reservation.pending_before();

        match sender.try_send(command) {
            Ok(()) => {
                reservation.commit();
                Ok(())
            }
            Err(TrySendError::Full(_)) => {
                drop(reservation);
                let error = CommandBackpressure {
                    attempted_bytes,
                    pending_bytes: pending_before,
                    limit_bytes: COMMAND_QUEUE_BYTES,
                };
                self.status.record(CommandEvent::Backpressure(error));
                Err(CommandRejection::Backpressure(error))
            }
            Err(TrySendError::Disconnected(_)) => {
                drop(reservation);
                self.status.record(CommandEvent::Failed(
                    "terminal command owner is not available".to_string(),
                ));
                Err(CommandRejection::Stopping)
            }
        }
    }

    pub fn poll_event(&self) -> Option<CommandEvent> {
        self.status.take()
    }

    pub fn close(&self) {
        if let Some(mut sender) = self
            .sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            && let Some(scroll) = sender.open_scroll.take()
        {
            scroll.close();
        }
    }
}

impl CommandReceiver {
    pub fn try_recv(&self) -> Result<TerminalCommand, TryRecvError> {
        match self.receiver.try_recv()? {
            TerminalCommand::ScrollBatch(scroll) => Ok(TerminalCommand::Scroll(scroll.take())),
            command => Ok(command),
        }
    }

    pub fn complete(&self, command_bytes: usize) {
        self.budget.release(command_bytes);
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

    #[test]
    fn keyboard_selection_commands_have_precise_queue_estimates() {
        assert_eq!(TerminalCommand::SelectionKeyboardStart.estimated_bytes(), 1);
        assert_eq!(
            TerminalCommand::SelectionKeyboardToggle.estimated_bytes(),
            1
        );
        assert_eq!(
            TerminalCommand::SelectionKeyboardMove(SelectionDirection::Down).estimated_bytes(),
            2
        );
    }

    #[test]
    fn adjacent_scroll_commands_share_one_queue_slot() {
        let (gate, receiver) = CommandGate::new();
        for _ in 0..10_000 {
            gate.try_send(TerminalCommand::Scroll(-3)).unwrap();
        }
        gate.try_send(TerminalCommand::SelectionAll).unwrap();

        assert!(matches!(
            receiver.try_recv(),
            Ok(TerminalCommand::Scroll(-30_000))
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(TerminalCommand::SelectionAll)
        ));
    }

    #[test]
    fn opposite_scroll_directions_keep_their_order_at_viewport_bounds() {
        let (gate, receiver) = CommandGate::new();
        gate.try_send(TerminalCommand::Scroll(1_000_000)).unwrap();
        gate.try_send(TerminalCommand::Scroll(-3)).unwrap();

        assert!(matches!(
            receiver.try_recv(),
            Ok(TerminalCommand::Scroll(1_000_000))
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(TerminalCommand::Scroll(-3))
        ));
    }

    #[test]
    fn non_scroll_commands_keep_scroll_batches_in_event_order() {
        let (gate, receiver) = CommandGate::new();
        gate.try_send(TerminalCommand::Scroll(-3)).unwrap();
        gate.try_send(TerminalCommand::SelectionAll).unwrap();
        gate.try_send(TerminalCommand::Scroll(3)).unwrap();

        assert!(matches!(
            receiver.try_recv(),
            Ok(TerminalCommand::Scroll(-3))
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(TerminalCommand::SelectionAll)
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(TerminalCommand::Scroll(3))
        ));
    }

    #[test]
    fn slot_rejection_releases_its_tentative_byte_reservation() {
        let (gate, receiver) = CommandGate::new();
        for _ in 0..COMMAND_QUEUE_SLOTS {
            gate.try_send(TerminalCommand::SelectionAll).unwrap();
        }

        assert_eq!(
            gate.try_send(TerminalCommand::SelectionAll),
            Err(CommandRejection::Backpressure(CommandBackpressure {
                attempted_bytes: 1,
                pending_bytes: COMMAND_QUEUE_SLOTS,
                limit_bytes: COMMAND_QUEUE_BYTES,
            }))
        );

        let command = receiver.try_recv().unwrap();
        receiver.complete(command.estimated_bytes());
        assert!(gate.try_send(TerminalCommand::SelectionAll).is_ok());
    }
}
