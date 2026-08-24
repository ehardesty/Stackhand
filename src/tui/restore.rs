use std::io::{self, Stdout};

use anyhow::{Context, Result};
use crossterm::cursor::{self, SetCursorStyle};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

pub type ApplicationTerminal = Terminal<CrosstermBackend<Stdout>>;

pub struct OuterTerminal {
    terminal: ApplicationTerminal,
}

impl OuterTerminal {
    pub fn enter() -> Result<Self> {
        terminal::enable_raw_mode().context("could not enable raw mode")?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, cursor::Hide) {
            let _ = terminal::disable_raw_mode();
            return Err(error).context("could not enter the alternate screen");
        }

        let backend = CrosstermBackend::new(stdout);
        match Terminal::new(backend) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                let mut stdout = io::stdout();
                let _ = execute!(stdout, LeaveAlternateScreen, cursor::Show);
                let _ = terminal::disable_raw_mode();
                Err(error).context("could not initialize Ratatui")
            }
        }
    }

    pub fn terminal_mut(&mut self) -> &mut ApplicationTerminal {
        &mut self.terminal
    }
}

impl Drop for OuterTerminal {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            cursor::Show,
            SetCursorStyle::DefaultUserShape
        );
    }
}
