//! Controlled Project shutdown ordering, admission, deadlines, and results.

use super::*;

fn shutdown_project() -> EffectiveProject {
    let api = depending_on("api", &["db"]);
    let worker = service("worker");
    let mut setup = simple("setup", ProcessKind::OneShot, Enabled::Yes, Autostart::No);
    setup.dependencies = vec![crate::model::DependencySpec {
        name: "api".to_string(),
        condition: DependencyCondition::Started,
    }];
    EffectiveProject::new(vec![api, service("db"), worker, setup]).expect("valid shutdown graph")
}

fn start_all(h: &mut Harness) {
    h.report_spawns();
    h.command(Command::Start("setup".into()));
    h.command(Command::Start("worker".into()));
    assert!(h.snapshot().processes.iter().all(|process| {
        process.current_run.is_some()
            && matches!(process.lifecycle, Lifecycle::Starting | Lifecycle::Running)
    }));
}

fn stop_names(runtime: &FakeRuntime) -> Vec<&'static str> {
    runtime
        .intents()
        .into_iter()
        .filter_map(|intent| match intent {
            Intent::Stop { process_id, .. } => Some(match process_id.get() {
                0 => "api",
                1 => "db",
                2 => "worker",
                3 => "setup",
                other => panic!("unexpected Process identity {other}"),
            }),
            Intent::Start { .. } => None,
        })
        .collect()
}

#[test]
fn shutdown_stops_dependent_waves_and_independent_processes_together() {
    let mut h = Harness::new(shutdown_project());
    start_all(&mut h);
    let deadline = h.clock.now() + Duration::from_secs(10);

    h.command(Command::Shutdown { deadline });

    assert_eq!(stop_names(&h.runtime), ["worker", "setup", "api", "db"]);
    assert!(
        h.snapshot()
            .processes
            .iter()
            .all(|process| process.desired == DesiredState::Stopped)
    );
    let result = h.snapshot().shutdown.expect("shutdown is observable");
    assert!(result.complete);
    assert!(!result.timed_out);
    assert!(result.failures.is_empty());
}

#[test]
fn shutdown_closes_admission_and_repeated_requests_share_one_operation() {
    let mut h = Harness::new(shutdown_project());
    start_all(&mut h);
    let deadline = h.clock.now() + Duration::from_secs(10);
    h.command(Command::Shutdown { deadline });
    let intents_after_shutdown = h.runtime.intents().len();

    h.command(Command::Start("api".into()));
    h.command(Command::Restart("worker".into()));
    h.command(Command::Rerun("setup".into()));
    h.command(Command::Shutdown {
        deadline: deadline + Duration::from_secs(30),
    });

    assert_eq!(h.runtime.intents().len(), intents_after_shutdown);
    assert!(h.snapshot().shutdown.expect("same shutdown").complete);
}

#[test]
fn every_cleanup_failure_is_retained_with_remaining_pids() {
    let runtime = FakeRuntime::shared();
    runtime
        .fail_cleanup
        .store(true, std::sync::atomic::Ordering::Release);
    let mut h = Harness::with(shutdown_project(), Arc::clone(&runtime));
    start_all(&mut h);

    h.command(Command::Shutdown {
        deadline: h.clock.now() + Duration::from_secs(10),
    });

    let result = h.snapshot().shutdown.expect("shutdown result exists");
    assert!(result.complete);
    assert_eq!(result.failures.len(), 4);
    assert!(
        result
            .failures
            .iter()
            .all(|failure| failure.remaining_pids == [99])
    );
    assert_eq!(stop_names(&runtime), ["worker", "setup", "api", "db"]);
}

#[test]
fn later_waves_receive_only_the_shared_deadlines_remaining_time() {
    let runtime = FakeRuntime::shared();
    runtime.set_hold_stops(true);
    let mut h = Harness::with(shutdown_project(), Arc::clone(&runtime));
    start_all(&mut h);
    h.command(Command::Shutdown {
        deadline: h.clock.now() + Duration::from_secs(10),
    });
    h.clock.advance(Duration::from_secs(3));

    for process in ["worker", "setup"] {
        h.event(SeamEvent::Finished(FinishedRun {
            process_id: ProcessId::new(process_index(process)),
            run_id: RunId::new(1),
            exit_code: Some(0),
            intentional_stop: true,
            cleanup_confirmed: true,
            detail: None,
            remaining_pids: Vec::new(),
        }));
    }
    let budgets: Vec<_> = runtime
        .intents()
        .into_iter()
        .filter_map(|intent| match intent {
            Intent::Stop { remaining, .. } => remaining,
            Intent::Start { .. } => None,
        })
        .collect();
    assert_eq!(
        budgets,
        [
            Duration::from_secs(10),
            Duration::from_secs(10),
            Duration::from_secs(7),
        ]
    );
}

#[test]
fn one_deadline_bounds_unfinished_runs_and_reports_each_process() {
    let runtime = FakeRuntime::shared();
    runtime.set_hold_stops(true);
    let mut h = Harness::with(shutdown_project(), Arc::clone(&runtime));
    start_all(&mut h);
    let deadline = h.clock.now() + Duration::from_secs(5);

    h.command(Command::Shutdown { deadline });
    assert_eq!(stop_names(&runtime), ["worker", "setup"]);
    h.advance_and_poll(Duration::from_secs(5));
    assert_eq!(stop_names(&runtime), ["worker", "setup", "api", "db"]);

    let result = h.snapshot().shutdown.expect("shutdown result exists");
    assert!(result.complete);
    assert!(result.timed_out);
    assert_eq!(result.failures.len(), 4);
    assert!(
        result
            .failures
            .iter()
            .all(|failure| failure.remaining_pids == [1])
    );
}
