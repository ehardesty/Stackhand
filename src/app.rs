use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};

use crate::console::{ConsoleInteraction, SelectionMove};
use crate::geometry::TerminalGeometry;
use crate::supervisor::Lifecycle;
use crate::supervisor::{
    Command, ConsoleView, Consoles, ProcessSnapshot, ProjectSnapshot, SupervisorHandle,
};
use crate::terminal::{OwnedTerminalSnapshot, TerminalEvent};
use crate::tui::{
    ConsoleWarning, OuterTerminal, ProcessRowView, pane_inner, project_layout, render_project,
};

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(16);
const RESIZE_SETTLE_INTERVAL: Duration = Duration::from_millis(16);
/// One shared bound for waiting out every Run's existing shutdown ladder.
const PROJECT_SHUTDOWN_WAIT: Duration = Duration::from_secs(20);

/// Load the Project, start enabled autostart Processes, and supervise them
/// interactively until the user quits with a controlled Project shutdown.
pub fn run_project(config_path: &Path) -> Result<()> {
    // Invalid configuration starts no Processes and never enters the TUI.
    let project = crate::config::load(config_path)
        .map_err(|error| anyhow!("configuration error: {error}"))?;
    let (supervisor, consoles) = crate::supervisor::start(project)?;
    supervisor.command(Command::StartAutostart);

    // The terminal restores when this scope ends, before the bounded
    // shutdown wait, so the user sees shutdown progress instead of a frozen
    // screen.
    let loop_result = {
        let mut outer = OuterTerminal::enter()?;
        run_event_loop(&mut outer, &supervisor, &consoles)
    };
    loop_result?;
    shutdown_project(supervisor)
}

/// Stop every desired-Running Process and wait, within one shared deadline,
/// for all current Runs to finish their existing bounded shutdown.
fn shutdown_project(supervisor: SupervisorHandle) -> Result<()> {
    supervisor.command(Command::StopAll);
    let deadline = Instant::now() + PROJECT_SHUTDOWN_WAIT;
    loop {
        match supervisor.snapshot() {
            None => break,
            Some(snapshot) => {
                if snapshot.processes.iter().all(|p| p.current_run.is_none()) {
                    break;
                }
            }
        }
        if Instant::now() >= deadline {
            bail!("Project shutdown did not finish within its shared deadline");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    supervisor.stop_task();
    Ok(())
}

fn run_event_loop(
    outer: &mut OuterTerminal,
    supervisor: &SupervisorHandle,
    consoles: &Consoles,
) -> Result<()> {
    let mut console = ConsoleInteraction::default();
    let mut selected: usize = 0;
    let mut pending_resize = PendingResize::default();
    let mut console_snapshot: Option<OwnedTerminalSnapshot> = None;
    let mut console_pane = project_layout(ratatui::layout::Rect::new(0, 0, 80, 24), 1).1;
    let mut dirty = true;

    loop {
        if console.poll_requests() {
            dirty = true;
        }
        let snapshot: ProjectSnapshot = supervisor
            .snapshot()
            .ok_or_else(|| anyhow!("the Supervisor stopped"))?;
        if snapshot.processes.is_empty() {
            bail!("this Project has no Processes");
        }
        selected = selected.min(snapshot.processes.len() - 1);
        for request in console.take_selection_moves() {
            // Selection is UI state only: movement clamps at the list ends
            // and never sends a command to the Supervisor.
            selected = match request {
                SelectionMove::Up => selected.saturating_sub(1),
                SelectionMove::Down => (selected + 1).min(snapshot.processes.len() - 1),
            };
            dirty = true;
        }
        let selected_process = &snapshot.processes[selected];
        let view = current_view(consoles, selected, selected_process);
        drain_console_events(&mut console, view.as_ref(), &mut dirty);

        if let Some(geometry) = pending_resize.take_ready(Instant::now()) {
            if view.as_ref().is_some_and(|view| !view.resize(geometry)) {
                console.warn(ConsoleWarning::InputRejected);
            }
            dirty = true;
        }

        if dirty || view.as_ref().is_some_and(|view| view.is_dirty()) {
            dirty = false;
            console_snapshot = view.as_ref().and_then(|view| view.snapshot());
            let rows = process_rows(&snapshot, selected);
            let pane = render_frame(outer, &rows, console_snapshot.as_ref(), console.view())?;
            console_pane = pane;
            let cursor = console_snapshot.as_ref().and_then(|snap| snap.cursor);
            outer.set_cursor_shape(cursor)?;
        }

        if !event::poll(pending_resize.poll_interval(Instant::now()))? {
            continue;
        }
        let input_event = event::read()?;
        match input_event {
            Event::Key(key) if key.kind == KeyEventKind::Press && is_quit(key) => return Ok(()),
            Event::Key(key) => {
                route_key(
                    &mut console,
                    view.as_ref(),
                    key,
                    console_pane.height,
                    &mut dirty,
                );
            }
            Event::Paste(data) => {
                if let Some(view) = view.as_ref() {
                    view.with(|session| console.handle_paste(&data, session));
                    dirty = true;
                }
            }
            Event::FocusGained | Event::FocusLost => {
                if let Some(view) = view.as_ref() {
                    let gained = matches!(input_event, Event::FocusGained);
                    let delivered = view.with(|session| session.send_focus(gained).is_ok());
                    if delivered != Some(true) {
                        console.warn(ConsoleWarning::InputRejected);
                        dirty = true;
                    }
                }
            }
            Event::Mouse(mouse) => {
                if let Some(view) = view.as_ref() {
                    let changed = view.with(|session| {
                        console.handle_mouse(
                            mouse,
                            console_pane,
                            console_snapshot
                                .as_ref()
                                .is_some_and(|snap| snap.mouse_tracking),
                            session,
                        )
                    });
                    if changed.unwrap_or(false) {
                        dirty = true;
                    }
                }
            }
            Event::Resize(cols, rows) => {
                // The child sees exactly the rendered console pane.
                let area = ratatui::layout::Rect::new(0, 0, cols, rows);
                let (_, pane, _) = project_layout(area, snapshot.processes.len());
                pending_resize.update(
                    TerminalGeometry::from_pane(pane_inner(pane)),
                    Instant::now(),
                );
            }
        }
    }
}

/// The live console of the selected Process's current Run. Process
/// identities are stable Project positions.
fn current_view(
    consoles: &Consoles,
    selected: usize,
    process: &ProcessSnapshot,
) -> Option<ConsoleView> {
    let run_id = process.current_run?;
    consoles.view(selected as u32, run_id)
}

fn drain_console_events(
    console: &mut ConsoleInteraction,
    view: Option<&ConsoleView>,
    dirty: &mut bool,
) {
    let Some(view) = view else {
        return;
    };
    while let Some(session_event) = view.poll_event() {
        match session_event {
            // One failed console must not kill Stackhand or garble the
            // screen; the warning channel carries the visible signal.
            TerminalEvent::Failed(_) => console.warn(ConsoleWarning::InputRejected),
            TerminalEvent::InputBackpressure { .. } => {
                console.warn(ConsoleWarning::InputBackpressure);
                *dirty = true;
            }
            TerminalEvent::OutputTruncated => {
                console.warn(ConsoleWarning::OutputTruncated);
                *dirty = true;
            }
            // A child exiting does not end Stackhand; the Supervisor keeps
            // the structured exit state visible.
            TerminalEvent::Exited | TerminalEvent::StateChanged => *dirty = true,
        }
    }
}

fn route_key(
    console: &mut ConsoleInteraction,
    view: Option<&ConsoleView>,
    key: KeyEvent,
    page_rows: u16,
    dirty: &mut bool,
) {
    let Some(view) = view else {
        return;
    };
    if view.with(|session| console.handle_key(key, session, page_rows)) == Some(true) {
        *dirty = true;
    }
}

fn process_rows(snapshot: &ProjectSnapshot, selected: usize) -> Vec<ProcessRowView> {
    snapshot
        .processes
        .iter()
        .enumerate()
        .map(|(index, process)| ProcessRowView {
            name: process.name.clone(),
            status: status_label(process),
            selected: index == selected,
        })
        .collect()
}

/// Project structured lifecycle state into the concise row label. The label
/// is a projection; the snapshot remains the authority.
fn status_label(process: &ProcessSnapshot) -> String {
    if !process.enabled {
        return "Disabled".to_string();
    }
    if process.lifecycle == Lifecycle::Done {
        return "Done".to_string();
    }
    // A failure stays visible while the Process is not mid-shutdown; the
    // Stopping branch folds its own failure reason into the label.
    if process.lifecycle != Lifecycle::Stopping
        && let Some(failure) = &process.failure
    {
        return format!("Failed ({})", short_reason(&failure.detail));
    }
    match process.lifecycle {
        // Done returns above; this arm keeps the match exhaustive.
        Lifecycle::Done | Lifecycle::Idle | Lifecycle::Stopped => "Stopped".to_string(),
        Lifecycle::Starting => "Starting".to_string(),
        Lifecycle::Running => "Ready".to_string(),
        Lifecycle::Waiting => match &process.blocked_reason {
            Some(reason) => format!("Waiting ({})", short_reason(reason)),
            None => "Waiting".to_string(),
        },
        Lifecycle::Stopping => {
            if let Some(failure) = &process.failure {
                format!("Stopping ({})", short_reason(&failure.detail))
            } else {
                "Stopping".to_string()
            }
        }
    }
}

/// A bounded, character-safe reason for one row.
fn short_reason(detail: &str) -> String {
    let mut truncated: String = detail.chars().take(40).collect();
    if detail.chars().count() > 40 {
        truncated.push('…');
    }
    truncated
}

fn render_frame(
    outer: &mut OuterTerminal,
    rows: &[ProcessRowView],
    console_snapshot: Option<&OwnedTerminalSnapshot>,
    view: crate::tui::ConsoleViewState,
) -> Result<ratatui::layout::Rect> {
    let mut pane = None;
    outer
        .terminal_mut()
        .draw(|frame| {
            pane = Some(render_project(frame, rows, console_snapshot, view));
        })
        .map_err(|error| anyhow!("render failed: {error}"))?;
    pane.ok_or_else(|| anyhow!("the frame did not render"))
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
    key.code == KeyCode::Char('q')
        && key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL)
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
