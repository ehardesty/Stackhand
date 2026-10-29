use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossterm::event::KeyEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::Position;

use super::command_gate::{CommandEvent, CommandGate, CommandRejection, TerminalCommand};
use super::metrics::TerminalMetrics;
use super::mouse::{TerminalMouseEvent, stackhand_wheel_delta};
use super::owner::{OwnerEvent, OwnerHandle};
use super::paste::{self, PasteRejection, PasteRequest};
pub use super::selection::{SelectionDirection, SelectionPoint};
use crate::geometry::TerminalGeometry;
use crate::runtime::{
    PtyIo, PtyWriterEvent, PtyWriterOwner, RunOutputObserver, spawn_bounded_pty_writer,
};

const SESSION_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
pub const INPUT_QUEUE_LIMIT_BYTES: usize = 256 * 1_024;
pub const SCROLLBACK_TARGET_BYTES: usize = 16 * 1_024 * 1_024;
const MAX_SAFE_SCROLL_DELTA: isize = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnedTerminalScrollbar {
    /// Total rows in the active terminal screen and its retained scrollback.
    pub total: usize,
    /// First visible row in the total row space.
    pub offset: usize,
    /// Number of visible rows in the terminal viewport.
    pub len: usize,
}

impl OwnedTerminalScrollbar {
    pub const fn new(viewport_rows: u16) -> Self {
        Self {
            total: viewport_rows as usize,
            offset: 0,
            len: viewport_rows as usize,
        }
    }
}

#[derive(Clone, Debug)]
pub struct OwnedTerminalSnapshot {
    pub buffer: Buffer,
    pub cursor: Option<OwnedCursorState>,
    pub mouse_tracking: bool,
    pub scrollbar: OwnedTerminalScrollbar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnedCursorState {
    pub position: Position,
    pub shape: CursorShape,
    pub blinking: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Bar,
    Underline,
}

#[derive(Debug)]
pub enum TerminalEvent {
    Exited,
    Failed(String),
    InputBackpressure {
        attempted_bytes: usize,
        pending_bytes: usize,
        limit_bytes: usize,
    },
    StateChanged,
}

/// Why an interactive input item was not admitted. The caller receives this
/// result immediately; no key, focus, mouse, or raw item is silently dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputRejection {
    /// The command owner is stopping or has already stopped.
    Stopping,
    /// The bounded command queue cannot admit the complete item.
    Backpressure {
        attempted_bytes: usize,
        pending_bytes: usize,
        limit_bytes: usize,
    },
}

impl From<CommandRejection> for InputRejection {
    fn from(rejection: CommandRejection) -> Self {
        match rejection {
            CommandRejection::Stopping => Self::Stopping,
            CommandRejection::Backpressure(error) => Self::Backpressure {
                attempted_bytes: error.attempted_bytes,
                pending_bytes: error.pending_bytes,
                limit_bytes: error.limit_bytes,
            },
        }
    }
}

impl OwnedTerminalSnapshot {
    pub fn text(&self) -> String {
        let area = self.buffer.area();
        let mut lines = Vec::with_capacity(usize::from(area.height));
        for y in area.top()..area.bottom() {
            let mut line = String::new();
            for x in area.left()..area.right() {
                line.push_str(self.buffer[(x, y)].symbol());
            }
            lines.push(line.trim_end().to_string());
        }
        lines.join("\n").trim_end().to_string()
    }
}

pub struct TerminalSession {
    owner: OwnerHandle,
    writer: PtyWriterOwner,
    commands: CommandGate,
    next_paste_request_id: AtomicU64,
    shutdown_complete: Mutex<bool>,
}

pub struct CopyRequest {
    receiver: mpsc::Receiver<Result<Option<String>, String>>,
}

impl CopyRequest {
    pub fn poll(&self) -> Option<Result<Option<String>, String>> {
        match self.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err(
                "terminal owner stopped before copy completed".to_string(),
            )),
        }
    }
}

impl TerminalSession {
    #[allow(dead_code)]
    pub fn spawn(
        io: PtyIo,
        geometry: TerminalGeometry,
        wake: impl Fn() + Send + 'static,
    ) -> Result<Self> {
        Self::spawn_with_observer(io, geometry, wake, None)
    }

    pub(crate) fn spawn_with_observer(
        io: PtyIo,
        geometry: TerminalGeometry,
        wake: impl Fn() + Send + 'static,
        observer: Option<Arc<dyn RunOutputObserver>>,
    ) -> Result<Self> {
        Self::spawn_configured(io, geometry, wake, observer, SCROLLBACK_TARGET_BYTES)
    }

    pub(crate) fn spawn_configured(
        io: PtyIo,
        geometry: TerminalGeometry,
        wake: impl Fn() + Send + 'static,
        observer: Option<Arc<dyn RunOutputObserver>>,
        scrollback_bytes: usize,
    ) -> Result<Self> {
        let (writer, writer_owner) = spawn_bounded_pty_writer(io.writer, INPUT_QUEUE_LIMIT_BYTES)
            .context("could not start the bounded PTY writer")?;
        let (commands, command_receiver) = CommandGate::new();
        let owner = OwnerHandle::spawn(
            io.reader,
            io.resizer,
            writer,
            command_receiver,
            geometry,
            wake,
            observer,
            scrollback_bytes,
        )?;
        Ok(Self {
            owner,
            writer: writer_owner,
            commands,
            next_paste_request_id: AtomicU64::new(1),
            shutdown_complete: Mutex::new(false),
        })
    }

    pub fn send_key(&self, event: KeyEvent) -> Result<(), InputRejection> {
        self.commands
            .try_send(TerminalCommand::Key(event))
            .map_err(Into::into)
    }

    pub fn send_focus(&self, gained: bool) -> Result<(), InputRejection> {
        self.commands
            .try_send(TerminalCommand::Focus(gained))
            .map_err(Into::into)
    }

    pub fn send_mouse(&self, event: TerminalMouseEvent) -> Result<(), InputRejection> {
        let command = stackhand_wheel_delta(event)
            .map(TerminalCommand::Scroll)
            .unwrap_or(TerminalCommand::Mouse(event));
        self.commands.try_send(command).map_err(Into::into)
    }

    pub fn send_raw(&self, data: Vec<u8>) -> Result<(), InputRejection> {
        self.commands
            .try_send(TerminalCommand::Raw(data))
            .map_err(Into::into)
    }

    /// Admit one whole paste to the bounded terminal owner.
    ///
    /// The returned token acknowledges bounded command admission only. Poll
    /// it for request-specific PTY delivery or failure. Saturation rejects the
    /// complete paste before admission. This call does not wait for delivery.
    pub fn send_paste(&self, data: &str) -> Result<PasteRequest, PasteRejection> {
        paste::validate(data)?;
        let request_id = self.next_paste_request_id.fetch_add(1, Ordering::Relaxed);
        let (completion_tx, completion_rx) = mpsc::channel();
        match self.commands.try_send(TerminalCommand::Paste {
            data: data.as_bytes().to_vec(),
            completion: completion_tx,
        }) {
            Ok(()) => {}
            Err(CommandRejection::Stopping) => return Err(PasteRejection::Stopping),
            Err(CommandRejection::Backpressure(error)) => {
                return Err(PasteRejection::Busy {
                    attempted_bytes: error.attempted_bytes,
                    pending_bytes: error.pending_bytes,
                    limit_bytes: error.limit_bytes,
                });
            }
        }
        Ok(PasteRequest::new(request_id, completion_rx))
    }

    pub fn resize(&self, geometry: TerminalGeometry) -> Result<(), InputRejection> {
        self.commands
            .try_send(TerminalCommand::Resize(geometry))
            .map_err(Into::into)
    }

    pub fn scroll_lines(&self, delta: isize) {
        let _ = self
            .commands
            .try_send(TerminalCommand::Scroll(bounded_scroll_delta(delta)));
    }

    pub fn scroll_to_row(&self, row: usize) {
        let _ = self.commands.try_send(TerminalCommand::ScrollTo(row));
    }

    pub fn follow_live(&self) {
        let _ = self
            .commands
            .try_send(TerminalCommand::Scroll(MAX_SAFE_SCROLL_DELTA));
    }

    pub fn select_all(&self) {
        let _ = self.commands.try_send(TerminalCommand::SelectionAll);
    }

    pub fn clear_selection(&self) {
        let _ = self.commands.try_send(TerminalCommand::SelectionClear);
    }

    /// Show the terminal-owned keyboard copy cursor. This never writes to the child.
    pub fn start_keyboard_selection(&self) {
        let _ = self
            .commands
            .try_send(TerminalCommand::SelectionKeyboardStart);
    }

    /// Toggle whether keyboard movement extends the terminal selection.
    pub fn toggle_keyboard_selection(&self) {
        let _ = self
            .commands
            .try_send(TerminalCommand::SelectionKeyboardToggle);
    }

    /// Move the terminal-owned copy cursor or active selection endpoint.
    pub fn move_keyboard_selection(&self, direction: SelectionDirection) {
        let _ = self
            .commands
            .try_send(TerminalCommand::SelectionKeyboardMove(direction));
    }

    pub fn request_copy(&self) -> CopyRequest {
        let (sender, receiver) = mpsc::channel();
        if self
            .commands
            .try_send(TerminalCommand::SelectionText(sender.clone()))
            .is_err()
        {
            let _ = sender.send(Err("terminal command queue is full".to_string()));
        }
        CopyRequest { receiver }
    }

    pub fn snapshot(&self) -> OwnedTerminalSnapshot {
        let render = self.owner.render();
        OwnedTerminalSnapshot {
            buffer: render.buffer,
            cursor: render.cursor,
            mouse_tracking: render.mouse_tracking,
            scrollbar: render.scrollbar,
        }
    }

    pub fn scrollbar(&self) -> OwnedTerminalScrollbar {
        self.owner.scrollbar()
    }

    pub fn is_dirty(&self) -> bool {
        self.owner.is_dirty()
    }

    pub fn mouse_tracking(&self) -> bool {
        self.owner.mouse_tracking()
    }

    pub fn metrics(&self) -> TerminalMetrics {
        self.owner.metrics()
    }

    pub fn poll_event(&self) -> Option<TerminalEvent> {
        if let Some(event) = self.commands.poll_event() {
            return Some(match event {
                CommandEvent::Backpressure(error) => TerminalEvent::InputBackpressure {
                    attempted_bytes: error.attempted_bytes,
                    pending_bytes: error.pending_bytes,
                    limit_bytes: error.limit_bytes,
                },
                CommandEvent::Failed(error) => TerminalEvent::Failed(error),
            });
        }
        if let Some(event) = self.writer.poll_event() {
            return Some(match event {
                PtyWriterEvent::Backpressure {
                    attempted_bytes,
                    pending_bytes,
                    limit_bytes,
                } => TerminalEvent::InputBackpressure {
                    attempted_bytes,
                    pending_bytes,
                    limit_bytes,
                },
                PtyWriterEvent::Failed(error) => {
                    TerminalEvent::Failed(format!("PTY writer failed: {error}"))
                }
            });
        }
        self.owner.poll_event().map(|event| match event {
            OwnerEvent::Exited => TerminalEvent::Exited,
            OwnerEvent::Failed(error) => TerminalEvent::Failed(error),
            OwnerEvent::StateChanged => TerminalEvent::StateChanged,
        })
    }

    pub fn shutdown(&self) -> Result<()> {
        self.shutdown_until(Instant::now() + SESSION_SHUTDOWN_TIMEOUT)
    }

    /// Stop the terminal owner and writer within an absolute deadline. A
    /// blocked PTY reader or writer is detached after the deadline and the
    /// returned error lets the Run outcome report the unconfirmed worker.
    pub fn shutdown_until(&self, deadline: Instant) -> Result<()> {
        let mut complete = self
            .shutdown_complete
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *complete {
            return Ok(());
        }
        self.commands.close();
        self.owner.request_shutdown();
        while self.owner.is_alive() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if self.owner.is_alive() {
            let owner_joined = self.owner.abandon_nonblocking();
            let writer_joined = self.writer.abandon_nonblocking();
            *complete = true;
            bail!(
                "terminal workers did not stop before the deadline (owner_joined={owner_joined}, writer_joined={writer_joined})"
            );
        }
        if !self
            .owner
            .join_until(deadline)
            .context("could not stop the terminal owner")?
        {
            *complete = true;
            bail!("terminal owner did not join before the deadline");
        }
        if !self
            .writer
            .join_until(deadline)
            .context("could not stop the PTY writer")?
        {
            *complete = true;
            bail!("PTY writer did not join before the deadline");
        }
        *complete = true;
        Ok(())
    }

    /// Close admission and detach terminal workers without waiting. This is
    /// used only after a Run's final deadline when a survivor may still own
    /// the PTY. The caller reports the returned worker state in its outcome.
    pub fn abandon_nonblocking(&self) -> (bool, bool) {
        let mut complete = self
            .shutdown_complete
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *complete {
            return (true, true);
        }
        self.commands.close();
        self.owner.request_shutdown();
        let owner_joined = self.owner.abandon_nonblocking();
        let writer_joined = self.writer.abandon_nonblocking();
        *complete = true;
        (owner_joined, writer_joined)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn bounded_scroll_delta(delta: isize) -> isize {
    delta.clamp(-MAX_SAFE_SCROLL_DELTA, MAX_SAFE_SCROLL_DELTA)
}

#[cfg(test)]
mod tests;
