//! Automatic restart policy behavior through the Supervisor's public test
//! seam. Fake time controls every backoff and no test sleeps on the wall clock.

use std::time::Duration;

use super::*;
use crate::model::RestartPolicy;
use crate::supervisor::RunTrigger;

fn restart_project(kind: ProcessKind, policy: RestartPolicy) -> EffectiveProject {
    let mut process = simple("api", kind, Enabled::Yes, Autostart::No);
    process.restart = RestartConfig {
        policy,
        backoff: Duration::from_secs(2),
    };
    EffectiveProject::new(vec![process]).expect("unique names and valid restart policy")
}

fn startup_timeout_restart_project(policy: RestartPolicy) -> EffectiveProject {
    let mut process = probed_service("api");
    process.restart = RestartConfig {
        policy,
        backoff: Duration::from_secs(2),
    };
    process
        .readiness
        .as_mut()
        .expect("the probe exists")
        .startup_timeout = Some(Duration::from_secs(1));
    EffectiveProject::new(vec![process]).expect("unique names")
}

fn start_service(h: &mut Harness) {
    h.command(Command::Start("api".into()));
    h.event(spawned("api", 1));
}

fn start_count(h: &Harness) -> usize {
    h.runtime
        .intents()
        .iter()
        .filter(|intent| matches!(intent, Intent::Start { .. }))
        .count()
}

#[test]
fn never_keeps_a_failed_service_stopped_without_a_replacement() {
    let mut h = Harness::new(restart_project(ProcessKind::Service, RestartPolicy::Never));
    start_service(&mut h);
    h.event(finished("api", 1, Some(7)));

    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Stopped);
    assert_eq!(api.desired, DesiredState::Running);
    assert_eq!(api.current_run, None);
    assert_eq!(api.restart_backoff, None);
    assert_eq!(start_count(&h), 1);
}

#[test]
fn on_failure_waits_for_cleanup_then_starts_a_service_again() {
    let mut h = Harness::new(restart_project(
        ProcessKind::Service,
        RestartPolicy::OnFailure,
    ));
    start_service(&mut h);
    h.event(finished("api", 1, Some(7)));

    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::RestartBackoff);
    assert_eq!(api.desired, DesiredState::Running);
    assert_eq!(api.current_run, None);
    assert_eq!(
        api.restart_backoff
            .as_ref()
            .map(|backoff| (&backoff.reason, backoff.next_attempt_at_ms)),
        Some((&String::from("failed Run"), 2000))
    );

    h.advance_and_poll(Duration::from_secs(1));
    assert_eq!(start_count(&h), 1);
    assert_eq!(h.process("api").lifecycle, Lifecycle::RestartBackoff);

    h.advance_and_poll(Duration::from_secs(1));
    assert_eq!(start_count(&h), 2);
    assert_eq!(h.process("api").current_run, Some(2));
    assert_eq!(h.process("api").lifecycle, Lifecycle::Starting);

    h.event(spawned("api", 2));
    h.event(finished("api", 2, Some(0)));
    let api = h.process("api");
    assert_eq!(api.recent_runs[0].trigger, RunTrigger::AutomaticRestart);
}

#[test]
fn on_failure_does_not_restart_a_successful_service() {
    let mut h = Harness::new(restart_project(
        ProcessKind::Service,
        RestartPolicy::OnFailure,
    ));
    start_service(&mut h);
    h.event(finished("api", 1, Some(0)));

    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Stopped);
    assert_eq!(api.desired, DesiredState::Running);
    assert_eq!(api.restart_backoff, None);
    assert_eq!(start_count(&h), 1);
}

#[test]
fn always_restarts_a_successful_service_after_the_fixed_backoff() {
    let mut h = Harness::new(restart_project(ProcessKind::Service, RestartPolicy::Always));
    start_service(&mut h);
    h.event(finished("api", 1, Some(0)));

    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::RestartBackoff);
    assert_eq!(
        api.restart_backoff
            .as_ref()
            .map(|backoff| backoff.reason.as_str()),
        Some("unexpected successful exit")
    );
    h.advance_and_poll(Duration::from_secs(2));
    assert_eq!(h.process("api").current_run, Some(2));
}

#[test]
fn on_failure_restarts_a_failed_one_shot_and_success_completes_it() {
    let mut h = Harness::new(restart_project(
        ProcessKind::OneShot,
        RestartPolicy::OnFailure,
    ));
    h.command(Command::Start("api".into()));
    h.event(spawned("api", 1));
    h.event(finished("api", 1, Some(7)));
    assert_eq!(h.process("api").lifecycle, Lifecycle::RestartBackoff);

    h.advance_and_poll(Duration::from_secs(2));
    assert_eq!(h.process("api").current_run, Some(2));
    h.event(spawned("api", 2));
    h.event(finished("api", 2, Some(0)));

    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Done);
    assert_eq!(api.desired, DesiredState::Stopped);
    assert_eq!(api.recent_runs[0].trigger, RunTrigger::AutomaticRestart);
}

#[test]
fn a_spawn_failure_enters_the_same_automatic_restart_path() {
    let runtime = FakeRuntime::shared();
    runtime.set_fail_spawn(true);
    let mut h = Harness::with(
        restart_project(ProcessKind::Service, RestartPolicy::OnFailure),
        Arc::clone(&runtime),
    );
    h.command(Command::Start("api".into()));

    assert_eq!(h.process("api").lifecycle, Lifecycle::RestartBackoff);
    assert_eq!(
        h.process("api")
            .restart_backoff
            .as_ref()
            .map(|backoff| backoff.reason.as_str()),
        Some("spawn failure")
    );
    runtime.set_fail_spawn(false);
    h.advance_and_poll(Duration::from_secs(2));
    assert_eq!(h.process("api").current_run, Some(2));
}

#[test]
fn a_startup_timeout_uses_the_automatic_restart_policy() {
    let runtime = FakeRuntime::shared();
    runtime.set_hold_stops(true);
    let mut h = Harness::with(
        startup_timeout_restart_project(RestartPolicy::OnFailure),
        Arc::clone(&runtime),
    );
    start_service(&mut h);
    h.advance_and_poll(Duration::from_secs(1));
    assert_eq!(h.process("api").lifecycle, Lifecycle::Stopping);

    // The timeout's stop request is an internal cleanup action. Its
    // intentional flag must not hide the startup-timeout failure.
    h.event(SeamEvent::Finished(FinishedRun {
        process_id: ProcessId::new(0),
        run_id: RunId::new(1),
        exit_code: None,
        intentional_stop: true,
        cleanup_confirmed: true,
        detail: None,
        remaining_pids: Vec::new(),
    }));
    assert_eq!(
        h.process("api")
            .restart_backoff
            .as_ref()
            .map(|backoff| backoff.reason.as_str()),
        Some("startup timeout")
    );

    h.advance_and_poll(Duration::from_secs(2));
    assert_eq!(h.process("api").current_run, Some(2));
}

#[test]
fn manual_stop_cancels_a_pending_automatic_restart() {
    let mut h = Harness::new(restart_project(
        ProcessKind::Service,
        RestartPolicy::OnFailure,
    ));
    start_service(&mut h);
    h.event(finished("api", 1, Some(7)));
    h.command(Command::Stop("api".into()));

    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Stopped);
    assert_eq!(api.desired, DesiredState::Stopped);
    assert_eq!(api.restart_backoff, None);
    h.advance_and_poll(Duration::from_secs(3));
    assert_eq!(start_count(&h), 1);
}

#[test]
fn manual_restart_cancels_backoff_and_starts_one_run_now() {
    let mut h = Harness::new(restart_project(
        ProcessKind::Service,
        RestartPolicy::OnFailure,
    ));
    start_service(&mut h);
    h.event(finished("api", 1, Some(7)));
    h.command(Command::Restart("api".into()));

    assert_eq!(h.process("api").current_run, Some(2));
    assert_eq!(h.process("api").lifecycle, Lifecycle::Starting);
    assert_eq!(h.process("api").restart_backoff, None);
    h.advance_and_poll(Duration::from_secs(3));
    assert_eq!(start_count(&h), 2);
}

#[test]
fn manual_restart_waits_for_unconfirmed_cleanup_before_one_replacement() {
    let runtime = FakeRuntime::shared();
    runtime
        .fail_cleanup
        .store(true, std::sync::atomic::Ordering::Release);
    let mut h = Harness::with(
        restart_project(ProcessKind::Service, RestartPolicy::OnFailure),
        Arc::clone(&runtime),
    );
    start_service(&mut h);
    h.command(Command::Stop("api".into()));
    assert_eq!(h.process("api").current_run, Some(1));

    h.command(Command::Restart("api".into()));
    assert_eq!(h.process("api").current_run, Some(1));
    assert_eq!(h.process("api").desired, DesiredState::Running);
    assert_eq!(start_count(&h), 1);

    runtime
        .fail_cleanup
        .store(false, std::sync::atomic::Ordering::Release);
    h.command(Command::Stop("api".into()));

    let api = h.process("api");
    assert_eq!(api.current_run, Some(2));
    assert_eq!(api.lifecycle, Lifecycle::Starting);
    assert_eq!(api.restart_backoff, None);
    assert_eq!(start_count(&h), 2);
}

#[test]
fn project_shutdown_cancels_backoff_and_suppresses_restart() {
    let mut h = Harness::new(restart_project(
        ProcessKind::Service,
        RestartPolicy::OnFailure,
    ));
    start_service(&mut h);
    h.event(finished("api", 1, Some(7)));
    h.command(Command::Shutdown {
        deadline: h.clock.now() + Duration::from_secs(20),
    });

    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Stopped);
    assert_eq!(api.desired, DesiredState::Stopped);
    assert_eq!(api.restart_backoff, None);
    assert!(h.snapshot().shutdown.expect("shutdown exists").complete);
    h.advance_and_poll(Duration::from_secs(3));
    assert_eq!(start_count(&h), 1);
}

#[test]
fn clearing_backoff_before_a_new_run_invalidates_the_old_timer() {
    let mut h = Harness::new(restart_project(
        ProcessKind::Service,
        RestartPolicy::OnFailure,
    ));
    start_service(&mut h);
    h.event(finished("api", 1, Some(7)));
    h.command(Command::Stop("api".into()));
    h.command(Command::Start("api".into()));

    assert_eq!(h.process("api").current_run, Some(2));
    h.advance_and_poll(Duration::from_secs(3));
    assert_eq!(start_count(&h), 2);
}

#[test]
fn automatic_restart_waits_for_confirmed_cleanup() {
    let mut h = Harness::new(restart_project(
        ProcessKind::Service,
        RestartPolicy::OnFailure,
    ));
    start_service(&mut h);
    h.event(SeamEvent::Finished(FinishedRun {
        process_id: ProcessId::new(0),
        run_id: RunId::new(1),
        exit_code: Some(7),
        intentional_stop: false,
        cleanup_confirmed: false,
        detail: Some("cleanup pending".to_string()),
        remaining_pids: vec![crate::runtime::OsPid::new(99)],
    }));
    assert_eq!(h.process("api").current_run, Some(1));
    assert_eq!(h.process("api").restart_backoff, None);

    h.event(finished("api", 1, Some(7)));
    assert_eq!(h.process("api").lifecycle, Lifecycle::RestartBackoff);
    assert_eq!(h.process("api").current_run, None);
}
