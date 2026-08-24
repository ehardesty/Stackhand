use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossterm::event::KeyEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui_ghostty::session::{SessionConfig, SessionEvent, SessionHandle, SessionIo};

use crate::geometry::TerminalGeometry;
use crate::runtime::PtyIo;

const SESSION_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub struct OwnedTerminalSnapshot {
    pub buffer: Buffer,
    pub cursor: Option<Position>,
}

#[derive(Debug)]
pub enum TerminalEvent {
    Exited,
    Failed(String),
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
        let io = SessionIo {
            reader: io.reader,
            writer: io.writer,
            resizer: io.resizer,
        };
        let inner = SessionHandle::spawn(config, io, geometry.cols(), geometry.rows(), wake)
            .context("could not start the Ghostty terminal owner")?;
        Ok(Self { inner })
    }

    pub fn send_key(&self, event: KeyEvent) {
        self.inner.send_key(event);
    }

    pub fn resize(&self, geometry: TerminalGeometry) {
        self.inner.send_resize(geometry.cols(), geometry.rows());
    }

    pub fn snapshot(&self) -> OwnedTerminalSnapshot {
        let (cols, rows) = self.inner.size();
        let mut buffer = Buffer::empty(Rect::new(0, 0, cols, rows));
        let area = buffer.area;
        self.inner.blit_to(&mut buffer, area);
        let cursor = self.inner.cursor_state().position;
        self.inner.mark_clean();
        OwnedTerminalSnapshot { buffer, cursor }
    }

    pub fn is_dirty(&self) -> bool {
        self.inner.is_dirty()
    }

    pub fn poll_event(&self) -> Option<TerminalEvent> {
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
        Ok(())
    }
}
