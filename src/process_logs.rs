//! Per-Process Logs interaction.
//!
//! This module owns the state that must move together for one Process: Logs
//! representation, search, navigation, selection, following, and scrollbar
//! state. Callers send semantic input and receive a frame or an external copy
//! effect. Retained output remains the source of text.

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::log_view::{LogView, LogViewAction, OutputRepresentation};
use crate::output::RetainedOutput;
use crate::pipe_scroll::PipeScroll;
use crate::tui::{LogsScrollbar, PipeLine};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LogsInput {
    Ignored,
    Changed,
    Copy(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LogsNavigation {
    Changed,
    Exit,
    Unknown,
}

pub(crate) struct LogsFrame {
    pub(crate) lines: Vec<PipeLine>,
    pub(crate) status: Option<String>,
    pub(crate) editing: bool,
    pub(crate) search_active: bool,
    pub(crate) following: bool,
    pub(crate) has_selection: bool,
    pub(crate) scrollbar: Option<LogsScrollbar>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ProcessLogs {
    search: LogView,
    scroll: PipeScroll,
}

impl ProcessLogs {
    pub(crate) fn representation(&self, has_terminal: bool) -> OutputRepresentation {
        self.search.representation(has_terminal)
    }

    pub(crate) fn is_search_editing(&self) -> bool {
        self.search.is_editing()
    }

    pub(crate) fn refresh(&mut self, output: &RetainedOutput) -> bool {
        self.search.refresh(output)
    }

    pub(crate) fn handle_key(
        &mut self,
        key: KeyEvent,
        command_context: bool,
        has_terminal: bool,
        output: &RetainedOutput,
        pane_rows: usize,
    ) -> LogsInput {
        let action = self
            .search
            .handle_key(key, command_context, has_terminal, output);
        self.apply_search_action(action, output, pane_rows)
    }

    pub(crate) fn paste_search(&mut self, text: &str, output: &RetainedOutput) -> bool {
        let action = self.search.paste_search(text, output);
        !matches!(
            self.apply_search_action(action, output, 1),
            LogsInput::Ignored
        )
    }

    pub(crate) fn frame(&mut self, output: &RetainedOutput, pane_rows: usize) -> LogsFrame {
        let mut lines = self.scroll.window(output, pane_rows).to_vec();
        if let Some(current) = self.search.current_match()
            && let Some(line) = lines
                .iter_mut()
                .find(|line| line.source == Some((current.sequence, current.line)))
        {
            line.highlight = Some((
                line.content_offset + current.start,
                line.content_offset + current.end,
            ));
        }
        LogsFrame {
            lines,
            status: self.search.status(),
            editing: self.search.is_editing(),
            search_active: self.search.has_search(),
            following: self.scroll.following(),
            has_selection: self.scroll.has_selection(),
            scrollbar: self.scroll.scrollbar(),
        }
    }

    pub(crate) fn scroll_page(&mut self, page_rows: u16, direction: isize) {
        self.scroll.scroll_page(page_rows, direction);
    }

    pub(crate) fn follow(&mut self) {
        self.scroll.follow();
    }

    pub(crate) fn handle_navigation_key(
        &mut self,
        key: KeyEvent,
        page_rows: u16,
    ) -> LogsNavigation {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.scroll.scroll_lines(1, -1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll.scroll_lines(1, 1),
            KeyCode::PageUp => self.scroll.scroll_page(page_rows, -1),
            KeyCode::PageDown => self.scroll.scroll_page(page_rows, 1),
            KeyCode::Home => self.scroll.head(),
            KeyCode::End | KeyCode::Char('f') => self.scroll.follow(),
            KeyCode::Esc if self.scroll.clear_selection() => return LogsNavigation::Changed,
            KeyCode::Esc | KeyCode::Char('q') => return LogsNavigation::Exit,
            _ => return LogsNavigation::Unknown,
        }
        LogsNavigation::Changed
    }

    pub(crate) fn handle_mouse(
        &mut self,
        mouse: MouseEvent,
        area: Rect,
        repeats: usize,
        output: &RetainedOutput,
    ) -> bool {
        if self.scroll.handle_scrollbar_mouse(mouse, area, output) {
            return true;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let direction = if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                    -1
                } else {
                    1
                };
                self.scroll
                    .scroll_lines(3usize.saturating_mul(repeats), direction);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let Some((row, column)) = relative_mouse_position(mouse, area) else {
                    return false;
                };
                self.scroll.begin_selection(row, column);
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some((row, column)) = relative_mouse_position(mouse, area) else {
                    return false;
                };
                if !self.scroll.update_selection(row, column) {
                    return false;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some((row, column)) = relative_mouse_position(mouse, area) else {
                    return false;
                };
                if !self.scroll.finish_selection(row, column) {
                    return false;
                }
            }
            _ => return false,
        }
        true
    }

    pub(crate) fn following(&self) -> bool {
        self.scroll.following()
    }

    pub(crate) fn scrollbar_gesture_active(&self) -> bool {
        self.scroll.scrollbar_gesture_active()
    }

    fn apply_search_action(
        &mut self,
        action: LogViewAction,
        output: &RetainedOutput,
        pane_rows: usize,
    ) -> LogsInput {
        match action {
            LogViewAction::Ignored => LogsInput::Ignored,
            LogViewAction::Changed => LogsInput::Changed,
            LogViewAction::Pause => {
                self.scroll.clear_selection();
                self.scroll.pause();
                LogsInput::Changed
            }
            LogViewAction::Follow => {
                self.scroll.follow();
                LogsInput::Changed
            }
            LogViewAction::ShowMatch(found) => {
                self.scroll.show_source((found.sequence, found.line));
                LogsInput::Changed
            }
            LogViewAction::Copy => {
                let text = self.scroll.selected_text(output).unwrap_or_else(|| {
                    self.scroll
                        .window(output, pane_rows)
                        .iter()
                        .map(|line| line.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                });
                LogsInput::Copy(text)
            }
        }
    }
}

fn relative_mouse_position(mouse: MouseEvent, area: Rect) -> Option<(usize, usize)> {
    let row = mouse.row.checked_sub(area.y)?;
    let column = mouse.column.checked_sub(area.x)?;
    (row < area.height && column < area.width).then_some((usize::from(row), usize::from(column)))
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyModifiers, MouseEvent};

    use super::*;
    use crate::output::OutputViews;
    use crate::runtime::OutputStream;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn search_navigation_and_highlight_stay_in_one_process_view() {
        let output = OutputViews::new(1).for_process(0).unwrap();
        output.append_at(1, OutputStream::Stdout, 0, b"one\ntwo\n".to_vec());
        let retained = output.snapshot();
        let mut logs = ProcessLogs::default();

        assert_eq!(
            logs.handle_key(key(KeyCode::Char('/')), true, false, &retained, 10),
            LogsInput::Changed
        );
        assert_eq!(
            logs.handle_key(key(KeyCode::Char('t')), true, false, &retained, 10),
            LogsInput::Changed
        );
        assert_eq!(
            logs.handle_key(key(KeyCode::Enter), true, false, &retained, 10),
            LogsInput::Changed
        );

        let frame = logs.frame(&retained, 10);
        assert!(frame.status.is_some_and(|status| status.contains("1/1")));
        assert!(frame.lines.iter().any(|line| line.highlight.is_some()));
        assert!(!frame.following);
    }

    #[test]
    fn mouse_scroll_updates_the_owned_navigation_state() {
        let output = OutputViews::new(1).for_process(0).unwrap();
        output.append_at(
            1,
            OutputStream::Stdout,
            0,
            (0..20)
                .map(|line| format!("line-{line}\n"))
                .collect::<String>()
                .into_bytes(),
        );
        let retained = output.snapshot();
        let mut logs = ProcessLogs::default();
        logs.frame(&retained, 5);

        assert!(logs.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            },
            Rect::new(0, 0, 20, 5),
            1,
            &retained,
        ));
        logs.frame(&retained, 5);
        assert!(!logs.following());
    }
}
