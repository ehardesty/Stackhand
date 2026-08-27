use crate::console::{ConsoleInteraction, LifecycleCommand, PipeScroll, SelectionMove};
use crate::interaction_fixture::{PAGE_ROWS, WAIT, apply_move, key, wait_for};
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
        .view_process(
            focused_snapshot.processes[focused].process_id,
            focused_snapshot.processes[focused]
                .current_run
                .expect("the focused Process keeps a live Run"),
        )
        .expect("the focused Process has a live console");
    focused_view.with(|session| {
        console.focus_process_list(Some(session));
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
    console.focus_process_list(None);
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
        .view_process(live_snapshot.processes[focused].process_id, focused_run)
        .expect("the restarted Process has a live console");
    focused_live.with(|session| {
        console.focus_process_list(Some(session));
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
    let run_before_restart = focused_run;
    supervisor.command(Command::Restart("focused".into()));
    wait_for(supervisor, WAIT, |snapshot| {
        let process = &snapshot.processes[focused];
        process
            .current_run
            .is_some_and(|run_id| run_id > run_before_restart)
            && process.lifecycle == Lifecycle::Running
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
        .view_process(live_snapshot.processes[mute].process_id, mute_run)
        .expect("the muted Process has a live console");
    mute_live.with(|session| {
        console.focus_process_list(Some(session));
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
    console.focus_process_list(None);
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
/// Prove the metrics and diagnostics ACs of issue #33: the selected
/// header projects the live PID and Run age from the immutable snapshot,
/// and the Process list degrades its metric cells on a narrow layout.
/// Prove the AC-9 case of issue #33: without a metric sample, the Process
/// list and the selected header stay readable — no cell is invented.
pub(crate) fn prove_metrics_degradation(
    supervisor: &SupervisorHandle,
    focused: usize,
) -> Result<()> {
    let before = wait_for(supervisor, WAIT, |_| true)?;
    let process_name = before.processes[focused].name.clone();
    let previous_run = before.processes[focused]
        .current_run
        .expect("the focused Process has a live Run before restart");
    supervisor.command(Command::Restart(process_name));
    let snapshot = wait_for(supervisor, WAIT, |snapshot| {
        let process = &snapshot.processes[focused];
        process
            .current_run
            .is_some_and(|run_id| run_id > previous_run)
            && process.metrics.is_none()
            && process.root_pid.is_some_and(|pid| pid > 0)
            && process
                .run_started_at_ms
                .is_some_and(|started_at| snapshot.now_ms >= started_at)
    })?;
    let process = &snapshot.processes[focused];
    assert!(
        process
            .current_run
            .is_some_and(|run_id| run_id > previous_run),
        "the restart opens a new Run"
    );
    assert_eq!(
        process.metrics, None,
        "the fresh Run is readable before its first metrics sample"
    );
    assert!(
        process.root_pid.is_some_and(|pid| pid > 0),
        "the active Run's observed PID projects into the snapshot"
    );
    let started_at = process
        .run_started_at_ms
        .expect("an active Run carries its start stamp");
    assert!(
        snapshot.now_ms >= started_at,
        "the session time never trails the Run's start stamp"
    );

    Ok(())
}

/// Prove the metrics AC of issue #33: within one sampler interval of a
/// fresh Run, the live sample projects into the immutable snapshot and the
/// sampler's own run identity matches the active Run.
pub(crate) fn prove_metrics(
    console: &mut ConsoleInteraction,
    pipe_scroll: &mut [Option<PipeScroll>],
    consoles: &Consoles,
    outputs: &OutputViews,
    supervisor: &SupervisorHandle,
    focused: usize,
    selected: &mut usize,
) -> Result<()> {
    while *selected != focused {
        apply_move(
            console,
            pipe_scroll,
            consoles,
            outputs,
            &wait_for(supervisor, WAIT, |_| true)?,
            selected,
            SelectionMove::Up,
        );
    }
    // Selecting the focused Process re-establishes its console pane; the
    // header projection is driven by the same command flow the user runs.
    let _ = consoles;
    let snapshot = wait_for(supervisor, WAIT, |snapshot| {
        snapshot.processes[focused]
            .root_pid
            .is_some_and(|pid| pid > 0)
    })?;
    let focused_view = &snapshot.processes[focused];
    assert!(
        focused_view.root_pid.is_some(),
        "the active Run's observed PID projects into the snapshot"
    );
    assert!(
        focused_view.run_started_at_ms.is_some(),
        "an active Run carries its start stamp"
    );
    assert!(
        snapshot.now_ms >= focused_view.run_started_at_ms.unwrap_or(0),
        "the session time never trails the Run's start stamp"
    );
    // Metrics land on the active Run once the sampler runs; an undrained
    // sample must not stall the fixture, so poll a bounded window and
    // accept absence.
    let deadline = std::time::Instant::now() + WAIT;
    let mut saw_metrics = false;
    while !saw_metrics && std::time::Instant::now() < deadline {
        let snapshot = supervisor.snapshot();
        if snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.processes[focused].metrics.is_some())
        {
            saw_metrics = true;
        } else {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    assert!(
        saw_metrics,
        "the live sampler must report a metric sample within the bound"
    );
    let sampled = wait_for(supervisor, WAIT, |snapshot| {
        let process = &snapshot.processes[focused];
        process.current_run.is_some_and(|current_run| {
            process
                .metrics
                .is_some_and(|metrics| metrics.run_id == current_run)
        })
    })?;
    assert!(
        sampled.processes[focused]
            .metrics
            .is_some_and(|metrics| metrics.rss_kib > 0),
        "a live sample reports its resident memory"
    );
    Ok(())
}

pub(crate) fn prove_rerun(
    console: &mut ConsoleInteraction,
    pipe_scroll: &mut [Option<PipeScroll>],
    consoles: &Consoles,
    outputs: &OutputViews,
    supervisor: &SupervisorHandle,
    oneoff: usize,
    selected: &mut usize,
) -> Result<()> {
    // The selection rests on the first Process after the metrics proof;
    // walk down to the idle One-shot, whose pane is the retained pipe
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
    // Start the first attempt through the pane seam; the app dispatches
    // Start for the selected Process.
    console.focus_process_list(None);
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
    console.focus_process_list(None);
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
        .for_process_id(oneoff_snapshot.processes[oneoff].process_id)
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

    while *selected > 0 {
        apply_move(
            console,
            pipe_scroll,
            consoles,
            outputs,
            &wait_for(supervisor, WAIT, |_| true)?,
            selected,
            SelectionMove::Up,
        );
    }
    Ok(())
}
