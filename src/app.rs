use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
};

use crate::console::{ConsoleInteraction, LifecycleCommand, PipeScroll};
use crate::geometry::TerminalGeometry;
use crate::output::OutputViews;
use crate::supervisor::Lifecycle;
use crate::supervisor::{
    Command, ConsoleView, Consoles, ProcessSnapshot, ProjectShutdownSnapshot, ProjectSnapshot,
    SupervisorHandle,
};
use crate::terminal::{OwnedTerminalSnapshot, TerminalEvent};
use crate::tui::{
    ConsolePaneKind, ConsoleWarning, OuterTerminal, ProcessRowView, pane_inner, project_layout,
    render_project,
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

    // The TUI remains active while shutdown progresses. This scope restores
    // the outer terminal before the already-observed result reaches the CLI.
    let shutdown = {
        let mut outer = OuterTerminal::enter()?;
        run_event_loop(&mut outer, &supervisor, &consoles, &outputs)
    }?;
    finish_project(supervisor, shutdown)
}

/// Stop the Supervisor task and report the immutable result already observed
/// by the interactive quit operation. This does not start or wait for a
/// second Project shutdown.
fn finish_project(supervisor: SupervisorHandle, result: ProjectShutdownSnapshot) -> Result<()> {
    eprintln!("Stackhand is shutting down the Project…");
    supervisor.stop_task();
    report_shutdown_result(&result)
}

fn report_shutdown_result(result: &ProjectShutdownSnapshot) -> Result<()> {
    if result.failures.is_empty() {
        eprintln!("Project shutdown complete.");
        return Ok(());
    }
    let details = result
        .failures
        .iter()
        .map(|failure| {
            if failure.remaining_pids.is_empty() {
                format!("{}: {}", failure.process, failure.detail)
            } else {
                format!(
                    "{}: {} (remaining PIDs: {:?})",
                    failure.process, failure.detail, failure.remaining_pids
                )
            }
        })
        .collect::<Vec<_>>()
        .join("; ");
    bail!("Project shutdown did not finish cleanly: {details}")
}

fn run_event_loop(
    outer: &mut OuterTerminal,
    supervisor: &SupervisorHandle,
    consoles: &Consoles,
    outputs: &OutputViews,
) -> Result<ProjectShutdownSnapshot> {
    let mut console = ConsoleInteraction::default();
    let mut selected: usize = 0;
    let mut pending_resize = PendingResize::default();
    let mut console_snapshot: Option<OwnedTerminalSnapshot> = None;
    let mut console_pane = project_layout(ratatui::layout::Rect::new(0, 0, 80, 24), 1).1;
    let mut pipe_truncation: Option<(usize, bool)> = None;
    let mut pipe_generation: u64 = 0;
    let mut pipe_generation_known = false;
    // One scroll view per Process, so scrolling or re-following one pane
    // never changes another Process's view. Resized with the snapshot.
    let mut pipe_scroll: Vec<Option<PipeScroll>> = Vec::new();
    let mut last_project_snapshot: Option<ProjectSnapshot> = None;
    let mut shutting_down = false;
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
        if shutting_down
            && let Some(result) = snapshot.shutdown.as_ref().filter(|result| result.complete)
        {
            return Ok(result.clone());
        }
        if last_project_snapshot.as_ref().is_none_or(|previous| {
            previous.processes != snapshot.processes
                || previous.now_ms / 1000 != snapshot.now_ms / 1000
        }) {
            dirty = true;
        }
        last_project_snapshot = Some(snapshot.clone());
        selected = selected.min(snapshot.processes.len() - 1);
        pipe_scroll.resize(snapshot.processes.len(), None);
        if console.apply_selection_moves(&mut selected, snapshot.processes.len()) {
            dirty = true;
        }
        for request in console.take_lifecycle_commands() {
            // Lifecycle commands target the selected Process by name and
            // never touch the terminal session; the Supervisor owns the
            // resulting Run changes.
            let selected_process = &snapshot.processes[selected];
            let name = selected_process.name.clone();
            supervisor.command(match request {
                LifecycleCommand::Start => Command::Start(name),
                LifecycleCommand::Stop => Command::Stop(name),
                // `r` restarts a Service and reruns a One-shot; the
                // Supervisor guards each command to its own kind.
                LifecycleCommand::Restart => {
                    if selected_process.kind == crate::model::ProcessKind::OneShot {
                        Command::Rerun(name)
                    } else {
                        Command::Restart(name)
                    }
                }
            });
        }
        let selected_process = &snapshot.processes[selected];
        let pane = selected_pane(consoles, outputs, selected_process);
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

        let mut pipe_window: Vec<crate::tui::PipeLine> = Vec::new();
        let (terminal_snapshot, pipe_lines) = match &pane {
            SelectedPane::Terminal(view) => {
                console.set_pane(ConsolePaneKind::Terminal);
                console_snapshot = view.snapshot();
                (console_snapshot.as_ref(), None)
            }
            SelectedPane::Pipe(retained) => {
                console.set_pane(ConsolePaneKind::Pipe);
                let pane_rows = console_pane.height.saturating_sub(2).max(1) as usize;
                let scroll = &pipe_scroll[selected].get_or_insert_with(PipeScroll::default);
                console.set_following(scroll.following());
                pipe_window.clear();
                pipe_window
                    .extend(retained.display_lines(pane_rows.saturating_add(scroll.offset())));
                (None, Some(scroll.window(&pipe_window, pane_rows)))
            }
            SelectedPane::Empty => {
                console.set_pane(ConsolePaneKind::Empty);
                (None, None)
            }
        };
        if dirty
            || matches!(
                &pane,
                SelectedPane::Terminal(view) if view.is_dirty()
            )
        {
            dirty = false;
            let rows = process_rows(&snapshot, selected);
            let mut header = selected_header(&snapshot.processes[selected], snapshot.now_ms);
            if shutting_down {
                header.insert_str(0, "Project shutdown in progress · ");
            }
            let pane = render_frame(
                outer,
                &rows,
                terminal_snapshot,
                pipe_lines,
                console.view(),
                &header,
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
            Event::Key(key) if key.kind == KeyEventKind::Press && is_quit(key, console.view()) => {
                if !shutting_down {
                    supervisor.command(Command::Shutdown {
                        deadline: Instant::now() + PROJECT_SHUTDOWN_WAIT,
                    });
                    shutting_down = true;
                    dirty = true;
                }
            }
            Event::Key(key) => match &pane {
                SelectedPane::Terminal(view) => {
                    // The pane key seam is the one production boundary that
                    // decides whether the event reaches the PTY child:
                    // focused input enabled, or it is rejected visibly.
                    let changed = view.with(|session| {
                        console.route_pane_key(
                            ConsolePaneKind::Terminal,
                            selected_process.input_focused,
                            key,
                            Some(session),
                            &mut pipe_scroll[selected],
                            console_pane.height.saturating_sub(2).max(1),
                        )
                    });
                    if changed.unwrap_or(false) {
                        dirty = true;
                    }
                }
                SelectedPane::Pipe(_) | SelectedPane::Empty => {
                    let kind = match &pane {
                        SelectedPane::Pipe(_) => ConsolePaneKind::Pipe,
                        _ => ConsolePaneKind::Empty,
                    };
                    let changed = console.route_pane_key(
                        kind,
                        selected_process.input_focused,
                        key,
                        None,
                        &mut pipe_scroll[selected],
                        console_pane.height.saturating_sub(2).max(1),
                    );
                    if changed {
                        dirty = true;
                    }
                }
            },
            Event::Paste(data) => match &pane {
                SelectedPane::Terminal(view)
                    if console.accepts_child_input(selected_process.input_focused) =>
                {
                    view.with(|session| console.handle_paste(&data, session));
                    dirty = true;
                }
                SelectedPane::Terminal(_) if !selected_process.input_focused => {
                    console.warn(ConsoleWarning::InputDisabled);
                    dirty = true;
                }
                // Process-list and Copy focus never leak paste bytes to the
                // child. Pipe and empty consoles are also read-only.
                SelectedPane::Terminal(_) | SelectedPane::Pipe(_) | SelectedPane::Empty => {
                    console.warn(ConsoleWarning::PasteRejected);
                    dirty = true;
                }
            },
            Event::FocusGained | Event::FocusLost => {
                if let SelectedPane::Terminal(view) = &pane {
                    let gained = matches!(input_event, Event::FocusGained);
                    // Focus reports are child-bound, so they follow the
                    // input policy like typed keys.
                    let delivered = if selected_process.input_focused {
                        view.with(|session| session.send_focus(gained).is_ok())
                            .unwrap_or(false)
                    } else {
                        console.warn(ConsoleWarning::InputDisabled);
                        false
                    };
                    // A failed delivery with focused input enabled is the
                    // rejected-input case; a disabled pane keeps the more
                    // specific disabled warning above.
                    if !delivered && selected_process.input_focused {
                        console.warn(ConsoleWarning::InputRejected);
                        dirty = true;
                    } else if !delivered {
                        dirty = true;
                    }
                }
            }
            Event::Mouse(mouse) => {
                if handle_app_mouse(
                    &mut console,
                    mouse,
                    &pane,
                    &mut selected,
                    selected_process.input_focused,
                    console_snapshot.as_ref(),
                    &mut pipe_scroll,
                ) {
                    dirty = true;
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

fn handle_app_mouse(
    console: &mut ConsoleInteraction,
    mouse: MouseEvent,
    pane: &SelectedPane,
    selected: &mut usize,
    input_focused: bool,
    console_snapshot: Option<&OwnedTerminalSnapshot>,
    pipe_scroll: &mut [Option<PipeScroll>],
) -> bool {
    let process_count = pipe_scroll.len();
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let (list, console_outer, _) =
        project_layout(ratatui::layout::Rect::new(0, 0, cols, rows), process_count);
    let console_inner = pane_inner(console_outer);

    // A console-owned drag keeps its original owner when the pointer leaves
    // the pane. Deliver Drag and Up before hit-testing another surface.
    if console.mouse_gesture_active()
        && matches!(mouse.kind, MouseEventKind::Drag(_) | MouseEventKind::Up(_))
        && let SelectedPane::Terminal(view) = pane
    {
        let child_tracking =
            input_focused && console_snapshot.is_some_and(|snapshot| snapshot.mouse_tracking);
        return view
            .with(|session| console.handle_mouse(mouse, console_inner, child_tracking, session))
            .unwrap_or(false);
    }

    if rect_contains(list, mouse.column, mouse.row) {
        if mouse_changes_focus(mouse.kind) {
            match pane {
                SelectedPane::Terminal(view) => {
                    view.with(|session| console.focus_process_list(Some(session)));
                }
                SelectedPane::Pipe(_) | SelectedPane::Empty => console.focus_process_list(None),
            }
        }
        let previous = *selected;
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(index) = process_row_at(list, mouse.row, process_count) {
                    *selected = index;
                    console.clear_pane_warning();
                }
            }
            MouseEventKind::ScrollUp => *selected = (*selected).saturating_sub(1),
            MouseEventKind::ScrollDown => {
                *selected = (*selected + 1).min(process_count.saturating_sub(1));
            }
            _ => {}
        }
        return previous != *selected || matches!(mouse.kind, MouseEventKind::Down(_));
    }

    if !rect_contains(console_outer, mouse.column, mouse.row) {
        return false;
    }
    match pane {
        SelectedPane::Terminal(view) => view
            .with(|session| {
                if mouse_starts_console_focus(mouse.kind, console.view().mode) {
                    console.focus_console(Some(session));
                }
                let child_tracking = input_focused
                    && console_snapshot.is_some_and(|snapshot| snapshot.mouse_tracking);
                console.handle_mouse(mouse, console_inner, child_tracking, session)
            })
            .unwrap_or(false),
        SelectedPane::Pipe(_) => {
            if mouse_starts_console_focus(mouse.kind, console.view().mode) {
                console.focus_console(None);
            }
            console.handle_read_only_mouse(mouse, &mut pipe_scroll[*selected])
                || mouse_changes_focus(mouse.kind)
        }
        SelectedPane::Empty => {
            if mouse_changes_focus(mouse.kind) {
                if mouse_starts_console_focus(mouse.kind, console.view().mode) {
                    console.focus_console(None);
                }
                true
            } else {
                false
            }
        }
    }
}

fn mouse_changes_focus(kind: MouseEventKind) -> bool {
    matches!(kind, MouseEventKind::Down(_))
}

fn mouse_starts_console_focus(kind: MouseEventKind, mode: crate::tui::ConsoleViewMode) -> bool {
    mouse_changes_focus(kind) && mode == crate::tui::ConsoleViewMode::ProcessList
}

fn rect_contains(area: ratatui::layout::Rect, column: u16, row: u16) -> bool {
    column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
}

fn process_row_at(list: ratatui::layout::Rect, row: u16, process_count: usize) -> Option<usize> {
    let inner = pane_inner(list);
    if row < inner.y || row >= inner.bottom() {
        return None;
    }
    let index = usize::from(row - inner.y);
    (index < process_count).then_some(index)
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
    process: &ProcessSnapshot,
) -> SelectedPane {
    match process.terminal_mode {
        crate::model::TerminalMode::Pty => {
            let Some(run_id) = process.current_run else {
                return SelectedPane::Empty;
            };
            match consoles.view_process(process.process_id, run_id) {
                Some(view) => SelectedPane::Terminal(view),
                None => SelectedPane::Empty,
            }
        }
        crate::model::TerminalMode::Pipe => match outputs.for_process_id(process.process_id) {
            Some(module) => SelectedPane::Pipe(module.snapshot()),
            None => SelectedPane::Empty,
        },
    }
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

fn process_rows(snapshot: &ProjectSnapshot, selected: usize) -> Vec<ProcessRowView> {
    snapshot
        .processes
        .iter()
        .enumerate()
        .map(|(index, process)| ProcessRowView {
            name: process.name.clone(),
            status: status_label(process),
            cpu: process
                .metrics
                .map(|metrics| format_cpu(metrics.cpu_percent)),
            memory: process.metrics.map(|metrics| format_rss(metrics.rss_kib)),
            selected: index == selected,
        })
        .collect()
}

/// A compact CPU column: one decimal place at most, no more precision than
/// the sample claims.
fn format_cpu(percent: f64) -> String {
    if percent >= 10.0 {
        format!("{}%", percent.round())
    } else {
        format!("{percent:.1}%")
    }
}

/// A compact resident-memory column in powers of 1024.
fn format_rss(kib: u64) -> String {
    const MIB: u64 = 1024;
    const GIB: u64 = 1024 * MIB;
    match kib {
        0 => "0".to_string(),
        value if value < MIB => format!("{kib}K"),
        value if value < GIB => format!("{}M", value / MIB),
        value => format!("{:.1}G", value as f64 / GIB as f64),
    }
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

/// Project the selected Process into the console pane's header: name, the
/// live Run identity and PID when one exists, the concise status label,
/// the Run's age and compact metrics when sampled, and the bounded
/// diagnostic (a blocked reason or failure detail) when one is present.
/// The header is a projection of the immutable Supervisor snapshot.
fn selected_header(process: &ProcessSnapshot, now_ms: u64) -> String {
    let mut header = process.name.clone();
    if let Some(run_id) = process.current_run {
        header.push_str(&format!(" · run {run_id}"));
    }
    if let Some(pid) = process.root_pid {
        header.push_str(&format!(" · PID {pid}"));
    }
    header.push_str(&format!(" · {}", status_label(process)));
    if let Some(started_at_ms) = process.run_started_at_ms {
        let age_ms = now_ms.saturating_sub(started_at_ms);
        header.push_str(&format!(" · {}", format_age(age_ms)));
    }
    if let Some(metrics) = &process.metrics {
        header.push_str(&format!(" · {}", format_rss(metrics.rss_kib)));
        header.push_str(&format!(" · {} CPU", format_cpu(metrics.cpu_percent)));
    }
    if process.lifecycle == Lifecycle::Waiting {
        if let Some(reason) = &process.blocked_reason {
            header.push_str(&format!(" · {reason}"));
        }
    } else if let Some(readiness) = &process.readiness {
        if let Some(last_error) = &readiness.last_error {
            header.push_str(&format!(
                " · readiness attempt {}: {}",
                readiness.attempts,
                short_reason(last_error)
            ));
        }
    } else if process.lifecycle != Lifecycle::Stopping
        && let Some(failure) = &process.failure
    {
        header.push_str(&format!(" · {}", short_reason(&failure.detail)));
    }
    header
}

/// A compact Run age: seconds under a minute, then whole minutes.
fn format_age(age_ms: u64) -> String {
    let seconds = age_ms / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        let minutes = seconds / 60;
        format!("{minutes}m{}s", seconds % 60)
    }
}

fn render_frame(
    outer: &mut OuterTerminal,
    rows: &[ProcessRowView],
    console_snapshot: Option<&OwnedTerminalSnapshot>,
    pipe_lines: Option<&[crate::tui::PipeLine]>,
    view: crate::tui::ConsoleViewState,
    selected_header: &str,
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
                selected_header,
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

fn is_quit(key: KeyEvent, view: crate::tui::ConsoleViewState) -> bool {
    if key.code != KeyCode::Char('q') {
        return false;
    }
    key.modifiers
        .contains(crossterm::event::KeyModifiers::CONTROL)
        || (view.mode == crate::tui::ConsoleViewMode::ProcessList && key.modifiers.is_empty())
}

#[cfg(test)]
mod tests;
