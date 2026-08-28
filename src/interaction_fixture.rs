//! The multi-Process interaction fixture: a headless proof of terminal
//! operation across Process selection (Issue #30). It drives the console
//! interaction, terminal sessions, and retained pipe output the same way
//! the application event loop does, and prints observable checkpoints.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::console::{ConsoleInteraction, LifecycleCommand, PipeScroll, SelectionMove};
use crate::supervisor::{Command, Consoles, Lifecycle, ProcessSnapshot, SupervisorHandle};
use crate::tui::{ConsolePaneKind, ConsoleViewMode, ConsoleWarning};

pub(crate) const WAIT: Duration = Duration::from_secs(15);
const SHUTDOWN_WAIT: Duration = Duration::from_secs(20);
/// The console pane height the fixture drives; the real app measures it.
pub(crate) const PAGE_ROWS: u16 = 20;

pub(crate) fn leader() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)
}

pub(crate) fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

pub fn run(config_path: &Path) -> Result<()> {
    let project = crate::config::resolve(crate::config::ResolutionRequest::explicit(config_path))
        .map_err(|error| anyhow!("configuration error: {error}"))?
        .into_project();
    let (supervisor, consoles, outputs) = crate::supervisor::start(project)?;
    supervisor.command(Command::StartAutostart);

    let result = prove(&supervisor, &consoles, &outputs);
    let proof_ok = result.is_ok();
    let shutdown_result = shutdown(supervisor, proof_ok);
    result?;
    shutdown_result?;
    println!("interaction-shutdown-ok");
    Ok(())
}

fn prove(
    supervisor: &SupervisorHandle,
    consoles: &Consoles,
    outputs: &crate::output::OutputViews,
) -> Result<()> {
    let snapshot = wait_for(supervisor, WAIT, |snapshot| {
        snapshot
            .processes
            .iter()
            .all(|process| match process.name.as_str() {
                "focused" | "mute" | "piped" => process.lifecycle == Lifecycle::Running,
                // The One-shot stays Idle until the rerun proof starts it.
                "oneoff" => process.lifecycle == Lifecycle::Idle,
                other => panic!("the fixture contract changed: {other}"),
            })
    })?;
    let index = |name: &str| -> usize {
        snapshot
            .processes
            .iter()
            .position(|process| process.name == name)
            .unwrap_or_else(|| panic!("Process {name} is part of the fixture contract"))
    };
    let focused = index("focused");
    let mute = index("mute");
    let piped = index("piped");
    let oneoff = index("oneoff");
    let run_of = |process: &ProcessSnapshot| {
        process
            .current_run
            .unwrap_or_else(|| panic!("Process {} has no active Run", process.name))
    };
    println!("interaction-started-ok");

    let mut console = ConsoleInteraction::default();
    let mut pipe_scroll = vec![None; snapshot.processes.len()];
    let focused_view = consoles
        .view_process(
            snapshot.processes[focused].process_id,
            run_of(&snapshot.processes[focused]),
        )
        .ok_or_else(|| anyhow!("no live console view for focused"))?;
    let mute_view = consoles
        .view_process(
            snapshot.processes[mute].process_id,
            run_of(&snapshot.processes[mute]),
        )
        .ok_or_else(|| anyhow!("no live console view for mute"))?;
    outputs
        .for_process_id(snapshot.processes[piped].process_id)
        .ok_or_else(|| anyhow!("no retained output module for piped"))?;

    // Both terminals are live: their tick counters are climbing in the
    // visible view (the ready banners may already have scrolled off).
    wait_for_tick(&focused_view, "tick-", 2)?;
    wait_for_tick(&mute_view, "tick-", 2)?;

    // The Process list owns the keyboard at startup. Ctrl-A moves focus to
    // the selected console before child input can cross the pane-key seam.
    assert_eq!(console.view().mode, ConsoleViewMode::ProcessList);
    focused_view.with(|session| {
        console.route_pane_key(
            ConsolePaneKind::Terminal,
            true,
            leader(),
            Some(session),
            &mut pipe_scroll[focused],
            PAGE_ROWS,
        );
        console.route_pane_key(
            ConsolePaneKind::Terminal,
            true,
            key(KeyCode::Char('x')),
            Some(session),
            &mut pipe_scroll[focused],
            PAGE_ROWS,
        );
        console.route_pane_key(
            ConsolePaneKind::Terminal,
            true,
            key(KeyCode::Enter),
            Some(session),
            &mut pipe_scroll[focused],
            PAGE_ROWS,
        );
    });
    // The first flushed input line starts with the typed 0x78 byte.
    wait_for_console_text(&focused_view, "input-hex-1:78")?;
    // Child Ctrl-C stays child input; the Ctrl-A leader never shadows it.
    focused_view.with(|session| {
        console.route_pane_key(
            ConsolePaneKind::Terminal,
            true,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Some(session),
            &mut pipe_scroll[focused],
            PAGE_ROWS,
        );
        console.route_pane_key(
            ConsolePaneKind::Terminal,
            true,
            key(KeyCode::Enter),
            Some(session),
            &mut pipe_scroll[focused],
            PAGE_ROWS,
        );
    });
    // The second flushed input line carries the 0x03 byte.
    wait_for_console(&focused_view, second_input_line_has_zero_three)?;
    // The leader round-trips between console and Process-list focus.
    focused_view.with(|session| {
        console.route_pane_key(
            ConsolePaneKind::Terminal,
            true,
            leader(),
            Some(session),
            &mut pipe_scroll[focused],
            PAGE_ROWS,
        );
    });
    assert_eq!(console.view().mode, ConsoleViewMode::ProcessList);
    focused_view.with(|session| {
        console.route_pane_key(
            ConsolePaneKind::Terminal,
            true,
            leader(),
            Some(session),
            &mut pipe_scroll[focused],
            PAGE_ROWS,
        );
    });
    assert_eq!(console.view().mode, ConsoleViewMode::Console);
    println!("interaction-input-ok");

    // The PTY Process without focused input visibly rejects child input
    // through the same seam the app uses: the attempt is warned, and the
    // child never sees a byte, so its console shows no received input.
    assert!(!snapshot.processes[mute].input_focused);
    console.clear_pane_warning();
    mute_view.with(|session| {
        console.route_pane_key(
            ConsolePaneKind::Terminal,
            false,
            key(KeyCode::Char('x')),
            Some(session),
            &mut pipe_scroll[mute],
            PAGE_ROWS,
        );
    });
    assert_eq!(console.view().warning, Some(ConsoleWarning::InputDisabled));
    // The leader still enters the command UI, and command keys stay
    // commands: none of it is child input.
    mute_view.with(|session| {
        console.route_pane_key(
            ConsolePaneKind::Terminal,
            false,
            leader(),
            Some(session),
            &mut pipe_scroll[mute],
            PAGE_ROWS,
        );
    });
    assert_eq!(console.view().mode, ConsoleViewMode::ProcessList);
    mute_view.with(|session| {
        console.route_pane_key(
            ConsolePaneKind::Terminal,
            false,
            leader(),
            Some(session),
            &mut pipe_scroll[mute],
            PAGE_ROWS,
        );
    });
    wait_for_tick(&mute_view, "tick-", 2)?;
    assert!(
        !console_text(&mute_view).contains("mute-input-hex"),
        "the disabled-input Process must receive no child input"
    );

    // The pipe pane rejects child input visibly; the leader, pipe
    // scrolling, and the unavailable selection command still behave.
    console.set_pane(ConsolePaneKind::Pipe);
    assert!(console.route_pane_key(
        ConsolePaneKind::Pipe,
        false,
        key(KeyCode::Char('x')),
        None,
        &mut pipe_scroll[piped],
        PAGE_ROWS,
    ));
    assert_eq!(console.view().warning, Some(ConsoleWarning::PipeReadOnly));
    console.clear_pane_warning();
    assert!(console.route_pane_key(
        ConsolePaneKind::Pipe,
        false,
        leader(),
        None,
        &mut pipe_scroll[piped],
        PAGE_ROWS,
    ));
    assert_eq!(console.view().mode, ConsoleViewMode::ProcessList);
    assert!(console.route_pane_key(
        ConsolePaneKind::Pipe,
        false,
        key(KeyCode::Char('v')),
        None,
        &mut pipe_scroll[piped],
        PAGE_ROWS,
    ));
    assert_eq!(
        console.view().warning,
        Some(ConsoleWarning::SelectionUnavailable)
    );
    console.clear_pane_warning();
    // Lifecycle commands work from a read-only pane: they are
    // application commands, never child input.
    assert!(console.route_pane_key(
        ConsolePaneKind::Pipe,
        false,
        key(KeyCode::Char('x')),
        None,
        &mut pipe_scroll[piped],
        PAGE_ROWS,
    ));
    assert_eq!(
        console.take_lifecycle_commands(),
        vec![LifecycleCommand::Stop]
    );
    assert!(console.route_pane_key(
        ConsolePaneKind::Pipe,
        false,
        key(KeyCode::PageUp),
        None,
        &mut pipe_scroll[piped],
        PAGE_ROWS,
    ));
    assert_eq!(
        pipe_scroll[piped].unwrap().offset(),
        (PAGE_ROWS - 1) as usize,
        "one pipe page moves the view one page above the tail"
    );
    assert!(console.route_pane_key(
        ConsolePaneKind::Pipe,
        false,
        key(KeyCode::Char('f')),
        None,
        &mut pipe_scroll[piped],
        PAGE_ROWS,
    ));
    assert!(pipe_scroll[piped].unwrap().following());
    println!("interaction-reject-ok");

    // Scroll and follow are per Process view, proved across a real
    // selection move: paging the focused terminal into its history, then
    // moving to the other terminal, operating on it, and moving back must
    // leave both views where they were left.
    let mut selected = focused;

    // Page the focused terminal up into its history.
    focused_view.with(|session| {
        console.route_pane_key(
            ConsolePaneKind::Terminal,
            true,
            leader(),
            Some(session),
            &mut pipe_scroll[focused],
            PAGE_ROWS,
        );
        console.route_pane_key(
            ConsolePaneKind::Terminal,
            true,
            key(KeyCode::PageUp),
            Some(session),
            &mut pipe_scroll[focused],
            PAGE_ROWS,
        );
        console.route_pane_key(
            ConsolePaneKind::Terminal,
            true,
            key(KeyCode::PageUp),
            Some(session),
            &mut pipe_scroll[focused],
            PAGE_ROWS,
        );
    });
    wait_for_console_text(&focused_view, "focused-ready")?;
    // Move the selection to the other terminal through the command path.
    apply_move(
        &mut console,
        &mut pipe_scroll,
        consoles,
        outputs,
        &snapshot,
        &mut selected,
        SelectionMove::Down,
    );
    assert_eq!(selected, mute);
    // Scroll the muted terminal's own history through its own pane.
    mute_view.with(|session| {
        console.route_pane_key(
            ConsolePaneKind::Terminal,
            false,
            leader(),
            Some(session),
            &mut pipe_scroll[mute],
            PAGE_ROWS,
        );
        console.route_pane_key(
            ConsolePaneKind::Terminal,
            false,
            key(KeyCode::PageUp),
            Some(session),
            &mut pipe_scroll[mute],
            PAGE_ROWS,
        );
        console.route_pane_key(
            ConsolePaneKind::Terminal,
            false,
            key(KeyCode::PageUp),
            Some(session),
            &mut pipe_scroll[mute],
            PAGE_ROWS,
        );
    });
    wait_for_console_text(&mute_view, "mute-ready")?;
    // Move back: both views keep the scroll each one was left at.
    apply_move(
        &mut console,
        &mut pipe_scroll,
        consoles,
        outputs,
        &snapshot,
        &mut selected,
        SelectionMove::Up,
    );
    assert_eq!(selected, focused);
    assert!(
        console_text(&focused_view).contains("focused-ready"),
        "the selection move reset the focused Process's scroll"
    );
    assert!(
        console_text(&mute_view).contains("mute-ready"),
        "the selection move reset the muted Process's scroll"
    );
    // Return the muted terminal to its live tail while Process-list focus
    // remains active, then focus the selected interactive console.
    mute_view.with(|session| {
        console.route_pane_key(
            ConsolePaneKind::Terminal,
            false,
            key(KeyCode::Char('f')),
            Some(session),
            &mut pipe_scroll[mute],
            PAGE_ROWS,
        );
    });
    focused_view.with(|session| {
        console.route_pane_key(
            ConsolePaneKind::Terminal,
            true,
            leader(),
            Some(session),
            &mut pipe_scroll[focused],
            PAGE_ROWS,
        );
    });
    assert_eq!(console.view().mode, ConsoleViewMode::Console);
    println!("interaction-scroll-ok");

    // A resize reaches only the selected live PTY, and the geometry it
    // sends is never zero: each child reports its own window size, and
    // one Process's resize never changes the other's.
    // The win-size lines arrive at the live tail, so first return the
    // focused terminal to its live tail after the scroll section left it
    // paged into the history.
    focused_view.with(|session| {
        console.route_pane_key(
            ConsolePaneKind::Terminal,
            true,
            leader(),
            Some(session),
            &mut pipe_scroll[focused],
            PAGE_ROWS,
        );
        console.route_pane_key(
            ConsolePaneKind::Terminal,
            true,
            key(KeyCode::Char('f')),
            Some(session),
            &mut pipe_scroll[focused],
            PAGE_ROWS,
        );
    });
    let focused_geometry =
        crate::geometry::TerminalGeometry::from_pane(ratatui::layout::Rect::new(0, 0, 120, 40));
    let mute_geometry =
        crate::geometry::TerminalGeometry::from_pane(ratatui::layout::Rect::new(0, 0, 100, 30));
    assert!(focused_geometry.rows() > 0 && focused_geometry.cols() > 0);
    assert!(mute_geometry.rows() > 0 && mute_geometry.cols() > 0);
    assert!(focused_view.resize(focused_geometry));
    // The child reports its window as "rows cols".
    wait_for_console_text(&focused_view, "win-40 120")?;
    assert!(mute_view.resize(mute_geometry));
    wait_for_console_text(&mute_view, "win-30 100")?;
    assert!(
        console_text(&focused_view).contains("win-40 120"),
        "a resize of one Process changed another Process's terminal"
    );
    println!("interaction-resize-ok");

    // Lifecycle commands target the selected Process through the
    // Supervisor: stop finishes as Stopped, restart brings the next Run
    // back, and a clean cycle leaves no failure behind.
    crate::lifecycle_fixture::prove_lifecycle(
        &mut console,
        &mut pipe_scroll[..],
        consoles,
        outputs,
        supervisor,
        crate::lifecycle_fixture::FixtureProcesses {
            focused,
            mute,
            piped,
        },
        &mut selected,
    )?;
    println!("interaction-lifecycle-ok");

    // The metrics proof: the selected header projects the live PID and
    // the sampler's sample; the list degrades its metric cells when they
    // do not fit.
    crate::lifecycle_fixture::prove_metrics_degradation(supervisor, focused)?;
    crate::lifecycle_fixture::prove_metrics(
        &mut console,
        &mut pipe_scroll[..],
        consoles,
        outputs,
        supervisor,
        focused,
        &mut selected,
    )?;
    println!("interaction-metrics-ok");

    // The One-shot proof: start it once, rerun it through the pane key
    // seam, and check its bounded Run summaries and output markers.
    crate::lifecycle_fixture::prove_rerun(
        &mut console,
        &mut pipe_scroll[..],
        consoles,
        outputs,
        supervisor,
        oneoff,
        &mut selected,
    )?;
    println!("interaction-rerun-ok");

    crate::ingest_fixture::prove_ingest(
        &mut console,
        &mut pipe_scroll[..],
        consoles,
        outputs,
        supervisor,
        crate::ingest_fixture::FixtureIndexes {
            focused,
            mute,
            piped,
            oneoff,
        },
        &mut selected,
    )?;
    println!("interaction-ingest-ok");
    Ok(())
}

/// Route one selection move through the production pane key seam from
/// the currently selected pane, then apply the drained request exactly
/// like the app event loop does: moves clamp at the list ends, and a
/// moved selection clears the pane-scoped warning.
pub(crate) fn apply_move(
    console: &mut ConsoleInteraction,
    pipe_scroll: &mut [Option<PipeScroll>],
    consoles: &Consoles,
    outputs: &crate::output::OutputViews,
    snapshot: &crate::supervisor::ProjectSnapshot,
    selected: &mut usize,
    direction: SelectionMove,
) {
    move_selection_key(
        console,
        pipe_scroll,
        consoles,
        outputs,
        snapshot,
        *selected,
        direction,
    );
    console.apply_selection_moves(selected, snapshot.processes.len());
}

/// Send one selection move through the production pane key seam, from
/// whichever pane currently owns the keys: the terminal session path for
/// a PTY Process, or the read-only path for the pipe Process. The move
/// request is only applied when the fixture drains it, exactly like the
/// app event loop does.
fn move_selection_key(
    console: &mut ConsoleInteraction,
    pipe_scroll: &mut [Option<PipeScroll>],
    consoles: &Consoles,
    outputs: &crate::output::OutputViews,
    snapshot: &crate::supervisor::ProjectSnapshot,
    selected: usize,
    direction: SelectionMove,
) {
    let move_key = match direction {
        SelectionMove::Down => KeyCode::Char('j'),
        SelectionMove::Up => KeyCode::Char('k'),
    };
    let process = &snapshot.processes[selected];
    let pane = if process.terminal_mode == crate::model::TerminalMode::Pty {
        let live = matches!(process.lifecycle, Lifecycle::Starting | Lifecycle::Running)
            .then(|| process.current_run)
            .flatten()
            .and_then(|run_id| consoles.view_process(process.process_id, run_id));
        match live {
            Some(view) => {
                view.with(|session| {
                    if console.view().mode != ConsoleViewMode::ProcessList {
                        console.route_pane_key(
                            ConsolePaneKind::Terminal,
                            process.input_focused,
                            leader(),
                            Some(session),
                            &mut pipe_scroll[selected],
                            PAGE_ROWS,
                        );
                    }
                    console.route_pane_key(
                        ConsolePaneKind::Terminal,
                        process.input_focused,
                        key(move_key),
                        Some(session),
                        &mut pipe_scroll[selected],
                        PAGE_ROWS,
                    );
                });
                return;
            }
            None => ConsolePaneKind::Empty,
        }
    } else {
        outputs
            .for_process_id(process.process_id)
            .expect("the fixture's pipe Process has a module");
        ConsolePaneKind::Pipe
    };
    if console.view().mode != ConsoleViewMode::ProcessList {
        console.route_pane_key(
            pane,
            process.input_focused,
            leader(),
            None,
            &mut pipe_scroll[selected],
            PAGE_ROWS,
        );
    }
    console.route_pane_key(
        pane,
        process.input_focused,
        key(move_key),
        None,
        &mut pipe_scroll[selected],
        PAGE_ROWS,
    );
}

fn shutdown(supervisor: SupervisorHandle, proof_ok: bool) -> Result<()> {
    supervisor.command(Command::Shutdown {
        deadline: Instant::now() + SHUTDOWN_WAIT,
    });
    let snapshot = wait_for(&supervisor, SHUTDOWN_WAIT, |snapshot| {
        snapshot
            .shutdown
            .as_ref()
            .is_some_and(|result| result.complete)
    })?;
    // A failed proof is already reported by the caller; a failed cleanup
    // behind it would only mask the real cause. A clean proof must stop
    // every Process without a cleanup failure.
    if proof_ok {
        let shutdown = snapshot.shutdown.as_ref().expect("shutdown completed");
        if !shutdown.failures.is_empty() {
            bail!("Project shutdown failures: {:?}", shutdown.failures);
        }
        for process in &snapshot.processes {
            if let Some(failure) = &process.failure {
                bail!(
                    "Process {} reported a cleanup failure: {}",
                    process.name,
                    failure.detail
                );
            }
        }
    }
    supervisor.stop_task();
    Ok(())
}

pub(crate) fn wait_for(
    supervisor: &SupervisorHandle,
    limit: Duration,
    done: impl Fn(&crate::supervisor::ProjectSnapshot) -> bool,
) -> Result<crate::supervisor::ProjectSnapshot> {
    crate::sync_fixture::wait_for_snapshot(
        supervisor,
        crate::sync_fixture::SnapshotWait {
            timeout: limit,
            poll_interval: Duration::from_millis(25),
            stopped_message: "the Supervisor stopped before the fixture condition was met",
            timeout_message: "the fixture condition was not met within its bound",
        },
        done,
    )
}

pub(crate) fn module_text(
    outputs: &crate::output::OutputViews,
    process_id: crate::supervisor::ProcessId,
) -> String {
    let module = outputs
        .for_process_id(process_id)
        .expect("the fixture defines the pipe Process");
    module
        .snapshot()
        .chunks
        .iter()
        .filter_map(|chunk| match chunk {
            crate::output::RetainedChunk::Data { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn console_text(view: &crate::supervisor::ConsoleView) -> String {
    view.snapshot()
        .map(|snapshot| {
            snapshot
                .buffer
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect()
        })
        .unwrap_or_default()
}

/// The highest `prefix NNNN` counter visible in a flattened view.
pub(crate) fn max_tick(text: &str, prefix: &str) -> Option<u32> {
    text.match_indices(prefix)
        .filter_map(|(position, _)| {
            let rest = &text[position + prefix.len()..];
            let digits: String = rest.chars().take_while(|ch| ch.is_ascii_digit()).collect();
            digits.parse().ok()
        })
        .max()
}

/// The most recent `prefix NNNN` counter visible in a flattened module,
/// regardless of older counters retained from earlier Runs.
pub(crate) fn last_tick(text: &str, prefix: &str) -> Option<u32> {
    text.match_indices(prefix).last().and_then(|(position, _)| {
        let rest = &text[position + prefix.len()..];
        let digits: String = rest.chars().take_while(|ch| ch.is_ascii_digit()).collect();
        digits.parse().ok()
    })
}

pub(crate) fn wait_for_tick(
    view: &crate::supervisor::ConsoleView,
    prefix: &str,
    minimum: u32,
) -> Result<u32> {
    let deadline = Instant::now() + WAIT;
    loop {
        let value = max_tick(&console_text(view), prefix).unwrap_or(0);
        if value >= minimum {
            return Ok(value);
        }
        if Instant::now() >= deadline {
            bail!("the console never reached {prefix}{minimum}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_console_text(view: &crate::supervisor::ConsoleView, needle: &str) -> Result<()> {
    wait_for_console(view, |text| text.contains(needle))
}

fn wait_for_console(
    view: &crate::supervisor::ConsoleView,
    done: impl Fn(&str) -> bool,
) -> Result<()> {
    let deadline = Instant::now() + WAIT;
    loop {
        if done(&console_text(view)) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("the fixture proof never reached the console");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Whether the second flushed input line of the focused fixture child
/// carries the 0x03 byte, whatever terminator byte the Enter key encoded.
fn second_input_line_has_zero_three(text: &str) -> bool {
    let second = match text.find("input-hex-2:") {
        Some(position) => position,
        None => return false,
    };
    let rest = &text[second + "input-hex-2:".len()..];
    let hex: String = rest
        .chars()
        .take_while(|ch| ch.is_ascii_hexdigit())
        .collect();
    !hex.is_empty() && hex.contains("03")
}
