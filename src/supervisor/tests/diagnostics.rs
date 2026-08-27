//! Metrics, structured diagnostics, and readiness bookkeeping: the
//! snapshot's diagnostic projections and the structured failure kinds.

use super::*;
use crate::supervisor::{FailureKind, FailureSummary, RunTrigger};

fn confirmed(process: &str, run: u64) -> SeamEvent {
    SeamEvent::ShutdownComplete {
        process_id: ProcessId::new(process_index(process)),
        run_id: RunId::new(run),
        confirmed: true,
        detail: None,
        remaining_pids: Vec::new(),
    }
}

fn unconfirmed(process: &str, run: u64) -> SeamEvent {
    SeamEvent::ShutdownComplete {
        process_id: ProcessId::new(process_index(process)),
        run_id: RunId::new(run),
        confirmed: false,
        detail: None,
        remaining_pids: Vec::new(),
    }
}

/// A runtime whose spawn always fails, for the Configuration-kind case.
fn fail_spawn_runtime() -> std::sync::Arc<crate::supervisor::support::FakeRuntime> {
    let runtime = crate::supervisor::support::FakeRuntime::shared();
    runtime.set_fail_spawn(true);
    runtime
}

/// A stale Run's metrics cannot change the newer Run's sample.
#[test]
fn stale_metrics_cannot_change_a_newer_run() {
    let mut h = Harness::new(four_process_project());
    h.report_spawns();
    h.command(Command::Start("api".into()));
    h.event(spawned("api", 1));
    h.event(SeamEvent::Metrics {
        process_id: ProcessId::new(process_index("api")),
        run_id: RunId::new(1),
        cpu_percent: 5.0,
        rss_kib: 1024,
    });

    // The Run ends and the replacement Run begins: the old sample's
    // identity must not bleed into the new Run.
    h.event(exited("api", 1, Some(0)));
    h.event(confirmed("api", 1));
    h.command(Command::Start("api".into()));
    h.event(spawned("api", 2));

    h.event(SeamEvent::Metrics {
        process_id: ProcessId::new(process_index("api")),
        run_id: RunId::new(1),
        cpu_percent: 99.0,
        rss_kib: 1 << 20,
    });

    let snapshot = h.process("api");
    assert_eq!(
        snapshot.metrics.map(|metrics| metrics.run_id),
        None,
        "a stale sample must not land on the new Run"
    );

    h.event(SeamEvent::Metrics {
        process_id: ProcessId::new(process_index("api")),
        run_id: RunId::new(2),
        cpu_percent: 7.5,
        rss_kib: 2048,
    });
    let sample = h
        .process("api")
        .metrics
        .expect("the current Run's sample lands");
    assert_eq!(sample.run_id, 2);
    assert_eq!(sample.cpu_percent, 7.5);
}

/// Active snapshots carry the observed PID, the Run start stamp, and the
/// session time an age is measured against.
#[test]
fn active_snapshots_carry_pid_start_time_and_session_time() {
    let mut h = Harness::new(four_process_project());
    h.report_spawns();
    h.command(Command::Start("api".into()));

    let snapshot = h.snapshot();
    let api = snapshot.named("api").expect("api exists");
    assert_eq!(api.current_run, Some(1));
    assert_eq!(api.root_pid, Some(1));
    let started_at = api.run_started_at_ms.expect("an active Run started");
    assert!(
        snapshot.now_ms >= started_at,
        "the session time never trails the Run's start"
    );

    // A finished Run clears the observed PID and the start stamp.
    h.event(exited("api", 1, Some(0)));
    h.event(confirmed("api", 1));
    let finished = h.process("api");
    assert_eq!(finished.root_pid, None);
    assert_eq!(finished.run_started_at_ms, None);
}

/// worker starts only when the never-started setup One-shot is started.
fn blocked_project() -> EffectiveProject {
    EffectiveProject::new(vec![
        service("api"),
        service("db"),
        depending_on("worker", &["setup"]),
        simple("setup", ProcessKind::OneShot, Enabled::Yes, Autostart::No),
    ])
    .expect("unique names")
}

/// A blocked Process names the blocking Dependency and its condition.
#[test]
fn blocked_reasons_name_the_dependency_and_condition() {
    let mut h = Harness::new(blocked_project());
    h.command(Command::Start("worker".into()));
    h.command(Command::Stop("setup".into()));

    let worker = h.process("worker");
    assert_eq!(worker.lifecycle, Lifecycle::Waiting);
    let reason = worker.blocked_reason.expect("worker is blocked");
    assert!(
        reason.starts_with("setup: "),
        "the reason names the blocking Dependency: {reason:?}"
    );
    assert!(
        reason.contains("started"),
        "the reason names the required condition: {reason:?}"
    );
}

/// A blocked reason for a disabled Dependency carries its structured label.
#[test]
fn a_disabled_dependency_reason_stays_structured() {
    let mut worker = depending_on("worker", &["db"]);
    worker.enabled = Enabled::No;
    let mut setup = simple("setup", ProcessKind::OneShot, Enabled::Yes, Autostart::No);
    setup.dependencies = vec![crate::model::DependencySpec {
        name: "worker".to_string(),
        condition: DependencyCondition::Started,
    }];
    let mut h = Harness::new(
        EffectiveProject::new(vec![service("api"), service("db"), worker, setup])
            .expect("unique names"),
    );

    // A disabled Process never starts, so its dependent's blocked reason
    // carries the dependency's structured Disabled label, not a bare
    // string.
    h.command(Command::Start("setup".into()));
    let setup = h.process("setup");
    assert_eq!(setup.lifecycle, Lifecycle::Waiting);
    assert_eq!(
        setup.blocked_reason.as_deref(),
        Some("worker: disabled"),
        "the reason names the dependency and the structured label"
    );
}

/// Every supported failure kind is structured, not a bare string.
#[test]
fn failure_kinds_cover_the_supported_failure_sources() {
    let mut h = Harness::with(four_process_project(), fail_spawn_runtime());
    h.command(Command::Start("api".into()));
    let failed = h.process("api");
    assert_eq!(
        failed.failure,
        Some(FailureSummary {
            kind: FailureKind::Configuration,
            detail: "scripted spawn failure".to_string(),
        }),
        "a spawn failure is a structured Configuration failure"
    );
    assert!(
        matches!(
            failed.failure,
            Some(FailureSummary {
                kind: FailureKind::Configuration,
                ..
            })
        ),
        "the kind distinguishes a configuration failure"
    );

    // A Process exit and an unconfirmed shutdown carry their own kinds.
    let mut h = Harness::new(four_process_project());
    h.command(Command::Start("setup".into()));
    h.event(spawned("setup", 1));
    h.event(exited("setup", 1, Some(3)));
    h.event(confirmed("setup", 1));
    let one_shot = h.process("setup");
    assert_eq!(
        one_shot.failure.as_ref().map(|failure| failure.kind),
        Some(FailureKind::ProcessExit),
        "a failed One-shot exit is a structured ProcessExit failure"
    );

    h.command(Command::Start("db".into()));
    h.event(spawned("db", 1));
    h.event(unconfirmed("db", 1));
    let stopped = h.process("db");
    assert_eq!(
        stopped.failure.as_ref().map(|failure| failure.kind),
        Some(FailureKind::Shutdown),
        "an unconfirmed cleanup is a structured Shutdown failure"
    );
}

/// Readiness diagnostics stay bounded: attempt count and the last error.
#[test]
fn readiness_diagnostics_carry_attempts_and_the_last_error() {
    let mut h =
        Harness::new(EffectiveProject::new(vec![probed_service("api")]).expect("unique names"));
    h.report_spawns();
    h.command(Command::Start("api".into()));
    h.event(spawned("api", 1));
    // The timer poll dispatches the first attempt; the failing result
    // lands through the probe's bounded report.
    h.advance_and_poll(std::time::Duration::from_millis(0));
    h.event(SeamEvent::Readiness {
        process_id: ProcessId::new(process_index("api")),
        run_id: RunId::new(1),
        passing: false,
        diagnostic: Some("connection refused".to_string()),
    });

    let api = h.process("api");
    let readiness = api.readiness.expect("a probed Run keeps its bookkeeping");
    assert_eq!(readiness.attempts, 1, "a failed attempt is counted");
    assert_eq!(
        readiness.last_error.as_deref(),
        Some("connection refused"),
        "the last error stays bounded and visible"
    );

    h.event(SeamEvent::Readiness {
        process_id: ProcessId::new(process_index("api")),
        run_id: RunId::new(1),
        passing: true,
        diagnostic: None,
    });
    let api = h.process("api");
    assert_eq!(api.readiness, None, "a passing probe ends the bookkeeping");
    assert_eq!(api.lifecycle, Lifecycle::Running);
}

/// A Run's recent summary records its trigger, so a rerun's attempts are
/// distinguishable from a first start.
#[test]
fn recent_summaries_record_what_started_the_run() {
    let mut h = Harness::new(four_process_project());
    h.report_spawns();
    h.command(Command::Start("setup".into()));
    h.event(spawned("setup", 1));
    h.event(exited("setup", 1, Some(0)));
    h.event(confirmed("setup", 1));

    let process = h.process("setup");
    let summary = process
        .recent_runs
        .first()
        .expect("the finished attempt is summarized");
    assert_eq!(summary.trigger, RunTrigger::Manual);
    assert_eq!(summary.failure, None);
}
