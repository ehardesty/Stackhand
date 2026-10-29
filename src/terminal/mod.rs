mod command_gate;
mod input;
mod metrics;
mod mouse;
mod owner;
mod paste;
mod render;
mod selection;
mod session;
mod state;

pub(crate) use metrics::{TerminalMemory, TerminalMetrics};
pub(crate) use mouse::{MouseButton, MouseKind, MouseModifiers, TerminalMouseEvent};
pub(crate) use paste::{PASTE_LIMIT_BYTES, PasteCompletion, PasteRejection, PasteRequest};
pub(crate) use session::{
    CopyRequest, CursorShape, InputRejection, OwnedCursorState, OwnedTerminalScrollbar,
    OwnedTerminalSnapshot, SCROLLBACK_TARGET_BYTES, SelectionDirection, SelectionPoint,
    TerminalEvent, TerminalSession,
};

/// Configure both limits explicitly; upstream also has a default line cap.
fn new_terminal<'cb>(
    cols: u16,
    rows: u16,
    bytes: usize,
) -> libghostty_vt::error::Result<libghostty_vt::terminal::Terminal<'static, 'cb>> {
    let mut terminal = libghostty_vt::terminal::Terminal::new(cols, rows)?;
    terminal.set_scrollback_max_bytes(Some(bytes))?;
    terminal.set_scrollback_max_lines(None)?;
    Ok(terminal)
}
