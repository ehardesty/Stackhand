//! Per-Process Logs interaction.
//!
//! This module owns the state that must move together for one Process: Logs
//! representation, search, navigation, selection, following, and scrollbar
//! state. Callers send semantic input and receive a frame or an external copy
//! effect. Retained output remains the source of text.

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::log_view::{LogView, LogViewAction, OutputRepresentation, SearchDialogView};
use crate::output::RetainedOutput;
use crate::pipe_scroll::PipeScroll;
use crate::tui::{ConsoleScrollbar, PipeLine};

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
    pub(crate) search_dialog: Option<SearchDialogView>,
    pub(crate) search_active: bool,
    pub(crate) following: bool,
    pub(crate) has_selection: bool,
    pub(crate) scrollbar: Option<ConsoleScrollbar>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ProcessLogs {
    search: LogView,
    scroll: PipeScroll,
    generation: Option<u64>,
    unseen_output: bool,
}

impl ProcessLogs {
    pub(crate) fn representation(&self, has_terminal: bool) -> OutputRepresentation {
        self.search.representation(has_terminal)
    }

    pub(crate) fn is_search_editing(&self) -> bool {
        self.search.is_editing()
    }

    pub(crate) fn refresh(&mut self, output: &RetainedOutput) -> bool {
        if self
            .generation
            .is_some_and(|previous| previous != output.generation)
            && !self.scroll.following()
        {
            self.unseen_output = true;
        }
        self.generation = Some(output.generation);
        let previous = self.search.current_match();
        let changed = self.search.refresh(output);
        if previous.is_none()
            && let Some(found) = self.search.current_match()
        {
            self.scroll.show_source((found.sequence, found.line));
        }
        changed
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
        let mut status = output.scope();
        if self.scroll.expired() {
            status.push_str(" · paused page expired · f: follow · scroll: retained Logs");
        } else if !self.scroll.following() && self.unseen_output {
            status.push_str(" · new output below · f: follow");
        }
        if let Some(search) = self.search.status() {
            status.push_str(&format!(" · {search}"));
        }
        if self.scroll.following() {
            self.unseen_output = false;
        }
        LogsFrame {
            lines,
            status: Some(status),
            search_dialog: self.search.dialog(),
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
        self.unseen_output = false;
        self.search.follow();
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
            KeyCode::End | KeyCode::Char('f') => self.follow(),
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
                self.follow();
                LogsInput::Changed
            }
            LogViewAction::ShowMatch(found) => {
                self.scroll.show_source((found.sequence, found.line));
                LogsInput::Changed
            }
            LogViewAction::Copy => {
                let text = if self.scroll.has_selection() {
                    let Some(text) = self.scroll.selected_text(output) else {
                        return LogsInput::Changed;
                    };
                    text
                } else {
                    self.scroll
                        .window(output, pane_rows)
                        .iter()
                        .map(|line| line.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                };
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
        output.append_at(1, OutputStream::Stdout, 0, b"one\ntwo\n");
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
        assert!(
            frame
                .status
                .is_some_and(|status| status.contains("Match 1 of 1"))
        );
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
                .as_bytes(),
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

    #[test]
    fn a_match_found_in_a_later_search_batch_becomes_visible_and_selected() {
        let output = OutputViews::new(1).for_process(0).unwrap();
        output.append(1, OutputStream::Stdout, b"needle\n");
        let line = format!("{}\n", "ordinary ".repeat(100));
        for _ in 0..3_000 {
            output.append(1, OutputStream::Stdout, line.as_bytes());
        }
        let retained = output.snapshot();
        let mut logs = ProcessLogs::default();
        logs.handle_key(key(KeyCode::Char('/')), true, false, &retained, 5);
        logs.paste_search("needle", &retained);
        logs.handle_key(key(KeyCode::Enter), true, false, &retained, 5);
        assert!(
            logs.frame(&retained, 5)
                .status
                .unwrap()
                .contains("Searching older")
        );
        for _ in 0..30 {
            logs.refresh(&retained);
            let frame = logs.frame(&retained, 5);
            if frame
                .lines
                .iter()
                .any(|line| line.text.contains("needle") && line.highlight.is_some())
            {
                assert!(!frame.following);
                return;
            }
        }
        panic!("completed incremental search did not reveal its first match");
    }

    #[test]
    fn explicit_follow_survives_new_output_and_pending_search_results() {
        for delayed in [false, true] {
            for follow in [KeyCode::Char('f'), KeyCode::End] {
                let output = OutputViews::new(1).for_process(0).unwrap();
                output.append(1, OutputStream::Stdout, b"needle\n");
                if delayed {
                    let line = format!("{}\n", "ordinary ".repeat(100));
                    for _ in 0..3_000 {
                        output.append(1, OutputStream::Stdout, line.as_bytes());
                    }
                }
                let retained = output.snapshot();
                let mut logs = ProcessLogs::default();
                logs.handle_key(key(KeyCode::Char('/')), true, false, &retained, 5);
                logs.paste_search("needle", &retained);
                logs.handle_key(key(KeyCode::Enter), true, false, &retained, 5);
                if follow == KeyCode::End {
                    logs.handle_navigation_key(key(follow), 5);
                } else {
                    logs.handle_key(key(follow), true, false, &retained, 5);
                }
                assert!(logs.frame(&retained, 5).following);

                output.append(1, OutputStream::Stdout, b"live tail sentinel\n");
                let latest = output.snapshot();
                for _ in 0..30 {
                    logs.refresh(&latest);
                    let frame = logs.frame(&latest, 5);
                    assert!(frame.following, "{follow:?}, delayed={delayed}");
                    assert!(
                        frame
                            .lines
                            .last()
                            .unwrap()
                            .text
                            .contains("live tail sentinel")
                    );
                    assert!(frame.lines.iter().all(|line| line.highlight.is_none()));
                }
                // Following keeps the query available for explicit navigation.
                logs.handle_key(key(KeyCode::Char('n')), true, false, &latest, 5);
                let frame = logs.frame(&latest, 5);
                assert!(!frame.following);
                assert!(
                    frame
                        .lines
                        .iter()
                        .any(|line| line.text.contains("needle") && line.highlight.is_some())
                );
            }
        }
    }

    #[test]
    fn copying_an_unavailable_selection_does_not_substitute_the_frozen_page() {
        let output = OutputViews::new(1).for_process(0).unwrap();
        for row in 0..20 {
            output.append(1, OutputStream::Stdout, format!("line-{row}\n").as_bytes());
        }
        let retained = output.snapshot();
        let mut logs = ProcessLogs::default();
        logs.scroll.head();
        logs.frame(&retained, 5);
        logs.scroll.begin_selection(1, 18);
        logs.scroll.scroll_lines(4, 1);
        logs.frame(&retained, 5);
        logs.scroll.finish_selection(2, 30);
        assert!(matches!(
            logs.handle_key(key(KeyCode::Char('c')), true, false, &retained, 5),
            LogsInput::Copy(_)
        ));
        let expired = RetainedOutput {
            chunks: retained.chunks.iter().skip(10).cloned().collect(),
            truncated: true,
            generation: retained.generation + 1,
            ..retained.clone()
        };
        logs.frame(&expired, 5);
        assert!(logs.scroll.expired());
        assert!(
            !matches!(
                logs.handle_key(key(KeyCode::Char('c')), true, false, &expired, 5),
                LogsInput::Copy(_)
            ),
            "an unavailable selection must not copy unselected text"
        );
    }
}
