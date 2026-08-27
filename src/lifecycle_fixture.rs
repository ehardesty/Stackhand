use crate::console::{ConsoleInteraction, LifecycleCommand, PipeScroll, SelectionMove};
use crate::interaction_fixture::{PAGE_ROWS, WAIT, apply_move, key, leader, wait_for};
use crate::output::OutputViews;
use crate::output::RetainedChunk;
use crate::supervisor::{Command, Consoles, Lifecycle, RunExitDisposition, SupervisorHandle};
use crate::tui::ConsolePaneKind;
use anyhow::Result;
use crossterm::event::KeyCode;

/// Prove the stop/start/restart ACs of issue #31 end to end: lifecycle
/// commands target the selected Process through the Supervisor, a manual
/// stop finishes as Stopped, a restart brings the next Run back, and a
/// clean cycle leaves no failure behind.
/// The fixture's three Processes, by role.
pub(crate) struct FixtureProcesses {
    pub(crate) focused: usize,
    pub(crate) mute: usize,
    pub(crate) piped: usize,
}

pub(crate) fn prove_lifecycle(
    console: &mut ConsoleInteraction,
    pipe_scroll: &mut [Option<PipeScroll>],
    consoles: &Consoles,
    outputs: &OutputViews,
    supervisor: &SupervisorHandle,
    processes: FixtureProcesses,
    selected: &mut usize,
) -> Result<()> {
    let FixtureProcesses {
        focused,
        mute,
        piped,
    } = processes;
    let focused_snapshot = wait_for(supervisor, WAIT, |_| true)?;
    let focused_view = consoles
        .view(
            focused as u32,
            focused_snapshot.processes[focused]
                .current_run
                .expect("the focused Process keeps a live Run"),
        )
        .expect("the focused Process has a live console");
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
    });
    assert_eq!(
        console.take_lifecycle_commands(),
        vec![LifecycleCommand::Stop]
    );
    supervisor.command(Command::Stop("focused".into()));
    wait_for(supervisor, WAIT, |snapshot| {
        snapshot.processes[focused].lifecycle == Lifecycle::Stopped
    })?;
    assert_eq!(
        wait_for(supervisor, WAIT, |_| true)?.processes[focused].failure,
        None
    );
    // While stopped its pane is empty; commands still queue through the
    // read-only path.
    console.route_pane_key(
        ConsolePaneKind::Empty,
        true,
        leader(),
        None,
        &mut pipe_scroll[focused],
        PAGE_ROWS,
    );
    console.route_pane_key(
        ConsolePaneKind::Empty,
        true,
        key(KeyCode::Char('s')),
        None,
        &mut pipe_scroll[focused],
        PAGE_ROWS,
    );
    assert_eq!(
        console.take_lifecycle_commands(),
        vec![LifecycleCommand::Start]
    );
    supervisor.command(Command::Start("focused".into()));
    wait_for(supervisor, WAIT, |snapshot| {
        snapshot.processes[focused].lifecycle == Lifecycle::Running
    })?;
    // Restart brings back the next Run ID through the same pane seam.
    let live_snapshot = wait_for(supervisor, WAIT, |_| true)?;
    let focused_run = live_snapshot.processes[focused]
        .current_run
        .expect("the restarted Process has a live Run");
    let focused_live = consoles
        .view(focused as u32, focused_run)
        .expect("the restarted Process has a live console");
    focused_live.with(|session| {
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
            key(KeyCode::Char('r')),
            Some(session),
            &mut pipe_scroll[focused],
            PAGE_ROWS,
        );
    });
    assert_eq!(
        console.take_lifecycle_commands(),
        vec![LifecycleCommand::Restart]
    );
    supervisor.command(Command::Restart("focused".into()));
    wait_for(supervisor, WAIT, |snapshot| {
        snapshot.processes[focused].current_run == Some(2)
    })?;
    assert_eq!(
        wait_for(supervisor, WAIT, |_| true)?.processes[focused].failure,
        None,
        "a clean stop and restart leaves no failure behind"
    );
    // Stop the other terminal Process through its own pane, then bring it
    // back.
    apply_move(
        console,
        pipe_scroll,
        consoles,
        outputs,
        &wait_for(supervisor, WAIT, |_| true)?,
        selected,
        SelectionMove::Down,
    );
    assert_eq!(*selected, mute);
    let live_snapshot = wait_for(supervisor, WAIT, |_| true)?;
    let mute_run = live_snapshot.processes[mute]
        .current_run
        .expect("the muted Process has a live Run");
    let mute_live = consoles
        .view(mute as u32, mute_run)
        .expect("the muted Process has a live console");
    mute_live.with(|session| {
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
            key(KeyCode::Char('x')),
            Some(session),
            &mut pipe_scroll[mute],
            PAGE_ROWS,
        );
    });
    assert_eq!(
        console.take_lifecycle_commands(),
        vec![LifecycleCommand::Stop]
    );
    supervisor.command(Command::Stop("mute".into()));
    wait_for(supervisor, WAIT, |snapshot| {
        snapshot.processes[mute].lifecycle == Lifecycle::Stopped
    })?;
    supervisor.command(Command::Start("mute".into()));
    wait_for(supervisor, WAIT, |snapshot| {
        snapshot.processes[mute].lifecycle == Lifecycle::Running
    })?;
    // The pipe Process stops and restarts through its read-only pane.
    apply_move(
        console,
        pipe_scroll,
        consoles,
        outputs,
        &wait_for(supervisor, WAIT, |_| true)?,
        selected,
        SelectionMove::Down,
    );
    assert_eq!(*selected, piped);
    console.route_pane_key(
        ConsolePaneKind::Pipe,
        false,
        leader(),
        None,
        &mut pipe_scroll[piped],
        PAGE_ROWS,
    );
    console.route_pane_key(
        ConsolePaneKind::Pipe,
        false,
        key(KeyCode::Char('x')),
        None,
        &mut pipe_scroll[piped],
        PAGE_ROWS,
    );
    assert_eq!(
        console.take_lifecycle_commands(),
        vec![LifecycleCommand::Stop]
    );
    supervisor.command(Command::Stop("piped".into()));
    wait_for(supervisor, WAIT, |snapshot| {
        snapshot.processes[piped].lifecycle == Lifecycle::Stopped
    })?;
    supervisor.command(Command::Start("piped".into()));
    wait_for(supervisor, WAIT, |snapshot| {
        snapshot.processes[piped].lifecycle == Lifecycle::Running
    })?;
    // Back to the first Process before the ingestion proof.
    apply_move(
        console,
        pipe_scroll,
        consoles,
        outputs,
        &wait_for(supervisor, WAIT, |_| true)?,
        selected,
        SelectionMove::Up,
    );
    apply_move(
        console,
        pipe_scroll,
        consoles,
        outputs,
        &wait_for(supervisor, WAIT, |_| true)?,
        selected,
        SelectionMove::Up,
    );
    assert_eq!(*selected, focused);
    Ok(())
}

/// Prove the One-shot rerun ACs of issue #32: the first attempt starts
/// and completes, the rerun opens the next Run through the pane key
/// seam, the retained output keeps a marker for both attempts, and the
/// Supervisor records bounded recent Run summaries.
pub(crate) fn prove_rerun(
    console: &mut ConsoleInteraction,
    pipe_scroll: &mut [Option<PipeScroll>],
    consoles: &Consoles,
    outputs: &OutputViews,
    supervisor: &SupervisorHandle,
    oneoff: usize,
    selected: &mut usize,
) -> Result<()> {
    // Select the One-shot; it is idle and its pane is the retained pipe
    // view.
    let mut snapshot = wait_for(supervisor, WAIT, |_| true)?;
    while *selected != oneoff {
        apply_move(
            console,
            pipe_scroll,
            consoles,
            outputs,
            &snapshot,
            selected,
            SelectionMove::Down,
        );
        snapshot = wait_for(supervisor, WAIT, |_| true)?;
    }
    assert_eq!(
        wait_for(supervisor, WAIT, |_| true)?.processes[oneoff].lifecycle,
        Lifecycle::Idle
    );
    // Start the first attempt through the pane seam; the app dispatches
    // Start for the selected Process.
    console.route_pane_key(
        ConsolePaneKind::Pipe,
        false,
        leader(),
        None,
        &mut pipe_scroll[oneoff],
        PAGE_ROWS,
    );
    console.route_pane_key(
        ConsolePaneKind::Pipe,
        false,
        key(KeyCode::Char('s')),
        None,
        &mut pipe_scroll[oneoff],
        PAGE_ROWS,
    );
    assert_eq!(
        console.take_lifecycle_commands(),
        vec![LifecycleCommand::Start]
    );
    supervisor.command(Command::Start("oneoff".into()));
    wait_for(supervisor, WAIT, |snapshot| {
        snapshot.processes[oneoff].lifecycle == Lifecycle::Done
    })?;
    assert_eq!(
        wait_for(supervisor, WAIT, |_| true)?.processes[oneoff].failure,
        None
    );
    // Rerun: the app maps `r` on a One-shot to the Supervisor's Rerun
    // command, and the new attempt receives the next Run ID.
    console.route_pane_key(
        ConsolePaneKind::Pipe,
        false,
        leader(),
        None,
        &mut pipe_scroll[oneoff],
        PAGE_ROWS,
    );
    console.route_pane_key(
        ConsolePaneKind::Pipe,
        false,
        key(KeyCode::Char('r')),
        None,
        &mut pipe_scroll[oneoff],
        PAGE_ROWS,
    );
    assert_eq!(
        console.take_lifecycle_commands(),
        vec![LifecycleCommand::Restart]
    );
    supervisor.command(Command::Rerun("oneoff".into()));
    wait_for(supervisor, WAIT, |snapshot| {
        snapshot.processes[oneoff].current_run == Some(2)
    })?;
    wait_for(supervisor, WAIT, |snapshot| {
        snapshot.processes[oneoff].lifecycle == Lifecycle::Done
    })?;

    // The bounded recent summaries record both attempts, newest first.
    let oneoff_snapshot = wait_for(supervisor, WAIT, |_| true)?;
    let recent = &oneoff_snapshot.processes[oneoff].recent_runs;
    assert_eq!(recent.len(), 2, "the summary window keeps both attempts");
    assert_eq!(recent[0].run_id, 2);
    assert_eq!(recent[0].exit, RunExitDisposition::Success);
    assert!(!recent[0].intentional_stop);
    assert_eq!(recent[1].run_id, 1);
    assert_eq!(recent[1].exit, RunExitDisposition::Success);

    // The retained output still carries a marker for each attempt within
    // its bounds.
    let retained = outputs
        .for_process(oneoff as u32)
        .expect("the One-shot has a retained output module")
        .snapshot();
    let markers: Vec<u64> = retained
        .chunks
        .iter()
        .filter_map(|chunk| match chunk {
            RetainedChunk::Marker { run_id, .. } => Some(*run_id),
            RetainedChunk::Data { .. } => None,
        })
        .collect();
    assert!(
        markers.contains(&1) && markers.contains(&2),
        "both attempts must keep their Run marker inside the bounds"
    );
    assert!(
        retained
            .chunks
            .iter()
            .any(|chunk| matches!(chunk, RetainedChunk::Data { text, .. } if text.contains("oneoff-run ok"))),
        "the One-shot's output stays retained"
    );
    // Back to the first Process (focused) before the ingestion proof:
    // every move is a fresh drain-and-apply from the current pane.
    while *selected > 0 {
        let snapshot = wait_for(supervisor, WAIT, |_| true)?;
        apply_move(
            console,
            pipe_scroll,
            consoles,
            outputs,
            &snapshot,
            selected,
            SelectionMove::Up,
        );
    }
    Ok(())
}
