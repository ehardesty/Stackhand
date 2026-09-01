use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;

use crate::pipe_scroll::{scale, scrollbar_thumb};
use crate::process_logs::{LogsNavigation, ProcessLogs};
use crate::runtime::TerminalHandle;
use crate::terminal::{
    CopyRequest, OwnedTerminalScrollbar, PasteCompletion, PasteRequest,
    SelectionDirection as CopyDirection,
};
use crate::tui::{
    ConsolePaneKind, ConsoleScrollbar, ConsoleViewMode, ConsoleViewState, ConsoleWarning,
    MouseRouter,
};

/// One requested move of the Process-list selection. The app event loop owns
/// the selected Process, so navigation never mutates Supervisor truth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionMove {
    Up,
    Down,
}

/// One requested lifecycle command for the currently selected Process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleCommand {
    Start,
    Stop,
    Restart,
}

fn lifecycle_request_for(c: char) -> Option<LifecycleCommand> {
    match c {
        's' => Some(LifecycleCommand::Start),
        'x' => Some(LifecycleCommand::Stop),
        'r' => Some(LifecycleCommand::Restart),
        _ => None,
    }
}

/// A key owned by Process-list focus. Terminal and pipe panes apply the same
/// meaning through their own output and selection seams.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcessCommand {
    MoveSelection(SelectionMove),
    ScrollPage(isize),
    Follow,
    Lifecycle(LifecycleCommand),
    EnterCopy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalScrollbarGesture {
    Track,
    Thumb { grab_row: usize },
}

fn process_command(code: KeyCode) -> Option<ProcessCommand> {
    match code {
        KeyCode::Up | KeyCode::Char('k') => Some(ProcessCommand::MoveSelection(SelectionMove::Up)),
        KeyCode::Down | KeyCode::Char('j') => {
            Some(ProcessCommand::MoveSelection(SelectionMove::Down))
        }
        KeyCode::PageUp => Some(ProcessCommand::ScrollPage(-1)),
        KeyCode::PageDown => Some(ProcessCommand::ScrollPage(1)),
        KeyCode::Char('f') => Some(ProcessCommand::Follow),
        KeyCode::Char('v') => Some(ProcessCommand::EnterCopy),
        KeyCode::Char(c) => lifecycle_request_for(c).map(ProcessCommand::Lifecycle),
        _ => None,
    }
}

pub(crate) struct ConsoleInteraction {
    view: ConsoleViewState,
    mouse: MouseRouter,
    paste_requests: Vec<PasteRequest>,
    copy_requests: Vec<CopyRequest>,
    selection_requests: Vec<SelectionMove>,
    lifecycle_requests: Vec<LifecycleCommand>,
    selection_clock: Instant,
    last_stackhand_press: Option<(u16, u16, Duration)>,
    copy_return_mode: ConsoleViewMode,
    terminal_scrollbar_gesture: Option<TerminalScrollbarGesture>,
}

impl Default for ConsoleInteraction {
    fn default() -> Self {
        Self {
            view: ConsoleViewState::default(),
            mouse: MouseRouter::default(),
            paste_requests: Vec::new(),
            copy_requests: Vec::new(),
            selection_requests: Vec::new(),
            lifecycle_requests: Vec::new(),
            selection_clock: Instant::now(),
            last_stackhand_press: None,
            copy_return_mode: ConsoleViewMode::ProcessList,
            terminal_scrollbar_gesture: None,
        }
    }
}

impl ConsoleInteraction {
    pub fn view(&self) -> ConsoleViewState {
        self.view
    }

    pub fn warn(&mut self, warning: ConsoleWarning) {
        self.view.warning = Some(warning);
    }

    pub fn set_pane(&mut self, pane: ConsolePaneKind) {
        if self.view.pane != pane {
            Self::clear_pane_warnings(&mut self.view);
        }
        self.view.pane = pane;
    }

    pub fn clear_pane_warning(&mut self) {
        Self::clear_pane_warnings(&mut self.view);
    }

    pub fn clear_warning(&mut self) {
        self.view.warning = None;
    }

    fn clear_pane_warnings(view: &mut ConsoleViewState) {
        if matches!(
            view.warning,
            Some(
                ConsoleWarning::InputDisabled
                    | ConsoleWarning::LogsCommandOnly
                    | ConsoleWarning::SelectionUnavailable
                    | ConsoleWarning::PasteRejected
            )
        ) {
            view.warning = None;
        }
    }

    /// Focus the Process list. Leaving Copy also clears its terminal
    /// selection so stale selected cells do not look active.
    pub fn focus_process_list(&mut self, session: Option<&TerminalHandle<'_>>) {
        if self.view.mode == ConsoleViewMode::Copy
            && let Some(session) = session
        {
            session.clear_selection();
        }
        self.view.mode = ConsoleViewMode::ProcessList;
        self.view.stackhand_mouse_gesture = false;
        self.terminal_scrollbar_gesture = None;
        self.last_stackhand_press = None;
    }

    /// Focus the selected console. Only an input-enabled PTY will forward
    /// unbound keys; pipe and empty consoles stay read-only.
    pub fn focus_console(&mut self, session: Option<&TerminalHandle<'_>>) {
        let reset_mouse_clicks = self.view.mode != ConsoleViewMode::Console;
        if self.view.mode == ConsoleViewMode::Copy
            && let Some(session) = session
        {
            session.clear_selection();
        }
        self.view.mode = ConsoleViewMode::Console;
        self.view.stackhand_mouse_gesture = false;
        self.terminal_scrollbar_gesture = None;
        if reset_mouse_clicks {
            self.last_stackhand_press = None;
        }
    }

    pub fn accepts_child_input(&self, input_focused: bool) -> bool {
        self.view.mode == ConsoleViewMode::Console
            && self.view.pane == ConsolePaneKind::Terminal
            && input_focused
    }

    pub fn mouse_gesture_active(&self) -> bool {
        self.mouse.gesture_active()
    }

    pub fn terminal_scrollbar_gesture_active(&self) -> bool {
        self.terminal_scrollbar_gesture.is_some()
    }

    pub fn cancel_terminal_scrollbar_gesture(&mut self) {
        self.terminal_scrollbar_gesture = None;
    }

    pub fn take_selection_moves(&mut self) -> Vec<SelectionMove> {
        std::mem::take(&mut self.selection_requests)
    }

    pub fn apply_selection_moves(&mut self, selected: &mut usize, process_count: usize) -> bool {
        if process_count == 0 {
            return false;
        }
        *selected = (*selected).min(process_count - 1);
        let requests = self.take_selection_moves();
        for request in &requests {
            *selected = match request {
                SelectionMove::Up => selected.saturating_sub(1),
                SelectionMove::Down => (*selected + 1).min(process_count - 1),
            };
            self.clear_pane_warning();
        }
        !requests.is_empty()
    }

    pub fn take_lifecycle_commands(&mut self) -> Vec<LifecycleCommand> {
        std::mem::take(&mut self.lifecycle_requests)
    }

    pub fn poll_requests(&mut self) -> bool {
        let mut changed = self.poll_paste_requests();
        let mut copy_results = Vec::new();
        self.copy_requests.retain(|request| {
            let Some(result) = request.poll() else {
                return true;
            };
            copy_results.push(result);
            false
        });
        for result in copy_results {
            self.view.warning = copy_warning(result, write_clipboard);
            changed = true;
        }
        changed
    }

    pub fn route_pane_key(
        &mut self,
        pane: ConsolePaneKind,
        input_focused: bool,
        key: KeyEvent,
        session: Option<&TerminalHandle<'_>>,
        logs: &mut ProcessLogs,
        page_rows: u16,
    ) -> bool {
        self.set_pane(pane);
        match pane {
            ConsolePaneKind::Terminal => {
                let session = session.expect("a terminal pane owns a live session");
                if child_input_rejected(self.view(), input_focused, &key) {
                    self.warn(ConsoleWarning::InputDisabled);
                    return true;
                }
                self.handle_key(key, session, page_rows)
            }
            ConsolePaneKind::Pipe => self.handle_key_read_only(key, logs, page_rows),
        }
    }

    pub fn handle_key(
        &mut self,
        key: KeyEvent,
        session: &TerminalHandle<'_>,
        page_rows: u16,
    ) -> bool {
        // Stackhand owns every event in the Ctrl-A key cycle. In terminals
        // that report event types, forwarding the release after focus moves
        // to the console would leak part of the binding to the child.
        if is_focus_toggle(key) {
            if key.kind == KeyEventKind::Press {
                match self.view.mode {
                    ConsoleViewMode::ProcessList => self.focus_console(Some(session)),
                    ConsoleViewMode::Console | ConsoleViewMode::Copy => {
                        self.focus_process_list(Some(session));
                    }
                }
            }
            return true;
        }

        if key.kind != KeyEventKind::Press {
            return if self.view.mode == ConsoleViewMode::Console {
                self.record_input_result(session.send_key(key))
            } else {
                false
            };
        }

        match self.view.mode {
            ConsoleViewMode::Console => self.record_input_result(session.send_key(key)),
            ConsoleViewMode::ProcessList => {
                let Some(command) = process_command(key.code) else {
                    return false;
                };
                self.apply_terminal_command(command, session, page_rows)
            }
            ConsoleViewMode::Copy => self.handle_copy_key(key, session, page_rows),
        }
    }

    pub fn handle_key_read_only(
        &mut self,
        key: KeyEvent,
        logs: &mut ProcessLogs,
        page_rows: u16,
    ) -> bool {
        if key.kind != KeyEventKind::Press {
            return false;
        }
        if is_focus_toggle(key) {
            self.view.mode = match self.view.mode {
                ConsoleViewMode::ProcessList => ConsoleViewMode::Console,
                ConsoleViewMode::Console | ConsoleViewMode::Copy => ConsoleViewMode::ProcessList,
            };
            return true;
        }
        match self.view.mode {
            ConsoleViewMode::Console if self.view.pane == ConsolePaneKind::Pipe => {
                self.handle_logs_focus_key(key, logs, page_rows)
            }
            ConsoleViewMode::Console => {
                self.view.warning = Some(ConsoleWarning::InputDisabled);
                true
            }
            ConsoleViewMode::ProcessList => {
                let Some(command) = process_command(key.code) else {
                    return false;
                };
                self.apply_read_only_command(command, logs, page_rows)
            }
            ConsoleViewMode::Copy => {
                self.view.mode = ConsoleViewMode::ProcessList;
                true
            }
        }
    }

    /// Copy the visible Logs projection. Logs coordinates never cross the
    /// terminal seam, and a clipboard failure leaves the visible text intact.
    pub fn copy_logs(&mut self, text: String) {
        self.view.warning = if text.is_empty() {
            Some(ConsoleWarning::NoLogsToCopy)
        } else {
            write_clipboard(text)
                .err()
                .map(|_| ConsoleWarning::ClipboardFailed)
        };
    }

    pub fn handle_paste(&mut self, data: &str, session: &TerminalHandle<'_>) {
        match session.send_paste(data) {
            Ok(request) => {
                self.paste_requests.push(request);
                self.view.warning = None;
            }
            Err(_) => self.view.warning = Some(ConsoleWarning::PasteRejected),
        }
    }

    pub fn handle_read_only_mouse(
        &mut self,
        mouse: MouseEvent,
        area: Rect,
        repeats: usize,
        logs: &mut ProcessLogs,
        output: &crate::output::RetainedOutput,
    ) -> bool {
        if !logs.handle_mouse(mouse, area, repeats, output) {
            return false;
        }
        self.clear_warning();
        true
    }

    /// Own hit testing and drag mapping for the live PTY scrollbar. The
    /// scrollbar uses Ghostty's absolute row space, so the terminal remains
    /// the source of truth for retained history and viewport position.
    pub fn handle_terminal_scrollbar_mouse(
        &mut self,
        mouse: MouseEvent,
        area: Rect,
        session: &TerminalHandle<'_>,
    ) -> bool {
        if self.terminal_scrollbar_gesture.is_some() {
            match mouse.kind {
                MouseEventKind::Up(MouseButton::Left) => {
                    self.terminal_scrollbar_gesture = None;
                    return true;
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    let ghostty_scrollbar = session.scrollbar();
                    let Some(scrollbar) = ConsoleScrollbar::from_terminal(ghostty_scrollbar) else {
                        return true;
                    };
                    let row = usize::from(
                        mouse
                            .row
                            .clamp(area.y, area.bottom().saturating_sub(1))
                            .saturating_sub(area.y),
                    );
                    if let Some(TerminalScrollbarGesture::Thumb { grab_row }) =
                        self.terminal_scrollbar_gesture
                    {
                        self.set_terminal_scrollbar_thumb(
                            session,
                            ghostty_scrollbar,
                            scrollbar,
                            usize::from(area.height),
                            row.saturating_sub(grab_row),
                        );
                    }
                    return true;
                }
                _ => {}
            }
        }

        let ghostty_scrollbar = session.scrollbar();
        let Some(scrollbar) = ConsoleScrollbar::from_terminal(ghostty_scrollbar) else {
            return false;
        };
        if area.width <= 1 || area.height == 0 {
            return false;
        }
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            || mouse.column != area.right().saturating_sub(1)
            || mouse.row < area.y
            || mouse.row >= area.bottom()
        {
            return false;
        }

        if self.view.mode == ConsoleViewMode::ProcessList {
            self.focus_console(Some(session));
        }

        let track_rows = usize::from(area.height);
        let row = usize::from(mouse.row - area.y);
        let (thumb_start, thumb_len) = scrollbar_thumb(scrollbar, track_rows);
        if row >= thumb_start && row < thumb_start.saturating_add(thumb_len) {
            self.terminal_scrollbar_gesture = Some(TerminalScrollbarGesture::Thumb {
                grab_row: row - thumb_start,
            });
            self.view.following = ghostty_scrollbar.offset
                >= ghostty_scrollbar
                    .total
                    .saturating_sub(ghostty_scrollbar.len);
        } else {
            self.set_terminal_scrollbar_thumb(
                session,
                ghostty_scrollbar,
                scrollbar,
                track_rows,
                row.saturating_sub(thumb_len / 2),
            );
            self.terminal_scrollbar_gesture = Some(TerminalScrollbarGesture::Track);
        }
        self.clear_warning();
        true
    }

    fn set_terminal_scrollbar_thumb(
        &mut self,
        session: &TerminalHandle<'_>,
        ghostty_scrollbar: OwnedTerminalScrollbar,
        scrollbar: ConsoleScrollbar,
        track_rows: usize,
        thumb_start: usize,
    ) {
        let (_, thumb_len) = scrollbar_thumb(scrollbar, track_rows);
        let max_thumb_start = track_rows.saturating_sub(thumb_len);
        let max_position = scrollbar.content_length.saturating_sub(1);
        let position = scale(
            thumb_start.min(max_thumb_start),
            max_position,
            max_thumb_start,
        );
        let max_row = ghostty_scrollbar
            .total
            .saturating_sub(ghostty_scrollbar.len);
        let row = position.min(max_row);
        session.scroll_to_row(row);
        self.view.following = row >= max_row;
    }

    pub fn handle_mouse(
        &mut self,
        mouse: MouseEvent,
        area: Rect,
        child_tracking: bool,
        session: &TerminalHandle<'_>,
    ) -> bool {
        let event_time = self.selection_clock.elapsed();
        let Some(route) = self
            .mouse
            .route(mouse, area, self.view.mode, child_tracking, event_time)
        else {
            return false;
        };
        self.view.stackhand_mouse_gesture = route.stackhand_gesture_active;
        if route.changes_history_view {
            self.view.following = false;
        }
        let repeated_press = route.event.stackhand_owned
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && self
                .last_stackhand_press
                .is_some_and(|(column, row, prior_time)| {
                    u32::from(column.abs_diff(mouse.column)) + u32::from(row.abs_diff(mouse.row))
                        <= 1
                        && event_time.saturating_sub(prior_time) <= Duration::from_millis(500)
                });
        if route.event.stackhand_owned
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        {
            self.last_stackhand_press = Some((mouse.column, mouse.row, event_time));
        }
        let starts_copy = route.event.stackhand_owned
            && (matches!(mouse.kind, MouseEventKind::Drag(MouseButton::Left)) || repeated_press);
        let enters_copy = starts_copy && self.view.mode != ConsoleViewMode::Copy;
        if enters_copy {
            self.copy_return_mode = self.view.mode;
        }
        if starts_copy {
            self.view.mode = ConsoleViewMode::Copy;
            self.view.warning = None;
        }
        if self.record_input_result(session.send_mouse(route.event)) {
            self.view.stackhand_mouse_gesture = false;
        } else if enters_copy || repeated_press {
            // The mouse selection supplies the anchor and endpoint. Enabling
            // keyboard navigation makes that endpoint visible and lets Vim
            // movement extend it without replacing the mouse selection.
            session.start_keyboard_selection();
        }
        true
    }

    fn record_input_result(&mut self, result: Result<(), crate::runtime::InputRejected>) -> bool {
        if result.is_err() {
            self.view.warning = Some(ConsoleWarning::InputRejected);
            true
        } else {
            self.view.warning.take().is_some()
        }
    }

    fn poll_paste_requests(&mut self) -> bool {
        let mut failed = false;
        self.paste_requests
            .retain_mut(|request| match request.poll() {
                Some(PasteCompletion::Delivered) => false,
                Some(PasteCompletion::Failed(_)) => {
                    failed = true;
                    false
                }
                None => true,
            });
        if failed {
            self.view.warning = Some(ConsoleWarning::PasteDeliveryFailed);
        }
        failed
    }

    fn apply_terminal_command(
        &mut self,
        command: ProcessCommand,
        session: &TerminalHandle<'_>,
        page_rows: u16,
    ) -> bool {
        match command {
            ProcessCommand::MoveSelection(direction) => self.selection_requests.push(direction),
            ProcessCommand::ScrollPage(direction) => {
                scroll_page(session, page_rows, direction);
                self.view.following = false;
            }
            ProcessCommand::Follow => self.return_to_live_tail(session),
            ProcessCommand::Lifecycle(request) => self.lifecycle_requests.push(request),
            ProcessCommand::EnterCopy => {
                session.start_keyboard_selection();
                self.copy_return_mode = self.view.mode;
                self.view.mode = ConsoleViewMode::Copy;
                self.view.following = false;
            }
        }
        true
    }

    fn apply_read_only_command(
        &mut self,
        command: ProcessCommand,
        logs: &mut ProcessLogs,
        page_rows: u16,
    ) -> bool {
        self.clear_warning();
        match command {
            ProcessCommand::MoveSelection(direction) => self.selection_requests.push(direction),
            ProcessCommand::ScrollPage(direction) => logs.scroll_page(page_rows, direction),
            ProcessCommand::Follow => logs.follow(),
            ProcessCommand::Lifecycle(request) => self.lifecycle_requests.push(request),
            ProcessCommand::EnterCopy => {
                self.view.warning = Some(ConsoleWarning::SelectionUnavailable);
            }
        }
        true
    }

    fn handle_logs_focus_key(
        &mut self,
        key: KeyEvent,
        logs: &mut ProcessLogs,
        page_rows: u16,
    ) -> bool {
        match logs.handle_navigation_key(key, page_rows) {
            LogsNavigation::Changed => self.clear_warning(),
            LogsNavigation::Exit => {
                self.focus_process_list(None);
                self.clear_warning();
            }
            LogsNavigation::Unknown => {
                self.view.warning = Some(ConsoleWarning::LogsCommandOnly);
            }
        }
        true
    }

    fn handle_copy_key(
        &mut self,
        key: KeyEvent,
        session: &TerminalHandle<'_>,
        page_rows: u16,
    ) -> bool {
        let direction = match key.code {
            KeyCode::Left | KeyCode::Char('h') => Some(CopyDirection::Left),
            KeyCode::Down | KeyCode::Char('j') => Some(CopyDirection::Down),
            KeyCode::Up | KeyCode::Char('k') => Some(CopyDirection::Up),
            KeyCode::Right | KeyCode::Char('l') => Some(CopyDirection::Right),
            _ => None,
        };
        if let Some(direction) = direction {
            session.move_keyboard_selection(direction);
            self.view.following = false;
            return true;
        }
        match key.code {
            KeyCode::Char('v') => {
                session.toggle_keyboard_selection();
                true
            }
            KeyCode::Char('a') => {
                session.select_all();
                self.view.following = false;
                true
            }
            KeyCode::Char('c') | KeyCode::Char('y') => {
                self.copy_requests.push(session.request_copy());
                self.view.warning = None;
                true
            }
            KeyCode::PageUp => {
                scroll_page(session, page_rows, -1);
                self.view.following = false;
                true
            }
            KeyCode::PageDown => {
                scroll_page(session, page_rows, 1);
                self.view.following = false;
                true
            }
            KeyCode::Char('q') | KeyCode::Esc => {
                session.clear_selection();
                self.view.mode = self.copy_return_mode;
                self.view.stackhand_mouse_gesture = false;
                self.last_stackhand_press = None;
                true
            }
            _ => false,
        }
    }

    fn return_to_live_tail(&mut self, session: &TerminalHandle<'_>) {
        session.follow_live();
        self.view.following = true;
    }
}

pub(crate) fn is_focus_toggle(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('a') && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn child_input_rejected(view: ConsoleViewState, input_focused: bool, key: &KeyEvent) -> bool {
    !input_focused
        && view.mode == ConsoleViewMode::Console
        && !(key.kind == KeyEventKind::Press && is_focus_toggle(*key))
}

fn scroll_page(session: &TerminalHandle<'_>, page_rows: u16, direction: isize) {
    let page = isize::try_from(page_rows.saturating_sub(1).max(1))
        .expect("u16 page size always fits in isize");
    session.scroll_lines(direction * page);
}

fn copy_warning(
    result: Result<Option<String>, String>,
    write: impl FnOnce(String) -> Result<()>,
) -> Option<ConsoleWarning> {
    match result {
        Ok(Some(text)) if !text.is_empty() => {
            write(text).err().map(|_| ConsoleWarning::ClipboardFailed)
        }
        Ok(_) => Some(ConsoleWarning::NothingSelected),
        Err(_) => Some(ConsoleWarning::ClipboardFailed),
    }
}

fn write_clipboard(text: String) -> Result<()> {
    let mut clipboard = arboard::Clipboard::new().context("system clipboard is unavailable")?;
    clipboard
        .set_text(text)
        .context("could not write selected text to the system clipboard")
}

#[cfg(test)]
mod tests;
