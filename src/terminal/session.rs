use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossterm::event::KeyEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui_ghostty::session::{SessionConfig, SessionEvent, SessionHandle, SessionIo};
use ratatui_ghostty::widget::{CursorState, CursorStyle};

use crate::geometry::TerminalGeometry;
use crate::runtime::{PtyIo, PtyWriterEvent, PtyWriterOwner, spawn_bounded_pty_writer};

const SESSION_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const INPUT_QUEUE_LIMIT_BYTES: usize = 256 * 1_024;

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
    inner: SessionHandle,
    writer: PtyWriterOwner,
    resize_failure: Arc<ResizeFailure>,
}

impl TerminalSession {
    pub fn spawn(
        io: PtyIo,
        geometry: TerminalGeometry,
        wake: impl Fn() + Send + 'static,
    ) -> Result<Self> {
        let config = SessionConfig {
            scrollback: 1_000,
            ..SessionConfig::default()
        };
        let (writer, writer_owner) = spawn_bounded_pty_writer(io.writer, INPUT_QUEUE_LIMIT_BYTES)
            .context("could not start the bounded PTY writer")?;
        let resize_failure = Arc::new(ResizeFailure::default());
        let resize_failure_callback = Arc::clone(&resize_failure);
        let mut pty_resizer = io.resizer;
        let io = SessionIo {
            reader: io.reader,
            writer,
            resizer: Box::new(move |cols, rows| {
                let result = pty_resizer(cols, rows);
                if let Err(error) = &result {
                    resize_failure_callback
                        .record(format!("PTY resize to {cols}x{rows} failed: {error}"));
                }
                result
            }),
        };
        let inner = SessionHandle::spawn(config, io, geometry.cols(), geometry.rows(), wake)
            .context("could not start the Ghostty terminal owner")?;
        Ok(Self {
            inner,
            writer: writer_owner,
            resize_failure,
        })
    }

    pub fn send_key(&self, event: KeyEvent) {
        self.inner.send_key(event);
    }

    pub fn send_focus(&self, gained: bool) {
        self.inner.send_focus(gained);
    }

    pub fn send_raw(&self, data: Vec<u8>) {
        self.inner.send_raw(data);
    }

    pub fn resize(&self, geometry: TerminalGeometry) {
        self.inner.send_resize(geometry.cols(), geometry.rows());
    }

    pub fn snapshot(&self) -> OwnedTerminalSnapshot {
        snapshot_from(&self.inner)
    }

    pub fn is_dirty(&self) -> bool {
        self.inner.is_dirty()
    }

    pub fn poll_event(&self) -> Option<TerminalEvent> {
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
        if let Some(error) = self.resize_failure.take() {
            return Some(TerminalEvent::Failed(error));
        }
        self.inner.poll_event().map(|event| match event {
            SessionEvent::Exited => TerminalEvent::Exited,
            SessionEvent::Error(error) => TerminalEvent::Failed(error.to_string()),
            _ => TerminalEvent::StateChanged,
        })
    }

    pub fn shutdown(&self) -> Result<()> {
        self.inner.send_shutdown();
        let deadline = Instant::now() + SESSION_SHUTDOWN_TIMEOUT;
        while self.inner.is_alive() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if self.inner.is_alive() {
            bail!("terminal owner did not stop within two seconds");
        }
        self.writer
            .join()
            .context("could not stop the PTY writer")?;
        Ok(())
    }
}

#[derive(Default)]
struct ResizeFailure {
    latest: Mutex<Option<String>>,
}

impl ResizeFailure {
    fn record(&self, error: String) {
        *self
            .latest
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error);
    }

    fn take(&self) -> Option<String> {
        self.latest
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}

trait SnapshotSource {
    fn size(&self) -> (u16, u16);
    fn blit_to(&self, buffer: &mut Buffer, area: Rect);
    fn cursor_state(&self) -> CursorState;
    fn mark_clean(&self);
}

impl SnapshotSource for SessionHandle {
    fn size(&self) -> (u16, u16) {
        self.size()
    }

    fn blit_to(&self, buffer: &mut Buffer, area: Rect) {
        self.blit_to(buffer, area);
    }

    fn cursor_state(&self) -> CursorState {
        self.cursor_state()
    }

    fn mark_clean(&self) {
        self.mark_clean();
    }
}

fn snapshot_from(source: &impl SnapshotSource) -> OwnedTerminalSnapshot {
    // Clear the signal before the copy. Output that arrives after this point
    // sets it again and forces a later snapshot instead of being acknowledged
    // without being observed.
    source.mark_clean();
    let (cols, rows) = source.size();
    let mut buffer = Buffer::empty(Rect::new(0, 0, cols, rows));
    let area = buffer.area;
    source.blit_to(&mut buffer, area);
    let cursor = owned_cursor(source.cursor_state());
    OwnedTerminalSnapshot { buffer, cursor }
}

fn owned_cursor(cursor: ratatui_ghostty::widget::CursorState) -> Option<OwnedCursorState> {
    Some(OwnedCursorState {
        position: cursor.position?,
        shape: match cursor.style {
            CursorStyle::Block => CursorShape::Block,
            CursorStyle::Bar => CursorShape::Bar,
            CursorStyle::Underline => CursorShape::Underline,
        },
        blinking: cursor.blinking,
    })
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::io;
    use std::os::unix::net::UnixStream;

    use super::*;

    struct OutputDuringCopy {
        dirty: Cell<bool>,
    }

    impl SnapshotSource for OutputDuringCopy {
        fn size(&self) -> (u16, u16) {
            (1, 1)
        }

        fn blit_to(&self, buffer: &mut Buffer, _area: Rect) {
            buffer[(0, 0)].set_symbol("old");
            self.dirty.set(true);
        }

        fn cursor_state(&self) -> CursorState {
            CursorState::default()
        }

        fn mark_clean(&self) {
            self.dirty.set(false);
        }
    }

    #[test]
    fn output_that_arrives_during_snapshot_copy_stays_dirty() {
        let source = OutputDuringCopy {
            dirty: Cell::new(true),
        };

        let _ = snapshot_from(&source);

        assert!(source.dirty.get());
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
        let failure = failure.expect("PTY resize failure must become a terminal event");
        assert!(failure.contains("PTY resize to 42x12 failed: fixture resize failure"));
    }
}
