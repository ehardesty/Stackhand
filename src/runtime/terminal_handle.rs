//! Non-owning terminal actions exposed by an owned Run.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::geometry::TerminalGeometry;
use crate::runtime::outcome::ResizeRejected;
use crate::terminal::{
    CopyRequest, InputRejection as SessionInputRejection, OutputHistoryMetrics,
    OwnedTerminalScrollbar, OwnedTerminalSnapshot, PasteRejection, PasteRequest,
    SelectionDirection, TerminalEvent, TerminalMouseEvent, TerminalSession,
};

use super::InputRejected;

/// Non-owning terminal actions for one Run.
///
/// The handle cannot shut down, replace, or detach the TerminalSession from
/// its Run. Terminal semantics stay inside `TerminalSession`; Process Tree
/// containment and shutdown policy stay inside the Run owner.
pub struct TerminalHandle<'a> {
    session: &'a TerminalSession,
    /// Shared admission gate: once shutdown starts, input is rejected.
    stopping: &'a AtomicBool,
}

impl<'a> TerminalHandle<'a> {
    pub(crate) fn new(session: &'a TerminalSession, stopping: &'a AtomicBool) -> Self {
        Self { session, stopping }
    }
}

/// Internal constructor for crate tests that already own a session.
#[cfg(test)]
pub(crate) fn handle_for_test<'s>(
    session: &'s TerminalSession,
    stopping: &'s AtomicBool,
) -> TerminalHandle<'s> {
    TerminalHandle::new(session, stopping)
}

impl TerminalHandle<'_> {
    fn admit_input(&self) -> Result<(), InputRejected> {
        if self.stopping.load(Ordering::Acquire) {
            Err(InputRejected::Stopping)
        } else {
            Ok(())
        }
    }

    /// Send one key event. Rejected once shutdown has started.
    pub fn send_key(&self, event: crossterm::event::KeyEvent) -> Result<(), InputRejected> {
        self.admit_input()?;
        self.session.send_key(event).map_err(Into::into)
    }

    /// Send a focus change. Rejected once shutdown has started.
    pub fn send_focus(&self, gained: bool) -> Result<(), InputRejected> {
        self.admit_input()?;
        self.session.send_focus(gained).map_err(Into::into)
    }

    /// Send a mouse event. Rejected once shutdown has started.
    pub fn send_mouse(&self, event: TerminalMouseEvent) -> Result<(), InputRejected> {
        self.admit_input()?;
        self.session.send_mouse(event).map_err(Into::into)
    }

    /// Send raw bytes. Rejected once shutdown has started.
    pub fn send_raw(&self, data: Vec<u8>) -> Result<(), InputRejected> {
        self.admit_input()?;
        self.session.send_raw(data).map_err(Into::into)
    }

    /// Admit one whole paste. Rejected with `PasteRejection::Stopping`
    /// once shutdown has started. See [`TerminalSession::send_paste`].
    pub fn send_paste(&self, data: &str) -> Result<PasteRequest, PasteRejection> {
        self.admit_input().map_err(|_| PasteRejection::Stopping)?;
        self.session.send_paste(data)
    }

    /// Resize the terminal. Rejected once shutdown has started.
    pub fn resize(&self, geometry: TerminalGeometry) -> Result<(), ResizeRejected> {
        if self.admit_input().is_err() {
            return Err(ResizeRejected::Stopping);
        }
        match self.session.resize(geometry) {
            Ok(()) => Ok(()),
            Err(SessionInputRejection::Stopping) => Err(ResizeRejected::Stopping),
            Err(SessionInputRejection::Backpressure {
                attempted_bytes,
                pending_bytes,
                limit_bytes,
            }) => Err(ResizeRejected::Backpressure {
                attempted_bytes,
                pending_bytes,
                limit_bytes,
            }),
        }
    }

    pub fn scroll_lines(&self, delta: isize) {
        self.session.scroll_lines(delta);
    }

    pub fn scroll_to_row(&self, row: usize) {
        self.session.scroll_to_row(row);
    }

    pub fn follow_live(&self) {
        self.session.follow_live();
    }

    pub fn select_all(&self) {
        self.session.select_all();
    }

    pub fn clear_selection(&self) {
        self.session.clear_selection();
    }

    /// Show a terminal-owned keyboard copy cursor without sending child input.
    pub fn start_keyboard_selection(&self) {
        self.session.start_keyboard_selection();
    }

    /// Toggle whether movement extends the terminal-owned selection endpoint.
    pub fn toggle_keyboard_selection(&self) {
        self.session.toggle_keyboard_selection();
    }

    /// Move the terminal-owned copy cursor or active selection endpoint.
    pub fn move_keyboard_selection(&self, direction: SelectionDirection) {
        self.session.move_keyboard_selection(direction);
    }

    pub fn request_copy(&self) -> CopyRequest {
        self.session.request_copy()
    }

    pub fn snapshot(&self) -> OwnedTerminalSnapshot {
        self.session.snapshot()
    }

    pub fn scrollbar(&self) -> OwnedTerminalScrollbar {
        self.session.scrollbar()
    }

    pub fn is_dirty(&self) -> bool {
        self.session.is_dirty()
    }

    pub fn mouse_tracking(&self) -> bool {
        self.session.mouse_tracking()
    }

    pub fn output_history_metrics(&self) -> OutputHistoryMetrics {
        self.session.output_history_metrics()
    }

    pub fn poll_event(&self) -> Option<TerminalEvent> {
        self.session.poll_event()
    }
}
