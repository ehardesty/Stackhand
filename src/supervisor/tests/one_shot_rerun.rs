//! One-shot rerun behavior: availability, completion invalidation,
//! stale-event rejection, and the bounded recent Run summary.

use super::*;
use crate::supervisor::{RECENT_RUNS, RunExitDisposition, RunSummary, RunTrigger};

/// api and setup: the minimal One-shot pair.
fn pair_project() -> EffectiveProject {
    EffectiveProject::new(vec![
        depending_completed_on("api", &["setup"]),
        simple("setup", ProcessKind::OneShot, Enabled::Yes, Autostart::No),
    ])
    .expect("unique names")
}

/// api and setup as in the One-shot lifecycle module, plus a never-started
/// dependent so rerun blocking is observable.
fn three_project() -> EffectiveProject {
    EffectiveProject::new(vec![
        depending_completed_on("api", &["setup"]),
        simple("setup", ProcessKind::OneShot, Enabled::Yes, Autostart::No),
        depending_completed_on("worker", &["setup"]),
    ])
    .expect("unique names")
}

fn local_spawned(process_id: u32, run: u64) -> SeamEvent {
    SeamEvent::Spawned {
        process_id: ProcessId::new(process_id),
        run_id: RunId::new(run),
        root_pid: None,
    }
}

fn local_exited(process_id: u32, run: u64, code: Option<i32>) -> SeamEvent {
    SeamEvent::Exited {
        process_id: ProcessId::new(process_id),
        run_id: RunId::new(run),
        code,
    }
}

fn local_shutdown_complete(process_id: u32, run: u64, confirmed: bool) -> SeamEvent {
    SeamEvent::ShutdownComplete {
        process_id: ProcessId::new(process_id),
        run_id: RunId::new(run),
        confirmed,
        detail: None,
        remaining_pids: Vec::new(),
    }
}

/// Bring the One-shot to a successful completion and release the dependent.
fn complete_run_1(h: &mut Harness) {
    h.event(local_spawned(1, 1));
    h.event(local_exited(1, 1, Some(0)));
    h.event(local_shutdown_complete(1, 1, true));
    assert_eq!(h.process("setup").lifecycle, Lifecycle::Done);
    assert_eq!(h.process("api").current_run, Some(1));
}

#[test]
fn rerun_is_available_only_for_enabled_one_shots() {
    let mut h = Harness::new(four_process_project());
    // A Service ignores Rerun; the command belongs to One-shots.
    h.command(Command::Rerun("api".into()));
    let api = h.process("api");
    assert_eq!(api.current_run, None);
    assert_eq!(api.lifecycle, Lifecycle::Idle);
    // An enabled One-shot reruns.
    h.command(Command::Rerun("setup".into()));
    assert_eq!(h.process("setup").current_run, Some(1));

    let disabled = EffectiveProject::new(vec![
        depending_completed_on("api", &["off"]),
        simple("off", ProcessKind::OneShot, Enabled::No, Autostart::No),
    ])
    .expect("unique names");
    let mut h = Harness::new(disabled);
    h.command(Command::Rerun("off".into()));
    assert_eq!(h.process("off").current_run, None);
    assert_eq!(h.process("off").lifecycle, Lifecycle::Idle);
}

#[test]
fn a_rerun_receives_the_next_run_id_and_blocks_new_dependents() {
    let mut h = Harness::new(three_project());
    h.command(Command::Start("api".into()));
    complete_run_1(&mut h);

    // The rerun opens the next Run and immediately invalidates the
    // previous completion: a new dependent that has not started yet must
    // wait for the new attempt.
    h.command(Command::Rerun("setup".into()));
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

    // Success satisfies the dependents again.
    h.event(local_spawned(1, 2));
    h.event(local_exited(1, 2, Some(0)));
    h.event(local_shutdown_complete(1, 2, true));
    let worker = h.process("worker");
    assert_eq!(worker.current_run, Some(1));
    assert_eq!(worker.lifecycle, Lifecycle::Starting);
    assert_eq!(h.process("setup").lifecycle, Lifecycle::Done);
}

#[test]
fn a_failed_rerun_keeps_a_structured_blocked_reason() {
    let mut h = Harness::new(three_project());
    h.command(Command::Start("api".into()));
    complete_run_1(&mut h);
    h.command(Command::Rerun("setup".into()));
    h.event(local_spawned(1, 2));

    h.command(Command::Start("worker".into()));
    h.event(local_exited(1, 2, Some(3)));
    let setup = h.process("setup");
    assert_eq!(
        setup.failure.expect("the failure is visible").detail,
        "exited with code 3"
    );

    let worker = h.process("worker");
    assert_eq!(worker.lifecycle, Lifecycle::Waiting);
    assert_eq!(
        worker.blocked_reason.as_deref(),
        Some("setup: completed_successfully (exited with code 3)")
    );
}

#[test]
fn late_events_from_the_prior_attempt_are_ignored() {
    let mut h = Harness::new(three_project());
    h.command(Command::Start("api".into()));
    complete_run_1(&mut h);
    h.command(Command::Rerun("setup".into()));

    // Every old event for the finished first attempt is dropped by the
    // Run-identity gate while the second attempt is active.
    h.event(local_spawned(1, 1));
    h.event(local_exited(1, 1, Some(0)));
    h.event(local_shutdown_complete(1, 1, true));
    let setup = h.process("setup");
    assert_eq!(setup.current_run, Some(2));
    assert_eq!(setup.lifecycle, Lifecycle::Starting);
    assert_eq!(setup.failure, None);

    // The real attempt still completes normally.
    h.event(local_spawned(1, 2));
    h.event(local_exited(1, 2, Some(0)));
    h.event(local_shutdown_complete(1, 2, true));
    assert_eq!(h.process("setup").lifecycle, Lifecycle::Done);
}

#[test]
fn a_bounded_recent_run_summary_records_attempts() {
    let mut h = Harness::new(pair_project());
    h.command(Command::Start("api".into()));
    complete_run_1(&mut h);
    h.advance_and_poll(std::time::Duration::from_secs(1));

    // An intentional stop records a stopped, intentional summary.
    h.command(Command::Rerun("setup".into()));
    h.event(local_spawned(1, 2));
    h.command(Command::Stop("setup".into()));
    h.event(local_exited(1, 2, Some(0)));
    h.event(local_shutdown_complete(1, 1, true)); // stale identity: ignored
    h.event(local_shutdown_complete(1, 2, true));
    h.advance_and_poll(std::time::Duration::from_secs(1));

    // A failed attempt records its exit code.
    h.command(Command::Rerun("setup".into()));
    h.event(local_spawned(1, 3));
    h.event(local_exited(1, 3, Some(5)));
    h.event(local_shutdown_complete(1, 3, true));

    let setup = h.process("setup");
    let recent = &setup.recent_runs;
    assert_eq!(recent.len(), 3);
    assert_eq!(
        recent[0],
        RunSummary {
            run_id: 3,
            started_at_ms: recent[0].started_at_ms,
            ended_at_ms: recent[0].ended_at_ms,
            exit: RunExitDisposition::Failed { code: Some(5) },
            intentional_stop: false,
            failure: Some("exited with code 5".to_string()),
            trigger: RunTrigger::Rerun,
        }
    );
    assert_eq!(recent[1].run_id, 2);
    assert_eq!(recent[1].exit, RunExitDisposition::Stopped);
    assert!(recent[1].intentional_stop);
    assert_eq!(
        recent[1].failure, None,
        "an intentional stop is not a failure"
    );
    assert_eq!(recent[2].run_id, 1);
    assert_eq!(recent[2].exit, RunExitDisposition::Success);
    assert!(!recent[2].intentional_stop);
    assert_eq!(recent[2].failure, None);
    assert_eq!(
        recent[2].trigger,
        RunTrigger::Dependency,
        "the first attempt started because the user's dependent needed it"
    );
    // Every summary's end is not before its start.
    for summary in recent {
        assert!(summary.ended_at_ms >= summary.started_at_ms);
    }

    // The window stays bounded at RECENT_RUNS entries, newest first.
    for attempt in 4..=(RECENT_RUNS + 3) as u64 {
        h.command(Command::Rerun("setup".into()));
        h.event(local_spawned(1, attempt));
        h.event(local_exited(1, attempt, Some(0)));
        h.event(local_shutdown_complete(1, attempt, true));
    }
    let recent = &h.process("setup").recent_runs;
    assert_eq!(recent.len(), RECENT_RUNS);
    assert_eq!(recent[0].run_id, (RECENT_RUNS + 3) as u64);
    assert_eq!(
        recent[RECENT_RUNS - 1].run_id,
        4,
        "the oldest attempts drop out of the bounded window"
    );
}
