use std::cell::Cell;
use std::io::{self, Stdout, Write};
use std::rc::Rc;

use anyhow::{Context, Result};
use crossterm::cursor::{self, SetCursorStyle};
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::terminal::{CursorShape, OwnedCursorState};

pub type ApplicationTerminal = Terminal<CrosstermBackend<TerminalOutput<Stdout>>>;

pub struct TerminalOutput<W> {
    writer: W,
    cleanup: Rc<Cell<bool>>,
}

impl<W: Write> Write for TerminalOutput<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.cleanup.get() {
            let _ = self.writer.write_all(bytes);
            Ok(bytes.len())
        } else {
            self.writer.write(bytes)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        let result = self.writer.flush();
        if self.cleanup.get() { Ok(()) } else { result }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    AlternateScreen,
    BracketedPaste,
    Focus,
    Mouse,
    Cursor,
    Keyboard,
}

impl Mode {
    fn enable(self, out: &mut impl Write) -> io::Result<()> {
        match self {
            Self::AlternateScreen => execute!(out, EnterAlternateScreen),
            Self::BracketedPaste => execute!(out, EnableBracketedPaste),
            Self::Focus => execute!(out, EnableFocusChange),
            Self::Mouse => execute!(out, EnableMouseCapture),
            Self::Cursor => execute!(out, cursor::Hide),
            Self::Keyboard => execute!(
                out,
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES,
                )
            ),
        }
    }

    fn disable(self, out: &mut impl Write) -> io::Result<()> {
        match self {
            Self::AlternateScreen => {
                let screen = execute!(out, LeaveAlternateScreen);
                let cursor = execute!(out, SetCursorStyle::DefaultUserShape);
                screen.and(cursor)
            }
            Self::BracketedPaste => execute!(out, DisableBracketedPaste),
            Self::Focus => execute!(out, DisableFocusChange),
            Self::Mouse => execute!(out, DisableMouseCapture),
            Self::Cursor => execute!(out, cursor::Show),
            Self::Keyboard => execute!(out, PopKeyboardEnhancementFlags),
        }
    }
}

#[derive(Default)]
struct OuterModes(Vec<Mode>);

impl OuterModes {
    fn enter(&mut self, out: &mut impl Write, keyboard: bool) -> io::Result<()> {
        for mode in [
            Mode::AlternateScreen,
            Mode::BracketedPaste,
            Mode::Focus,
            Mode::Mouse,
            Mode::Cursor,
            Mode::Keyboard,
        ] {
            if mode == Mode::Keyboard {
                if keyboard {
                    // A failed push may be partial. Popping without a confirmed
                    // push risks removing the caller's keyboard stack entry.
                    mode.enable(out)?;
                    self.0.push(mode);
                }
            } else {
                self.0.push(mode);
                mode.enable(out)?;
            }
        }
        Ok(())
    }

    fn restore_terminal<W: Write>(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<TerminalOutput<W>>>,
    ) {
        self.restore(|mode| {
            if mode == Mode::Cursor {
                terminal.show_cursor()
            } else {
                mode.disable(terminal.backend_mut())
            }
        });
    }

    fn restore(&mut self, mut disable: impl FnMut(Mode) -> io::Result<()>) {
        while let Some(mode) = self.0.pop() {
            let _ = disable(mode);
        }
    }
}

pub struct OuterTerminal {
    terminal: ApplicationTerminal,
    modes: OuterModes,
    cleanup: Rc<Cell<bool>>,
}

impl OuterTerminal {
    pub fn enter() -> Result<Self> {
        terminal::enable_raw_mode().context("could not enable raw mode")?;
        let keyboard = terminal::supports_keyboard_enhancement().unwrap_or(false);
        let mut stdout = io::stdout();
        let mut modes = OuterModes::default();
        if let Err(error) = modes.enter(&mut stdout, keyboard) {
            modes.restore(|mode| mode.disable(&mut stdout));
            let _ = terminal::disable_raw_mode();
            return Err(error).context("could not enable terminal modes");
        }

        let cleanup = Rc::new(Cell::new(false));
        let backend = CrosstermBackend::new(TerminalOutput {
            writer: stdout,
            cleanup: Rc::clone(&cleanup),
        });
        match Terminal::new(backend) {
            Ok(terminal) => Ok(Self {
                terminal,
                modes,
                cleanup,
            }),
            Err(error) => {
                modes.restore(|mode| mode.disable(&mut io::stdout()));
                let _ = terminal::disable_raw_mode();
                Err(error).context("could not initialize Ratatui")
            }
        }
    }

    pub fn terminal_mut(&mut self) -> &mut ApplicationTerminal {
        &mut self.terminal
    }

    pub fn set_cursor_shape(&mut self, cursor: Option<OwnedCursorState>) -> Result<()> {
        let Some(cursor) = cursor else {
            return Ok(());
        };
        let style = match (cursor.shape, cursor.blinking) {
            (CursorShape::Block, false) => SetCursorStyle::SteadyBlock,
            (CursorShape::Block, true) => SetCursorStyle::BlinkingBlock,
            (CursorShape::Bar, false) => SetCursorStyle::SteadyBar,
            (CursorShape::Bar, true) => SetCursorStyle::BlinkingBar,
            (CursorShape::Underline, false) => SetCursorStyle::SteadyUnderScore,
            (CursorShape::Underline, true) => SetCursorStyle::BlinkingUnderScore,
        };
        execute!(self.terminal.backend_mut(), style)
            .context("could not set the child cursor shape")?;
        Ok(())
    }
}

impl Drop for OuterTerminal {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        // ratatui-core 0.1.2's Drop prints errors if its cursor remains hidden.
        // Cleanup writes remain best effort; show_cursor clears that flag so
        // Ratatui can free its buffers without a second restoration attempt.
        self.cleanup.set(true);
        self.modes.restore_terminal(&mut self.terminal);
    }
}

#[cfg(test)]
mod tests;
