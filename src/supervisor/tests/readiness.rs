//! Readiness behavior tests. They drive the same highest seam as the
//! parent module: semantic commands, typed seam events, dispatched probe
//! intents, and immutable snapshots — with a fake clock so interval
//! scheduling stays deterministic.

use std::time::Duration;

use super::*;

fn probed_project() -> EffectiveProject {
    EffectiveProject::new(vec![probed_service("api")]).expect("unique names")
}

fn all_check(
    probe: ReadinessProbe,
    initial_delay: Duration,
    success_threshold: u32,
    failure_threshold: u32,
) -> ReadinessCheck {
    ReadinessCheck {
        probe,
        initial_delay,
        interval: Duration::from_secs(1),
        timeout: Duration::from_millis(100),
        success_threshold,
        failure_threshold,
    }
}

fn all_project(
    initial_delays: [Duration; 2],
    success_thresholds: [u32; 2],
    failure_thresholds: [u32; 2],
    startup_timeout: Option<Duration>,
) -> EffectiveProject {
    let mut process = service("api");
    process.autostart = Autostart::No;
    process.readiness = Some(ReadinessConfig {
        checks: vec![
            all_check(
                ReadinessProbe::Tcp {
                    host: "127.0.0.1".into(),
                    port: 1,
                },
                initial_delays[0],
                success_thresholds[0],
                failure_thresholds[0],
            ),
            all_check(
                ReadinessProbe::Http {
                    host: "127.0.0.1".into(),
                    port: 2,
                    path: "/healthz".into(),
                },
                initial_delays[1],
                success_thresholds[1],
                failure_thresholds[1],
            ),
        ],
        startup_timeout,
    });
    EffectiveProject::new(vec![process]).expect("unique names")
}

fn child_readiness(
    process: &str,
    run: u64,
    work: u64,
    attempt: u64,
    passing: bool,
    diagnostic: Option<String>,
) -> SeamEvent {
    SeamEvent::Readiness {
        process_id: ProcessId::new(process_index(process)),
        run_id: RunId::new(run),
        work_id: WorkId::new(work),
        attempt_id: AttemptId::new(attempt),
        passing,
        diagnostic,
    }
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
fn exec_probe_intent_carries_process_context_and_has_its_own_snapshot_kind() {
    let mut process = probed_service("api");
    process.working_dir = std::path::PathBuf::from("/tmp");
    process.env = vec![("BASE".into(), "process".into())];
    process.readiness.as_mut().expect("readiness exists").checks[0].probe = ReadinessProbe::Exec {
        command: CommandForm::Shell {
            text: "test \"$CHECK\" = probe".to_string(),
        },
        working_dir: Some(std::path::PathBuf::from("/var/empty")),
        env: vec![("CHECK".into(), "probe".into())],
        success_exit_codes: vec![0, 2],
    };
    let project = EffectiveProject::with_shell(
        vec![process],
        ShellConfig {
            program: "/bin/bash".into(),
            args: vec!["-c".into()],
        },
    )
    .expect("exec readiness project is valid");
    let mut h = Harness::new(project);
    start_probed(&mut h);
    h.advance_and_poll(Duration::ZERO);

    let request = h
        .probes
        .requests()
        .into_iter()
        .next()
        .expect("exec attempt exists");
    assert!(matches!(request.probe, ReadinessProbe::Exec { .. }));
    let context = request.exec_context.expect("exec context is attached");
    assert_eq!(context.working_dir, std::path::PathBuf::from("/tmp"));
    assert_eq!(context.env, vec![("BASE".into(), "process".into())]);
    assert_eq!(context.shell.program, std::ffi::OsString::from("/bin/bash"));
    assert_eq!(context.shell.args, vec![std::ffi::OsString::from("-c")]);
    assert_eq!(
        h.process("api")
            .readiness
            .expect("readiness is visible")
            .kind,
        ReadinessCheckKind::Exec
    );
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

#[test]
fn duplicate_spawn_keeps_child_work_identities_for_the_current_run() {
    let mut h = Harness::new(all_project(
        [Duration::ZERO, Duration::ZERO],
        [1, 1],
        [1, 1],
        None,
    ));
    start_probed(&mut h);
    h.advance_and_poll(Duration::ZERO);
    assert_eq!(
        h.probes
            .requests()
            .iter()
            .map(|request| request.work_id)
            .collect::<Vec<_>>(),
        vec![WorkId::new(1), WorkId::new(2)]
    );

    h.event(spawned("api", 1));
    assert_eq!(h.process("api").lifecycle, Lifecycle::Starting);
    assert_eq!(h.probes.requests().len(), 2);

    h.event(child_readiness("api", 1, 1, 1, true, None));
    h.event(child_readiness("api", 1, 2, 1, true, None));
    assert_eq!(h.process("api").lifecycle, Lifecycle::Running);
}

#[test]
fn all_children_have_independent_schedules_and_gate_composite_readiness() {
    let mut h = Harness::new(all_project(
        [Duration::ZERO, Duration::from_secs(1)],
        [2, 1],
        [1, 1],
        None,
    ));
    start_probed(&mut h);

    h.advance_and_poll(Duration::ZERO);
    let requests = h.probes.requests();
    assert_eq!(requests.len(), 1, "only the first child is due");
    assert_eq!(requests[0].work_id, WorkId::new(1));
    assert_eq!(requests[0].attempt_id, AttemptId::new(1));

    // The first child is still in flight, but the second child gets its own
    // attempt as soon as its independent initial delay expires.
    h.advance_and_poll(Duration::from_millis(999));
    assert_eq!(h.probes.requests().len(), 1);
    h.advance_and_poll(Duration::from_millis(1));
    let requests = h.probes.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].work_id, WorkId::new(2));

    h.event(child_readiness("api", 1, 1, 1, true, None));
    let status = h.process("api");
    assert_eq!(status.lifecycle, Lifecycle::Starting);
    let readiness = status
        .readiness
        .expect("composite readiness remains visible");
    assert_eq!(readiness.kind, ReadinessCheckKind::All);
    assert_eq!(readiness.state, ReadinessState::Pending);
    assert_eq!(readiness.children.len(), 2);
    assert_eq!(readiness.children[0].index, 1);
    assert_eq!(readiness.children[0].kind, ReadinessCheckKind::Tcp);
    assert_eq!(readiness.children[0].consecutive_successes, 1);
    assert_eq!(readiness.children[1].kind, ReadinessCheckKind::Http);
    assert_eq!(readiness.children[1].state, ReadinessState::Pending);

    h.event(child_readiness("api", 1, 2, 1, true, None));
    assert_eq!(
        h.process("api").readiness.as_ref().unwrap().state,
        ReadinessState::Pending,
        "the first child still needs its own success threshold"
    );

    // Both children are due at the next interval. The second child keeps its
    // own attempt cadence while the first child completes its threshold.
    h.advance_and_poll(Duration::from_secs(1));
    assert_eq!(h.probes.requests().len(), 4);
    h.event(child_readiness("api", 1, 1, 2, true, None));
    h.event(child_readiness("api", 1, 2, 2, true, None));
    let status = h.process("api");
    assert_eq!(status.lifecycle, Lifecycle::Running);
    let readiness = status.readiness.expect("passing composite remains visible");
    assert_eq!(readiness.state, ReadinessState::Passing);
    assert_eq!(readiness.children[0].state, ReadinessState::Passing);
    assert_eq!(readiness.children[1].state, ReadinessState::Passing);
}

#[test]
fn a_failing_child_marks_the_composite_failed_and_recovers_without_resetting_others() {
    let mut h = Harness::new(all_project(
        [Duration::ZERO, Duration::ZERO],
        [1, 1],
        [1, 1],
        None,
    ));
    start_probed(&mut h);
    h.advance_and_poll(Duration::ZERO);
    h.event(child_readiness("api", 1, 1, 1, true, None));
    h.event(child_readiness("api", 1, 2, 1, true, None));
    assert_eq!(
        h.process("api").readiness.as_ref().unwrap().state,
        ReadinessState::Passing
    );

    h.advance_and_poll(Duration::from_secs(1));
    h.event(child_readiness(
        "api",
        1,
        1,
        2,
        false,
        Some("tcp unavailable".into()),
    ));
    h.event(child_readiness("api", 1, 2, 2, true, None));
    let status = h.process("api");
    assert_eq!(status.lifecycle, Lifecycle::Running);
    let readiness = status.readiness.expect("failing composite remains visible");
    assert_eq!(readiness.state, ReadinessState::Failing);
    assert_eq!(
        readiness.last_error.as_deref(),
        Some("all child 1: tcp unavailable")
    );
    assert_eq!(readiness.children[0].state, ReadinessState::Failing);
    assert_eq!(
        readiness.children[0].last_error.as_deref(),
        Some("tcp unavailable")
    );
    assert_eq!(readiness.children[1].state, ReadinessState::Passing);
    assert_eq!(readiness.children[1].consecutive_successes, 2);

    h.advance_and_poll(Duration::from_secs(1));
    h.event(child_readiness("api", 1, 1, 3, true, None));
    h.event(child_readiness(
        "api",
        1,
        2,
        3,
        false,
        Some("http unavailable".into()),
    ));
    let readiness = h
        .process("api")
        .readiness
        .expect("failing composite remains visible");
    assert_eq!(readiness.state, ReadinessState::Failing);
    assert_eq!(
        readiness.last_error.as_deref(),
        Some("all child 2: http unavailable")
    );
    assert_eq!(readiness.children[0].state, ReadinessState::Passing);
    assert_eq!(readiness.children[1].state, ReadinessState::Failing);

    h.advance_and_poll(Duration::from_secs(1));
    h.event(child_readiness("api", 1, 1, 4, true, None));
    h.event(child_readiness("api", 1, 2, 4, true, None));
    let readiness = h
        .process("api")
        .readiness
        .expect("recovered composite remains visible");
    assert_eq!(readiness.state, ReadinessState::Passing);
    assert_eq!(readiness.children[0].state, ReadinessState::Passing);
    assert_eq!(readiness.children[1].state, ReadinessState::Passing);
    assert_eq!(readiness.children[1].consecutive_failures, 0);
}

#[test]
fn all_readiness_cancels_every_child_and_rejects_late_results() {
    let mut h = Harness::new(all_project(
        [Duration::ZERO, Duration::ZERO],
        [1, 1],
        [1, 1],
        None,
    ));
    start_probed(&mut h);
    h.advance_and_poll(Duration::ZERO);
    let requests = h.probes.requests();
    assert_eq!(requests.len(), 2);

    h.command(Command::Stop("api".into()));
    assert_eq!(
        h.probes.cancellations(),
        vec![
            (requests[0].process_id, requests[0].run_id, WorkId::new(1)),
            (requests[1].process_id, requests[1].run_id, WorkId::new(2)),
        ]
    );
    assert_eq!(h.process("api").lifecycle, Lifecycle::Stopped);
    assert_eq!(h.process("api").readiness, None);

    h.event(child_readiness("api", 1, 1, 1, true, None));
    h.event(child_readiness("api", 1, 2, 1, true, None));
    assert_eq!(h.process("api").lifecycle, Lifecycle::Stopped);
    assert_eq!(h.process("api").current_run, None);
}

#[test]
fn composite_readiness_releases_a_ready_dependent_once_and_does_not_cascade_loss() {
    let api =
        all_project([Duration::ZERO, Duration::ZERO], [1, 1], [1, 1], None).processes()[0].clone();
    let dependent = depending_ready_on("db", &["api"]);
    let project = EffectiveProject::new(vec![api, dependent]).expect("unique names");
    let mut h = Harness::new(project);
    h.command(Command::Start("db".into()));
    assert_eq!(h.process("db").lifecycle, Lifecycle::Waiting);

    h.event(spawned("api", 1));
    h.advance_and_poll(Duration::ZERO);
    h.event(child_readiness("api", 1, 1, 1, true, None));
    assert_eq!(h.process("db").lifecycle, Lifecycle::Waiting);
    h.event(child_readiness("api", 1, 2, 1, true, None));
    assert_eq!(h.process("db").current_run, Some(1));
    assert_eq!(h.process("db").lifecycle, Lifecycle::Starting);
    h.event(spawned("db", 1));

    h.advance_and_poll(Duration::from_secs(1));
    h.event(child_readiness(
        "api",
        1,
        1,
        2,
        false,
        Some("tcp unavailable".into()),
    ));
    h.event(child_readiness("api", 1, 2, 2, true, None));
    assert_eq!(
        h.process("api").readiness.unwrap().state,
        ReadinessState::Failing
    );
    assert_eq!(h.process("db").current_run, Some(1));
    assert_eq!(h.process("db").lifecycle, Lifecycle::Running);
}

#[test]
fn composite_startup_timeout_waits_for_all_children() {
    let runtime = FakeRuntime::shared();
    runtime.set_hold_stops(true);
    let mut h = Harness::with(
        all_project(
            [Duration::ZERO, Duration::ZERO],
            [1, 1],
            [1, 1],
            Some(Duration::from_secs(1)),
        ),
        runtime,
    );
    start_probed(&mut h);
    h.advance_and_poll(Duration::ZERO);
    h.event(child_readiness("api", 1, 1, 1, true, None));
    assert_eq!(h.process("api").lifecycle, Lifecycle::Starting);

    h.advance_and_poll(Duration::from_secs(1));
    let status = h.process("api");
    assert_eq!(status.lifecycle, Lifecycle::Stopping);
    assert_eq!(status.current_run, Some(1));
    assert_eq!(
        status.failure.as_ref().map(|failure| failure.kind),
        Some(FailureKind::Readiness)
    );
    assert_eq!(h.probes.cancellations().len(), 2);

    h.event(child_readiness("api", 1, 2, 1, true, None));
    assert_eq!(h.process("api").lifecycle, Lifecycle::Stopping);
}
