//! Liveness behavior through the Supervisor seam. Fake time and scripted
//! probes keep scheduling deterministic without wall-clock sleeps.

use std::time::Duration;

use super::*;
use crate::supervisor::LivenessState;
use crate::supervisor::seam::ProbeScope;

fn liveness_check(
    probe: ReadinessProbe,
    initial_delay: Duration,
    success_threshold: u32,
    failure_threshold: u32,
) -> ReadinessCheck {
    ReadinessCheck {
        probe,
        initial_delay,
        interval: Duration::from_secs(1),
        timeout: Duration::from_millis(500),
        success_threshold,
        failure_threshold,
    }
}

fn liveness_project(
    check: ReadinessCheck,
    on_unhealthy: bool,
    max_restarts: u32,
) -> EffectiveProject {
    let mut process = service("api");
    process.autostart = Autostart::No;
    process.liveness = Some(crate::model::LivenessConfig {
        checks: vec![check],
    });
    process.restart = RestartConfig {
        on_unhealthy,
        max_restarts,
        ..RestartConfig::default()
    };
    EffectiveProject::new(vec![process]).expect("valid liveness project")
}

fn liveness_attempt(
    process: &str,
    run: u64,
    work: u64,
    attempt: u64,
    passing: bool,
    diagnostic: Option<&str>,
) -> SeamEvent {
    SeamEvent::Liveness {
        process_id: ProcessId::new(process_index(process)),
        run_id: RunId::new(run),
        work_id: WorkId::new(work),
        attempt_id: AttemptId::new(attempt),
        passing,
        diagnostic: diagnostic.map(str::to_string),
    }
}

fn liveness_log_match(process: &str, run: u64, work: u64, attempt: u64) -> SeamEvent {
    SeamEvent::LogMatched {
        process_id: ProcessId::new(process_index(process)),
        run_id: RunId::new(run),
        work_id: WorkId::new(work),
        attempt_id: Some(AttemptId::new(attempt)),
    }
}

fn tcp_liveness(initial_delay: Duration) -> ReadinessCheck {
    liveness_check(
        ReadinessProbe::Tcp {
            host: "127.0.0.1".into(),
            port: 1,
        },
        initial_delay,
        1,
        1,
    )
}

fn start_and_spawn(h: &mut Harness) {
    h.command(Command::Start("api".into()));
    h.event(spawned("api", 1));
}

#[test]
fn liveness_stays_inactive_until_spawn_then_checks_a_service_without_readiness() {
    let mut h = Harness::new(liveness_project(
        tcp_liveness(Duration::from_secs(1)),
        false,
        0,
    ));
    h.command(Command::Start("api".into()));

    assert_eq!(
        h.process("api").liveness.unwrap().state,
        LivenessState::Inactive
    );
    h.event(spawned("api", 1));
    assert_eq!(
        h.process("api").liveness.unwrap().state,
        LivenessState::Pending
    );
    assert_eq!(h.process("api").lifecycle, Lifecycle::Running);

    h.advance_and_poll(Duration::from_millis(999));
    assert!(h.probes.requests().is_empty());
    h.advance_and_poll(Duration::from_millis(1));
    let request = h.probes.requests().pop().expect("liveness attempt is due");
    assert_eq!(request.scope, ProbeScope::Liveness);
    assert_eq!(request.work_id, WorkId::new(1));

    h.event(liveness_attempt(
        "api",
        1,
        1,
        1,
        false,
        Some("connection refused"),
    ));
    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Running);
    assert_eq!(api.current_run, Some(1));
    assert_eq!(api.liveness.unwrap().state, LivenessState::Failing);
    assert_eq!(
        api.failure.as_ref().map(|failure| failure.kind),
        Some(FailureKind::Liveness)
    );
    assert!(
        !h.runtime
            .intents()
            .iter()
            .any(|intent| matches!(intent, Intent::Stop { .. }))
    );
}

#[test]
fn liveness_thresholds_require_consecutive_results_and_recover_in_place() {
    let mut h = Harness::new(liveness_project(
        liveness_check(
            ReadinessProbe::Tcp {
                host: "127.0.0.1".into(),
                port: 1,
            },
            Duration::ZERO,
            2,
            2,
        ),
        false,
        0,
    ));
    start_and_spawn(&mut h);

    for attempt in 1..=2 {
        h.advance_and_poll(if attempt == 1 {
            Duration::ZERO
        } else {
            Duration::from_secs(1)
        });
        h.event(liveness_attempt("api", 1, 1, attempt, true, None));
    }
    let api = h.process("api");
    assert_eq!(api.liveness.as_ref().unwrap().state, LivenessState::Passing);
    assert_eq!(api.liveness.as_ref().unwrap().consecutive_successes, 2);

    for attempt in 3..=4 {
        h.advance_and_poll(Duration::from_secs(1));
        h.event(liveness_attempt(
            "api",
            1,
            1,
            attempt,
            false,
            Some("status 503"),
        ));
    }
    let failing = h.process("api");
    assert_eq!(failing.lifecycle, Lifecycle::Running);
    assert_eq!(
        failing.liveness.as_ref().unwrap().state,
        LivenessState::Failing
    );
    assert_eq!(failing.liveness.as_ref().unwrap().consecutive_failures, 2);
    assert_eq!(
        failing.failure.as_ref().map(|failure| failure.kind),
        Some(FailureKind::Liveness)
    );

    h.advance_and_poll(Duration::from_secs(1));
    h.event(liveness_attempt("api", 1, 1, 5, true, None));
    assert_eq!(
        h.process("api").liveness.unwrap().state,
        LivenessState::Failing
    );
    h.advance_and_poll(Duration::from_secs(1));
    h.event(liveness_attempt("api", 1, 1, 6, true, None));

    let recovered = h.process("api");
    assert_eq!(
        recovered.liveness.as_ref().unwrap().state,
        LivenessState::Passing
    );
    assert_eq!(
        recovered.liveness.as_ref().unwrap().consecutive_successes,
        2
    );
    assert_eq!(recovered.failure, None);
}

#[test]
fn readiness_gates_liveness_but_liveness_loss_does_not_stop_a_ready_dependent() {
    let mut api = service("api");
    api.autostart = Autostart::No;
    api.readiness = Some(ReadinessConfig {
        checks: vec![ReadinessCheck {
            probe: ReadinessProbe::Tcp {
                host: "127.0.0.1".into(),
                port: 1,
            },
            initial_delay: Duration::ZERO,
            interval: Duration::from_secs(1),
            timeout: Duration::from_millis(500),
            success_threshold: 1,
            failure_threshold: 1,
        }],
        startup_timeout: None,
    });
    api.liveness = Some(crate::model::LivenessConfig {
        checks: vec![tcp_liveness(Duration::ZERO)],
    });
    let worker = depending_ready_on("worker", &["api"]);
    let project = EffectiveProject::new(vec![api, worker]).expect("valid dependency project");
    let mut h = Harness::new(project);

    h.command(Command::Start("worker".into()));
    h.event(spawned("api", 1));
    assert_eq!(
        h.process("api").liveness.unwrap().state,
        LivenessState::Inactive
    );
    assert_eq!(h.process("worker").current_run, None);

    h.advance_and_poll(Duration::ZERO);
    let readiness_request = h
        .probes
        .requests()
        .into_iter()
        .next()
        .expect("readiness attempt is due");
    assert_eq!(readiness_request.scope, ProbeScope::Readiness);
    h.event(SeamEvent::Readiness {
        process_id: ProcessId::new(0),
        run_id: RunId::new(1),
        work_id: readiness_request.work_id,
        attempt_id: readiness_request.attempt_id,
        passing: true,
        diagnostic: None,
    });

    let worker_run = h.process("worker").current_run;
    assert!(
        worker_run.is_some(),
        "ready dependency starts its dependent"
    );
    assert_eq!(
        h.process("api").liveness.unwrap().state,
        LivenessState::Pending
    );

    h.advance_and_poll(Duration::ZERO);
    let liveness_request = h
        .probes
        .requests()
        .into_iter()
        .find(|request| request.scope == ProbeScope::Liveness)
        .expect("liveness starts after readiness");
    h.event(liveness_attempt(
        "api",
        1,
        liveness_request.work_id.get(),
        liveness_request.attempt_id.get(),
        false,
        Some("health endpoint failed"),
    ));

    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Running);
    assert_eq!(api.liveness.unwrap().state, LivenessState::Failing);
    assert_eq!(h.process("worker").current_run, worker_run);
}

#[test]
fn liveness_log_checks_use_fresh_attempt_windows_and_ignore_stale_matches() {
    let mut h = Harness::new(liveness_project(
        liveness_check(
            ReadinessProbe::Log {
                contains: "heartbeat".into(),
            },
            Duration::ZERO,
            1,
            1,
        ),
        false,
        0,
    ));
    start_and_spawn(&mut h);

    // A placeholder match before the first armed window is not a liveness
    // result and cannot make the policy pass.
    h.event(SeamEvent::LogMatched {
        process_id: ProcessId::new(0),
        run_id: RunId::new(1),
        work_id: WorkId::new(1),
        attempt_id: None,
    });
    assert_eq!(
        h.process("api").liveness.unwrap().state,
        LivenessState::Pending
    );

    h.advance_and_poll(Duration::ZERO);
    h.event(liveness_log_match("api", 1, 1, 1));
    assert_eq!(h.process("api").liveness.unwrap().attempts, 1);

    h.advance_and_poll(Duration::from_secs(1));
    h.event(liveness_log_match("api", 1, 1, 1));
    assert_eq!(h.process("api").liveness.unwrap().attempts, 2);
    h.event(liveness_log_match("api", 1, 1, 2));
    assert_eq!(h.process("api").liveness.unwrap().attempts, 2);

    // The next window times out. Its old match cannot rescue it.
    h.advance_and_poll(Duration::from_secs(1));
    h.event(liveness_log_match("api", 1, 1, 2));
    h.advance_and_poll(Duration::from_millis(500));
    let failed = h.process("api");
    assert_eq!(
        failed.liveness.as_ref().unwrap().state,
        LivenessState::Failing
    );
    assert!(
        failed
            .liveness
            .as_ref()
            .unwrap()
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("timed out"))
    );

    h.advance_and_poll(Duration::from_secs(1));
    h.event(liveness_log_match("api", 1, 1, 4));
    assert_eq!(
        h.process("api").liveness.unwrap().state,
        LivenessState::Passing
    );
}

#[test]
fn a_late_liveness_log_match_after_timeout_cannot_pass_the_attempt() {
    let mut h = Harness::new(liveness_project(
        liveness_check(
            ReadinessProbe::Log {
                contains: "heartbeat".into(),
            },
            Duration::ZERO,
            1,
            1,
        ),
        false,
        0,
    ));
    start_and_spawn(&mut h);
    h.advance_and_poll(Duration::ZERO);

    h.clock.advance(Duration::from_millis(501));
    h.event(liveness_log_match("api", 1, 1, 1));
    assert_eq!(
        h.process("api").liveness.unwrap().state,
        LivenessState::Pending
    );

    h.advance_and_poll(Duration::ZERO);
    let liveness = h.process("api").liveness.unwrap();
    assert_eq!(liveness.state, LivenessState::Failing);
    assert_eq!(liveness.attempts, 1);
}

#[test]
fn on_unhealthy_restarts_the_run_and_consumes_the_shared_budget() {
    let mut h = Harness::new(liveness_project(tcp_liveness(Duration::ZERO), true, 1));
    start_and_spawn(&mut h);
    h.advance_and_poll(Duration::ZERO);
    let first = h
        .probes
        .requests()
        .into_iter()
        .next()
        .expect("first liveness attempt exists");
    h.event(liveness_attempt(
        "api",
        1,
        first.work_id.get(),
        first.attempt_id.get(),
        false,
        Some("unhealthy"),
    ));

    let stopping = h.process("api");
    assert_eq!(stopping.lifecycle, Lifecycle::Stopping);
    assert_eq!(stopping.desired, DesiredState::Stopped);
    assert_eq!(stopping.current_run, Some(1));
    h.drain();

    let backoff = h.process("api");
    assert_eq!(backoff.lifecycle, Lifecycle::RestartBackoff);
    assert_eq!(
        backoff.restart_backoff.as_ref().unwrap().reason,
        "unhealthy"
    );
    assert_eq!(backoff.automatic_restart_budget.automatic_retries_used, 0);

    h.advance_and_poll(Duration::from_secs(2));
    assert_eq!(h.process("api").current_run, Some(2));
    assert_eq!(
        h.process("api")
            .automatic_restart_budget
            .automatic_retries_used,
        1
    );
    h.event(spawned("api", 2));
    h.advance_and_poll(Duration::ZERO);
    let second = h
        .probes
        .requests()
        .into_iter()
        .find(|request| request.run_id == RunId::new(2))
        .expect("replacement liveness attempt exists");
    h.event(liveness_attempt(
        "api",
        2,
        second.work_id.get(),
        second.attempt_id.get(),
        false,
        Some("still unhealthy"),
    ));
    h.drain();

    let exhausted = h.process("api");
    assert_eq!(exhausted.lifecycle, Lifecycle::Stopped);
    assert_eq!(exhausted.current_run, None);
    assert_eq!(exhausted.desired, DesiredState::Running);
    assert_eq!(
        exhausted.failure.as_ref().map(|failure| failure.kind),
        Some(FailureKind::RestartLimit)
    );
    assert!(
        exhausted
            .failure
            .as_ref()
            .is_some_and(|failure| failure.detail.contains("Restart limit"))
    );
    assert!(exhausted.automatic_restart_budget.exhausted);
}

#[test]
fn unhealthy_restart_retries_unconfirmed_cleanup_before_backoff() {
    let runtime = FakeRuntime::shared();
    runtime
        .fail_cleanup
        .store(true, std::sync::atomic::Ordering::Release);
    let mut h = Harness::with(
        liveness_project(tcp_liveness(Duration::ZERO), true, 1),
        Arc::clone(&runtime),
    );
    start_and_spawn(&mut h);
    h.advance_and_poll(Duration::ZERO);
    let request = h
        .probes
        .requests()
        .into_iter()
        .next()
        .expect("liveness attempt exists");
    h.event(liveness_attempt(
        "api",
        1,
        request.work_id.get(),
        request.attempt_id.get(),
        false,
        Some("unhealthy"),
    ));
    h.drain();

    let stopping = h.process("api");
    assert_eq!(stopping.lifecycle, Lifecycle::Stopping);
    assert_eq!(stopping.current_run, Some(1));

    runtime
        .fail_cleanup
        .store(false, std::sync::atomic::Ordering::Release);
    h.command(Command::Stop("api".into()));

    let backoff = h.process("api");
    assert_eq!(backoff.current_run, None);
    assert_eq!(backoff.lifecycle, Lifecycle::RestartBackoff);
    assert_eq!(
        backoff.restart_backoff.as_ref().unwrap().reason,
        "unhealthy"
    );
}

#[test]
fn stop_cancels_liveness_and_late_results_cannot_change_a_new_run() {
    let mut h = Harness::new(liveness_project(tcp_liveness(Duration::ZERO), false, 0));
    start_and_spawn(&mut h);
    h.advance_and_poll(Duration::ZERO);
    let request = h
        .probes
        .requests()
        .into_iter()
        .next()
        .expect("liveness attempt exists");

    h.command(Command::Stop("api".into()));
    assert!(h.probes.cancellations().contains(&(
        ProcessId::new(0),
        RunId::new(1),
        request.work_id,
    )));
    h.event(liveness_attempt(
        "api",
        1,
        request.work_id.get(),
        request.attempt_id.get(),
        true,
        None,
    ));
    assert_eq!(h.process("api").current_run, None);

    h.command(Command::Start("api".into()));
    h.event(spawned("api", 2));
    h.event(liveness_attempt(
        "api",
        1,
        request.work_id.get(),
        request.attempt_id.get(),
        false,
        Some("stale"),
    ));
    let api = h.process("api");
    assert_eq!(api.current_run, Some(2));
    assert_eq!(api.liveness.unwrap().state, LivenessState::Pending);
}
