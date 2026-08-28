//! Milestone 2 transition proofs. These tests exercise serialized commands and
//! seam events, then inspect immutable snapshots for the resulting state.
//! They keep races explicit: the event order in each test is the contract.

use super::*;

fn intentional_finished(process: &str, run: u64) -> SeamEvent {
    SeamEvent::Finished(FinishedRun {
        process_id: ProcessId::new(process_index(process)),
        run_id: RunId::new(run),
        exit_code: Some(0),
        intentional_stop: true,
        cleanup_confirmed: true,
        detail: None,
        remaining_pids: Vec::new(),
    })
}

fn start_count(h: &Harness, process: &str) -> usize {
    h.runtime
        .intents()
        .iter()
        .filter(|intent| {
            matches!(
                intent,
                Intent::Start { process_id, .. }
                    if *process_id == ProcessId::new(process_index(process))
            )
        })
        .count()
}

fn recovery_project() -> EffectiveProject {
    EffectiveProject::new(vec![
        depending_completed_on("api", &["setup"]),
        service("db"),
        service("worker"),
        simple("setup", ProcessKind::OneShot, Enabled::Yes, Autostart::No),
    ])
    .expect("unique names")
}

#[test]
fn stopping_during_spawn_discards_a_late_spawn() {
    let mut h = Harness::new(four_process_project());
    h.command(Command::Start("api".into()));
    assert_eq!(h.process("api").lifecycle, Lifecycle::Starting);
    assert_eq!(h.process("api").current_run, Some(1));

    // The stop crosses the same command seam before the runtime reports
    // Spawned. The fake runtime confirms cleanup, so the Run is released.
    h.command(Command::Stop("api".into()));
    assert_eq!(h.process("api").lifecycle, Lifecycle::Stopped);
    assert_eq!(h.process("api").current_run, None);

    // A late spawn report cannot restore a stopped Run.
    h.event(spawned("api", 1));
    let api = h.process("api");
    assert_eq!(api.desired, DesiredState::Stopped);
    assert_eq!(api.lifecycle, Lifecycle::Stopped);
    assert_eq!(api.current_run, None);
    assert_eq!(start_count(&h, "api"), 1);
}

#[test]
fn readiness_and_exit_order_has_one_serialized_result() {
    // Readiness first promotes the Run. The later exit still ends that same
    // Run as an unexpected process failure.
    let mut readiness_first =
        Harness::new(EffectiveProject::new(vec![probed_service("api")]).expect("unique names"));
    start_probed(&mut readiness_first);
    readiness_first.advance_and_poll(Duration::ZERO);
    readiness_first.event(readiness("api", 1, true, None));
    assert_eq!(readiness_first.process("api").lifecycle, Lifecycle::Running);
    readiness_first.event(finished("api", 1, Some(0)));

    // Exit first releases the Run. A later readiness result is stale and
    // cannot promote it.
    let mut exit_first =
        Harness::new(EffectiveProject::new(vec![probed_service("api")]).expect("unique names"));
    start_probed(&mut exit_first);
    exit_first.advance_and_poll(Duration::ZERO);
    exit_first.event(finished("api", 1, Some(0)));
    exit_first.event(readiness("api", 1, true, None));

    for harness in [&readiness_first, &exit_first] {
        let api = harness.process("api");
        assert_eq!(api.desired, DesiredState::Running);
        assert_eq!(api.lifecycle, Lifecycle::Stopped);
        assert_eq!(api.current_run, None);
        assert_eq!(api.readiness, None);
        assert_eq!(
            api.failure.as_ref().map(|failure| failure.kind),
            Some(FailureKind::ProcessExit)
        );
    }
}

#[test]
fn manual_restart_cancels_in_flight_check_before_replacement_run() {
    let runtime = FakeRuntime::shared();
    runtime.set_hold_stops(true);
    let mut h = Harness::with(
        EffectiveProject::new(vec![probed_service("api")]).expect("unique names"),
        Arc::clone(&runtime),
    );
    start_probed(&mut h);
    h.advance_and_poll(Duration::ZERO);
    let old_request = h
        .probes
        .requests()
        .into_iter()
        .next()
        .expect("the first readiness attempt exists");

    h.command(Command::Restart("api".into()));
    let stopping = h.process("api");
    assert_eq!(stopping.desired, DesiredState::Running);
    assert_eq!(stopping.lifecycle, Lifecycle::Stopping);
    assert_eq!(stopping.current_run, Some(1));
    assert_eq!(start_count(&h, "api"), 1);
    assert_eq!(
        h.probes.cancellations(),
        vec![(
            old_request.process_id,
            old_request.run_id,
            old_request.work_id
        )]
    );

    // The replacement cannot start until the old Run's cleanup fact arrives.
    h.event(intentional_finished("api", 1));
    assert_eq!(h.process("api").current_run, Some(2));
    assert_eq!(h.process("api").lifecycle, Lifecycle::Starting);
    assert_eq!(start_count(&h, "api"), 2);

    h.event(spawned("api", 2));
    h.advance_and_poll(Duration::ZERO);
    let new_request = h
        .probes
        .requests()
        .into_iter()
        .nth(1)
        .expect("the replacement readiness attempt exists");
    assert_eq!(new_request.run_id, RunId::new(2));
    assert_ne!(new_request.work_id, old_request.work_id);

    // A released result from the canceled Run cannot affect the replacement.
    let before_stale_result = h.process("api");
    h.event(readiness_attempt("api", 1, 1, true, None));
    assert_eq!(h.process("api"), before_stale_result);

    h.event(SeamEvent::Readiness {
        process_id: new_request.process_id,
        run_id: new_request.run_id,
        work_id: new_request.work_id,
        attempt_id: new_request.attempt_id,
        passing: true,
        diagnostic: None,
    });
    assert_eq!(h.process("api").lifecycle, Lifecycle::Running);
}

#[test]
fn duplicate_current_run_events_are_harmless() {
    let mut metrics = Harness::new(four_process_project());
    metrics.command(Command::Start("api".into()));
    metrics.event(spawned("api", 1));
    let metrics_event = SeamEvent::Metrics {
        process_id: ProcessId::new(process_index("api")),
        run_id: RunId::new(1),
        cpu_percent: 12.5,
        rss_kib: 2048,
    };
    metrics.event(metrics_event.clone());
    let metrics_snapshot = metrics.process("api");
    metrics.event(metrics_event);
    assert_eq!(metrics.process("api"), metrics_snapshot);

    let finished_event = finished("api", 1, Some(0));
    metrics.event(finished_event.clone());
    let finished_snapshot = metrics.process("api");
    metrics.event(finished_event);
    assert_eq!(metrics.process("api"), finished_snapshot);

    let mut readiness_harness =
        Harness::new(EffectiveProject::new(vec![probed_service("api")]).expect("unique names"));
    start_probed(&mut readiness_harness);
    readiness_harness.advance_and_poll(Duration::ZERO);
    let request = readiness_harness
        .probes
        .requests()
        .into_iter()
        .next()
        .expect("the readiness attempt exists");
    let readiness_event = SeamEvent::Readiness {
        process_id: request.process_id,
        run_id: request.run_id,
        work_id: request.work_id,
        attempt_id: request.attempt_id,
        passing: true,
        diagnostic: None,
    };
    readiness_harness.event(readiness_event.clone());
    let readiness_snapshot = readiness_harness.process("api");
    readiness_harness.event(readiness_event);
    assert_eq!(readiness_harness.process("api"), readiness_snapshot);
}

#[test]
fn duplicate_spawn_for_a_current_run_is_ignored() {
    let mut h = Harness::new(four_process_project());
    h.command(Command::Start("api".into()));
    h.event(SeamEvent::Spawned {
        process_id: ProcessId::new(process_index("api")),
        run_id: RunId::new(1),
        root_pid: Some(crate::runtime::OsPid::new(10)),
    });
    let before = h.process("api");

    // A repeated callback with a different PID is not a second spawn. It
    // must not overwrite the first observation or schedule another Run.
    h.event(SeamEvent::Spawned {
        process_id: ProcessId::new(process_index("api")),
        run_id: RunId::new(1),
        root_pid: Some(crate::runtime::OsPid::new(11)),
    });
    let after = h.process("api");
    assert_eq!(after, before);
    assert_eq!(start_count(&h, "api"), 1);
}

#[test]
fn old_run_events_of_every_kind_cannot_change_a_replacement() {
    let mut h = Harness::new(four_process_project());
    h.command(Command::Start("api".into()));
    h.event(spawned("api", 1));
    h.event(finished("api", 1, Some(0)));
    h.command(Command::Start("api".into()));
    h.event(spawned("api", 2));

    h.event(SeamEvent::Spawned {
        process_id: ProcessId::new(0),
        run_id: RunId::new(1),
        root_pid: Some(crate::runtime::OsPid::new(99)),
    });
    h.event(SeamEvent::Failed {
        process_id: ProcessId::new(0),
        run_id: RunId::new(1),
        kind: FailureKind::ProcessExit,
        detail: "old failure".into(),
    });
    h.event(SeamEvent::OutputFailure {
        process_id: ProcessId::new(0),
        run_id: RunId::new(1),
        detail: "old output failure".into(),
    });
    h.event(SeamEvent::Metrics {
        process_id: ProcessId::new(0),
        run_id: RunId::new(1),
        cpu_percent: 99.0,
        rss_kib: 9999,
    });
    h.event(SeamEvent::Readiness {
        process_id: ProcessId::new(0),
        run_id: RunId::new(1),
        work_id: WorkId::new(1),
        attempt_id: AttemptId::new(1),
        passing: true,
        diagnostic: None,
    });
    h.event(SeamEvent::Liveness {
        process_id: ProcessId::new(0),
        run_id: RunId::new(1),
        work_id: WorkId::new(1),
        attempt_id: AttemptId::new(1),
        passing: false,
        diagnostic: Some("old liveness failure".into()),
    });
    h.event(SeamEvent::LogMatched {
        process_id: ProcessId::new(0),
        run_id: RunId::new(1),
        work_id: WorkId::new(1),
        attempt_id: None,
    });
    h.event(finished("api", 1, Some(7)));

    let api = h.process("api");
    assert_eq!(api.desired, DesiredState::Running);
    assert_eq!(api.lifecycle, Lifecycle::Running);
    assert_eq!(api.current_run, Some(2));
    assert_eq!(api.root_pid, None);
    assert_eq!(api.failure, None);
    assert_eq!(api.metrics, None);
}

#[test]
fn dependency_recovery_then_manual_stop_leaves_the_dependent_stopped() {
    let mut h = Harness::new(recovery_project());
    h.command(Command::Start("api".into()));
    assert_eq!(h.process("api").lifecycle, Lifecycle::Waiting);

    h.event(spawned("setup", 1));
    h.event(finished("setup", 1, Some(0)));
    assert_eq!(h.process("setup").lifecycle, Lifecycle::Done);
    assert_eq!(h.process("api").current_run, Some(1));
    assert_eq!(h.process("api").lifecycle, Lifecycle::Starting);

    // Recovery starts the existing waiter. A later manual stop changes its
    // desired state and prevents any additional Run.
    h.command(Command::Stop("api".into()));
    let api = h.process("api");
    assert_eq!(api.desired, DesiredState::Stopped);
    assert_eq!(api.lifecycle, Lifecycle::Stopped);
    assert_eq!(api.current_run, None);
    assert_eq!(start_count(&h, "api"), 1);
    assert_eq!(h.process("setup").lifecycle, Lifecycle::Done);
}
