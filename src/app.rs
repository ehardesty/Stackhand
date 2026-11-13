use std::path::Path;
use std::time::{Duration, Instant};

use crate::geometry::TerminalGeometry;
use crate::output::OutputViews;
use crate::supervisor::{Command, Consoles, ProjectSnapshot};
use crate::tui::{
    ConsoleBody, ConsoleContent, OuterTerminal, ProcessListRow, ProcessRowView, ProjectProfileMenu,
    RenderPhase, VisibleActions, pane_inner, project_layout, render_project_with_search,
};
use anyhow::{Result, anyhow, bail};
use crossterm::event::Event;

use self::input_scheduler::InputScheduler;
use self::resize::PendingResize;
use self::session::ProjectSession;
use crate::host::{HostAction, SessionSignal, SessionSignals, SessionStop, write_regular_report};

mod input_scheduler;
pub(crate) mod interaction;
mod resize;
mod session;
mod view_model;

use interaction::{InputResult, ProjectInteraction, SelectedPane};
use view_model::{process_list_title, process_rows, selected_header};

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(16);
const RESIZE_SETTLE_INTERVAL: Duration = Duration::from_millis(16);

/// Load the explicit Project, start enabled autostart Processes, and
/// supervise them interactively until the user quits with a controlled
/// Project shutdown.
pub fn run_project(config_path: &Path) -> Result<()> {
    run_project_with_profile(config_path, None)
}

/// Load the explicit Project with one selected profile, then run it
/// interactively.
pub fn run_project_with_profile(config_path: &Path, profile: Option<&str>) -> Result<()> {
    run_project_session(Some(config_path), profile, true)
}

/// Discover the nearest base Project, then run it interactively.
pub fn run_discovered_project() -> Result<()> {
    run_discovered_project_with_profile(None)
}

/// Discover the nearest base Project with one selected profile, then run it
/// interactively.
pub fn run_discovered_project_with_profile(profile: Option<&str>) -> Result<()> {
    run_project_session(None, profile, true)
}

/// Run an explicit or discovered Project, optionally leaving all Processes stopped
/// on entry. This does not change configured autostart or later start actions.
pub fn run_project_session(
    config_path: Option<&Path>,
    profile: Option<&str>,
    autostart: bool,
) -> Result<()> {
    run_project_cli_session(config_path, profile, autostart)?.into_result()
}

/// Interactive completion whose diagnostics have already passed through the
/// terminal finalizer. The CLI must return its exit code without printing again.
pub struct SessionExit {
    result: Result<()>,
    protocol_submitted: bool,
}

impl SessionExit {
    pub fn exit_code(&self) -> std::process::ExitCode {
        if self.result.is_ok() {
            std::process::ExitCode::SUCCESS
        } else {
            std::process::ExitCode::FAILURE
        }
    }

    /// Whether final terminal commands were accepted by the OS. This does not
    /// confirm host display.
    pub fn protocol_submitted(&self) -> bool {
        self.protocol_submitted
    }

    pub fn into_result(self) -> Result<()> {
        self.result
    }
}

/// Resolve configuration before acquiring the host. Errors returned directly
/// are preflight errors; SessionExit carries completion after host acquisition.
pub fn run_project_cli_session(
    config_path: Option<&Path>,
    profile: Option<&str>,
    autostart: bool,
) -> Result<SessionExit> {
    let request = match config_path {
        Some(path) => crate::config::ResolutionRequest::explicit_with_profile(
            path,
            profile.map(str::to_owned),
        ),
        None => crate::config::ResolutionRequest::discover_with_profile(profile.map(str::to_owned)),
    };
    let resolution =
        crate::config::resolve(request).map_err(|error| anyhow!("configuration error: {error}"))?;
    session::require_interactive_terminal()?;
    let signals = SessionSignals::install()?;
    let stop = SessionStop::default();
    let (mut outer, source) = OuterTerminal::acquire(stop.clone())?;
    let mut input = None;
    let outcome = (|| -> Result<(Result<()>, Result<()>)> {
        input = Some(InputScheduler::start(source, stop.clone())?);
        let input = input.as_ref().expect("input scheduler started");
        outer.query_colors()?;
        outer.query_keyboard()?;
        let keyboard = input.keyboard_support();
        let colors = outer.color_profile();
        outer.enable_modes(keyboard)?;
        if let Some(error) = stop.error() {
            bail!(error);
        }
        if stop.requested() {
            return Ok((Ok(()), Ok(())));
        }
        let (mut project, consoles, outputs) =
            ProjectSession::start(resolution.into_project(), colors)?;
        if autostart && !stop.requested() {
            project.supervisor().command(Command::StartAutostart);
        }
        let end = run_event_loop(
            &mut outer,
            input,
            &mut project,
            &consoles,
            &outputs,
            &signals,
            &stop,
        );
        input.discard();
        outer.quiesce();
        let cleanup = project.finish(None);
        Ok((end, cleanup))
    })();
    stop.stop();
    drop(input);
    let (end, cleanup) = outcome.unwrap_or_else(|error| (Err(error), Ok(())));
    let mut report = session::report(end, signals.first_observed(), cleanup);
    if let Some(error) = stop.error()
        && (signals.received().is_none() || stop.framing_fault())
    {
        report.fail(anyhow!(error));
    }
    let reported_length = report.text.len();
    let restoration = outer.finish(&report.text);
    if let Err(error) = restoration.local_termios {
        report.fail(error.into());
    }
    let protocol_submitted = restoration.protocol == crate::host::ProtocolDelivery::Submitted;
    if !protocol_submitted && signals.received().is_none() {
        report.fail(anyhow!("terminal protocol restoration was incomplete"));
    }
    write_regular_report(&report.text[reported_length..]);
    Ok(SessionExit {
        result: report.result,
        protocol_submitted,
    })
}

fn run_event_loop(
    outer: &mut OuterTerminal,
    input: &InputScheduler,
    project: &mut ProjectSession,
    consoles: &Consoles,
    outputs: &OutputViews,
    signals: &SessionSignals,
    stop: &SessionStop,
) -> Result<()> {
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
    let mut dirty = true;

    loop {
        if let Some(error) = stop.error() {
            bail!(error);
        }
        if observe_shutdown_signal(project, input, signals, &mut dirty) {
            return Ok(());
        }
        if interaction.poll_requests() {
            dirty = true;
        }
        let snapshot: ProjectSnapshot = project
            .supervisor()
            .snapshot()
            .ok_or_else(|| anyhow!("the Supervisor stopped"))?;
        if snapshot.processes.is_empty() {
            bail!("this Project has no Processes");
        }
        if project.is_shutting_down()
            && (snapshot
                .shutdown
                .as_ref()
                .is_some_and(|result| result.complete)
                || project.shutdown_wait_expired())
        {
            return Ok(());
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
            project.supervisor().command(command);
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
        if (interaction.needs_logs(selected_process) || known_generation.is_none())
            && let Some(changed) = output.snapshot_if_changed(known_generation)
        {
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

        if observe_shutdown_signal(project, input, signals, &mut dirty) {
            return Ok(());
        }
        if dirty || ProjectInteraction::terminal_is_dirty(&pane) {
            dirty = false;
            // Terminal snapshots and formatted Logs lines exist only for a
            // frame that will be drawn.
            let frame = interaction.frame(&pane, retained, console_area.height.max(1) as usize);
            let rows = process_rows(&snapshot, selected);
            // The sequence is selection-independent; the frame borrows a copy
            // while the table state is mutably borrowed.
            let process_list = interaction.process_list().to_vec();
            let list_title = process_list_title(&snapshot);
            let mut header = selected_header(&snapshot.processes[selected], snapshot.now_ms);
            if project.is_shutting_down() {
                header.insert_str(0, "Project shutdown in progress · ");
            }
            let (process_table_state, profile_menu, visible_actions) = interaction.render_state();
            let rendered = render_frame(
                outer,
                frame.terminal.as_ref().and_then(|snapshot| snapshot.cursor),
                if project.is_shutting_down() {
                    RenderPhase::ShuttingDown
                } else {
                    RenderPhase::Running
                },
                &rows,
                &process_list,
                process_table_state,
                ConsoleContent {
                    body: frame.lines.as_deref().map_or(
                        ConsoleBody::Terminal(frame.terminal.as_ref()),
                        ConsoleBody::Logs,
                    ),
                    logs_prompt: frame.logs_prompt.as_ref(),
                    header: &header,
                    status: frame.logs_status.as_deref(),
                },
                frame.view,
                &list_title,
                profile_menu,
                visible_actions,
            );
            console_area = match rendered {
                Ok(area) => area,
                Err(_) if project.is_shutting_down() => return Ok(()),
                Err(error) => return Err(error),
            };
        }

        if observe_shutdown_signal(project, input, signals, &mut dirty) {
            return Ok(());
        }
        let input_batch = input.receive(pending_resize.poll_interval(Instant::now()))?;
        if input_batch.is_empty() {
            continue;
        }

        for (action, repeats) in input_batch {
            let input_event = match action {
                HostAction::Input(event) => event,
                HostAction::PasteRejected => {
                    interaction.warn(crate::tui::ConsoleWarning::PasteTooLarge);
                    dirty = true;
                    continue;
                }
            };
            if observe_shutdown_signal(project, input, signals, &mut dirty) {
                return Ok(());
            }
            if project.is_shutting_down() {
                continue;
            }
            if let Event::Resize(cols, rows) = input_event {
                // The child sees exactly the rendered console pane.
                let area = ratatui::layout::Rect::new(0, 0, cols, rows);
                let (_, pane, _) = project_layout(area, interaction.process_list_row_count());
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
                InputResult::Quit => {
                    project.begin();
                    input.discard();
                    dirty = true;
                }
                InputResult::Changed => dirty = true,
                InputResult::OpenPort(port) => {
                    if open_local_port(port).is_err() {
                        interaction.warn(crate::tui::ConsoleWarning::LinkOpenFailed);
                    }
                    dirty = true;
                }
                InputResult::Ignored => {}
            }
        }
    }
}

fn observe_shutdown_signal(
    project: &mut ProjectSession,
    input: &InputScheduler,
    signals: &SessionSignals,
    dirty: &mut bool,
) -> bool {
    match signals.received() {
        Some(SessionSignal::Hangup) => true,
        Some(SessionSignal::Terminate) if !project.is_shutting_down() => {
            project.begin();
            input.discard();
            *dirty = true;
            false
        }
        _ => false,
    }
}

fn open_local_port(port: u16) -> std::io::Result<()> {
    let url = format!("http://localhost:{port}/");
    #[cfg(target_os = "macos")]
    let mut child = std::process::Command::new("/usr/bin/open")
        .arg(url)
        .spawn()?;
    #[cfg(target_os = "linux")]
    let mut child = std::process::Command::new("xdg-open").arg(url).spawn()?;
    #[cfg(all(not(target_os = "macos"), not(target_os = "linux")))]
    return Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "opening browser links is not implemented on this platform",
    ));
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn render_frame(
    outer: &mut OuterTerminal,
    cursor: Option<crate::terminal::OwnedCursorState>,
    phase: RenderPhase,
    rows: &[ProcessRowView],
    process_list: &[ProcessListRow],
    process_table_state: &mut ratatui::widgets::TableState,
    console: ConsoleContent<'_>,
    view: crate::tui::ConsoleViewState,
    process_list_title: &str,
    profile_menu: &mut ProjectProfileMenu,
    visible_actions: &mut VisibleActions,
) -> Result<ratatui::layout::Rect> {
    let mut pane = None;
    outer
        .draw(cursor, phase, |frame| {
            pane = Some(render_project_with_search(
                frame,
                rows,
                process_list,
                process_table_state,
                console,
                view,
                process_list_title,
                profile_menu,
                visible_actions,
            ));
        })
        .map_err(|error| anyhow!("render failed: {error}"))?;
    pane.ok_or_else(|| anyhow!("the frame did not render"))
}

#[cfg(test)]
mod tests;
