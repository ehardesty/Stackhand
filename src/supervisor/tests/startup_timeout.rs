//! Startup-timeout behavior tests. They use the Supervisor seam for fake time,
//! cancellation order, cleanup outcomes, and stale-result handling.

use std::time::Duration;

use super::*;

fn startup_timeout_project(timeout: Duration) -> EffectiveProject {
    let mut process = probed_service("api");
    process
        .readiness
        .as_mut()
        .expect("the probe exists")
        .startup_timeout = Some(timeout);
    EffectiveProject::new(vec![process]).expect("unique names")
}

#[test]
fn startup_timeout_is_measured_from_spawn_and_stops_the_run() {
    let runtime = FakeRuntime::shared();
    runtime.set_hold_stops(true);
    let mut h = Harness::with(startup_timeout_project(Duration::from_secs(1)), runtime);
    h.command(Command::Start("api".into()));

    // Time spent waiting for the spawn report is not startup time.
    h.clock.advance(Duration::from_secs(5));
    h.event(spawned("api", 1));
    h.advance_and_poll(Duration::ZERO);
    assert_eq!(h.core.time_until_next_timer(), Some(Duration::from_secs(1)));

    h.advance_and_poll(Duration::from_secs(1));
    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Stopping);
    assert_eq!(api.desired, DesiredState::Stopped);
    assert_eq!(api.current_run, Some(1));
    assert_eq!(
        api.failure.as_ref().map(|failure| failure.kind),
        Some(FailureKind::Readiness)
    );
    assert!(
        api.failure
            .as_ref()
            .is_some_and(|failure| failure.detail.contains("1000 ms"))
    );
    assert_eq!(
        h.probes.cancellations(),
        vec![(ProcessId::new(0), RunId::new(1), WorkId::new(1))]
    );
    assert!(matches!(
        h.runtime.intents().as_slice(),
        [
            Intent::Start { .. },
            Intent::Cancel { process_id, run_id },
            Intent::Stop {
                process_id: stop_process,
                run_id: stop_run,
                deadline: None,
            },
        ] if *process_id == ProcessId::new(0)
            && *run_id == RunId::new(1)
            && *stop_process == ProcessId::new(0)
            && *stop_run == RunId::new(1)
    ));

    // A process exit during timeout cleanup cannot rescue the Run.
    h.event(SeamEvent::Readiness {
        process_id: ProcessId::new(0),
        run_id: RunId::new(1),
        work_id: WorkId::new(1),
        attempt_id: AttemptId::new(1),
        passing: true,
        diagnostic: None,
    });
    h.event(SeamEvent::Finished(FinishedRun {
        process_id: ProcessId::new(0),
        run_id: RunId::new(1),
        exit_code: Some(0),
        intentional_stop: false,
        cleanup_confirmed: true,
        detail: None,
        remaining_pids: Vec::new(),
    }));
    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Stopped);
    assert_eq!(api.current_run, None);
    assert_eq!(
        api.failure.as_ref().map(|failure| failure.kind),
        Some(FailureKind::Readiness)
    );
    let summary = api.recent_runs.first().expect("timed-out Run is retained");
    assert_eq!(summary.exit, RunExitDisposition::Failed { code: Some(0) });
    assert!(!summary.intentional_stop);
    assert!(
        summary
            .failure
            .as_deref()
            .is_some_and(|detail| detail.contains("cleanup confirmed"))
    );
}

#[test]
fn readiness_at_startup_deadline_wins_when_result_arrives_first() {
    let mut h = Harness::new(startup_timeout_project(Duration::from_secs(1)));
    start_probed(&mut h);
    h.advance_and_poll(Duration::ZERO);

    // Apply the result at the same instant as the deadline before polling
    // timers. The result wins because the Supervisor serializes this order.
    h.clock.advance(Duration::from_secs(1));
    h.event(readiness("api", 1, true, None));
    h.core.poll_timers(h.clock.now());
    h.drain();

    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Running);
    assert_eq!(
        api.readiness.as_ref().unwrap().state,
        ReadinessState::Passing
    );
    assert!(
        !h.runtime
            .intents()
            .iter()
            .any(|intent| matches!(intent, Intent::Stop { .. }))
    );
}

#[test]
fn omitted_startup_timeout_does_not_add_a_deadline() {
    let mut h = Harness::new(configured_readiness_project(Duration::from_secs(5), 1, 1));
    start_probed(&mut h);
    assert_eq!(h.core.time_until_next_timer(), Some(Duration::from_secs(5)));
}

#[test]
fn an_old_startup_deadline_cannot_stop_a_newer_run() {
    let mut h = Harness::new(startup_timeout_project(Duration::from_secs(1)));
    start_probed(&mut h);

    h.clock.advance(Duration::from_millis(500));
    h.command(Command::Stop("api".into()));
    h.command(Command::Start("api".into()));
    h.event(spawned("api", 2));
    h.advance_and_poll(Duration::from_millis(500));

    let api = h.process("api");
    assert_eq!(api.current_run, Some(2));
    assert_eq!(api.lifecycle, Lifecycle::Starting);
    assert!(!h.runtime.intents().iter().any(|intent| {
        matches!(intent, Intent::Stop { run_id, .. } if *run_id == RunId::new(2))
    }));
}

#[test]
fn readiness_before_the_deadline_cancels_startup_timeout_permanently() {
    let mut h = Harness::new(startup_timeout_project(Duration::from_secs(1)));
    start_probed(&mut h);
    h.advance_and_poll(Duration::ZERO);
    h.advance_and_poll(Duration::from_millis(999));
    h.event(readiness("api", 1, true, None));

    h.advance_and_poll(Duration::from_secs(5));
    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Running);
    assert_eq!(api.failure, None);
    assert!(
        !h.runtime
            .intents()
            .iter()
            .any(|intent| matches!(intent, Intent::Stop { .. }))
    );
}

#[test]
fn unconfirmed_startup_timeout_cleanup_keeps_the_timeout_and_cleanup_diagnostics() {
    let runtime = FakeRuntime::shared();
    runtime
        .fail_cleanup
        .store(true, std::sync::atomic::Ordering::Release);
    let mut h = Harness::with(startup_timeout_project(Duration::from_secs(1)), runtime);
    start_probed(&mut h);
    h.advance_and_poll(Duration::ZERO);
    h.advance_and_poll(Duration::from_secs(1));

    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Stopping);
    assert_eq!(api.current_run, Some(1));
    let failure = api.failure.expect("timeout failure remains visible");
    assert_eq!(failure.kind, FailureKind::Readiness);
    assert!(failure.detail.contains("1000 ms"));
    assert!(failure.detail.contains("cleanup failed"));

    h.runtime
        .fail_cleanup
        .store(false, std::sync::atomic::Ordering::Release);
    h.command(Command::Stop("api".into()));
    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Stopped);
    assert_eq!(api.current_run, None);
    let failure = api.failure.expect("timeout failure remains after retry");
    assert_eq!(failure.kind, FailureKind::Readiness);
    assert!(failure.detail.contains("cleanup confirmed"));
    assert_eq!(
        api.recent_runs.first().map(|run| run.exit),
        Some(RunExitDisposition::Failed { code: Some(0) })
    );
}

#[test]
fn manual_stop_cancels_startup_timeout_without_creating_a_failure() {
    let mut h = Harness::new(startup_timeout_project(Duration::from_secs(1)));
    start_probed(&mut h);
    h.command(Command::Stop("api".into()));

    h.advance_and_poll(Duration::from_secs(5));
    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Stopped);
    assert_eq!(api.current_run, None);
    assert_eq!(api.failure, None);
    assert_eq!(
        h.runtime
            .intents()
            .iter()
            .filter(|intent| matches!(intent, Intent::Stop { .. }))
            .count(),
        1
    );
}

#[test]
fn project_shutdown_cancels_startup_timeout_without_creating_a_failure() {
    let mut h = Harness::new(startup_timeout_project(Duration::from_secs(1)));
    start_probed(&mut h);
    h.command(Command::Shutdown {
        deadline: h.clock.now() + Duration::from_secs(5),
    });

    h.advance_and_poll(Duration::from_secs(5));
    let api = h.process("api");
    assert_eq!(api.failure, None);
    assert_eq!(api.current_run, None);
    assert!(h.snapshot().shutdown.expect("shutdown exists").complete);
    assert_eq!(
        h.runtime
            .intents()
            .iter()
            .filter(|intent| matches!(intent, Intent::Stop { .. }))
            .count(),
        1
    );
}
