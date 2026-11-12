use std::io::{self, Write};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::cursor::{self, SetCursorStyle};
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::{Terminal, TerminalOptions, Viewport};

use crate::host::{
    HostOutput, HostReader, ProtocolDelivery, RawMode, RestoreOutcome, SessionStop, open_terminal,
    shares_output_terminal, terminal_report, write_redirected_report,
};
use crate::terminal::{CursorShape, OwnedCursorState};

mod staging;
use staging::CommandStager;

const FINAL_OUTPUT_BUDGET: Duration = Duration::from_millis(500);
const SHUTDOWN_FRAME_BUDGET: Duration = Duration::from_millis(100);
type ApplicationTerminal = Terminal<CrosstermBackend<CommandStager>>;

pub(crate) enum RenderPhase {
    Running,
    ShuttingDown,
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
        ] {
            self.0.push(mode);
            mode.enable(out)?;
        }
        if keyboard {
            Mode::Keyboard.enable(out)?;
            self.0.push(Mode::Keyboard);
        }
        Ok(())
    }

    fn restore(&mut self, mut disable: impl FnMut(Mode) -> io::Result<()>) {
        while let Some(mode) = self.0.pop() {
            let _ = disable(mode);
        }
    }
}

pub(crate) struct OuterTerminal {
    terminal: ApplicationTerminal,
    staging: CommandStager,
    output: HostOutput,
    raw: RawMode,
    modes: OuterModes,
    finished: bool,
}

impl OuterTerminal {
    pub(crate) fn acquire(stop: SessionStop) -> Result<(Self, HostReader)> {
        let input =
            open_terminal(libc::STDIN_FILENO, true).context("could not acquire host input")?;
        let output =
            open_terminal(libc::STDOUT_FILENO, false).context("could not acquire host output")?;
        if !crate::host::tty::same_terminal(libc::STDIN_FILENO, libc::STDOUT_FILENO) {
            anyhow::bail!("stdin and stdout must identify the same terminal");
        }
        let raw = RawMode::enter(&input).context("could not enable raw mode")?;
        let (cols, rows) = raw.size()?;
        let staging = CommandStager::default();
        let terminal = Terminal::with_options(
            CrosstermBackend::new(staging.clone()),
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, cols, rows)),
            },
        )?;
        let reader = HostReader::new(input, stop.clone())?;
        Ok((
            Self {
                terminal,
                staging,
                output: HostOutput::new(output, stop),
                raw,
                modes: OuterModes::default(),
                finished: false,
            },
            reader,
        ))
    }

    pub(crate) fn query_keyboard(&mut self) -> Result<()> {
        self.output
            .send(b"\x1b[?u\x1b[c".to_vec())
            .context("could not query host keyboard support")
    }

    pub(crate) fn enable_modes(&mut self, keyboard: bool) -> Result<()> {
        let mut bytes = Vec::new();
        self.modes.enter(&mut bytes, keyboard)?;
        if let Err(error) = self.output.send(bytes) {
            if !self.output.started() {
                self.modes.0.clear();
            }
            return Err(error).context("could not enable terminal modes");
        }
        Ok(())
    }

    pub(crate) fn draw(
        &mut self,
        cursor: Option<OwnedCursorState>,
        phase: RenderPhase,
        render: impl FnOnce(&mut ratatui::Frame<'_>),
    ) -> Result<()> {
        let result = (|| -> Result<()> {
            let (cols, rows) = self.raw.size()?;
            let area = Rect::new(0, 0, cols, rows);
            if self.terminal.get_frame().area() != area {
                self.terminal.resize(area)?;
            }
            self.terminal.draw(render)?;
            if let Some(cursor) = cursor {
                let style = match (cursor.shape, cursor.blinking) {
                    (CursorShape::Block, false) => SetCursorStyle::SteadyBlock,
                    (CursorShape::Block, true) => SetCursorStyle::BlinkingBlock,
                    (CursorShape::Bar, false) => SetCursorStyle::SteadyBar,
                    (CursorShape::Bar, true) => SetCursorStyle::BlinkingBar,
                    (CursorShape::Underline, false) => SetCursorStyle::SteadyUnderScore,
                    (CursorShape::Underline, true) => SetCursorStyle::BlinkingUnderScore,
                };
                execute!(self.terminal.backend_mut(), style)?;
            }
            let bytes = self.staging.take()?;
            match phase {
                RenderPhase::Running => self.output.send(bytes)?,
                RenderPhase::ShuttingDown => self
                    .output
                    .send_until(bytes, Instant::now() + SHUTDOWN_FRAME_BUDGET)?,
            }
            Ok(())
        })();
        if result.is_err() {
            self.quiesce();
        }
        result.context("could not draw host terminal")
    }

    pub(crate) fn quiesce(&mut self) {
        self.output.quiesce();
        self.staging.close();
        // Ratatui's Drop reports cursor errors to stderr. This backend is inert.
        let _ = self.terminal.show_cursor();
    }

    pub(crate) fn finish(&mut self, report: &str) -> RestoreOutcome {
        if self.finished {
            return RestoreOutcome {
                local_termios: Ok(()),
                protocol: ProtocolDelivery::Incomplete,
            };
        }
        self.finished = true;
        self.quiesce();
        let _ = self.raw.discard_input();
        let local_termios = self.raw.restore();
        let deadline = Instant::now() + FINAL_OUTPUT_BUDGET;
        let mut protocol = self.output.finish_pending(deadline);
        if protocol == ProtocolDelivery::Submitted {
            let mut bytes = Vec::new();
            self.modes.restore(|mode| mode.disable(&mut bytes));
            if shares_output_terminal() {
                bytes.extend_from_slice(&terminal_report(report));
            }
            protocol = self.output.finish_batch(bytes, deadline);
        }
        write_redirected_report(report, deadline);
        if !self.output.drain_or_abandon_leader_output(deadline) {
            protocol = ProtocolDelivery::Incomplete;
        }
        let _ = self.raw.discard_input();
        RestoreOutcome {
            local_termios,
            protocol,
        }
    }
}

impl Drop for OuterTerminal {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.finish("");
        }
    }
}

#[cfg(test)]
mod tests;
