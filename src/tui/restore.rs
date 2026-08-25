use std::io::{self, Stdout};

use anyhow::{Context, Result};
use crossterm::cursor::{self, SetCursorStyle};
use crossterm::event::{
    DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::terminal::{CursorShape, OwnedCursorState};

pub type ApplicationTerminal = Terminal<CrosstermBackend<Stdout>>;

pub struct OuterTerminal {
    terminal: ApplicationTerminal,
    keyboard_enhancement_enabled: bool,
}

impl OuterTerminal {
    pub fn enter() -> Result<Self> {
        terminal::enable_raw_mode().context("could not enable raw mode")?;
        let keyboard_enhancement_enabled =
            terminal::supports_keyboard_enhancement().unwrap_or(false);
        let mut stdout = io::stdout();
        if let Err(error) = execute!(
            stdout,
            EnterAlternateScreen,
            EnableFocusChange,
            EnableMouseCapture,
            cursor::Hide
        ) {
            let _ = terminal::disable_raw_mode();
            return Err(error).context("could not enter the alternate screen");
        }
        if keyboard_enhancement_enabled
            && let Err(error) = execute!(
                stdout,
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES,
                )
            )
        {
            let _ = execute!(
                stdout,
                DisableFocusChange,
                DisableMouseCapture,
                LeaveAlternateScreen,
                cursor::Show
            );
            let _ = terminal::disable_raw_mode();
            return Err(error).context("could not enable enhanced keyboard events");
        }

        let backend = CrosstermBackend::new(stdout);
        match Terminal::new(backend) {
            Ok(terminal) => Ok(Self {
                terminal,
                keyboard_enhancement_enabled,
            }),
            Err(error) => {
                let mut stdout = io::stdout();
                if keyboard_enhancement_enabled {
                    let _ = execute!(stdout, PopKeyboardEnhancementFlags);
                }
                let _ = execute!(
                    stdout,
                    DisableFocusChange,
                    DisableMouseCapture,
                    LeaveAlternateScreen,
                    cursor::Show
                );
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
        if self.keyboard_enhancement_enabled {
            let _ = execute!(self.terminal.backend_mut(), PopKeyboardEnhancementFlags);
        }
        let _ = execute!(
            self.terminal.backend_mut(),
            DisableFocusChange,
            DisableMouseCapture,
            LeaveAlternateScreen,
            cursor::Show,
            SetCursorStyle::DefaultUserShape
        );
    }
}
