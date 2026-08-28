//! Supervisor tests for live log readiness.

use super::*;
use crate::supervisor::seam::WorkId;

fn log_check(contains: &str) -> ReadinessCheck {
    ReadinessCheck {
        probe: ReadinessProbe::Log {
            contains: contains.to_string(),
        },
        initial_delay: Duration::from_secs(30),
        interval: Duration::from_secs(1),
        timeout: Duration::from_secs(2),
        success_threshold: 1,
        failure_threshold: 1,
    }
}

fn log_project(startup_timeout: Option<Duration>) -> EffectiveProject {
    let mut process = service("api");
    process.autostart = Autostart::No;
    process.readiness = Some(ReadinessConfig {
        checks: vec![log_check("ready")],
        startup_timeout,
    });
    EffectiveProject::new(vec![process]).expect("unique names")
}

fn log_match(process: &str, run: u64, work: u64) -> SeamEvent {
    SeamEvent::LogMatched {
        process_id: ProcessId::new(process_index(process)),
        run_id: RunId::new(run),
        work_id: WorkId::new(work),
        attempt_id: None,
    }
}

fn probe_match(process: &str, run: u64, work: u64, attempt: u64, passing: bool) -> SeamEvent {
    SeamEvent::Readiness {
        process_id: ProcessId::new(process_index(process)),
        run_id: RunId::new(run),
        work_id: WorkId::new(work),
        attempt_id: AttemptId::new(attempt),
        passing,
        diagnostic: None,
    }
}

#[test]
fn a_live_log_match_can_arrive_before_spawn_and_latches_readiness() {
    let mut h = Harness::new(log_project(None));
    h.command(Command::Start("api".into()));

    // The Run reservation creates the WorkId before the adapter starts. This
    // models output that arrives before the adapter's Spawned fact.
    h.event(log_match("api", 1, 1));
    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Running);
    let readiness = api.readiness.expect("log readiness remains visible");
    assert_eq!(readiness.kind, ReadinessCheckKind::Log);
    assert_eq!(readiness.state, ReadinessState::Passing);
    assert_eq!(readiness.attempts, 1);
    assert_eq!(readiness.consecutive_successes, 1);

    // Spawn only supplies the process identity. It cannot reset the latched
    // match or create a readiness probe attempt.
    h.event(spawned("api", 1));
    h.advance_and_poll(Duration::from_secs(60));
    assert_eq!(h.probes.attempts(), Vec::new());
    assert_eq!(h.process("api").readiness.unwrap().attempts, 1);
}

#[test]
fn a_late_match_from_an_older_run_cannot_pass_the_replacement() {
    let mut h = Harness::new(log_project(None));
    h.command(Command::Start("api".into()));
    h.command(Command::Stop("api".into()));
    h.command(Command::Start("api".into()));

    assert_eq!(h.process("api").current_run, Some(2));
    h.event(log_match("api", 1, 1));
    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Starting);
    assert_eq!(
        api.readiness.as_ref().unwrap().state,
        ReadinessState::Pending
    );

    h.event(log_match("api", 2, 2));
    assert_eq!(h.process("api").lifecycle, Lifecycle::Running);
}

#[test]
fn log_child_works_with_all_without_scheduling_repeated_attempts() {
    let mut process = service("api");
    process.autostart = Autostart::No;
    process.readiness = Some(ReadinessConfig {
        checks: vec![
            log_check("ready"),
            ReadinessCheck {
                probe: ReadinessProbe::Tcp {
                    host: "127.0.0.1".into(),
                    port: 1,
                },
                initial_delay: Duration::ZERO,
                interval: Duration::from_secs(1),
                timeout: Duration::from_millis(100),
                success_threshold: 1,
                failure_threshold: 1,
            },
        ],
        startup_timeout: None,
    });
    let project = EffectiveProject::new(vec![process]).expect("unique names");
    let mut h = Harness::new(project);
    h.command(Command::Start("api".into()));

    h.event(log_match("api", 1, 1));
    assert_eq!(
        h.process("api").readiness.unwrap().state,
        ReadinessState::Pending
    );
    h.event(spawned("api", 1));

    h.advance_and_poll(Duration::from_secs(30));
    let requests = h.probes.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].work_id, WorkId::new(2));
    h.event(probe_match("api", 1, 2, 1, true));
    assert_eq!(h.process("api").lifecycle, Lifecycle::Running);

    h.advance_and_poll(Duration::from_secs(60));
    assert_eq!(h.probes.requests().len(), 2);
    assert_eq!(h.process("api").readiness.unwrap().children[0].attempts, 1);
}

#[test]
fn missing_log_match_hits_startup_timeout_and_ignores_late_output() {
    let runtime = FakeRuntime::shared();
    runtime.set_hold_stops(true);
    let mut h = Harness::with(log_project(Some(Duration::from_secs(1))), runtime);
    h.command(Command::Start("api".into()));
    h.event(spawned("api", 1));

    h.advance_and_poll(Duration::from_secs(1));
    let api = h.process("api");
    assert_eq!(api.lifecycle, Lifecycle::Stopping);
    assert_eq!(api.desired, DesiredState::Stopped);
    assert_eq!(api.current_run, Some(1));
    assert_eq!(
        api.failure.as_ref().map(|failure| failure.kind),
        Some(FailureKind::Readiness)
    );
    assert!(h.probes.cancellations().is_empty());
    assert!(h.runtime.intents().iter().any(|intent| {
        matches!(intent, Intent::Cancel { process_id, run_id }
            if *process_id == ProcessId::new(0) && *run_id == RunId::new(1))
    }));

    h.event(log_match("api", 1, 1));
    assert_eq!(h.process("api").lifecycle, Lifecycle::Stopping);
}
