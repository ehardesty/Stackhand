use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::geometry::TerminalGeometry;
use crate::runtime::{PtyProcess, SpawnCommand};
use crate::terminal::{
    OwnedTerminalSnapshot, PasteCompletion, PasteRequest, TerminalEvent, TerminalSession,
};
use crate::tui::{
    ConsoleViewMode, ConsoleViewState, ConsoleWarning, OuterTerminal, console_area, render,
};

pub use crate::fixtures::{
    run_fixture_input, run_fixture_paste, run_fixture_rendering, run_fixture_round_trip,
};

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(16);
const RESIZE_SETTLE_INTERVAL: Duration = Duration::from_millis(16);

pub fn run_interactive() -> Result<()> {
    let mut outer = OuterTerminal::enter()?;
    let size = outer.terminal_mut().size()?;
    let geometry = TerminalGeometry::from_pane(console_area(size.into()));
    let spawned = PtyProcess::spawn(SpawnCommand::shell(), geometry)?;
    let dirty = Arc::new(AtomicBool::new(true));
    let wake_dirty = Arc::clone(&dirty);
    let session = TerminalSession::spawn(spawned.io, geometry, move || {
        wake_dirty.store(true, Ordering::Release);
    })?;
    let mut process = spawned.process;
    let mut snapshot = empty_snapshot(geometry);
    let mut pending_resize = PendingResize::default();
    let mut console_view = ConsoleViewState::default();

    let run_result = run_event_loop(
        &mut outer,
        &session,
        &dirty,
        &mut snapshot,
        &mut pending_resize,
        &mut console_view,
    );
    let process_result = process.shutdown();
    let session_result = session.shutdown();

    run_result?;
    process_result?;
    session_result
}

fn run_event_loop(
    outer: &mut OuterTerminal,
    session: &TerminalSession,
    dirty: &AtomicBool,
    snapshot: &mut OwnedTerminalSnapshot,
    pending_resize: &mut PendingResize,
    console_view: &mut ConsoleViewState,
) -> Result<()> {
    let mut paste_requests = Vec::new();
    loop {
        poll_paste_requests(&mut paste_requests, console_view, dirty);
        if let Some(geometry) = pending_resize.take_ready(Instant::now()) {
            session.resize(geometry);
            dirty.store(true, Ordering::Release);
        }

        while let Some(session_event) = session.poll_event() {
            match session_event {
                TerminalEvent::Failed(error) => bail!("terminal owner failed: {error}"),
                TerminalEvent::InputBackpressure { .. } => {
                    console_view.warning = Some(ConsoleWarning::InputBackpressure);
                    dirty.store(true, Ordering::Release);
                }
                TerminalEvent::OutputTruncated { .. } => {
                    console_view.warning = Some(ConsoleWarning::OutputTruncated);
                    dirty.store(true, Ordering::Release);
                }
                TerminalEvent::Exited => return Ok(()),
                TerminalEvent::StateChanged => {}
            }
        }

        if dirty.swap(false, Ordering::AcqRel) || session.is_dirty() {
            *snapshot = session.snapshot();
            outer
                .terminal_mut()
                .draw(|frame| render(frame, snapshot, *console_view))?;
            outer.set_cursor_shape(snapshot.cursor)?;
        }

        if !event::poll(pending_resize.poll_interval(Instant::now()))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press && is_quit(key) => return Ok(()),
            Event::Key(key) => {
                if route_console_key(key, session, snapshot.buffer.area().height, console_view) {
                    dirty.store(true, Ordering::Release);
                }
            }
            Event::Paste(data) => {
                match session.send_paste(&data) {
                    Ok(request) => {
                        paste_requests.push(request);
                        console_view.warning = None;
                    }
                    Err(_) => console_view.warning = Some(ConsoleWarning::PasteRejected),
                }
                dirty.store(true, Ordering::Release);
            }
            Event::FocusGained => session.send_focus(true),
            Event::FocusLost => session.send_focus(false),
            Event::Resize(cols, rows) => {
                let geometry = TerminalGeometry::from_pane(console_area(
                    ratatui::layout::Rect::new(0, 0, cols, rows),
                ));
                pending_resize.update(geometry, Instant::now());
            }
            _ => {}
        }
    }
}

fn poll_paste_requests(
    requests: &mut Vec<PasteRequest>,
    view: &mut ConsoleViewState,
    dirty: &AtomicBool,
) {
    let mut failed = false;
    requests.retain_mut(|request| match request.poll() {
        Some(PasteCompletion::Delivered) => false,
        Some(PasteCompletion::Failed(_)) => {
            failed = true;
            false
        }
        None => true,
    });
    if failed {
        view.warning = Some(ConsoleWarning::PasteDeliveryFailed);
        dirty.store(true, Ordering::Release);
    }
}

fn route_console_key(
    key: KeyEvent,
    session: &TerminalSession,
    page_rows: u16,
    view: &mut ConsoleViewState,
) -> bool {
    if key.kind != KeyEventKind::Press {
        if view.mode == ConsoleViewMode::ChildInput {
            session.send_key(key);
        }
        return false;
    }

    match view.mode {
        ConsoleViewMode::ChildInput if is_command_leader(key) => {
            view.mode = ConsoleViewMode::AppCommand;
            true
        }
        ConsoleViewMode::ChildInput => {
            session.send_key(key);
            false
        }
        ConsoleViewMode::AppCommand => match key.code {
            KeyCode::PageUp => {
                scroll_page(session, page_rows, -1);
                view.mode = ConsoleViewMode::Scroll;
                view.following = false;
                true
            }
            KeyCode::PageDown => {
                scroll_page(session, page_rows, 1);
                view.mode = ConsoleViewMode::Scroll;
                view.following = false;
                true
            }
            KeyCode::Char('f') => {
                return_to_live_tail(session, view);
                true
            }
            KeyCode::Esc => {
                view.mode = ConsoleViewMode::ChildInput;
                true
            }
            _ => false,
        },
        ConsoleViewMode::Scroll => match key.code {
            KeyCode::PageUp => {
                scroll_page(session, page_rows, -1);
                true
            }
            KeyCode::PageDown => {
                scroll_page(session, page_rows, 1);
                true
            }
            KeyCode::Char('f') => {
                return_to_live_tail(session, view);
                true
            }
            KeyCode::Esc => {
                view.mode = ConsoleViewMode::AppCommand;
                true
            }
            _ => false,
        },
    }
}

fn is_command_leader(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('a') && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn scroll_page(session: &TerminalSession, page_rows: u16, direction: isize) {
    let page = isize::try_from(page_rows.saturating_sub(1).max(1))
        .expect("u16 page size always fits in isize");
    session.scroll_lines(direction * page);
}

fn return_to_live_tail(session: &TerminalSession, view: &mut ConsoleViewState) {
    session.follow_live();
    view.following = true;
    view.mode = ConsoleViewMode::ChildInput;
}

#[derive(Default)]
struct PendingResize {
    latest: Option<(TerminalGeometry, Instant)>,
}

impl PendingResize {
    fn update(&mut self, geometry: TerminalGeometry, now: Instant) {
        self.latest = Some((geometry, now + RESIZE_SETTLE_INTERVAL));
    }

    fn take_ready(&mut self, now: Instant) -> Option<TerminalGeometry> {
        let (geometry, ready_at) = self.latest?;
        if now < ready_at {
            return None;
        }
        self.latest = None;
        Some(geometry)
    }

    fn poll_interval(&self, now: Instant) -> Duration {
        self.latest
            .map(|(_, ready_at)| ready_at.saturating_duration_since(now))
            .unwrap_or(EVENT_POLL_INTERVAL)
            .min(EVENT_POLL_INTERVAL)
    }
}

fn is_quit(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('q') && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn empty_snapshot(geometry: TerminalGeometry) -> OwnedTerminalSnapshot {
    OwnedTerminalSnapshot {
        buffer: ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(
            0,
            0,
            geometry.cols(),
            geometry.rows(),
        )),
        cursor: None,
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::os::unix::net::UnixStream;

    use super::*;
    use crate::runtime::PtyIo;

    #[test]
    fn rapid_resize_uses_only_the_last_valid_geometry() {
        let started = Instant::now();
        let mut pending = PendingResize::default();
        pending.update(TerminalGeometry::new(120, 40).unwrap(), started);
        pending.update(
            TerminalGeometry::new(1, 1).unwrap(),
            started + Duration::from_millis(2),
        );
        pending.update(
            TerminalGeometry::new(73, 19).unwrap(),
            started + Duration::from_millis(4),
        );

        assert_eq!(
            pending.take_ready(started + Duration::from_millis(19)),
            None
        );
        assert_eq!(
            pending.take_ready(started + Duration::from_millis(20)),
            TerminalGeometry::new(73, 19)
        );
        assert_eq!(
            pending.take_ready(started + Duration::from_millis(21)),
            None
        );
    }

    #[test]
    fn scroll_navigation_stops_following_and_f_returns_to_live_tail() {
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
        let mut view = ConsoleViewState::default();

        assert!(route_console_key(
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
            &session,
            20,
            &mut view,
        ));
        assert!(route_console_key(
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            &session,
            20,
            &mut view,
        ));
        assert_eq!(view.mode, ConsoleViewMode::Scroll);
        assert!(!view.following);

        assert!(route_console_key(
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE),
            &session,
            20,
            &mut view,
        ));
        assert_eq!(view.mode, ConsoleViewMode::ChildInput);
        assert!(view.following);

        drop(peer);
        session.shutdown().unwrap();
    }
}
