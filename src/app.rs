use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};

use crate::console::{ConsoleInteraction, SelectionMove};
use crate::geometry::TerminalGeometry;
use crate::output::OutputViews;
use crate::runtime::OutputStream;
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
    let (supervisor, consoles, outputs) = crate::supervisor::start(project)?;
    supervisor.command(Command::StartAutostart);

    // The terminal restores when this scope ends, before the bounded
    // shutdown wait, so the user sees shutdown progress instead of a frozen
    // screen.
    let loop_result = {
        let mut outer = OuterTerminal::enter()?;
        run_event_loop(&mut outer, &supervisor, &consoles, &outputs)
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
    outputs: &OutputViews,
) -> Result<()> {
    let mut console = ConsoleInteraction::default();
    let mut selected: usize = 0;
    let mut pending_resize = PendingResize::default();
    let mut console_snapshot: Option<OwnedTerminalSnapshot> = None;
    let mut console_pane = project_layout(ratatui::layout::Rect::new(0, 0, 80, 24), 1).1;
    let mut pipe_truncation: Option<(usize, bool)> = None;
    let mut pipe_generation: u64 = 0;
    let mut pipe_generation_known = false;
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
        let pane = selected_pane(consoles, outputs, selected, selected_process);
        match &pane {
            SelectedPane::Terminal(view) => drain_console_events(&mut console, view, &mut dirty),
            SelectedPane::Pipe(retained) => {
                // One user-visible warning when the view becomes truncated:
                // on a fresh truncation, or when a newly selected Process
                // is already showing truncated output.
                let (index, was) = pipe_truncation.take().unwrap_or((usize::MAX, false));
                if retained.truncated && (index != selected || !was) {
                    console.warn(ConsoleWarning::OutputTruncated);
                }
                pipe_truncation = Some((selected, retained.truncated));
                if !pipe_generation_known || retained.generation != pipe_generation {
                    pipe_generation = retained.generation;
                    pipe_generation_known = true;
                    dirty = true;
                }
            }
            SelectedPane::Empty => pipe_truncation = None,
        }

        if let Some(geometry) = pending_resize.take_ready(Instant::now()) {
            if let SelectedPane::Terminal(view) = &pane
                && !view.resize(geometry)
            {
                console.warn(ConsoleWarning::InputRejected);
            }
            dirty = true;
        }

        let (terminal_snapshot, pipe_lines) = match &pane {
            SelectedPane::Terminal(view) => {
                console_snapshot = view.snapshot();
                (console_snapshot.as_ref(), None)
            }
            SelectedPane::Pipe(retained) => {
                let tail_limit = console_pane.height.saturating_sub(2).max(1) as usize;
                (None, Some(pipe_lines(retained, tail_limit)))
            }
            SelectedPane::Empty => (None, None),
        };
        if dirty
            || matches!(
                &pane,
                SelectedPane::Terminal(view) if view.is_dirty()
            )
        {
            dirty = false;
            let rows = process_rows(&snapshot, selected);
            let pane = render_frame(
                outer,
                &rows,
                terminal_snapshot,
                pipe_lines.as_deref(),
                console.view(),
            )?;
            console_pane = pane;
            let cursor = terminal_snapshot.and_then(|snap| snap.cursor);
            outer.set_cursor_shape(cursor)?;
        }

        if !event::poll(pending_resize.poll_interval(Instant::now()))? {
            continue;
        }
        let input_event = event::read()?;
        match input_event {
            Event::Key(key) if key.kind == KeyEventKind::Press && is_quit(key) => return Ok(()),
            Event::Key(key) => {
                if let SelectedPane::Terminal(view) = &pane {
                    route_key(&mut console, view, key, console_pane.height, &mut dirty);
                }
            }
            Event::Paste(data) => {
                if let SelectedPane::Terminal(view) = &pane {
                    view.with(|session| console.handle_paste(&data, session));
                    dirty = true;
                }
            }
            Event::FocusGained | Event::FocusLost => {
                if let SelectedPane::Terminal(view) = &pane {
                    let gained = matches!(input_event, Event::FocusGained);
                    let delivered = view.with(|session| session.send_focus(gained).is_ok());
                    if delivered != Some(true) {
                        console.warn(ConsoleWarning::InputRejected);
                        dirty = true;
                    }
                }
            }
            Event::Mouse(mouse) => {
                if let SelectedPane::Terminal(view) = &pane {
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

/// What the selected Process's console pane shows. A PTY Run owns a live
/// terminal session; a pipe Process owns nothing to focus, so its pane
/// renders the Process's retained output — which spans Runs, so it stays
/// visible after a Run ends.
enum SelectedPane {
    Terminal(ConsoleView),
    Pipe(crate::output::RetainedOutput),
    Empty,
}

fn selected_pane(
    consoles: &Consoles,
    outputs: &OutputViews,
    selected: usize,
    process: &ProcessSnapshot,
) -> SelectedPane {
    match process.terminal_mode {
        crate::model::TerminalMode::Pty => {
            let Some(run_id) = process.current_run else {
                return SelectedPane::Empty;
            };
            match consoles.view(selected as u32, run_id) {
                Some(view) => SelectedPane::Terminal(view),
                None => SelectedPane::Empty,
            }
        }
        crate::model::TerminalMode::Pipe => match outputs.for_process(selected as u32) {
            Some(module) => SelectedPane::Pipe(module.snapshot()),
            None => SelectedPane::Empty,
        },
    }
}

/// Flatten one Process's retained output into the newest display lines.
/// Only `tail_limit` lines of work happen: older lines stay in the module.
/// Run markers keep their marker identity; pipe chunks keep their stream
/// identity in a prefix on the first line of each chunk.
fn pipe_lines(
    retained: &crate::output::RetainedOutput,
    tail_limit: usize,
) -> Vec<crate::tui::PipeLine> {
    use crate::output::RetainedChunk;
    use crate::tui::PipeLine;
    let mut lines: Vec<PipeLine> = Vec::new();
    for chunk in retained.chunks.iter().rev() {
        match chunk {
            RetainedChunk::Marker { label, .. } => {
                lines.push(PipeLine {
                    text: label.clone(),
                    marker: true,
                });
                if lines.len() >= tail_limit {
                    break;
                }
            }
            RetainedChunk::Data { stream, text, .. } => {
                let prefix = match stream {
                    OutputStream::Stdout => "out: ",
                    OutputStream::Stderr => "err: ",
                };
                let split: Vec<&str> = text.split('\n').collect();
                let mut chunk_lines: Vec<PipeLine> = Vec::new();
                for (index, line) in split.iter().enumerate().rev() {
                    // The final empty split is the chunk's trailing newline,
                    // not a line of its own.
                    if index + 1 == split.len() && line.is_empty() {
                        continue;
                    }
                    if index == 0 && !line.is_empty() {
                        chunk_lines.push(PipeLine {
                            text: format!("{prefix}{line}"),
                            marker: false,
                        });
                    } else {
                        chunk_lines.push(PipeLine {
                            text: (*line).to_string(),
                            marker: false,
                        });
                    }
                }
                for line in chunk_lines.into_iter().rev() {
                    lines.push(line);
                    if lines.len() >= tail_limit {
                        break;
                    }
                }
                if lines.len() >= tail_limit {
                    break;
                }
            }
        }
    }
    lines.reverse();
    lines
}

fn drain_console_events(console: &mut ConsoleInteraction, view: &ConsoleView, dirty: &mut bool) {
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
    view: &ConsoleView,
    key: KeyEvent,
    page_rows: u16,
    dirty: &mut bool,
) {
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
    pipe_lines: Option<&[crate::tui::PipeLine]>,
    view: crate::tui::ConsoleViewState,
) -> Result<ratatui::layout::Rect> {
    let mut pane = None;
    outer
        .terminal_mut()
        .draw(|frame| {
            pane = Some(render_project(
                frame,
                rows,
                console_snapshot,
                pipe_lines,
                view,
            ));
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
