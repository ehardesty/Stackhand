use std::sync::mpsc::Sender as CompletionSender;

use anyhow::{Result, anyhow};
use libghostty_vt::key;
use libghostty_vt::terminal::{ScrollViewport, Terminal};

use super::command_gate::TerminalCommand;
use super::input;
use super::mouse::MouseController;
use super::selection::SelectionController;
use crate::runtime::PtyResizer;

pub struct PendingInput {
    pub data: Vec<u8>,
    pub command_bytes: usize,
    pub completion: Option<CompletionSender<std::result::Result<(), String>>>,
}

#[allow(clippy::too_many_arguments)]
pub fn apply_command(
    command: TerminalCommand,
    command_bytes: usize,
    terminal: &mut Terminal<'static, 'static>,
    key_encoder: &mut key::Encoder<'static>,
    mouse_controller: &mut MouseController,
    resizer: &mut PtyResizer,
    focused: &mut bool,
    cols: &mut u16,
    rows: &mut u16,
    selection: &mut SelectionController,
) -> Result<Option<PendingInput>> {
    let data = match command {
        TerminalCommand::Key(event) => input::encode_key(key_encoder, event, terminal)?,
        TerminalCommand::Focus(gained) => {
            *focused = gained;
            input::encode_focus(gained).to_vec()
        }
        TerminalCommand::Mouse(event) => mouse_controller.apply(terminal, selection, event)?,
        TerminalCommand::Raw(data) => data,
        TerminalCommand::Paste { data, completion } => {
            return Ok(Some(PendingInput {
                data: input::encode_paste(&data, terminal),
                command_bytes,
                completion: Some(completion),
            }));
        }
        TerminalCommand::Resize(geometry) => {
            terminal.resize(geometry.cols(), geometry.rows(), 0, 0)?;
            resizer(geometry.cols(), geometry.rows()).map_err(|error| {
                anyhow!(
                    "PTY resize to {}x{} failed: {error}",
                    geometry.cols(),
                    geometry.rows()
                )
            })?;
            *cols = geometry.cols();
            *rows = geometry.rows();
            return Ok(None);
        }
        TerminalCommand::Scroll(delta) => {
            terminal.scroll_viewport(ScrollViewport::Delta(delta));
            return Ok(None);
        }
        TerminalCommand::SelectionPress { point, time } => {
            selection.press(terminal, point, time)?;
            return Ok(None);
        }
        TerminalCommand::SelectionDrag(point) => {
            selection.drag(terminal, point)?;
            return Ok(None);
        }
        TerminalCommand::SelectionRelease(point) => {
            selection.release(terminal, point)?;
            return Ok(None);
        }
        TerminalCommand::SelectionAll => {
            selection.select_all(terminal)?;
            return Ok(None);
        }
        TerminalCommand::SelectionClear => {
            selection.clear(terminal)?;
            return Ok(None);
        }
        TerminalCommand::SelectionText(completion) => {
            let result = selection.text(terminal).map_err(|error| error.to_string());
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
