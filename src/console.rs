use std::time::Instant;

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent};
use ratatui::layout::Rect;

use crate::runtime::TerminalHandle;
use crate::terminal::{CopyRequest, PasteCompletion, PasteRequest};
use crate::tui::{ConsoleViewMode, ConsoleViewState, ConsoleWarning, MouseRouter};

pub(crate) struct ConsoleInteraction {
    view: ConsoleViewState,
    mouse: MouseRouter,
    paste_requests: Vec<PasteRequest>,
    copy_requests: Vec<CopyRequest>,
    selection_clock: Instant,
}

impl Default for ConsoleInteraction {
    fn default() -> Self {
        Self {
            view: ConsoleViewState::default(),
            mouse: MouseRouter::default(),
            paste_requests: Vec::new(),
            copy_requests: Vec::new(),
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
                let _ = session.send_key(key);
            }
            return false;
        }

        match self.view.mode {
            ConsoleViewMode::ChildInput if is_command_leader(key) => {
                self.view.mode = ConsoleViewMode::AppCommand;
                true
            }
            ConsoleViewMode::ChildInput => {
                let _ = session.send_key(key);
                false
            }
            ConsoleViewMode::AppCommand => self.handle_app_command(key, session, page_rows),
            ConsoleViewMode::Scroll => self.handle_scroll_command(key, session, page_rows),
            ConsoleViewMode::Selection => self.handle_selection_command(key, session),
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
        let _ = session.send_mouse(route.event);
        true
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

fn is_command_leader(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('a') && key.modifiers.contains(KeyModifiers::CONTROL)
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
}
