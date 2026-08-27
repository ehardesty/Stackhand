//! One-shot lifecycle behavior tests: completion, failure, persistence,
//! and the `completed_successfully` gate. They share the harness and helpers
//! of the parent tests module.

use super::*;

fn one_shot_project() -> EffectiveProject {
    EffectiveProject::new(vec![
        depending_completed_on("api", &["setup"]),
        simple("setup", ProcessKind::OneShot, Enabled::Yes, Autostart::No),
    ])
    .expect("unique names")
}

/// These two-process tests register: api (index 0), setup (index 1).
fn pair_index(name: &str) -> u32 {
    match name {
        "api" => 0,
        "setup" => 1,
        "worker" => 2,
        other => panic!("unknown test process {other}"),
    }
}

fn pair_spawned(process: &str, run: u64) -> SeamEvent {
    SeamEvent::Spawned {
        process_id: ProcessId::new(pair_index(process)),
        run_id: RunId::new(run),
        root_pid: None,
    }
}

fn pair_exited(process: &str, run: u64, code: Option<i32>) -> SeamEvent {
    SeamEvent::Exited {
        process_id: ProcessId::new(pair_index(process)),
        run_id: RunId::new(run),
        code,
    }
}

fn shutdown_complete(process: &str, run: u64) -> SeamEvent {
    SeamEvent::ShutdownComplete {
        process_id: ProcessId::new(pair_index(process)),
        run_id: RunId::new(run),
        confirmed: true,
        detail: None,
        remaining_pids: Vec::new(),
    }
}

#[test]
fn exit_zero_becomes_done_and_releases_the_dependent() {
    let mut h = Harness::new(one_shot_project());
    h.command(Command::Start("api".into()));

    // Starting the dependent schedules the One-shot too; the dependent
    // waits visibly until completion.
    assert_eq!(h.process("setup").current_run, Some(1));
    assert_eq!(h.process("api").lifecycle, Lifecycle::Waiting);
    assert_eq!(
        h.process("api").blocked_reason.as_deref(),
        Some("setup: completed_successfully")
    );

    h.event(pair_spawned("setup", 1));
    h.event(pair_exited("setup", 1, Some(0)));

    let setup = h.process("setup");
    assert_eq!(setup.lifecycle, Lifecycle::Done);
    assert_eq!(setup.desired, DesiredState::Stopped);
    assert_eq!(setup.failure, None);

    // Cleanup keeps Done instead of falling back to Stopped.
    h.event(shutdown_complete("setup", 1));
    assert_eq!(h.process("setup").lifecycle, Lifecycle::Done);

    // The satisfied condition releases the dependent automatically.
    assert_eq!(h.process("api").current_run, Some(1));
    assert_eq!(h.process("api").lifecycle, Lifecycle::Starting);
}

#[test]
fn non_zero_exit_fails_and_keeps_the_dependent_blocked_with_a_reason() {
    let mut h = Harness::new(one_shot_project());
    h.command(Command::Start("api".into()));
    h.event(pair_spawned("setup", 1));
    h.event(pair_exited("setup", 1, Some(7)));
    h.event(shutdown_complete("setup", 1));

    let setup = h.process("setup");
    assert_eq!(setup.lifecycle, Lifecycle::Stopped);
    assert_eq!(setup.desired, DesiredState::Stopped);
    assert_eq!(
        setup.failure.expect("failure is visible").detail,
        "exited with code 7"
    );

    let api = h.process("api");
    assert_eq!(api.current_run, None);
    assert_eq!(
        api.blocked_reason.as_deref(),
        Some("setup: completed_successfully (exited with code 7)")
    );
}

#[test]
fn done_persists_across_evaluations_stops_and_a_manual_restart() {
    let mut h = Harness::new(one_shot_project());
    h.command(Command::Start("api".into()));
    h.event(pair_spawned("setup", 1));
    h.event(pair_exited("setup", 1, Some(0)));
    h.event(shutdown_complete("setup", 1));
    assert_eq!(h.process("setup").lifecycle, Lifecycle::Done);

    // Extra scheduling passes, StopAll, and a direct Stop never clobber
    // the terminal Done state.
    h.command(Command::StopAll);
    assert_eq!(h.process("setup").lifecycle, Lifecycle::Done);
    h.command(Command::Stop("setup".into()));
    assert_eq!(h.process("setup").lifecycle, Lifecycle::Done);

    // A Done One-shot may be started again; it receives the next Run id.
    h.command(Command::Start("setup".into()));
    assert_eq!(h.process("setup").current_run, Some(2));
    assert_eq!(h.process("setup").desired, DesiredState::Running);
}

#[test]
fn stopping_a_running_one_shot_finishes_stopped_never_done() {
    let mut h = Harness::new(one_shot_project());
    h.command(Command::Start("setup".into()));
    h.event(pair_spawned("setup", 1));

    h.command(Command::Stop("setup".into()));

    let setup = h.process("setup");
    assert_eq!(setup.lifecycle, Lifecycle::Stopped);
    assert_eq!(setup.failure, None);
}

#[test]
fn service_natural_exit_shows_stopped_with_failure_never_done() {
    let mut h = Harness::new(four_process_project());
    h.command(Command::Start("api".into()));
    h.event(spawned("api", 1));
    // Even a zero exit is not Service completion; it is an unexpected
    // natural exit observed by the adapter.
    h.event(exited("api", 1, Some(0)));

    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Running);
    assert_eq!(
        api.failure.expect("failure is visible").detail,
        "exited unexpectedly with code 0"
    );
    assert_eq!(api.desired, DesiredState::Stopped);

    h.event(shutdown_complete("api", 1));
    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Stopped);
    assert_eq!(api.current_run, None);
    assert!(api.failure.is_some());
    // No automatic restart: only the original Start ever reached the
    // runtime.
    assert_eq!(
        h.runtime
            .intents()
            .iter()
            .filter(|intent| matches!(intent, Intent::Start { .. }))
            .count(),
        1
    );
}

#[test]
fn a_rerun_invalidates_the_previous_completion_until_it_completes() {
    let project = EffectiveProject::new(vec![
        depending_completed_on("api", &["setup"]),
        simple("setup", ProcessKind::OneShot, Enabled::Yes, Autostart::No),
        depending_completed_on("worker", &["setup"]),
    ])
    .expect("unique names");
    let mut h = Harness::new(project);
    h.command(Command::Start("api".into()));
    h.event(pair_spawned("setup", 1));
    h.event(pair_exited("setup", 1, Some(0)));
    h.event(shutdown_complete("setup", 1));
    assert_eq!(h.process("setup").lifecycle, Lifecycle::Done);
    assert_eq!(h.process("api").current_run, Some(1));

    // Rerunning the One-shot immediately invalidates the previous
    // completion: a dependent that has not started yet must still wait
    // for the new Run to complete.
    h.command(Command::Restart("setup".into()));
    let setup = h.process("setup");
    assert_eq!(setup.current_run, Some(2));
    assert_eq!(setup.lifecycle, Lifecycle::Starting);

    h.command(Command::Start("worker".into()));
    let worker = h.process("worker");
    assert_eq!(worker.lifecycle, Lifecycle::Waiting);
    assert_eq!(
        worker.blocked_reason.as_deref(),
        Some("setup: completed_successfully")
    );

    // The new Run completing satisfies the condition again.
    h.event(pair_spawned("setup", 2));
    h.event(pair_exited("setup", 2, Some(0)));
    h.event(shutdown_complete("setup", 2));
    let worker = h.process("worker");
    assert_eq!(worker.current_run, Some(1));
    assert_eq!(worker.lifecycle, Lifecycle::Starting);
}
