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

pub(crate) struct ConsoleInteraction {
    view: ConsoleViewState,
    mouse: MouseRouter,
    paste_requests: Vec<PasteRequest>,
    copy_requests: Vec<CopyRequest>,
    selection_requests: Vec<SelectionMove>,
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
            ConsoleViewMode::AppCommand => self.handle_app_command(key, session, page_rows),
            ConsoleViewMode::Scroll => self.handle_scroll_command(key, session, page_rows),
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
            ConsoleViewMode::AppCommand => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.selection_requests.push(SelectionMove::Up);
                    true
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.selection_requests.push(SelectionMove::Down);
                    true
                }
                KeyCode::PageUp => {
                    scroll.get_or_insert_default().scroll_page(page_rows, -1);
                    self.view.mode = ConsoleViewMode::Scroll;
                    true
                }
                KeyCode::PageDown => {
                    scroll.get_or_insert_default().scroll_page(page_rows, 1);
                    self.view.mode = ConsoleViewMode::Scroll;
                    true
                }
                KeyCode::Char('f') => {
                    scroll.get_or_insert_default().follow();
                    self.view.mode = ConsoleViewMode::ChildInput;
                    true
                }
                KeyCode::Char('s') => {
                    self.view.warning = Some(ConsoleWarning::SelectionUnavailable);
                    true
                }
                KeyCode::Esc => {
                    self.view.mode = ConsoleViewMode::ChildInput;
                    true
                }
                _ => false,
            },
            ConsoleViewMode::Scroll => match key.code {
                KeyCode::PageUp => {
                    scroll.get_or_insert_default().scroll_page(page_rows, -1);
                    true
                }
                KeyCode::PageDown => {
                    scroll.get_or_insert_default().scroll_page(page_rows, 1);
                    true
                }
                KeyCode::Char('f') => {
                    scroll.get_or_insert_default().follow();
                    self.view.mode = ConsoleViewMode::ChildInput;
                    true
                }
                KeyCode::Esc => {
                    self.view.mode = ConsoleViewMode::AppCommand;
                    true
                }
                _ => false,
            },
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

    fn handle_app_command(
        &mut self,
        key: KeyEvent,
        session: &TerminalHandle<'_>,
        page_rows: u16,
    ) -> bool {
        match key.code {
            KeyCode::PageUp => {
                scroll_page(session, page_rows, -1);
                self.view.mode = ConsoleViewMode::Scroll;
                self.view.following = false;
                true
            }
            KeyCode::PageDown => {
                scroll_page(session, page_rows, 1);
                self.view.mode = ConsoleViewMode::Scroll;
                self.view.following = false;
                true
            }
            KeyCode::Char('f') => {
                self.return_to_live_tail(session);
                true
            }
            KeyCode::Char('s') => {
                self.view.mode = ConsoleViewMode::Selection;
                true
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selection_requests.push(SelectionMove::Up);
                true
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selection_requests.push(SelectionMove::Down);
                true
            }
            KeyCode::Esc => {
                self.view.mode = ConsoleViewMode::ChildInput;
                true
            }
            _ => false,
        }
    }

    fn handle_scroll_command(
        &mut self,
        key: KeyEvent,
        session: &TerminalHandle<'_>,
        page_rows: u16,
    ) -> bool {
        match key.code {
            KeyCode::PageUp => {
                scroll_page(session, page_rows, -1);
                true
            }
            KeyCode::PageDown => {
                scroll_page(session, page_rows, 1);
                true
            }
            KeyCode::Char('f') => {
                self.return_to_live_tail(session);
                true
            }
            KeyCode::Esc => {
                self.view.mode = ConsoleViewMode::AppCommand;
                true
            }
            _ => false,
        }
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
mod tests {
    use std::io;
    use std::os::unix::net::UnixStream;

    use super::*;
    use crate::geometry::TerminalGeometry;
    use crate::runtime::PtyIo;
    use crate::terminal::TerminalSession;

    fn session() -> (TerminalSession, UnixStream) {
        let (reader, peer) = UnixStream::pair().unwrap();
        let session = TerminalSession::spawn(
            PtyIo {
                reader: Box::new(reader),
                writer: Box::new(io::sink()),
                resizer: Box::new(|_, _| Ok(())),
            },
            TerminalGeometry::DEFAULT,
            || {},
        )
        .unwrap();
        (session, peer)
    }

    #[test]
    fn scroll_navigation_stops_following_and_f_returns_to_live_tail() {
        let (session, peer) = session();
        let stopped = std::sync::atomic::AtomicBool::new(false);
        let handle = crate::runtime::handle_for_test(&session, &stopped);
        let mut interaction = ConsoleInteraction::default();

        assert!(interaction.handle_key(
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
            &handle,
            20,
        ));
        assert!(interaction.handle_key(
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            &handle,
            20,
        ));
        assert_eq!(interaction.view().mode, ConsoleViewMode::Scroll);
        assert!(!interaction.view().following);

        assert!(interaction.handle_key(
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE),
            &handle,
            20,
        ));
        assert_eq!(interaction.view().mode, ConsoleViewMode::ChildInput);
        assert!(interaction.view().following);

        drop(peer);
        session.shutdown().unwrap();
    }

    #[test]
    fn clipboard_failure_is_a_visible_warning_not_a_terminal_failure() {
        let warning = copy_warning(Ok(Some("selected".to_string())), |_| {
            Err(anyhow::anyhow!("clipboard unavailable"))
        });

        assert_eq!(warning, Some(ConsoleWarning::ClipboardFailed));
    }

    #[test]
    fn empty_selection_does_not_call_the_clipboard() {
        let warning = copy_warning(Ok(None), |_| panic!("clipboard must not be called"));

        assert_eq!(warning, Some(ConsoleWarning::NothingSelected));
    }

    #[test]
    fn app_command_j_k_and_arrows_queue_selection_moves_without_touching_the_child() {
        let (session, peer) = session();
        let stopped = std::sync::atomic::AtomicBool::new(false);
        let handle = crate::runtime::handle_for_test(&session, &stopped);
        let mut interaction = ConsoleInteraction::default();

        assert!(interaction.handle_key(
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
            &handle,
            20,
        ));
        assert_eq!(interaction.take_selection_moves(), Vec::new());

        for (key, expected) in [
            (KeyCode::Down, SelectionMove::Down),
            (KeyCode::Char('j'), SelectionMove::Down),
            (KeyCode::Up, SelectionMove::Up),
            (KeyCode::Char('k'), SelectionMove::Up),
        ] {
            assert!(interaction.handle_key(KeyEvent::new(key, KeyModifiers::NONE), &handle, 20));
            assert_eq!(interaction.take_selection_moves(), vec![expected]);
            assert_eq!(interaction.view().mode, ConsoleViewMode::AppCommand);
        }
        assert_eq!(interaction.take_selection_moves(), Vec::new());

        drop(peer);
        session.shutdown().unwrap();
    }

    #[test]
    fn child_input_gate_rejects_only_disabled_terminal_child_input() {
        let plain = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        let repeat = KeyEvent {
            kind: KeyEventKind::Repeat,
            ..plain
        };
        let release = KeyEvent {
            kind: KeyEventKind::Release,
            ..plain
        };
        let leader = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        let leader_repeat = KeyEvent {
            kind: KeyEventKind::Repeat,
            ..leader
        };
        let default = ConsoleViewState::default();

        // Disabled input in child-input mode rejects keys of every kind.
        assert!(child_input_rejected(default, false, &plain));
        assert!(child_input_rejected(default, false, &repeat));
        assert!(child_input_rejected(default, false, &release));
        // Enabled focused input delivers everything on the terminal path.
        assert!(!child_input_rejected(default, true, &plain));
        assert!(!child_input_rejected(default, true, &repeat));
        // The leader's press is never rejected; it enters the list.
        assert!(!child_input_rejected(default, false, &leader));
        // A leader repeat or release is still child input.
        assert!(child_input_rejected(default, false, &leader_repeat));
        // Command modes are not child input; keys route as commands.
        let commands = ConsoleViewState {
            mode: ConsoleViewMode::AppCommand,
            ..ConsoleViewState::default()
        };
        assert!(!child_input_rejected(commands, false, &plain));
    }

    #[test]
    fn pane_key_seam_rejects_disabled_terminal_input_and_keeps_read_only_keys() {
        let mut interaction = ConsoleInteraction::default();
        let mut scroll: Option<PipeScroll> = None;
        let key = |code: KeyCode| -> KeyEvent { KeyEvent::new(code, KeyModifiers::NONE) };

        // A disabled terminal pane rejects child input visibly and keeps
        // the leader available; no session write happens without one. The
        // pane still owns a live session: a disabled-input PTY is a live
        // PTY whose keys are gated.
        let (session, _peer) = session();
        let stopped = std::sync::atomic::AtomicBool::new(false);
        let handle = crate::runtime::handle_for_test(&session, &stopped);
        assert!(interaction.route_pane_key(
            ConsolePaneKind::Terminal,
            false,
            key(KeyCode::Char('x')),
            Some(&handle),
            &mut scroll,
            20,
        ));
        assert_eq!(
            interaction.view().warning,
            Some(ConsoleWarning::InputDisabled)
        );
        // The read-only pane rejects child input and keeps commands.
        assert!(interaction.route_pane_key(
            ConsolePaneKind::Pipe,
            false,
            key(KeyCode::Char('x')),
            None,
            &mut scroll,
            20,
        ));
        assert_eq!(
            interaction.view().warning,
            Some(ConsoleWarning::PipeReadOnly)
        );
        assert!(interaction.route_pane_key(
            ConsolePaneKind::Pipe,
            false,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
            None,
            &mut scroll,
            20,
        ));
        assert_eq!(interaction.view().mode, ConsoleViewMode::AppCommand);

        // A pane change drops the pane-scoped warning.
        interaction.set_pane(ConsolePaneKind::Terminal);
        assert_eq!(interaction.view().warning, None);
        // The explicit clear does the same from the app selection path.
        interaction.clear_pane_warning();
        assert_eq!(interaction.view().warning, None);
    }

    #[test]
    fn read_only_pane_keys_reject_child_input_and_keep_commands_working() {
        let mut interaction = ConsoleInteraction::default();
        let mut scroll: Option<PipeScroll> = None;

        // Plain child input is rejected visibly and consumed.
        assert!(interaction.handle_key_read_only(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            &mut scroll,
            20,
        ));
        assert_eq!(
            interaction.view().warning,
            Some(ConsoleWarning::PipeReadOnly)
        );
        interaction.clear_pane_warning();

        // The leader enters command mode without a session.
        assert!(interaction.handle_key_read_only(
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
            &mut scroll,
            20,
        ));
        assert_eq!(interaction.view().mode, ConsoleViewMode::AppCommand);

        // Selection moves queue without a session.
        assert!(interaction.handle_key_read_only(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut scroll,
            20,
        ));
        assert_eq!(
            interaction.take_selection_moves(),
            vec![SelectionMove::Down]
        );

        // Pipe scrolling and re-following work without a session.
        assert!(interaction.handle_key_read_only(
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            &mut scroll,
            20,
        ));
        assert_eq!(scroll.unwrap().offset(), 19);
        assert!(!scroll.unwrap().following());
        assert!(interaction.handle_key_read_only(
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE),
            &mut scroll,
            20,
        ));
        assert!(scroll.unwrap().following());
        assert_eq!(interaction.view().mode, ConsoleViewMode::ChildInput);

        // Text selection is unavailable in a read-only pane.
        assert!(interaction.handle_key_read_only(
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
            &mut scroll,
            20,
        ));
        assert!(interaction.handle_key_read_only(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
            &mut scroll,
            20,
        ));
        assert_eq!(
            interaction.view().warning,
            Some(ConsoleWarning::SelectionUnavailable)
        );
    }

    #[test]
    fn child_input_keys_never_queue_selection_moves() {
        let (session, peer) = session();
        let stopped = std::sync::atomic::AtomicBool::new(false);
        let handle = crate::runtime::handle_for_test(&session, &stopped);
        let mut interaction = ConsoleInteraction::default();

        // In ChildInput mode j/k are child keystrokes; selection stays put.
        assert!(!interaction.handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &handle,
            20,
        ));

        assert_eq!(interaction.take_selection_moves(), Vec::new());

        drop(peer);
        session.shutdown().unwrap();
    }
}
