use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossterm::event::KeyEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::Position;

use super::command_gate::{CommandEvent, CommandGate, TerminalCommand};
use super::history::OutputHistoryMetrics;
use super::owner::{OwnerEvent, OwnerHandle};
use super::paste::{self, PasteRejection, PasteRequest};
pub use super::selection::SelectionPoint;
use crate::geometry::TerminalGeometry;
use crate::runtime::{PtyIo, PtyWriterEvent, PtyWriterOwner, spawn_bounded_pty_writer};

const SESSION_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
pub const INPUT_QUEUE_LIMIT_BYTES: usize = 256 * 1_024;
pub const SCROLLBACK_TARGET_BYTES: usize = 64 * 1_024;
const MAX_SAFE_SCROLL_DELTA: isize = 1_000_000;

#[derive(Clone, Debug)]
pub struct OwnedTerminalSnapshot {
    pub buffer: Buffer,
    pub cursor: Option<OwnedCursorState>,
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
    OutputTruncated {
        evicted_bytes: usize,
    },
    StateChanged,
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
    pub fn spawn(
        io: PtyIo,
        geometry: TerminalGeometry,
        wake: impl Fn() + Send + 'static,
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
        )?;
        Ok(Self {
            owner,
            writer: writer_owner,
            commands,
            next_paste_request_id: AtomicU64::new(1),
            shutdown_complete: Mutex::new(false),
        })
    }

    pub fn send_key(&self, event: KeyEvent) {
        let _ = self.commands.try_send(TerminalCommand::Key(event));
    }

    pub fn send_focus(&self, gained: bool) {
        let _ = self.commands.try_send(TerminalCommand::Focus(gained));
    }

    pub fn send_raw(&self, data: Vec<u8>) {
        let _ = self.commands.try_send(TerminalCommand::Raw(data));
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
        self.commands
            .try_send(TerminalCommand::Paste {
                data: data.as_bytes().to_vec(),
                completion: completion_tx,
            })
            .map_err(|error| PasteRejection::Busy {
                attempted_bytes: error.attempted_bytes,
                pending_bytes: error.pending_bytes,
                limit_bytes: error.limit_bytes,
            })?;
        Ok(PasteRequest::new(request_id, completion_rx))
    }

    pub fn resize(&self, geometry: TerminalGeometry) {
        let _ = self.commands.try_send(TerminalCommand::Resize(geometry));
    }

    pub fn scroll_lines(&self, delta: isize) {
        let _ = self
            .commands
            .try_send(TerminalCommand::Scroll(bounded_scroll_delta(delta)));
    }

    pub fn follow_live(&self) {
        let _ = self
            .commands
            .try_send(TerminalCommand::Scroll(MAX_SAFE_SCROLL_DELTA));
    }

    pub fn selection_press(&self, point: SelectionPoint, time: Duration) {
        let _ = self
            .commands
            .try_send(TerminalCommand::SelectionPress { point, time });
    }

    pub fn selection_drag(&self, point: SelectionPoint) {
        let _ = self
            .commands
            .try_send(TerminalCommand::SelectionDrag(point));
    }

    pub fn selection_release(&self, point: SelectionPoint) {
        let _ = self
            .commands
            .try_send(TerminalCommand::SelectionRelease(point));
    }

    pub fn select_all(&self) {
        let _ = self.commands.try_send(TerminalCommand::SelectionAll);
    }

    pub fn clear_selection(&self) {
        let _ = self.commands.try_send(TerminalCommand::SelectionClear);
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
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.owner.is_dirty()
    }

    pub fn output_history_metrics(&self) -> OutputHistoryMetrics {
        self.owner.history_metrics()
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
            OwnerEvent::OutputTruncated { evicted_bytes } => {
                TerminalEvent::OutputTruncated { evicted_bytes }
            }
        })
    }

    pub fn shutdown(&self) -> Result<()> {
        let mut complete = self
            .shutdown_complete
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *complete {
            return Ok(());
        }
        self.commands.close();
        self.owner.request_shutdown();
        let deadline = Instant::now() + SESSION_SHUTDOWN_TIMEOUT;
        while self.owner.is_alive() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if self.owner.is_alive() {
            bail!("terminal owner did not stop within two seconds");
        }
        self.owner.join()?;
        self.writer
            .join()
            .context("could not stop the PTY writer")?;
        *complete = true;
        Ok(())
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
mod tests {
    use std::io::{self, Read};
    use std::os::unix::net::UnixStream;

    use super::*;

    #[test]
    fn extreme_scroll_deltas_are_bounded_before_the_ghostty_call() {
        assert_eq!(bounded_scroll_delta(isize::MIN), -1_000_000);
        assert_eq!(bounded_scroll_delta(isize::MAX), 1_000_000);
        assert_eq!(bounded_scroll_delta(-5), -5);
    }

    #[test]
    fn failed_pty_resize_becomes_a_terminal_failure() {
        let (reader, peer) = UnixStream::pair().unwrap();
        let io = PtyIo {
            reader: Box::new(reader),
            writer: Box::new(io::sink()),
            resizer: Box::new(|_, _| Err("fixture resize failure".into())),
        };
        let session = TerminalSession::spawn(io, TerminalGeometry::DEFAULT, || {}).unwrap();
        session.resize(TerminalGeometry::new(42, 12).unwrap());

        let deadline = Instant::now() + Duration::from_secs(1);
        let mut failure = None;
        while Instant::now() < deadline {
            if let Some(TerminalEvent::Failed(error)) = session.poll_event() {
                failure = Some(error);
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }

        drop(peer);
        session.shutdown().unwrap();
        assert!(failure.unwrap().contains("PTY resize to 42x12 failed"));
    }

    struct OneByteChunks {
        remaining: usize,
    }

    impl Read for OneByteChunks {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Ok(0);
            }
            buffer[0] = b'x';
            self.remaining -= 1;
            Ok(1)
        }
    }

    #[test]
    fn each_real_reader_chunk_enters_the_bounded_output_history() {
        let io = PtyIo {
            reader: Box::new(OneByteChunks {
                remaining: super::super::OUTPUT_HISTORY_CHUNKS + 1,
            }),
            writer: Box::new(io::sink()),
            resizer: Box::new(|_, _| Ok(())),
        };
        let session = TerminalSession::spawn(io, TerminalGeometry::DEFAULT, || {}).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while session.output_history_metrics().evicted_bytes == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }

        let history = session.output_history_metrics();
        session.shutdown().unwrap();
        assert_eq!(history.chunks, super::super::OUTPUT_HISTORY_CHUNKS);
        assert_eq!(history.bytes, super::super::OUTPUT_HISTORY_CHUNKS);
        assert_eq!(history.evicted_bytes, 1);
    }
}
