use std::sync::mpsc::Sender as CompletionSender;

use anyhow::{Result, anyhow};
use libghostty_vt::key;
use libghostty_vt::render::RenderState;
use libghostty_vt::terminal::{ScrollViewport, Terminal};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};

use super::command_gate::TerminalCommand;
use super::input;
use super::mouse::MouseController;
use super::render;
use super::selection::SelectionController;
use super::session::OwnedCursorState;
use crate::geometry::TerminalGeometry;
use crate::runtime::PtyResizer;

pub(super) struct PendingInput {
    pub data: Vec<u8>,
    pub command_bytes: usize,
    pub completion: Option<CompletionSender<std::result::Result<(), String>>>,
}

pub(super) struct TerminalState {
    terminal: Box<Terminal<'static, 'static>>,
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
        resizer: PtyResizer,
        geometry: TerminalGeometry,
    ) -> Result<Self> {
        Ok(Self {
            terminal,
            render: RenderState::new()?,
            key_encoder: key::Encoder::new()?,
            mouse: MouseController::new()?,
            selection: SelectionController::new()?,
            resizer,
            focused: true,
            geometry,
        })
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
                input::encode_focus(gained).to_vec()
            }
            TerminalCommand::Mouse(event) => {
                self.mouse
                    .apply(&mut self.terminal, &mut self.selection, event)?
            }
            TerminalCommand::Raw(data) => data,
            TerminalCommand::Paste { data, completion } => {
                return Ok(Some(PendingInput {
                    data: input::encode_paste(&data, &self.terminal),
                    command_bytes,
                    completion: Some(completion),
                }));
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

    pub fn write_output(&mut self, data: &[u8]) {
        self.terminal.vt_write(data);
    }

    pub fn tick_selection_autoscroll(&mut self) -> Result<bool> {
        Ok(self.selection.tick_autoscroll(&mut self.terminal)?)
    }

    pub fn area(&self) -> Rect {
        Rect::new(0, 0, self.geometry.cols(), self.geometry.rows())
    }

    pub fn render(&mut self, buffer: &mut Buffer) -> Result<Option<OwnedCursorState>> {
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
        Ok(copy_cursor.or(terminal_cursor))
    }

    pub fn mouse_tracking(&self) -> bool {
        self.terminal.is_mouse_tracking().unwrap_or(false)
    }
}
