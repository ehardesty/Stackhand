use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::Sender as CompletionSender;

use anyhow::{Result, anyhow};
use libghostty_vt::render::RenderState;
use libghostty_vt::terminal::{
    ClipboardMime, CompressionActivity, CompressionMode, CompressionResult, Mode, ScrollViewport,
    Terminal,
};
use libghostty_vt::{focus, key, paste};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};

use super::command_gate::TerminalCommand;
use super::input;
use super::mouse::MouseController;
use super::owner::Effects;
use super::paste::{PASTE_FRAMING_BYTES, PasteCompletion};
use super::render;
use super::selection::SelectionController;
use super::session::{OwnedCursorState, OwnedTerminalScrollbar};
use crate::geometry::TerminalGeometry;
use crate::runtime::PtyResizer;

pub(super) struct PendingInput {
    pub data: Vec<u8>,
    pub command_bytes: usize,
    pub completion: Option<CompletionSender<PasteCompletion>>,
}

pub(super) struct TerminalState {
    terminal: Box<Terminal<'static, 'static>>,
    effects: Rc<RefCell<Effects>>,
    render: RenderState<'static>,
    key_encoder: key::Encoder<'static>,
    mouse: MouseController,
    selection: SelectionController,
    resizer: PtyResizer,
    focused: bool,
    geometry: TerminalGeometry,
}

impl TerminalState {
    pub fn new(
        terminal: Box<Terminal<'static, 'static>>,
        effects: Rc<RefCell<Effects>>,
        resizer: PtyResizer,
        geometry: TerminalGeometry,
    ) -> Result<Self> {
        Ok(Self {
            terminal,
            effects,
            render: RenderState::new()?,
            key_encoder: key::Encoder::new()?,
            mouse: MouseController::new()?,
            selection: SelectionController::new()?,
            resizer,
            focused: true,
            geometry,
        })
    }

    pub fn scrollback_limit_bytes(&self) -> Result<usize> {
        Ok(self.terminal.scrollback_max_bytes()?.unwrap_or(usize::MAX))
    }

    pub fn memory_usage(&self) -> Result<super::TerminalMemory> {
        Ok(self.terminal.memory_usage()?.into())
    }

    pub fn compression_activity(&self) -> Result<CompressionActivity> {
        Ok(self.terminal.compression_activity()?)
    }

    /// Perform one Ghostty-bounded idle compression step. Logical terminal
    /// content and the viewport do not change.
    pub fn compress_scrollback(&mut self) -> Result<CompressionResult> {
        Ok(self.terminal.compress(CompressionMode::Incremental)?)
    }

    pub fn apply_command(
        &mut self,
        command: TerminalCommand,
        command_bytes: usize,
    ) -> Result<Option<PendingInput>> {
        let data = match command {
            TerminalCommand::Key(event) => {
                input::encode_key(&mut self.key_encoder, event, &self.terminal)?
            }
            TerminalCommand::Focus(gained) => {
                self.focused = gained;
                self.focus_report(gained)?
            }
            TerminalCommand::Mouse(event) => {
                self.mouse
                    .apply(&mut self.terminal, &mut self.selection, event)?
            }
            TerminalCommand::Raw(data) => data,
            TerminalCommand::Paste { data, completion } => {
                return Ok(match self.paste(&data) {
                    Ok(data) => Some(PendingInput {
                        data,
                        command_bytes,
                        completion: Some(completion),
                    }),
                    Err(outcome) => {
                        let _ = completion.send(outcome);
                        None
                    }
                });
            }
            TerminalCommand::Resize(geometry) => {
                self.terminal
                    .resize(geometry.cols(), geometry.rows(), 0, 0)?;
                (self.resizer)(geometry.cols(), geometry.rows()).map_err(|error| {
                    anyhow!(
                        "PTY resize to {}x{} failed: {error}",
                        geometry.cols(),
                        geometry.rows()
                    )
                })?;
                self.geometry = geometry;
                return Ok(None);
            }
            TerminalCommand::Scroll(delta) => {
                self.terminal.scroll_viewport(ScrollViewport::Delta(delta));
                return Ok(None);
            }
            TerminalCommand::ScrollTo(row) => {
                self.terminal.scroll_viewport(ScrollViewport::Row(row));
                return Ok(None);
            }
            TerminalCommand::ScrollBatch(scroll) => {
                self.terminal
                    .scroll_viewport(ScrollViewport::Delta(scroll.take()));
                return Ok(None);
            }
            TerminalCommand::SelectionAll => {
                self.selection.select_all(&self.terminal)?;
                return Ok(None);
            }
            TerminalCommand::SelectionClear => {
                self.selection.clear(&self.terminal)?;
                return Ok(None);
            }
            TerminalCommand::SelectionKeyboardStart => {
                self.selection
                    .enter_keyboard_navigation(&mut self.terminal)?;
                return Ok(None);
            }
            TerminalCommand::SelectionKeyboardToggle => {
                self.selection
                    .toggle_keyboard_endpoint(&mut self.terminal)?;
                return Ok(None);
            }
            TerminalCommand::SelectionKeyboardMove(direction) => {
                self.selection
                    .move_keyboard_cursor(&mut self.terminal, direction)?;
                return Ok(None);
            }
            TerminalCommand::SelectionText(completion) => {
                let result = self
                    .selection
                    .text(&self.terminal)
                    .map_err(|error| error.to_string());
                let _ = completion.send(result);
                return Ok(None);
            }
        };
        if data.is_empty() {
            Ok(None)
        } else {
            Ok(Some(PendingInput {
                data,
                command_bytes,
                completion: None,
            }))
        }
    }

    fn paste(&mut self, data: &[u8]) -> Result<Vec<u8>, PasteCompletion> {
        let (result, captured) =
            Effects::capture(&self.effects, data.len() + PASTE_FRAMING_BYTES, || {
                self.terminal.paste(
                    paste::Options::new(),
                    &[ClipboardMime::new("text/plain")],
                    |_, writer| writer.write_all(data),
                )
            });
        match result {
            Ok(_) => captured,
            Err(libghostty_vt::Error::Rejected) => Err(PasteCompletion::Refused),
            Err(error) => Err(PasteCompletion::Failed(error.to_string())),
        }
    }

    fn focus_report(&self, gained: bool) -> Result<Vec<u8>> {
        if !self.terminal.mode(Mode::FOCUS_EVENT)? {
            return Ok(Vec::new());
        }
        let event = if gained {
            focus::Event::Gained
        } else {
            focus::Event::Lost
        };
        let mut bytes = [0; 3];
        let len = event.encode(&mut bytes)?;
        Ok(bytes[..len].to_vec())
    }

    pub fn write_output(&mut self, data: &[u8]) {
        self.terminal.vt_write(data);
    }

    pub fn tick_selection_autoscroll(&mut self) -> Result<bool> {
        Ok(self.selection.tick_autoscroll(&mut self.terminal)?)
    }

    pub fn area(&self) -> Rect {
        Rect::new(0, 0, self.geometry.cols(), self.geometry.rows())
    }

    pub fn render(
        &mut self,
        buffer: &mut Buffer,
    ) -> Result<(Option<OwnedCursorState>, OwnedTerminalScrollbar)> {
        let area = self.area();
        let terminal_cursor =
            render::render(&self.terminal, &mut self.render, buffer, self.focused, area)?;
        let copy_cursor = self
            .selection
            .keyboard_cursor(&self.terminal)?
            .and_then(|point| {
                let row = u16::try_from(point.surface_row).ok()?;
                (point.col < area.width && row < area.height).then_some(OwnedCursorState {
                    position: Position::new(area.x + point.col, area.y + row),
                    shape: super::session::CursorShape::Block,
                    blinking: false,
                })
            });
        let scrollbar = self.terminal.scrollbar()?;
        Ok((
            copy_cursor.or(terminal_cursor),
            OwnedTerminalScrollbar {
                total: usize::try_from(scrollbar.total)?,
                offset: usize::try_from(scrollbar.offset)?,
                len: usize::try_from(scrollbar.len)?,
            },
        ))
    }

    pub fn mouse_tracking(&self) -> bool {
        self.terminal.is_mouse_tracking().unwrap_or(false)
    }
}
