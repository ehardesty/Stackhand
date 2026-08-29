use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
};

use crate::console::{ConsoleInteraction, LifecycleCommand, PipeScroll};
use crate::geometry::TerminalGeometry;
use crate::log_view::{LogView, LogViewAction, OutputRepresentation};
use crate::output::OutputViews;
use crate::supervisor::{
    Command, ConsoleView, Consoles, ProcessSnapshot, ProjectShutdownSnapshot, ProjectSnapshot,
    SupervisorHandle,
};
use crate::terminal::{OwnedTerminalSnapshot, TerminalEvent};
use crate::tui::{
    ConsolePaneKind, ConsoleWarning, OuterTerminal, ProcessRowView, pane_inner, project_layout,
    render_project,
};

use self::input_scheduler::InputScheduler;
use self::resize::PendingResize;

mod input_scheduler;
mod resize;
mod view_model;

use view_model::{process_rows, selected_header};

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(16);
const RESIZE_SETTLE_INTERVAL: Duration = Duration::from_millis(16);
/// One shared bound for waiting out every Run's existing shutdown ladder.
const PROJECT_SHUTDOWN_WAIT: Duration = Duration::from_secs(20);

/// Load the explicit Project, start enabled autostart Processes, and
/// supervise them interactively until the user quits with a controlled
/// Project shutdown.
pub fn run_project(config_path: &Path) -> Result<()> {
    run_project_with_profile(config_path, None)
}

/// Load the explicit Project with one selected profile, then run it
/// interactively.
pub fn run_project_with_profile(config_path: &Path, profile: Option<&str>) -> Result<()> {
    let profiles = profile.into_iter().map(str::to_owned).collect::<Vec<_>>();
    run_project_with_profiles(config_path, &profiles)
}

/// Load the explicit Project with profiles selected in CLI order, then run it
/// interactively.
pub fn run_project_with_profiles(config_path: &Path, profiles: &[String]) -> Result<()> {
    run_resolved(crate::config::ResolutionRequest::explicit_with_profiles(
        config_path,
        profiles.iter().cloned(),
    ))
}

/// Discover the nearest base Project, then run it interactively.
pub fn run_discovered_project() -> Result<()> {
    run_discovered_project_with_profile(None)
}

/// Discover the nearest base Project with one selected profile, then run it
/// interactively.
pub fn run_discovered_project_with_profile(profile: Option<&str>) -> Result<()> {
    let profiles = profile.into_iter().map(str::to_owned).collect::<Vec<_>>();
    run_discovered_project_with_profiles(&profiles)
}

/// Discover the nearest base Project with profiles selected in CLI order,
/// then run it interactively.
pub fn run_discovered_project_with_profiles(profiles: &[String]) -> Result<()> {
    run_resolved(crate::config::ResolutionRequest::discover_with_profiles(
        profiles.iter().cloned(),
    ))
}

fn run_resolved(request: crate::config::ResolutionRequest) -> Result<()> {
    // Invalid configuration starts no Processes and never enters the TUI.
    let resolution =
        crate::config::resolve(request).map_err(|error| anyhow!("configuration error: {error}"))?;
    let (supervisor, consoles, outputs) = crate::supervisor::start(resolution.into_project())?;
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
    let mut console_area =
        pane_inner(project_layout(ratatui::layout::Rect::new(0, 0, 80, 24), 1).1);
    let mut pipe_truncation: Option<(usize, bool)> = None;
    let mut pipe_generation: u64 = 0;
    let mut pipe_generation_known = false;
    // One scroll view per Process, so scrolling or re-following one pane
    // never changes another Process's view. Resized with the snapshot.
    let mut pipe_scroll: Vec<Option<PipeScroll>> = Vec::new();
    // Logs representation, search, and navigation belong to each Process.
    // Terminal viewport and selection remain in that Process's Ghostty state.
    let mut log_views: Vec<LogView> = Vec::new();
    // The selected idle Logs view reuses its immutable retained snapshot.
    // Wheel input changes only view state, so it must not clone all retained
    // output. One cache avoids duplicating the Project retention budget.
    let mut retained_view: Option<(crate::runtime::ProcessId, crate::output::RetainedOutput)> =
        None;
    let mut last_project_snapshot: Option<ProjectSnapshot> = None;
    let mut shutting_down = false;
    let mut dirty = true;
    let input = InputScheduler::start()?;

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
        log_views.resize_with(snapshot.processes.len(), LogView::default);
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
        let has_terminal = selected_process.terminal_mode == crate::model::TerminalMode::Pty;
        let output = outputs
            .for_process_id(selected_process.process_id)
            .expect("the Logs registry covers every configured Process");
        let known_generation = retained_view
            .as_ref()
            .filter(|(process_id, _)| *process_id == selected_process.process_id)
            .map(|(_, retained)| retained.generation);
        if let Some(changed) = output.snapshot_if_changed(known_generation) {
            retained_view = Some((selected_process.process_id, changed));
        }
        let retained = &retained_view
            .as_ref()
            .expect("the first Logs snapshot is always returned")
            .1;
        if log_views[selected].refresh(retained) {
            dirty = true;
        }
        let representation = log_views[selected].representation(has_terminal);
        let pane = selected_pane(consoles, selected_process, representation, retained);
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

        match &pane {
            SelectedPane::Terminal(_) => console.set_pane(ConsolePaneKind::Terminal),
            SelectedPane::Pipe(_) => {
                console.set_pane(ConsolePaneKind::Pipe);
                let scroll = pipe_scroll[selected].get_or_insert_with(PipeScroll::default);
                console.set_following(scroll.following());
            }
            SelectedPane::Empty => console.set_pane(ConsolePaneKind::Empty),
        }
        if dirty
            || matches!(
                &pane,
                SelectedPane::Terminal(view) if view.is_dirty()
            )
        {
            dirty = false;
            // Terminal snapshots and formatted Logs lines exist only for a
            // frame that will be drawn. Mouse-motion input that changes no
            // view state must not rebuild retained output.
            let console_snapshot = match &pane {
                SelectedPane::Terminal(view) => view.snapshot(),
                SelectedPane::Pipe(_) | SelectedPane::Empty => None,
            };
            let mut pipe_window: Vec<crate::tui::PipeLine> = Vec::new();
            let (terminal_snapshot, pipe_lines) = match &pane {
                SelectedPane::Terminal(_) => (console_snapshot.as_ref(), None),
                SelectedPane::Pipe(retained) => {
                    let pane_rows = console_area.height.max(1) as usize;
                    let scroll = pipe_scroll[selected].get_or_insert_with(PipeScroll::default);
                    pipe_window.extend_from_slice(scroll.window(retained, pane_rows));
                    console.set_following(scroll.following());
                    if let Some(current) = log_views[selected].current_match()
                        && let Some(line) = pipe_window
                            .iter_mut()
                            .find(|line| line.source == Some((current.sequence, current.line)))
                    {
                        line.highlight = Some((
                            line.content_offset + current.start,
                            line.content_offset + current.end,
                        ));
                    }
                    (None, Some(pipe_window.as_slice()))
                }
                SelectedPane::Empty => (None, None),
            };
            let rows = process_rows(&snapshot, selected);
            let mut header = selected_header(&snapshot.processes[selected], snapshot.now_ms);
            let view_label = match representation {
                OutputRepresentation::Terminal => "Terminal",
                OutputRepresentation::Logs => "Logs",
            };
            header.insert_str(0, &format!("{view_label} · "));
            if let Some(status) = log_views[selected].status() {
                if log_views[selected].is_editing() {
                    header.insert_str(0, &format!("{status} · "));
                } else {
                    header.push_str(&format!(" · {status}"));
                }
            }
            if shutting_down {
                header.insert_str(0, "Project shutdown in progress · ");
            }
            let mut view = console.view();
            view.search_editing = log_views[selected].is_editing();
            view.search_active = log_views[selected].has_search();
            view.logs_selection = pipe_scroll[selected]
                .as_ref()
                .is_some_and(PipeScroll::has_selection);
            view.logs_scrollbar = pipe_scroll[selected]
                .as_ref()
                .and_then(PipeScroll::scrollbar);
            console_area =
                render_frame(outer, &rows, terminal_snapshot, pipe_lines, view, &header)?;
            let cursor = terminal_snapshot.and_then(|snap| snap.cursor);
            outer.set_cursor_shape(cursor)?;
        }

        let input_batch = input.receive(pending_resize.poll_interval(Instant::now()))?;
        if input_batch.is_empty() {
            continue;
        }

        for (input_event, repeats) in input_batch {
            match input_event {
                Event::Key(key)
                    if should_quit(key, console.view(), log_views[selected].is_editing()) =>
                {
                    if !shutting_down {
                        supervisor.command(Command::Shutdown {
                            deadline: Instant::now() + PROJECT_SHUTDOWN_WAIT,
                        });
                        shutting_down = true;
                        dirty = true;
                    }
                }
                Event::Key(key) => {
                    let action = log_views[selected].handle_key(
                        key,
                        console.view().mode == crate::tui::ConsoleViewMode::ProcessList,
                        has_terminal,
                        retained,
                    );
                    if action != LogViewAction::Ignored {
                        console.clear_warning();
                    }
                    match action {
                        LogViewAction::Changed => dirty = true,
                        LogViewAction::Pause => {
                            let scroll = pipe_scroll[selected].get_or_insert_default();
                            scroll.clear_selection();
                            scroll.pause();
                            console.set_following(false);
                            dirty = true;
                        }
                        LogViewAction::Follow => {
                            pipe_scroll[selected].get_or_insert_default().follow();
                            console.set_following(true);
                            dirty = true;
                        }
                        LogViewAction::ShowMatch(found) => {
                            pipe_scroll[selected]
                                .get_or_insert_default()
                                .show_source((found.sequence, found.line));
                            console.set_following(false);
                            dirty = true;
                        }
                        LogViewAction::Copy => {
                            let rows = console_area.height.max(1) as usize;
                            let scroll = pipe_scroll[selected].get_or_insert_default();
                            let text = scroll.selected_text(retained).unwrap_or_else(|| {
                                scroll
                                    .window(retained, rows)
                                    .iter()
                                    .map(|line| line.text.as_str())
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            });
                            console.copy_logs(text);
                            dirty = true;
                        }
                        LogViewAction::Ignored => match &pane {
                            SelectedPane::Terminal(view) => {
                                // This seam decides whether a key reaches the child.
                                let changed = view.with(|session| {
                                    console.route_pane_key(
                                        ConsolePaneKind::Terminal,
                                        selected_process.input_focused,
                                        key,
                                        Some(session),
                                        &mut pipe_scroll[selected],
                                        console_area.height.max(1),
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
                                if console.route_pane_key(
                                    kind,
                                    selected_process.input_focused,
                                    key,
                                    None,
                                    &mut pipe_scroll[selected],
                                    console_area.height.max(1),
                                ) {
                                    dirty = true;
                                }
                            }
                        },
                    }
                }
                Event::Paste(data) if log_views[selected].is_editing() => {
                    console.clear_warning();
                    match log_views[selected].paste_search(&data, retained) {
                        LogViewAction::ShowMatch(found) => {
                            pipe_scroll[selected]
                                .get_or_insert_default()
                                .show_source((found.sequence, found.line));
                            console.set_following(false);
                        }
                        LogViewAction::Ignored
                        | LogViewAction::Changed
                        | LogViewAction::Pause
                        | LogViewAction::Follow
                        | LogViewAction::Copy => {}
                    }
                    dirty = true;
                }
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
                    let child_tracks_mouse = selected_process.input_focused
                        && matches!(&pane, SelectedPane::Terminal(view) if view.mouse_tracking());
                    if handle_app_mouse(
                        &mut console,
                        mouse,
                        &pane,
                        &mut selected,
                        child_tracks_mouse,
                        repeats,
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
}

fn handle_app_mouse(
    console: &mut ConsoleInteraction,
    mouse: MouseEvent,
    pane: &SelectedPane<'_>,
    selected: &mut usize,
    child_tracks_mouse: bool,
    repeats: usize,
    pipe_scroll: &mut [Option<PipeScroll>],
) -> bool {
    let process_count = pipe_scroll.len();
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let (list, console_outer, _) =
        project_layout(ratatui::layout::Rect::new(0, 0, cols, rows), process_count);
    let console_inner = pane_inner(console_outer);

    // A Logs scrollbar drag keeps its owner when the pointer leaves the pane.
    if matches!(pane, SelectedPane::Pipe(_))
        && pipe_scroll[*selected]
            .as_ref()
            .is_some_and(PipeScroll::scrollbar_gesture_active)
        && matches!(mouse.kind, MouseEventKind::Drag(_) | MouseEventKind::Up(_))
        && let SelectedPane::Pipe(retained) = pane
    {
        return console.handle_read_only_mouse(
            mouse,
            console_inner,
            repeats,
            &mut pipe_scroll[*selected],
            retained,
        );
    }

    // A console-owned drag keeps its original owner when the pointer leaves
    // the pane. Deliver Drag and Up before hit-testing another surface.
    if console.mouse_gesture_active()
        && matches!(mouse.kind, MouseEventKind::Drag(_) | MouseEventKind::Up(_))
        && let SelectedPane::Terminal(view) = pane
    {
        return view
            .with(|session| {
                (0..repeats).fold(false, |changed, _| {
                    console.handle_mouse(mouse, console_inner, child_tracks_mouse, session)
                        || changed
                })
            })
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
            MouseEventKind::ScrollUp => *selected = (*selected).saturating_sub(repeats),
            MouseEventKind::ScrollDown => {
                *selected = selected
                    .saturating_add(repeats)
                    .min(process_count.saturating_sub(1));
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
                (0..repeats).fold(false, |changed, _| {
                    console.handle_mouse(mouse, console_inner, child_tracks_mouse, session)
                        || changed
                })
            })
            .unwrap_or(false),
        SelectedPane::Pipe(_) => {
            if mouse_starts_console_focus(mouse.kind, console.view().mode) {
                console.focus_console(None);
            }
            console.handle_read_only_mouse(
                mouse,
                console_inner,
                repeats,
                &mut pipe_scroll[*selected],
                match pane {
                    SelectedPane::Pipe(retained) => retained,
                    _ => unreachable!("the selected pane is Logs"),
                },
            ) || mouse_changes_focus(mouse.kind)
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
enum SelectedPane<'a> {
    Terminal(ConsoleView),
    Pipe(&'a crate::output::RetainedOutput),
    Empty,
}

fn selected_pane<'a>(
    consoles: &Consoles,
    process: &ProcessSnapshot,
    representation: OutputRepresentation,
    retained: &'a crate::output::RetainedOutput,
) -> SelectedPane<'a> {
    if representation == OutputRepresentation::Logs
        || process.terminal_mode == crate::model::TerminalMode::Pipe
    {
        return SelectedPane::Pipe(retained);
    }
    let Some(run_id) = process.current_run else {
        return SelectedPane::Empty;
    };
    consoles
        .view_process(process.process_id, run_id)
        .map(SelectedPane::Terminal)
        .unwrap_or(SelectedPane::Empty)
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

fn should_quit(key: KeyEvent, view: crate::tui::ConsoleViewState, search_editing: bool) -> bool {
    key.kind == KeyEventKind::Press
        && is_quit(key, view)
        && (!search_editing
            || key
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL))
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
