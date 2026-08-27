use std::time::Instant;

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent};
use ratatui::layout::Rect;

use crate::runtime::TerminalHandle;
use crate::terminal::{CopyRequest, PasteCompletion, PasteRequest};
use crate::tui::{ConsolePaneKind, ConsoleViewMode, ConsoleViewState, ConsoleWarning, MouseRouter};

/// One requested move of the Process-list selection. Command modes carry
/// only this request; the app event loop owns the selection itself, so a
/// keypress never reaches the PTY child or changes lifecycle truth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionMove {
    Up,
    Down,
}

pub use crate::pipe_scroll::PipeScroll;

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

/// A key meaning shared by terminal and read-only console panes. The pane
/// handlers keep terminal effects and pipe-view effects separate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConsoleCommand {
    MoveSelection(SelectionMove),
    ScrollPage(isize),
    Follow,
    Lifecycle(LifecycleCommand),
    EnterSelection,
    Escape,
}

fn console_command(mode: ConsoleViewMode, code: KeyCode) -> Option<ConsoleCommand> {
    if !matches!(mode, ConsoleViewMode::AppCommand | ConsoleViewMode::Scroll) {
        return None;
    }
    match code {
        KeyCode::Up | KeyCode::Char('k') if mode == ConsoleViewMode::AppCommand => {
            Some(ConsoleCommand::MoveSelection(SelectionMove::Up))
        }
        KeyCode::Down | KeyCode::Char('j') if mode == ConsoleViewMode::AppCommand => {
            Some(ConsoleCommand::MoveSelection(SelectionMove::Down))
        }
        KeyCode::PageUp => Some(ConsoleCommand::ScrollPage(-1)),
        KeyCode::PageDown => Some(ConsoleCommand::ScrollPage(1)),
        KeyCode::Char('f') => Some(ConsoleCommand::Follow),
        KeyCode::Char('v') => Some(ConsoleCommand::EnterSelection),
        KeyCode::Char(c) => lifecycle_request_for(c).map(ConsoleCommand::Lifecycle),
        KeyCode::Esc => Some(ConsoleCommand::Escape),
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

    /// Set which console pane the view renders; the footer shows the pane's
    /// controls and the pane-aware input gating reads it. A pane change
    /// invalidates the pane-scoped routing warnings.
    pub fn set_pane(&mut self, pane: ConsolePaneKind) {
        if self.view.pane != pane {
            Self::clear_pane_warnings(&mut self.view);
        }
        self.view.pane = pane;
    }

    /// Clear the warnings that describe the selected pane's input routing.
    /// Call when the selected Process changes; use before asserting a
    /// fresh rejection so the asserted warning is the new one.
    pub fn clear_pane_warning(&mut self) {
        Self::clear_pane_warnings(&mut self.view);
    }

    /// The routing warnings belong to the selected pane and Process: a
    /// pane or selection change makes them stale.
    fn clear_pane_warnings(view: &mut ConsoleViewState) {
        if matches!(
            view.warning,
            Some(
                ConsoleWarning::InputDisabled
                    | ConsoleWarning::PipeReadOnly
                    | ConsoleWarning::SelectionUnavailable
                    | ConsoleWarning::PasteRejected
            )
        ) {
            view.warning = None;
        }
    }

    /// Drain every Process-selection move queued by command modes.
    pub fn take_selection_moves(&mut self) -> Vec<SelectionMove> {
        std::mem::take(&mut self.selection_requests)
    }

    /// Apply every queued Process-list move to application selection state.
    /// Movement clamps to Project bounds. Per-Process scroll state is not
    /// changed, and pane warnings are cleared after each handled move.
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

    /// Drain every lifecycle command queued by command modes. The app event
    /// loop dispatches each one for the currently selected Process.
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

    /// Route one key event for the selected pane through the input
    /// policy: the production boundary that decides whether the event
    /// reaches a terminal session. A terminal pane delivers only while
    /// its Process's focused input is enabled; pipe and empty panes
    /// reject child input visibly while command mode keeps working.
    pub fn route_pane_key(
        &mut self,
        pane: ConsolePaneKind,
        input_focused: bool,
        key: KeyEvent,
        session: Option<&TerminalHandle<'_>>,
        pipe_scroll: &mut Option<PipeScroll>,
        page_rows: u16,
    ) -> bool {
        // Routing a key into a pane makes that pane the current one; a
        // pane change drops the stale pane-scoped warning.
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
            ConsolePaneKind::Pipe | ConsolePaneKind::Empty => {
                self.handle_key_read_only(key, pipe_scroll, page_rows)
            }
        }
    }

    pub fn handle_key(
        &mut self,
        key: KeyEvent,
        session: &TerminalHandle<'_>,
        page_rows: u16,
    ) -> bool {
        if key.kind == KeyEventKind::Press
            && self.view.mode == ConsoleViewMode::Selection
            && key.code == KeyCode::Char('y')
        {
            self.copy_requests.push(session.request_copy());
            self.view.warning = None;
            return true;
        }

        if key.kind != KeyEventKind::Press {
            if self.view.mode == ConsoleViewMode::ChildInput {
                return self.record_input_result(session.send_key(key));
            }
            return false;
        }

        match self.view.mode {
            ConsoleViewMode::ChildInput if is_command_leader(key) => {
                self.view.mode = ConsoleViewMode::AppCommand;
                true
            }
            ConsoleViewMode::ChildInput => self.record_input_result(session.send_key(key)),
            ConsoleViewMode::AppCommand | ConsoleViewMode::Scroll => {
                let Some(command) = console_command(self.view.mode, key.code) else {
                    return false;
                };
                self.apply_terminal_command(command, session, page_rows)
            }
            ConsoleViewMode::Selection => self.handle_selection_command(key, session),
        }
    }

    /// Route one key when the selected pane has no terminal session: the
    /// pipe output pane, or a Process without an active Run. The command
    /// leader still enters command mode and plain child input is rejected
    /// visibly; selection moves and pipe scrolling work without a session.
    /// Route one key into a read-only pane (pipe output or no active
    /// Run). Child input is a Press event; repeat and release events
    /// carry no input of their own and are consumed here. The Press
    /// rejection warning they would trigger is already visible from the
    /// original press until the pane or selection changes.
    pub fn handle_key_read_only(
        &mut self,
        key: KeyEvent,
        scroll: &mut Option<PipeScroll>,
        page_rows: u16,
    ) -> bool {
        if key.kind != KeyEventKind::Press {
            return false;
        }
        match self.view.mode {
            ConsoleViewMode::ChildInput => {
                if is_command_leader(key) {
                    self.view.mode = ConsoleViewMode::AppCommand;
                    return true;
                }
                self.view.warning = Some(ConsoleWarning::PipeReadOnly);
                true
            }
            ConsoleViewMode::AppCommand | ConsoleViewMode::Scroll => {
                let Some(command) = console_command(self.view.mode, key.code) else {
                    return false;
                };
                self.apply_read_only_command(command, scroll, page_rows)
            }
            ConsoleViewMode::Selection => {
                // 's' is rejected in read-only panes, so this mode is
                // unreachable; Esc is kept for a future path that enters it.
                self.view.mode = ConsoleViewMode::AppCommand;
                key.code == KeyCode::Esc
            }
        }
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

    pub fn handle_mouse(
        &mut self,
        mouse: MouseEvent,
        area: Rect,
        child_tracking: bool,
        session: &TerminalHandle<'_>,
    ) -> bool {
        let Some(route) = self.mouse.route(
            mouse,
            area,
            self.view.mode,
            child_tracking,
            self.selection_clock.elapsed(),
        ) else {
            return false;
        };
        self.view.stackhand_mouse_gesture = route.stackhand_gesture_active;
        if route.changes_history_view {
            self.view.following = false;
        }
        if self.record_input_result(session.send_mouse(route.event)) {
            self.view.stackhand_mouse_gesture = false;
        }
        true
    }

    fn record_input_result(&mut self, result: Result<(), crate::runtime::InputRejected>) -> bool {
        if result.is_err() {
            self.view.warning = Some(ConsoleWarning::InputRejected);
            true
        } else {
            false
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
        command: ConsoleCommand,
        session: &TerminalHandle<'_>,
        page_rows: u16,
    ) -> bool {
        match command {
            ConsoleCommand::MoveSelection(direction) => {
                self.selection_requests.push(direction);
            }
            ConsoleCommand::ScrollPage(direction) => {
                scroll_page(session, page_rows, direction);
                self.view.mode = ConsoleViewMode::Scroll;
                self.view.following = false;
            }
            ConsoleCommand::Follow => self.return_to_live_tail(session),
            ConsoleCommand::Lifecycle(request) => self.lifecycle_requests.push(request),
            ConsoleCommand::EnterSelection => self.view.mode = ConsoleViewMode::Selection,
            ConsoleCommand::Escape => {
                self.view.mode = match self.view.mode {
                    ConsoleViewMode::AppCommand => ConsoleViewMode::ChildInput,
                    ConsoleViewMode::Scroll => ConsoleViewMode::AppCommand,
                    _ => unreachable!("only command modes produce Escape"),
                };
            }
        }
        true
    }

    fn apply_read_only_command(
        &mut self,
        command: ConsoleCommand,
        scroll: &mut Option<PipeScroll>,
        page_rows: u16,
    ) -> bool {
        match command {
            ConsoleCommand::MoveSelection(direction) => {
                self.selection_requests.push(direction);
            }
            ConsoleCommand::ScrollPage(direction) => {
                scroll
                    .get_or_insert_default()
                    .scroll_page(page_rows, direction);
                self.view.mode = ConsoleViewMode::Scroll;
            }
            ConsoleCommand::Follow => {
                scroll.get_or_insert_default().follow();
                self.view.mode = ConsoleViewMode::ChildInput;
            }
            ConsoleCommand::Lifecycle(request) => self.lifecycle_requests.push(request),
            ConsoleCommand::EnterSelection => {
                self.view.warning = Some(ConsoleWarning::SelectionUnavailable);
            }
            ConsoleCommand::Escape => {
                self.view.mode = match self.view.mode {
                    ConsoleViewMode::AppCommand => ConsoleViewMode::ChildInput,
                    ConsoleViewMode::Scroll => ConsoleViewMode::AppCommand,
                    _ => unreachable!("only command modes produce Escape"),
                };
            }
        }
        true
    }

    fn handle_selection_command(&mut self, key: KeyEvent, session: &TerminalHandle<'_>) -> bool {
        match key.code {
            KeyCode::Char('a') => {
                session.select_all();
                self.view.following = false;
                true
            }
            KeyCode::Esc => {
                session.clear_selection();
                self.view.mode = ConsoleViewMode::AppCommand;
                true
            }
            _ => false,
        }
    }

    fn return_to_live_tail(&mut self, session: &TerminalHandle<'_>) {
        session.follow_live();
        self.view.following = true;
        self.view.mode = ConsoleViewMode::ChildInput;
    }
}

pub(crate) fn is_command_leader(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('a') && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// The application's child-input gate for the selected terminal pane. True
/// when this key attempt is rejected visibly instead of delivered: the
/// Process's focused input is disabled and the key is plain child input.
/// The command leader is never rejected, so the Process list stays
/// reachable from a disabled pane.
/// Whether the event is child input the selected PTY Process must never
/// receive: its focused input is disabled, and the event is a plain child
/// key of any kind in child-input mode. The leader's Press is the only
/// exception: it moves the user into the command UI, where nothing
/// reaches the child. Repeat and release events are gated too, so no
/// byte of any kind reaches a disabled Process.
fn child_input_rejected(view: ConsoleViewState, input_focused: bool, key: &KeyEvent) -> bool {
    !input_focused
        && view.mode == ConsoleViewMode::ChildInput
        && !(key.kind == KeyEventKind::Press && is_command_leader(*key))
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
