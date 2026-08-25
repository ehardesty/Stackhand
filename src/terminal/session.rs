use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossterm::event::KeyEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::Position;

use super::command_gate::{CommandEvent, CommandGate, CommandRejection, TerminalCommand};
use super::history::OutputHistoryMetrics;
use super::mouse::TerminalMouseEvent;
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
    pub mouse_tracking: bool,
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
    OutputTruncated,
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
        self.commands
            .try_send(TerminalCommand::Mouse(event))
            .map_err(Into::into)
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
            OwnerEvent::OutputTruncated { .. } => TerminalEvent::OutputTruncated,
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
mod tests {
    use std::io::{self, Read, Write};
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::super::paste::PasteCompletion;
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
        let _ = session.resize(TerminalGeometry::new(42, 12).unwrap());

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

    struct ActiveOutputReader {
        chunk: Vec<u8>,
        stop: Arc<AtomicBool>,
        produced: Arc<AtomicUsize>,
    }

    impl Read for ActiveOutputReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.stop.load(Ordering::Acquire) {
                return Ok(0);
            }

            let amount = buffer.len().min(self.chunk.len());
            buffer[..amount].copy_from_slice(&self.chunk[..amount]);
            self.produced.fetch_add(amount, Ordering::Release);
            Ok(amount)
        }
    }

    struct RecordingWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for RecordingWriter {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.bytes.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn active_output_does_not_delay_an_admitted_terminal_command() {
        const ACTIVE_OUTPUT_BYTES: usize = 550 * 1_024;
        const COMMAND_LATENCY_BOUND: Duration = Duration::from_secs(1);

        let mut chunk = vec![0; 4_096];
        let line = b"active-output-line-0123456789\r\n";
        for (index, byte) in chunk.iter_mut().enumerate() {
            *byte = line[index % line.len()];
        }
        let stop = Arc::new(AtomicBool::new(false));
        let produced = Arc::new(AtomicUsize::new(0));
        let captured = Arc::new(Mutex::new(Vec::new()));
        let io = PtyIo {
            reader: Box::new(ActiveOutputReader {
                chunk,
                stop: Arc::clone(&stop),
                produced: Arc::clone(&produced),
            }),
            writer: Box::new(RecordingWriter {
                bytes: Arc::clone(&captured),
            }),
            resizer: Box::new(|_, _| Ok(())),
        };
        let session = TerminalSession::spawn(io, TerminalGeometry::DEFAULT, || {}).unwrap();

        let produced_deadline = Instant::now() + Duration::from_secs(10);
        while produced.load(Ordering::Acquire) < ACTIVE_OUTPUT_BYTES {
            while let Some(event) = session.poll_event() {
                if let TerminalEvent::Failed(error) = event {
                    panic!("terminal owner failed during the output burst: {error}");
                }
            }
            assert!(
                Instant::now() < produced_deadline,
                "active output fixture did not produce its initial burst"
            );
            thread::sleep(Duration::from_millis(1));
        }
        let output_before_command = {
            let metrics = session.output_history_metrics();
            metrics.bytes.saturating_add(metrics.evicted_bytes)
        };
        let command_started = Instant::now();
        let mut request = session.send_paste("fairness-sentinel").unwrap();
        let completion_deadline = command_started + COMMAND_LATENCY_BOUND;
        let completion = loop {
            if let Some(completion) = request.poll() {
                break completion;
            }
            let elapsed = command_started.elapsed();
            assert!(
                Instant::now() < completion_deadline,
                "accepted terminal command exceeded {COMMAND_LATENCY_BOUND:?} while output was active (elapsed {elapsed:?})"
            );
            thread::sleep(Duration::from_millis(1));
        };

        assert_eq!(completion, PasteCompletion::Delivered);
        let output_after_command = {
            let metrics = session.output_history_metrics();
            metrics.bytes.saturating_add(metrics.evicted_bytes)
        };
        assert!(
            output_after_command > output_before_command,
            "output parsing did not progress while the command was pending"
        );
        assert!(
            captured
                .lock()
                .unwrap()
                .windows(b"fairness-sentinel".len())
                .any(|window| window == b"fairness-sentinel"),
            "accepted terminal command was not delivered to the PTY writer"
        );

        stop.store(true, Ordering::Release);
        session.shutdown().unwrap();
    }
}
