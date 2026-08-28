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

fn pair_finished(process: &str, run: u64, code: Option<i32>) -> SeamEvent {
    SeamEvent::Finished(FinishedRun {
        process_id: ProcessId::new(pair_index(process)),
        run_id: RunId::new(run),
        exit_code: code,
        intentional_stop: false,
        cleanup_confirmed: true,
        detail: None,
        remaining_pids: Vec::new(),
    })
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
    h.event(pair_finished("setup", 1, Some(0)));

    let setup = h.process("setup");
    assert_eq!(setup.lifecycle, Lifecycle::Done);
    assert_eq!(setup.desired, DesiredState::Stopped);
    assert_eq!(setup.failure, None);

    // Confirmed cleanup keeps Done instead of falling back to Stopped.
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
    h.event(pair_finished("setup", 1, Some(7)));

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
fn an_accepted_nonzero_exit_code_completes_and_is_retained() {
    let mut setup = simple("api", ProcessKind::OneShot, Enabled::Yes, Autostart::No);
    setup.success_exit_codes = vec![42];
    let mut h = Harness::new(EffectiveProject::new(vec![setup]).expect("unique names"));
    h.command(Command::Start("api".into()));
    h.event(spawned("api", 1));
    h.event(finished("api", 1, Some(42)));

    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Done);
    assert_eq!(api.failure, None);
    assert_eq!(api.recent_runs[0].exit, RunExitDisposition::Success);
    assert_eq!(api.recent_runs[0].exit_code, Some(42));
}

#[test]
fn a_terminating_one_shot_result_never_matches_success_exit_codes() {
    let mut h = Harness::new(
        EffectiveProject::new(vec![simple(
            "api",
            ProcessKind::OneShot,
            Enabled::Yes,
            Autostart::No,
        )])
        .expect("unique names"),
    );
    h.command(Command::Start("api".into()));
    h.event(spawned("api", 1));
    h.event(finished("api", 1, None));

    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Stopped);
    assert!(api.failure.is_some());
    assert_eq!(api.recent_runs[0].exit_code, None);
}

#[test]
fn a_spawn_failed_one_shot_releases_an_exited_dependent() {
    let project = EffectiveProject::new(vec![
        depending_exited_on("api", &["setup"]),
        simple("setup", ProcessKind::OneShot, Enabled::Yes, Autostart::No),
    ])
    .expect("unique names");
    let mut h = Harness::new(project);
    h.command(Command::Start("api".into()));
    h.event(SeamEvent::Failed {
        process_id: ProcessId::new(pair_index("setup")),
        run_id: RunId::new(1),
        kind: FailureKind::Spawn,
        detail: "spawn failed".to_string(),
    });

    assert_eq!(h.process("setup").lifecycle, Lifecycle::Stopped);
    assert_eq!(h.process("api").lifecycle, Lifecycle::Starting);
}

#[test]
fn an_exited_one_shot_releases_a_dependent_after_failure() {
    let project = EffectiveProject::new(vec![
        depending_exited_on("api", &["setup"]),
        simple("setup", ProcessKind::OneShot, Enabled::Yes, Autostart::No),
    ])
    .expect("unique names");
    let mut h = Harness::new(project);
    h.command(Command::Start("api".into()));
    assert_eq!(h.process("api").lifecycle, Lifecycle::Waiting);
    h.event(pair_spawned("setup", 1));
    h.event(pair_finished("setup", 1, Some(7)));

    let setup = h.process("setup");
    assert_eq!(setup.lifecycle, Lifecycle::Stopped);
    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Starting);
    assert_eq!(api.current_run, Some(1));
}

#[test]
fn rerunning_a_one_shot_invalidates_its_exited_condition() {
    let project = EffectiveProject::new(vec![
        depending_exited_on("api", &["setup"]),
        simple("setup", ProcessKind::OneShot, Enabled::Yes, Autostart::No),
        depending_exited_on("worker", &["setup"]),
    ])
    .expect("unique names");
    let mut h = Harness::new(project);
    h.command(Command::Start("api".into()));
    h.event(pair_spawned("setup", 1));
    h.event(pair_finished("setup", 1, Some(7)));
    assert_eq!(h.process("api").lifecycle, Lifecycle::Starting);

    h.command(Command::Restart("setup".into()));
    assert_eq!(h.process("setup").current_run, Some(2));
    h.command(Command::Start("worker".into()));
    let worker = h.process("worker");
    assert_eq!(worker.lifecycle, Lifecycle::Waiting);
    assert_eq!(worker.blocked_reason.as_deref(), Some("setup: exited"));

    h.event(pair_spawned("setup", 2));
    h.event(pair_finished("setup", 2, Some(7)));
    assert_eq!(h.process("worker").lifecycle, Lifecycle::Starting);
}

#[test]
fn an_unexpected_service_exit_keeps_desired_running_without_restarting() {
    let mut h = Harness::new(four_process_project());
    h.command(Command::Start("api".into()));
    h.event(spawned("api", 1));
    h.event(finished("api", 1, Some(7)));

    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Stopped);
    assert_eq!(api.desired, DesiredState::Running);
    assert_eq!(
        api.failure.as_ref().expect("failure is visible").detail,
        "exited unexpectedly with code 7"
    );
    assert_eq!(api.current_run, None);
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
fn done_persists_across_evaluations_stops_and_a_manual_restart() {
    let mut h = Harness::new(one_shot_project());
    h.command(Command::Start("api".into()));
    h.event(pair_spawned("setup", 1));
    h.event(pair_finished("setup", 1, Some(0)));
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
    h.event(finished("api", 1, Some(0)));

    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Stopped);
    assert_eq!(
        api.failure.as_ref().expect("failure is visible").detail,
        "exited unexpectedly with code 0"
    );
    // Desired State remains Running so a later restart policy can make an
    // explicit decision; the baseline does not start a replacement Run.
    assert_eq!(api.desired, DesiredState::Running);

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
    h.event(pair_finished("setup", 1, Some(0)));
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
    h.event(pair_finished("setup", 2, Some(0)));
    let worker = h.process("worker");
    assert_eq!(worker.current_run, Some(1));
    assert_eq!(worker.lifecycle, Lifecycle::Starting);
}
