use crate::app::interaction::ProjectInteraction;
use crate::console::SelectionMove;
use crate::interaction_fixture::{WAIT, apply_move, key, route_project_key, wait_for};
use crate::output::OutputViews;
use crate::output::RetainedChunk;
use crate::supervisor::{Command, Consoles, Lifecycle, RunExitDisposition, SupervisorHandle};
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
    consoles: &Consoles,
    outputs: &OutputViews,
    supervisor: &SupervisorHandle,
    processes: FixtureProcesses,
    interaction: &mut ProjectInteraction,
) -> Result<()> {
    let FixtureProcesses {
        focused,
        mute,
        piped,
    } = processes;

    let snapshot = wait_for(supervisor, WAIT, |_| true)?;
    dispatch_project_key(
        interaction,
        consoles,
        outputs,
        supervisor,
        &snapshot,
        KeyCode::Char('x'),
    );
    wait_for(supervisor, WAIT, |snapshot| {
        snapshot.processes[focused].lifecycle == Lifecycle::Stopped
    })?;
    assert_eq!(
        wait_for(supervisor, WAIT, |_| true)?.processes[focused].failure,
        None
    );

    let snapshot = wait_for(supervisor, WAIT, |_| true)?;
    dispatch_project_key(
        interaction,
        consoles,
        outputs,
        supervisor,
        &snapshot,
        KeyCode::Char('s'),
    );
    wait_for(supervisor, WAIT, |snapshot| {
        snapshot.processes[focused].lifecycle == Lifecycle::Running
    })?;

    let live_snapshot = wait_for(supervisor, WAIT, |_| true)?;
    let run_before_restart = live_snapshot.processes[focused]
        .current_run
        .expect("the restarted Process has a live Run");
    dispatch_project_key(
        interaction,
        consoles,
        outputs,
        supervisor,
        &live_snapshot,
        KeyCode::Char('r'),
    );
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

    apply_move(
        interaction,
        consoles,
        outputs,
        &wait_for(supervisor, WAIT, |_| true)?,
        SelectionMove::Down,
    );
    assert_eq!(interaction.selected(), mute);
    let snapshot = wait_for(supervisor, WAIT, |_| true)?;
    dispatch_project_key(
        interaction,
        consoles,
        outputs,
        supervisor,
        &snapshot,
        KeyCode::Char('x'),
    );
    wait_for(supervisor, WAIT, |snapshot| {
        snapshot.processes[mute].lifecycle == Lifecycle::Stopped
    })?;
    let snapshot = wait_for(supervisor, WAIT, |_| true)?;
    dispatch_project_key(
        interaction,
        consoles,
        outputs,
        supervisor,
        &snapshot,
        KeyCode::Char('s'),
    );
    wait_for(supervisor, WAIT, |snapshot| {
        snapshot.processes[mute].lifecycle == Lifecycle::Running
    })?;

    apply_move(
        interaction,
        consoles,
        outputs,
        &wait_for(supervisor, WAIT, |_| true)?,
        SelectionMove::Down,
    );
    assert_eq!(interaction.selected(), piped);
    let snapshot = wait_for(supervisor, WAIT, |_| true)?;
    dispatch_project_key(
        interaction,
        consoles,
        outputs,
        supervisor,
        &snapshot,
        KeyCode::Char('x'),
    );
    wait_for(supervisor, WAIT, |snapshot| {
        snapshot.processes[piped].lifecycle == Lifecycle::Stopped
    })?;
    let snapshot = wait_for(supervisor, WAIT, |_| true)?;
    dispatch_project_key(
        interaction,
        consoles,
        outputs,
        supervisor,
        &snapshot,
        KeyCode::Char('s'),
    );
    wait_for(supervisor, WAIT, |snapshot| {
        snapshot.processes[piped].lifecycle == Lifecycle::Running
    })?;

    apply_move(
        interaction,
        consoles,
        outputs,
        &wait_for(supervisor, WAIT, |_| true)?,
        SelectionMove::Up,
    );
    apply_move(
        interaction,
        consoles,
        outputs,
        &wait_for(supervisor, WAIT, |_| true)?,
        SelectionMove::Up,
    );
    assert_eq!(interaction.selected(), focused);
    Ok(())
}

fn dispatch_project_key(
    interaction: &mut ProjectInteraction,
    consoles: &Consoles,
    outputs: &OutputViews,
    supervisor: &SupervisorHandle,
    snapshot: &crate::supervisor::ProjectSnapshot,
    code: KeyCode,
) {
    for command in route_project_key(interaction, consoles, outputs, snapshot, key(code)) {
        supervisor.command(command);
    }
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
    consoles: &Consoles,
    outputs: &OutputViews,
    supervisor: &SupervisorHandle,
    focused: usize,
    interaction: &mut ProjectInteraction,
) -> Result<()> {
    while interaction.selected() != focused {
        apply_move(
            interaction,
            consoles,
            outputs,
            &wait_for(supervisor, WAIT, |_| true)?,
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
    consoles: &Consoles,
    outputs: &OutputViews,
    supervisor: &SupervisorHandle,
    oneoff: usize,
    interaction: &mut ProjectInteraction,
) -> Result<()> {
    // The selection rests on the first Process after the metrics proof;
    // walk down to the idle One-shot, whose pane is the retained pipe
    // view.
    let mut snapshot = wait_for(supervisor, WAIT, |_| true)?;
    while interaction.selected() != oneoff {
        apply_move(
            interaction,
            consoles,
            outputs,
            &snapshot,
            SelectionMove::Down,
        );
        snapshot = wait_for(supervisor, WAIT, |_| true)?;
    }
    // Start the first attempt through the production interaction seam.
    dispatch_project_key(
        interaction,
        consoles,
        outputs,
        supervisor,
        &snapshot,
        KeyCode::Char('s'),
    );
    wait_for(supervisor, WAIT, |snapshot| {
        snapshot.processes[oneoff].lifecycle == Lifecycle::Done
    })?;
    assert_eq!(
        wait_for(supervisor, WAIT, |_| true)?.processes[oneoff].failure,
        None
    );
    // Rerun: the production interaction seam maps `r` on a One-shot to
    // the Supervisor's Rerun command.
    let snapshot = wait_for(supervisor, WAIT, |_| true)?;
    dispatch_project_key(
        interaction,
        consoles,
        outputs,
        supervisor,
        &snapshot,
        KeyCode::Char('r'),
    );
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

    while interaction.selected() > 0 {
        apply_move(
            interaction,
            consoles,
            outputs,
            &wait_for(supervisor, WAIT, |_| true)?,
            SelectionMove::Up,
        );
    }
    Ok(())
}
