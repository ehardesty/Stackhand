use std::path::Path;
use std::time::{Duration, Instant};

use crate::geometry::TerminalGeometry;
use crate::output::OutputViews;
use crate::supervisor::{
    Command, Consoles, ProjectShutdownSnapshot, ProjectSnapshot, SupervisorHandle,
};
use crate::terminal::OwnedTerminalSnapshot;
use crate::tui::{
    OuterTerminal, ProcessRowView, ProjectProfileMenu, pane_inner, project_layout,
    render_project_with_search,
};
use anyhow::{Result, anyhow, bail};
use crossterm::event::Event;

use self::input_scheduler::InputScheduler;
use self::resize::PendingResize;

mod input_scheduler;
pub(crate) mod interaction;
mod resize;
mod view_model;

use interaction::{InputResult, ProjectInteraction, SelectedPane};
use view_model::{process_list_title, process_rows, selected_header};

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
    run_resolved(crate::config::ResolutionRequest::explicit_with_profile(
        config_path,
        profile.map(str::to_owned),
    ))
}

/// Discover the nearest base Project, then run it interactively.
pub fn run_discovered_project() -> Result<()> {
    run_discovered_project_with_profile(None)
}

/// Discover the nearest base Project with one selected profile, then run it
/// interactively.
pub fn run_discovered_project_with_profile(profile: Option<&str>) -> Result<()> {
    run_resolved(crate::config::ResolutionRequest::discover_with_profile(
        profile.map(str::to_owned),
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
    let mut interaction = ProjectInteraction::default();
    let mut pending_resize = PendingResize::default();
    let mut console_area =
        pane_inner(project_layout(ratatui::layout::Rect::new(0, 0, 80, 24), 1).1);
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
        if interaction.poll_requests() {
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
        let update = interaction.update_project(&snapshot);
        dirty |= update.changed;
        for command in update.commands {
            supervisor.command(command);
        }
        let selected = interaction.selected();
        let selected_process = &snapshot.processes[selected];
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
        if interaction.refresh_logs(retained) {
            dirty = true;
        }
        let pane = interaction.pane(consoles, selected_process, retained);
        dirty |= interaction.update_pane(&pane);

        if let Some(geometry) = pending_resize.take_ready(Instant::now()) {
            if let SelectedPane::Terminal(view) = &pane
                && !view.resize(geometry)
            {
                interaction.warn(crate::tui::ConsoleWarning::InputRejected);
            }
            dirty = true;
        }

        if dirty || ProjectInteraction::terminal_is_dirty(&pane) {
            dirty = false;
            // Terminal snapshots and formatted Logs lines exist only for a
            // frame that will be drawn.
            let frame = interaction.frame(&pane, retained, console_area.height.max(1) as usize);
            let rows = process_rows(&snapshot, selected);
            let list_title = process_list_title(&snapshot);
            let mut header = selected_header(&snapshot.processes[selected], snapshot.now_ms);
            let view_label = match frame.representation {
                crate::log_view::OutputRepresentation::Terminal => "Terminal",
                crate::log_view::OutputRepresentation::Logs => "Logs",
            };
            header.insert_str(0, &format!("{view_label} · "));
            if let Some(status) = &frame.logs_status {
                header.push_str(&format!(" · {status}"));
            }
            if shutting_down {
                header.insert_str(0, "Project shutdown in progress · ");
            }
            let (process_table_state, profile_menu) = interaction.render_state();
            console_area = render_frame(
                outer,
                &rows,
                process_table_state,
                frame.terminal.as_ref(),
                frame.lines.as_deref(),
                frame.view,
                frame.search_dialog.as_ref(),
                &list_title,
                &header,
                profile_menu,
            )?;
            let cursor = frame.terminal.as_ref().and_then(|snapshot| snapshot.cursor);
            outer.set_cursor_shape(cursor)?;
        }

        let input_batch = input.receive(pending_resize.poll_interval(Instant::now()))?;
        if input_batch.is_empty() {
            continue;
        }

        for (input_event, repeats) in input_batch {
            if let Event::Resize(cols, rows) = input_event {
                // The child sees exactly the rendered console pane.
                let area = ratatui::layout::Rect::new(0, 0, cols, rows);
                let (_, pane, _) = project_layout(area, snapshot.processes.len());
                pending_resize.update(
                    TerminalGeometry::from_pane(pane_inner(pane)),
                    Instant::now(),
                );
                continue;
            }
            match interaction.route_input(
                input_event,
                repeats,
                &pane,
                selected_process,
                retained,
                console_area,
            ) {
                InputResult::Quit if !shutting_down => {
                    supervisor.command(Command::Shutdown {
                        deadline: Instant::now() + PROJECT_SHUTDOWN_WAIT,
                    });
                    shutting_down = true;
                    dirty = true;
                }
                InputResult::Changed => dirty = true,
                InputResult::Ignored | InputResult::Quit => {}
            }
        }
    }
}

fn render_frame(
    outer: &mut OuterTerminal,
    rows: &[ProcessRowView],
    process_table_state: &mut ratatui::widgets::TableState,
    console_snapshot: Option<&OwnedTerminalSnapshot>,
    pipe_lines: Option<&[crate::tui::PipeLine]>,
    view: crate::tui::ConsoleViewState,
    search_dialog: Option<&crate::log_view::SearchDialogView>,
    process_list_title: &str,
    selected_header: &str,
    profile_menu: &mut ProjectProfileMenu,
) -> Result<ratatui::layout::Rect> {
    let mut pane = None;
    outer
        .terminal_mut()
        .draw(|frame| {
            pane = Some(render_project_with_search(
                frame,
                rows,
                process_table_state,
                console_snapshot,
                pipe_lines,
                view,
                search_dialog,
                process_list_title,
                selected_header,
                profile_menu,
            ));
        })
        .map_err(|error| anyhow!("render failed: {error}"))?;
    pane.ok_or_else(|| anyhow!("the frame did not render"))
}

#[cfg(test)]
mod tests;
