//! Service stop and restart behavior tests: manual stop finishes as
//! Stopped, restart starts the next Run only after the bounded cleanup
//! confirms, stale Run events stay stale, a stopping Service never
//! satisfies `started`, and unconfirmed cleanup blocks replacement Runs.
//! They share the harness and helpers of the parent tests module.

use super::*;

/// Standard positions (api=0, db=1, worker=2, setup=3) so the shared
/// `process_index` helper stays correct. `worker` holds no autostart and
/// starts only while `db` is started.
fn lifecycle_project() -> EffectiveProject {
    let mut worker = service("worker");
    worker.autostart = Autostart::No;
    worker.dependencies = depending_on("worker", &["db"]).dependencies;
    EffectiveProject::new(vec![service("api"), service("db"), worker]).expect("unique names")
}

/// The same layout plus a never-started `setup` gated on the same
/// dependency: the gate test needs a dependent without its own Run.
fn dependency_project() -> EffectiveProject {
    let mut setup = simple("setup", ProcessKind::Service, Enabled::Yes, Autostart::No);
    setup.dependencies = depending_on("setup", &["db"]).dependencies;
    let mut worker = service("worker");
    worker.autostart = Autostart::No;
    worker.dependencies = depending_on("worker", &["db"]).dependencies;
    EffectiveProject::new(vec![service("api"), service("db"), worker, setup]).expect("unique names")
}

#[test]
fn manual_stop_finishes_as_stopped_not_failed() {
    let mut h = Harness::new(lifecycle_project());
    h.command(Command::Start("worker".into()));
    h.event(spawned("db", 1));
    h.event(spawned("worker", 1));
    assert_eq!(h.process("worker").lifecycle, Lifecycle::Running);

    h.command(Command::Stop("worker".into()));

    let worker = h.process("worker");
    assert_eq!(worker.lifecycle, Lifecycle::Stopped);
    assert_eq!(worker.failure, None);
    assert_eq!(worker.current_run, None);
    assert_eq!(worker.desired, DesiredState::Stopped);
}

#[test]
fn restart_starts_the_next_run_only_after_cleanup_completes() {
    let mut h = Harness::new(lifecycle_project());
    h.command(Command::Start("worker".into()));
    h.event(spawned("db", 1));
    h.event(spawned("worker", 1));
    assert_eq!(h.process("worker").current_run, Some(1));

    // Before the bounded cleanup reports, the restart stops the active
    // Run but holds the identity; no replacement starts yet.
    h.core.command(Command::Restart("worker".into()));
    assert_eq!(h.process("worker").lifecycle, Lifecycle::Stopping);
    assert_eq!(h.process("worker").current_run, Some(1));

    // The confirming completion releases the identity and the scheduler
    // starts the replacement with the next Run ID.
    h.drain();
    let worker = h.process("worker");
    assert_eq!(worker.current_run, Some(2));
    assert_eq!(worker.lifecycle, Lifecycle::Starting);
}

#[test]
fn a_stopping_dependency_does_not_satisfy_started() {
    let mut h = Harness::new(dependency_project());
    h.command(Command::Start("worker".into()));
    h.event(spawned("db", 1));
    h.event(spawned("worker", 1));

    // While db's Run is Stopping, a fresh start request for a dependent
    // without a Run must not see the `started` condition satisfied.
    h.core.command(Command::Stop("db".into()));
    h.core.command(Command::Start("setup".into()));
    let setup = h.process("setup");
    assert_eq!(setup.lifecycle, Lifecycle::Waiting);
    assert_eq!(setup.blocked_reason.as_deref(), Some("db: started"));

    // Once db's cleanup confirms and db starts again, setup follows.
    h.drain();
    assert_eq!(h.process("db").current_run, Some(2));
    assert_eq!(h.process("setup").current_run, Some(1));
    assert_eq!(h.process("setup").lifecycle, Lifecycle::Starting);
}

#[test]
fn stale_events_for_finished_runs_cannot_change_state() {
    let mut h = Harness::new(lifecycle_project());
    h.command(Command::Start("worker".into()));
    h.event(spawned("db", 1));
    h.event(spawned("worker", 1));

    h.command(Command::Restart("worker".into()));
    assert_eq!(h.process("worker").current_run, Some(2));

    // Every Run-1 report is stale now: it cannot spawn, exit, report
    // metrics, or probe the new Run.
    h.event(spawned("worker", 1));
    h.event(exited("worker", 1, Some(0)));
    h.event(readiness("worker", 1, true, None));
    let worker = h.process("worker");
    assert_eq!(worker.current_run, Some(2));
    assert_eq!(worker.lifecycle, Lifecycle::Starting);
    assert_eq!(worker.failure, None);
}

#[test]
fn unconfirmed_cleanup_ignores_stale_events_for_the_held_run() {
    let runtime = FakeRuntime::shared();
    runtime
        .fail_cleanup
        .store(true, std::sync::atomic::Ordering::Release);
    let mut h = Harness::with(lifecycle_project(), Arc::clone(&runtime));
    h.command(Command::Start("worker".into()));
    h.event(spawned("db", 1));
    h.event(spawned("worker", 1));

    h.core.command(Command::Stop("worker".into()));
    h.drain();
    let held = h.process("worker");
    assert_eq!(held.lifecycle, Lifecycle::Stopped);
    assert_eq!(held.current_run, Some(1));
    assert!(held.failure.is_some());

    // Stale reports for the held Run cannot change state; only the
    // confirming completion may.
    h.event(spawned("worker", 1));
    h.event(exited("worker", 1, Some(0)));
    h.event(readiness("worker", 1, true, None));
    let worker = h.process("worker");
    assert_eq!(worker.lifecycle, Lifecycle::Stopped);
    assert_eq!(worker.current_run, Some(1));
    assert!(worker.failure.is_some());
}

#[test]
fn restart_while_cleanup_unconfirmed_stays_pending() {
    let runtime = FakeRuntime::shared();
    runtime
        .fail_cleanup
        .store(true, std::sync::atomic::Ordering::Release);
    let mut h = Harness::with(lifecycle_project(), Arc::clone(&runtime));
    h.command(Command::Start("worker".into()));
    h.event(spawned("db", 1));
    h.event(spawned("worker", 1));
    h.command(Command::Stop("worker".into()));
    assert!(h.process("worker").failure.is_some());

    // Restart keeps the desire but cannot replace an unconfirmed Run.
    h.command(Command::Restart("worker".into()));
    let worker = h.process("worker");
    assert_eq!(worker.current_run, Some(1));
    assert_eq!(worker.lifecycle, Lifecycle::Stopped);
    assert_eq!(worker.desired, DesiredState::Running);
    assert!(worker.failure.is_some());

    // The retry path confirms, and only then does the replacement start.
    runtime
        .fail_cleanup
        .store(false, std::sync::atomic::Ordering::Release);
    h.command(Command::Stop("worker".into()));
    assert_eq!(h.process("worker").current_run, Some(2));
}
