use super::*;
use std::ffi::OsString;
use std::time::{Duration, Instant};

use crate::model::{
    Autostart, CommandForm, EffectiveProject, Enabled, InputPolicy, ProcessKind, ProcessSpec,
    ReadinessCheck, ReadinessConfig, ReadinessProbe, TerminalMode,
};
use crate::supervisor::clock::SystemClock;
use crate::supervisor::core::Core;
use crate::supervisor::runtime::RealRunSeam;
use crate::supervisor::seam::{LogMatcherIntent, ProbeIntent, ProbeSeam};
use crate::supervisor::{Command, Lifecycle, ReadinessState};

struct NoProbes;

impl ProbeSeam for NoProbes {
    fn probe(&self, _intent: ProbeIntent, _events: &SeamSender) {
        panic!("the test Project has no readiness probe");
    }
}

fn one_shot_project(command: CommandForm) -> EffectiveProject {
    EffectiveProject::new(vec![ProcessSpec {
        name: "setup".to_string(),
        kind: ProcessKind::OneShot,
        enabled: Enabled::Yes,
        autostart: Autostart::No,
        success_exit_codes: vec![0],
        command,
        working_dir: std::env::temp_dir(),
        env: Vec::new(),
        terminal_mode: TerminalMode::Pipe,
        input_policy: InputPolicy::Disabled,
        dependencies: Vec::new(),
        readiness: None,
    }])
    .expect("valid one-Process Project")
}

fn intent(program: &str, args: &[&str]) -> StartIntent {
    StartIntent {
        process_id: ProcessId::new(0),
        run_id: RuntimeRunId::new(1),
        program: OsString::from(program),
        args: args.iter().map(OsString::from).collect(),
        working_dir: std::env::temp_dir(),
        env: Vec::new(),
        initial_geometry: TerminalGeometry::DEFAULT,
        pty: false,
        log_matchers: Vec::new(),
    }
}

fn log_intent(program: &str, args: &[&str], pty: bool) -> StartIntent {
    let mut intent = intent(program, args);
    intent.pty = pty;
    intent.log_matchers = vec![LogMatcherIntent {
        work_id: WorkId::new(9),
        contains: "early-log-marker".to_string(),
    }];
    intent
}

fn real_log_project(marker_file: &std::path::Path) -> EffectiveProject {
    EffectiveProject::new(vec![ProcessSpec {
        name: "api".to_string(),
        kind: ProcessKind::Service,
        enabled: Enabled::Yes,
        autostart: Autostart::No,
        success_exit_codes: vec![0],
        command: CommandForm::Shell {
            text: "if [ ! -e \"$STACKHAND_LOG_RUN\" ]; then : > \"$STACKHAND_LOG_RUN\"; printf 'ready'; fi; sleep 30"
                .to_string(),
        },
        working_dir: std::env::temp_dir(),
        env: vec![(
            "STACKHAND_LOG_RUN".to_string(),
            marker_file.to_string_lossy().into_owned(),
        )],
        terminal_mode: TerminalMode::Pipe,
        input_policy: InputPolicy::Disabled,
        dependencies: Vec::new(),
        readiness: Some(ReadinessConfig {
            checks: vec![ReadinessCheck {
                probe: ReadinessProbe::Log {
                    contains: "ready".to_string(),
                },
                initial_delay: Duration::from_secs(30),
                interval: Duration::from_secs(1),
                timeout: Duration::from_secs(1),
                success_threshold: 1,
                failure_threshold: 1,
            }],
            startup_timeout: None,
        }),
    }])
    .expect("valid real log-readiness Project")
}

fn receive_log_match(receiver: &crossbeam_channel::Receiver<SeamEvent>) -> SeamEvent {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "log match did not arrive");
        let event = receiver
            .recv_timeout(remaining)
            .expect("production adapter event arrived");
        if matches!(event, SeamEvent::LogMatched { .. }) {
            return event;
        }
    }
}

fn receive_finished(
    receiver: &crossbeam_channel::Receiver<SeamEvent>,
) -> (Vec<SeamEvent>, FinishedRun) {
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut received = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "Run did not finish: {received:?}");
        let event = receiver
            .recv_timeout(remaining)
            .expect("production adapter event arrived");
        received.push(event.clone());
        if let SeamEvent::Finished(finished) = event {
            return (received, finished);
        }
    }
}

#[test]
fn natural_completion_reports_one_finished_run_after_spawn() {
    let seam = RealRunSeam::new(Arc::new(OutputViews::new(1)));
    let (tx, rx) = crossbeam_channel::unbounded();
    let events = SeamSender::new(tx);

    seam.start(intent("sh", &["-c", "exit 0"]), &events);
    let (received, finished) = receive_finished(&rx);

    assert!(matches!(received.first(), Some(SeamEvent::Spawned { .. })));
    assert_eq!(
        received
            .iter()
            .filter(|event| matches!(event, SeamEvent::Finished(_)))
            .count(),
        1
    );
    assert_eq!(finished.exit_code, Some(0));
    assert!(!finished.intentional_stop);
    assert!(finished.cleanup_confirmed);
    assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
}

#[test]
fn next_run_reservation_replaces_the_one_published_completion_record() {
    let seam = RealRunSeam::new(Arc::new(OutputViews::new(1)));
    let (tx, rx) = crossbeam_channel::unbounded();
    let events = SeamSender::new(tx);

    seam.start(intent("sh", &["-c", "exit 0"]), &events);
    let (_, first) = receive_finished(&rx);
    assert_eq!(first.run_id, RuntimeRunId::new(1));
    assert_eq!(seam.runs.lock().unwrap().len(), 1);

    let mut second = intent("sh", &["-c", "exit 0"]);
    second.run_id = RuntimeRunId::new(2);
    seam.start(second, &events);
    {
        let runs = seam.runs.lock().unwrap();
        assert_eq!(runs.len(), 1);
        assert!(!runs.contains_key(&(0, 1)));
        assert!(runs.contains_key(&(0, 2)));
    }

    let (_, second) = receive_finished(&rx);
    assert_eq!(second.run_id, RuntimeRunId::new(2));
    assert_eq!(seam.runs.lock().unwrap().len(), 1);
}

#[test]
fn stop_after_reservation_has_one_owner_and_one_finished_run() {
    let seam = RealRunSeam::new(Arc::new(OutputViews::new(1)));
    let (tx, rx) = crossbeam_channel::unbounded();
    let events = SeamSender::new(tx);
    let start = intent("sh", &["-c", "sleep 30"]);

    seam.start(start.clone(), &events);
    seam.stop(
        start.process_id,
        start.run_id,
        Some(Duration::from_secs(2)),
        &events,
    );
    let (received, finished) = receive_finished(&rx);

    assert!(matches!(received.first(), Some(SeamEvent::Spawned { .. })));
    assert_eq!(
        received
            .iter()
            .filter(|event| matches!(event, SeamEvent::Finished(_)))
            .count(),
        1
    );
    assert!(finished.intentional_stop);
    assert!(finished.cleanup_confirmed, "{finished:?}");
    assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
}

#[test]
fn project_shutdown_stop_racing_a_published_completion_is_a_no_op() {
    let after_finished = TestPause::new();
    let seam = RealRunSeam::with_test_hooks(
        Arc::new(OutputViews::new(1)),
        AdapterTestHooks {
            after_finished: Some(after_finished.clone()),
            ..Default::default()
        },
    );
    let (tx, rx) = crossbeam_channel::unbounded();
    let mut core = Core::new(
        one_shot_project(CommandForm::Shell {
            text: "exit 0".to_string(),
        }),
        Box::new(seam),
        Box::new(NoProbes),
        Arc::new(SystemClock),
        SeamSender::new(tx),
        TerminalGeometry::DEFAULT,
    );

    core.command(Command::Start("setup".to_string()));
    let spawned = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("the Run reports its spawn");
    assert!(matches!(spawned, SeamEvent::Spawned { .. }));
    core.event(spawned);

    // The owner has published the real completion and finalized its
    // registry state, but the Core has not applied that completion yet.
    after_finished.wait_until_reached();
    core.command(Command::Shutdown {
        deadline: Instant::now() + Duration::from_secs(2),
    });
    let pending: Vec<_> = rx.try_iter().collect();
    after_finished.resume();

    assert_eq!(
        pending
            .iter()
            .filter(|event| matches!(event, SeamEvent::Finished(_)))
            .count(),
        1,
        "the real completion is the only completion fact: {pending:?}"
    );
    assert!(
        !pending
            .iter()
            .any(|event| matches!(event, SeamEvent::Failed { .. })),
        "Project shutdown fabricated a failure: {pending:?}"
    );
    for event in pending {
        core.event(event);
    }
    let snapshot = core.snapshot();
    let shutdown = snapshot.shutdown.expect("Project shutdown is visible");
    assert!(shutdown.complete);
    assert!(shutdown.failures.is_empty());
    assert_eq!(snapshot.processes[0].recent_runs.len(), 1);
}

#[test]
fn stop_during_spawn_retains_one_marker_and_early_pipe_output() {
    const SENTINEL: &str = "stackhand-stop-during-spawn-sentinel";

    let after_spawn = TestPause::new();
    let outputs = Arc::new(OutputViews::new(1));
    let seam = RealRunSeam::with_test_hooks(
        Arc::clone(&outputs),
        AdapterTestHooks {
            after_spawn: Some(after_spawn.clone()),
            ..Default::default()
        },
    );
    let (tx, rx) = crossbeam_channel::unbounded();
    let events = SeamSender::new(tx);
    let ready = std::env::temp_dir().join(format!(
        "stackhand-stop-during-spawn-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let _ = std::fs::remove_file(&ready);
    let mut start = intent(
        "sh",
        &[
            "-c",
            "printf 'stackhand-stop-during-spawn-sentinel\\n'; : > \"$READY_FILE\"; sleep 30",
        ],
    );
    start.env.push((
        "READY_FILE".to_string(),
        ready.to_string_lossy().into_owned(),
    ));

    seam.start(start.clone(), &events);
    after_spawn.wait_until_reached();
    let ready_deadline = Instant::now() + Duration::from_secs(2);
    while !ready.exists() && Instant::now() < ready_deadline {
        thread::sleep(Duration::from_millis(2));
    }
    if !ready.exists() {
        after_spawn.resume();
        panic!("the child did not write its early-output ready file");
    }
    seam.stop(
        start.process_id,
        start.run_id,
        Some(Duration::from_secs(2)),
        &events,
    );
    after_spawn.resume();
    let (_, finished) = receive_finished(&rx);
    assert!(finished.cleanup_confirmed, "{finished:?}");

    let output = outputs
        .for_process_id(start.process_id)
        .expect("the Process output exists");
    let retained_deadline = Instant::now() + Duration::from_secs(2);
    let snapshot = loop {
        let snapshot = output.snapshot();
        if snapshot.chunks.iter().any(|chunk| {
                matches!(chunk, crate::output::RetainedChunk::Data { text, .. } if text.contains(SENTINEL))
            }) {
                break snapshot;
            }
        assert!(
            Instant::now() < retained_deadline,
            "early output was not retained: {snapshot:?}"
        );
        thread::sleep(Duration::from_millis(2));
    };
    let _ = std::fs::remove_file(ready);

    let marker_positions: Vec<_> = snapshot
        .chunks
        .iter()
        .enumerate()
        .filter_map(|(index, chunk)| {
            matches!(
                chunk,
                crate::output::RetainedChunk::Marker { run_id: 1, .. }
            )
            .then_some(index)
        })
        .collect();
    let sentinel_position = snapshot
        .chunks
        .iter()
        .position(|chunk| {
            matches!(chunk, crate::output::RetainedChunk::Data { run_id: 1, text, .. } if text.contains(SENTINEL))
        })
        .expect("the retained sentinel has the stopped Run identity");

    assert_eq!(marker_positions.len(), 1);
    assert!(marker_positions[0] < sentinel_position);
    assert_eq!(snapshot.latest_run, Some(1));
}

#[test]
fn real_pipe_run_reports_a_live_log_match() {
    let seam = RealRunSeam::new(Arc::new(OutputViews::new(1)));
    let (tx, rx) = crossbeam_channel::unbounded();
    let events = SeamSender::new(tx);
    let start = log_intent("sh", &["-c", "printf 'early-log-marker'; sleep 30"], false);

    seam.start(start.clone(), &events);
    let event = receive_log_match(&rx);
    assert_eq!(event.process_id(), start.process_id);
    assert_eq!(event.run_id(), start.run_id);
    assert!(matches!(event, SeamEvent::LogMatched { work_id, .. } if work_id == WorkId::new(9)));

    seam.stop(
        start.process_id,
        start.run_id,
        Some(Duration::from_secs(2)),
        &events,
    );
    let (_, finished) = receive_finished(&rx);
    assert!(finished.cleanup_confirmed, "{finished:?}");
}

#[test]
fn real_pipe_without_matching_output_emits_no_log_match() {
    let seam = RealRunSeam::new(Arc::new(OutputViews::new(1)));
    let (tx, rx) = crossbeam_channel::unbounded();
    let events = SeamSender::new(tx);
    let start = log_intent("sh", &["-c", "printf 'not-ready'; sleep 30"], false);

    seam.start(start.clone(), &events);
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut matched = false;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok(event) = rx.recv_timeout(remaining) else {
            break;
        };
        if matches!(event, SeamEvent::LogMatched { .. }) {
            matched = true;
            break;
        }
    }
    assert!(!matched, "non-matching output produced a log match");

    seam.stop(
        start.process_id,
        start.run_id,
        Some(Duration::from_secs(2)),
        &events,
    );
    let (_, finished) = receive_finished(&rx);
    assert!(finished.cleanup_confirmed, "{finished:?}");
}

#[test]
fn real_replacement_ignores_a_live_match_from_the_previous_run() {
    let marker_file = std::env::temp_dir().join(format!(
        "stackhand-log-rerun-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let _ = std::fs::remove_file(&marker_file);
    let outputs = Arc::new(OutputViews::new(1));
    let seam = RealRunSeam::new(Arc::clone(&outputs));
    let (tx, rx) = crossbeam_channel::unbounded();
    let events = SeamSender::new(tx);
    let mut core = Core::new(
        real_log_project(&marker_file),
        Box::new(seam),
        Box::new(NoProbes),
        Arc::new(SystemClock),
        events,
        TerminalGeometry::DEFAULT,
    );

    core.command(Command::Start("api".to_string()));
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut first_spawned = None;
    let mut first_match = None;
    while first_spawned.is_none() || first_match.is_none() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "the first Run did not emit its setup facts"
        );
        let event = rx
            .recv_timeout(remaining)
            .expect("the first real Run emitted an event");
        match &event {
            SeamEvent::Spawned { run_id, .. } if *run_id == RuntimeRunId::new(1) => {
                first_spawned = Some(event);
            }
            SeamEvent::LogMatched { run_id, .. } if *run_id == RuntimeRunId::new(1) => {
                first_match = Some(event);
            }
            _ => {}
        }
    }

    // Hold the real first-Run match instead of applying it, then stop the
    // first Run and wait for its confirmed cleanup before starting Run 2.
    core.event(first_spawned.expect("the first Run reports Spawned"));
    core.command(Command::Stop("api".to_string()));
    let (_, finished) = receive_finished(&rx);
    assert_eq!(finished.run_id, RuntimeRunId::new(1));
    core.event(SeamEvent::Finished(finished));
    assert_eq!(core.snapshot().processes[0].current_run, None);

    core.command(Command::Start("api".to_string()));
    let second_spawned = loop {
        let event = rx
            .recv_timeout(Duration::from_secs(8))
            .expect("the replacement Run reports Spawned");
        match &event {
            SeamEvent::Spawned { run_id, .. } if *run_id == RuntimeRunId::new(2) => break event,
            SeamEvent::LogMatched { run_id, .. } if *run_id == RuntimeRunId::new(2) => {
                panic!("the replacement Run unexpectedly matched its output")
            }
            _ => {}
        }
    };
    core.event(second_spawned);
    core.event(first_match.expect("the first Run emitted its live match"));

    let snapshot = core.snapshot();
    assert_eq!(snapshot.processes[0].lifecycle, Lifecycle::Starting);
    assert_eq!(
        snapshot.processes[0].readiness.as_ref().unwrap().state,
        ReadinessState::Pending
    );

    core.command(Command::Stop("api".to_string()));
    let (_, finished) = receive_finished(&rx);
    assert_eq!(finished.run_id, RuntimeRunId::new(2));
    core.event(SeamEvent::Finished(finished));
    let _ = std::fs::remove_file(marker_file);
}

#[test]
fn real_pty_run_reports_a_live_log_match() {
    let seam = RealRunSeam::new(Arc::new(OutputViews::new(1)));
    let (tx, rx) = crossbeam_channel::unbounded();
    let events = SeamSender::new(tx);
    let start = log_intent("sh", &["-c", "printf 'early-log-marker'; sleep 30"], true);

    seam.start(start.clone(), &events);
    let event = receive_log_match(&rx);
    assert_eq!(event.process_id(), start.process_id);
    assert_eq!(event.run_id(), start.run_id);
    assert!(matches!(event, SeamEvent::LogMatched { work_id, .. } if work_id == WorkId::new(9)));

    seam.stop(
        start.process_id,
        start.run_id,
        Some(Duration::from_secs(2)),
        &events,
    );
    let (_, finished) = receive_finished(&rx);
    assert!(finished.cleanup_confirmed, "{finished:?}");
}

#[test]
fn real_pipe_log_match_is_not_limited_by_retained_history() {
    const MARKER: &str = "retained-history-log-marker";
    let outputs = Arc::new(OutputViews::new(1));
    let seam = RealRunSeam::new(Arc::clone(&outputs));
    let (tx, rx) = crossbeam_channel::unbounded();
    let events = SeamSender::new(tx);
    let mut start = intent(
        "sh",
        &[
            "-c",
            "dd if=/dev/zero bs=1048576 count=2 2>/dev/null; printf 'retained-history-log-marker'; sleep 30",
        ],
    );
    start.log_matchers = vec![LogMatcherIntent {
        work_id: WorkId::new(9),
        contains: MARKER.to_string(),
    }];

    seam.start(start.clone(), &events);
    let event = receive_log_match(&rx);
    assert!(matches!(event, SeamEvent::LogMatched { work_id, .. } if work_id == WorkId::new(9)));

    let output = outputs
        .for_process_id(start.process_id)
        .expect("the Process output exists");
    let deadline = Instant::now() + Duration::from_secs(5);
    let snapshot = loop {
        let snapshot = output.snapshot();
        if snapshot.truncated {
            break snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "history did not evict output: {snapshot:?}"
        );
        thread::sleep(Duration::from_millis(2));
    };
    assert!(snapshot.dropped_bytes > 0);

    seam.stop(
        start.process_id,
        start.run_id,
        Some(Duration::from_secs(2)),
        &events,
    );
    let (_, finished) = receive_finished(&rx);
    assert!(finished.cleanup_confirmed, "{finished:?}");
}
