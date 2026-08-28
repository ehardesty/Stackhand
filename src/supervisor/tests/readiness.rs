//! Readiness behavior tests. They drive the same highest seam as the
//! parent module: semantic commands, typed seam events, dispatched probe
//! intents, and immutable snapshots — with a fake clock so interval
//! scheduling stays deterministic.

use std::time::Duration;

use super::*;

fn probed_project() -> EffectiveProject {
    EffectiveProject::new(vec![probed_service("api")]).expect("unique names")
}

fn configured_readiness_project(
    initial_delay: Duration,
    success_threshold: u32,
    failure_threshold: u32,
) -> EffectiveProject {
    let mut process = probed_service("api");
    let readiness = process.readiness.as_mut().expect("the probe exists");
    readiness.initial_delay = initial_delay;
    readiness.success_threshold = success_threshold;
    readiness.failure_threshold = failure_threshold;
    EffectiveProject::new(vec![process]).expect("unique names")
}

fn start_probed(h: &mut Harness) {
    h.command(Command::Start("api".into()));
    h.event(spawned("api", 1));
}

#[test]
fn a_probed_service_stays_starting_until_its_probe_passes() {
    let mut h = Harness::new(probed_project());
    start_probed(&mut h);
    h.advance_and_poll(Duration::from_millis(0));

    assert_eq!(h.process("api").lifecycle, Lifecycle::Starting);
    assert!(h.process("api").readiness.is_some());

    h.event(readiness("api", 1, true, None));
    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Running);
    let readiness = api
        .readiness
        .expect("readiness stays visible after passing");
    assert_eq!(readiness.kind, ReadinessCheckKind::Tcp);
    assert_eq!(readiness.state, ReadinessState::Passing);
    assert_eq!(readiness.attempts, 1);
    assert_eq!(readiness.consecutive_successes, 1);
    assert_eq!(readiness.consecutive_failures, 0);
    assert_eq!(readiness.startup_elapsed_ms, 0);
    assert_eq!(api.failure, None);
}

#[test]
fn initial_delay_defers_the_first_readiness_attempt() {
    let mut h = Harness::new(configured_readiness_project(Duration::from_secs(1), 1, 1));
    start_probed(&mut h);

    h.advance_and_poll(Duration::from_millis(999));
    assert!(h.probes.attempts().is_empty());
    assert_eq!(h.process("api").readiness.unwrap().startup_elapsed_ms, 999);

    h.advance_and_poll(Duration::from_millis(1));
    assert_eq!(h.probes.attempts().len(), 1);
}

#[test]
fn readiness_thresholds_support_pending_failing_and_recovery_states() {
    let mut h = Harness::new(configured_readiness_project(Duration::ZERO, 2, 2));
    start_probed(&mut h);
    h.advance_and_poll(Duration::ZERO);

    h.event(readiness("api", 1, true, None));
    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Starting);
    let status = api.readiness.expect("pending readiness remains visible");
    assert_eq!(status.state, ReadinessState::Pending);
    assert_eq!(status.consecutive_successes, 1);
    assert_eq!(status.consecutive_failures, 0);

    h.advance_and_poll(Duration::from_secs(1));
    h.event(readiness_attempt("api", 1, 2, true, None));
    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Running);
    assert_eq!(
        api.readiness.as_ref().unwrap().state,
        ReadinessState::Passing
    );

    // A single failure does not cross the failure threshold.
    h.advance_and_poll(Duration::from_secs(1));
    h.event(readiness_attempt(
        "api",
        1,
        3,
        false,
        Some("connection refused".into()),
    ));
    let status = h
        .process("api")
        .readiness
        .expect("readiness remains visible");
    assert_eq!(status.state, ReadinessState::Passing);
    assert_eq!(status.consecutive_failures, 1);

    // The second consecutive failure marks readiness Failing without
    // stopping the live Service.
    h.advance_and_poll(Duration::from_secs(1));
    h.event(readiness_attempt(
        "api",
        1,
        4,
        false,
        Some("connection refused".into()),
    ));
    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Running);
    let status = api.readiness.expect("failing readiness remains visible");
    assert_eq!(status.state, ReadinessState::Failing);
    assert_eq!(status.consecutive_failures, 2);

    // Recovery also needs two consecutive passes and does not rerun startup.
    h.advance_and_poll(Duration::from_secs(1));
    h.event(readiness_attempt("api", 1, 5, true, None));
    assert_eq!(
        h.process("api").readiness.unwrap().state,
        ReadinessState::Failing
    );
    h.advance_and_poll(Duration::from_secs(1));
    h.event(readiness_attempt("api", 1, 6, true, None));
    let status = h
        .process("api")
        .readiness
        .expect("recovered readiness remains visible");
    assert_eq!(status.state, ReadinessState::Passing);
    assert_eq!(status.consecutive_successes, 2);
    assert_eq!(status.consecutive_failures, 0);
}

#[test]
fn attempts_are_dispatched_only_when_the_clock_advances_past_the_interval() {
    let mut h = Harness::new(probed_project());
    start_probed(&mut h);

    // Nothing runs before the first timer poll.
    assert!(h.probes.attempts().is_empty());
    h.advance_and_poll(Duration::from_millis(0));
    assert_eq!(h.probes.attempts().len(), 1);

    // The failed result schedules the next attempt one interval out.
    h.event(readiness(
        "api",
        1,
        false,
        Some("connection refused".into()),
    ));
    h.advance_and_poll(Duration::from_millis(999));
    assert_eq!(h.probes.attempts().len(), 1, "interval not yet elapsed");
    h.advance_and_poll(Duration::from_millis(1));
    assert_eq!(h.probes.attempts().len(), 2);

    // One Run never has two attempts at once.
    h.advance_and_poll(Duration::from_secs(10));
    assert_eq!(h.probes.attempts().len(), 2, "attempt still in flight");
}

#[test]
fn failing_attempts_keep_bounded_diagnostics_visible_while_starting() {
    let mut h = Harness::new(probed_project());
    start_probed(&mut h);
    h.advance_and_poll(Duration::from_millis(0));
    h.event(readiness(
        "api",
        1,
        false,
        Some("connection refused".into()),
    ));

    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Starting);
    let readiness = api
        .readiness
        .expect("readiness stays visible while starting");
    assert_eq!(readiness.attempts, 1);
    assert_eq!(readiness.last_error.as_deref(), Some("connection refused"));
}

#[test]
fn passing_readiness_releases_ready_dependents_exactly_once_per_run() {
    let project = EffectiveProject::new(vec![
        depending_ready_on("api", &["db"]),
        probed_service("db"),
    ])
    .expect("unique names");
    let mut h = Harness::new(project);
    h.command(Command::Start("api".into()));

    // The dependency starts and probes; the dependent waits on `ready`.
    assert_eq!(h.process("db").lifecycle, Lifecycle::Starting);
    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Waiting);
    assert_eq!(api.blocked_reason.as_deref(), Some("db: ready"));

    // A spawned-but-not-ready Run does not satisfy `ready`.
    h.event(spawned("db", 1));
    assert_eq!(h.process("api").lifecycle, Lifecycle::Waiting);
    h.advance_and_poll(Duration::from_millis(0));

    h.event(readiness("db", 1, true, None));
    assert_eq!(h.process("db").lifecycle, Lifecycle::Running);
    assert_eq!(h.process("api").current_run, Some(1));

    // Readiness loss does not stop an already-running dependent.
    h.advance_and_poll(Duration::from_secs(1));
    h.event(readiness_attempt(
        "db",
        1,
        2,
        false,
        Some("connection refused".into()),
    ));
    assert_eq!(
        h.process("db").readiness.unwrap().state,
        ReadinessState::Failing
    );
    assert_eq!(h.process("api").current_run, Some(1));

    // A duplicate passing result cannot release anyone again.
    h.event(readiness("db", 1, true, None));
    let starts = h
        .runtime
        .intents()
        .iter()
        .filter(|intent| {
            matches!(intent, Intent::Start { process_id, .. }
                if *process_id == ProcessId::new(process_index("api")))
        })
        .count();
    assert_eq!(starts, 1);
}

#[test]
fn stopping_invalidates_pending_readiness_and_stale_results_are_ignored() {
    let mut h = Harness::new(probed_project());
    start_probed(&mut h);
    h.advance_and_poll(Duration::from_millis(0));
    h.command(Command::Stop("api".into()));
    assert_eq!(h.process("api").lifecycle, Lifecycle::Stopped);
    assert_eq!(h.process("api").readiness, None);

    // The stopped Run's late probe result changes nothing.
    h.event(readiness("api", 1, true, None));
    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Stopped);
    assert_eq!(api.current_run, None);

    // No further attempts are scheduled for the ended Run.
    h.advance_and_poll(Duration::from_secs(60));
    assert_eq!(h.probes.attempts().len(), 1);
}

#[test]
fn cancellation_records_work_identity_and_rejects_a_released_result() {
    let mut h = Harness::new(probed_project());
    start_probed(&mut h);
    h.advance_and_poll(Duration::from_millis(0));
    let request = h
        .probes
        .requests()
        .into_iter()
        .next()
        .expect("attempt exists");

    h.command(Command::Stop("api".into()));

    assert_eq!(
        h.probes.cancellations(),
        vec![(request.process_id, request.run_id, request.work_id)]
    );
    assert!(h.runtime.intents().iter().any(|intent| {
        matches!(intent, Intent::Cancel { process_id, run_id }
            if *process_id == request.process_id && *run_id == request.run_id)
    }));

    // Metrics and output-owner reports released after cancellation are also
    // harmless; they cannot write into the stopped snapshot.
    h.event(SeamEvent::Metrics {
        process_id: request.process_id,
        run_id: request.run_id,
        cpu_percent: 99.0,
        rss_kib: 9999,
    });
    h.event(SeamEvent::OutputFailure {
        process_id: request.process_id,
        run_id: request.run_id,
        detail: "late output failure".to_string(),
    });

    // The adapter may release the already-started attempt after cancellation,
    // but the stopped Run cannot be made ready again.
    h.probes.release(
        request.process_id,
        request.run_id,
        request.work_id,
        request.attempt_id,
        true,
        None,
    );
    h.drain();
    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Stopped);
    assert_eq!(api.metrics, None);
    assert_eq!(api.failure, None);
}

#[test]
fn a_restarted_run_probes_again_with_its_own_identity() {
    let mut h = Harness::new(probed_project());
    start_probed(&mut h);
    h.advance_and_poll(Duration::from_millis(0));
    h.command(Command::Stop("api".into()));
    h.command(Command::Start("api".into()));

    assert_eq!(h.process("api").current_run, Some(2));
    h.event(spawned("api", 2));
    h.advance_and_poll(Duration::from_millis(0));
    assert_eq!(h.probes.attempts().len(), 2);
    assert_eq!(
        h.probes.attempts()[1],
        (ProcessId::new(process_index("api")), RunId::new(2))
    );

    // The old Run's stale failure cannot touch the new Run.
    h.event(readiness("api", 1, false, Some("stale".into())));
    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Starting);
    assert_eq!(api.readiness.unwrap().last_error, None);
}
