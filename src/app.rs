use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::console::ConsoleInteraction;
use crate::geometry::TerminalGeometry;
use crate::runtime::{
    ProcessId, RunId, RunMode, RunRuntime, RunStartRequest, SpawnCommand, TerminalHandle,
};
use crate::terminal::{OwnedTerminalSnapshot, TerminalEvent};
use crate::tui::{ConsoleWarning, OuterTerminal, console_area, render};

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(16);
const RESIZE_SETTLE_INTERVAL: Duration = Duration::from_millis(16);

pub fn run_interactive() -> Result<()> {
    let mut outer = OuterTerminal::enter()?;
    let size = outer.terminal_mut().size()?;
    let geometry = TerminalGeometry::from_pane(console_area(size.into()));
    let (events, _run_event_log) = mpsc::channel();
    let (output, _output_log) = crate::runtime::output_channel();
    let dirty = Arc::new(AtomicBool::new(true));
    let wake_dirty = Arc::clone(&dirty);
    let mut run = RunRuntime.start(RunStartRequest {
        process_id: ProcessId::new(1),
        run_id: RunId::new(1),
        command: SpawnCommand::shell(),
        mode: RunMode::Pty {
            initial_geometry: geometry,
        },
        events,
        output,
        ladder: Default::default(),
        metrics_interval: None,
        on_output_wake: Some(Box::new(move || {
            wake_dirty.store(true, Ordering::Release);
        })),
    })?;
    let mut snapshot = empty_snapshot(geometry);
    let mut pending_resize = PendingResize::default();
    let mut console = ConsoleInteraction::default();

    let run_result = run_event_loop(
        &mut outer,
        run.terminal().expect("interactive Run is PTY-mode"),
        &dirty,
        &mut snapshot,
        &mut pending_resize,
        &mut console,
    );
    run_result?;
    run.shutdown().map(|_outcome| ())
}

fn run_event_loop(
    outer: &mut OuterTerminal,
    session: TerminalHandle<'_>,
    dirty: &AtomicBool,
    snapshot: &mut OwnedTerminalSnapshot,
    pending_resize: &mut PendingResize,
    console: &mut ConsoleInteraction,
) -> Result<()> {
    loop {
        if console.poll_requests() {
            dirty.store(true, Ordering::Release);
        }
        if let Some(geometry) = pending_resize.take_ready(Instant::now()) {
            if session.resize(geometry).is_err() {
                console.warn(ConsoleWarning::InputRejected);
            }
            dirty.store(true, Ordering::Release);
        }

        while let Some(session_event) = session.poll_event() {
            match session_event {
                TerminalEvent::Failed(error) => bail!("terminal owner failed: {error}"),
                TerminalEvent::InputBackpressure { .. } => {
                    console.warn(ConsoleWarning::InputBackpressure);
                    dirty.store(true, Ordering::Release);
                }
                TerminalEvent::OutputTruncated => {
                    console.warn(ConsoleWarning::OutputTruncated);
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
                .draw(|frame| render(frame, snapshot, console.view()))?;
            outer.set_cursor_shape(snapshot.cursor)?;
        }

        if !event::poll(pending_resize.poll_interval(Instant::now()))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press && is_quit(key) => return Ok(()),
            Event::Key(key) => {
                if console.handle_key(key, &session, snapshot.buffer.area().height) {
                    dirty.store(true, Ordering::Release);
                }
            }
            Event::Paste(data) => {
                console.handle_paste(&data, &session);
                dirty.store(true, Ordering::Release);
            }
            Event::FocusGained => {
                if session.send_focus(true).is_err() {
                    console.warn(ConsoleWarning::InputRejected);
                    dirty.store(true, Ordering::Release);
                }
            }
            Event::FocusLost => {
                if session.send_focus(false).is_err() {
                    console.warn(ConsoleWarning::InputRejected);
                    dirty.store(true, Ordering::Release);
                }
            }
            Event::Mouse(mouse) => {
                let area = console_area(outer.terminal_mut().size()?.into());
                if console.handle_mouse(mouse, area, snapshot.mouse_tracking, &session) {
                    dirty.store(true, Ordering::Release);
                }
            }
            Event::Resize(cols, rows) => {
                let geometry = TerminalGeometry::from_pane(console_area(
                    ratatui::layout::Rect::new(0, 0, cols, rows),
                ));
                pending_resize.update(geometry, Instant::now());
            }
        }
    }
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
        mouse_tracking: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
