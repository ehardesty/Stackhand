//! Supervisor behavior tests. They drive the highest seam only: semantic
//! commands, typed seam events, observable runtime intent, and immutable
//! snapshots — never internal state-machine fields.

use std::sync::Arc;
use std::time::Duration;

use super::ProjectSnapshot;
use super::support::{FakeClock, FakeProbes, FakeRuntime, Intent};
use super::{Command, Core, DesiredState, Lifecycle, ProcessSnapshot, SeamEvent, SeamSender};
use crate::model::{
    Autostart, CommandForm, DependencyCondition, EffectiveProject, Enabled, InputPolicy,
    ProcessKind, ProcessSpec, ReadinessConfig, ReadinessProbe, TerminalMode,
};
use crate::runtime::{ProcessId, RunId};
use crate::supervisor::clock::Clock;

fn service(name: &str) -> ProcessSpec {
    simple(name, ProcessKind::Service, Enabled::Yes, Autostart::Yes)
}

fn simple(name: &str, kind: ProcessKind, enabled: Enabled, autostart: Autostart) -> ProcessSpec {
    ProcessSpec {
        name: name.to_string(),
        kind,
        enabled,
        autostart,
        command: CommandForm::Direct {
            program: "sleep".into(),
            args: vec!["1".into()],
        },
        working_dir: std::env::temp_dir(),
        env: Vec::new(),
        terminal_mode: TerminalMode::Pipe,
        input_policy: InputPolicy::Disabled,
        dependencies: Vec::new(),
        readiness: None,
    }
}

/// A Service gated on a TCP probe against a port nothing listens on; the
/// scripted probes never succeed unless a test reports a passing event.
fn probed_service(name: &str) -> ProcessSpec {
    let mut spec = service(name);
    spec.autostart = Autostart::No;
    spec.readiness = Some(ReadinessConfig {
        probe: ReadinessProbe::Tcp {
            host: "127.0.0.1".into(),
            port: 1,
        },
        interval: Duration::from_secs(1),
        timeout: Duration::from_millis(100),
    });
    spec
}

/// A Process that starts only after each named Dependency satisfies its
/// `started` condition.
fn depending_on(name: &str, dependencies: &[&str]) -> ProcessSpec {
    let mut spec = simple(name, ProcessKind::Service, Enabled::Yes, Autostart::No);
    spec.dependencies = dependencies
        .iter()
        .map(|dependency| crate::model::DependencySpec {
            name: (*dependency).to_string(),
            condition: crate::model::DependencyCondition::Started,
        })
        .collect();
    spec
}

/// A Process that starts only after each named Service Dependency's
/// readiness probe has passed.
fn depending_ready_on(name: &str, dependencies: &[&str]) -> ProcessSpec {
    let mut spec = depending_on(name, dependencies);
    for dependency in &mut spec.dependencies {
        dependency.condition = DependencyCondition::Ready;
    }
    spec
}

/// A Service that starts only after each named One-shot Dependency reports
/// `completed_successfully`.
fn depending_completed_on(name: &str, dependencies: &[&str]) -> ProcessSpec {
    let mut spec = depending_on(name, dependencies);
    for dependency in &mut spec.dependencies {
        dependency.condition = DependencyCondition::CompletedSuccessfully;
    }
    spec
}

struct Harness {
    core: Core,
    runtime: Arc<FakeRuntime>,
    probes: Arc<FakeProbes>,
    clock: Arc<FakeClock>,
    emitted: crossbeam_channel::Receiver<SeamEvent>,
}

impl Harness {
    fn new(project: EffectiveProject) -> Self {
        Self::with(project, FakeRuntime::shared())
    }

    fn with(project: EffectiveProject, runtime: Arc<FakeRuntime>) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let clock = Arc::new(FakeClock::new());
        let probes = FakeProbes::shared();
        // The core and the harness share one fake timeline through the
        // clock's interior shared `Instant`.
        let core_clock: Arc<dyn Clock> = Arc::new(clock.as_ref().clone());
        let core = Core::new(
            project,
            Box::new(Arc::clone(&runtime)),
            Box::new(Arc::clone(&probes)),
            core_clock,
            SeamSender::new(tx),
            crate::geometry::TerminalGeometry::DEFAULT,
        );
        Self {
            core,
            runtime,
            probes,
            clock,
            emitted: rx,
        }
    }

    /// Apply every event that scripted adapter calls emitted while handling
    /// commands.
    fn drain(&mut self) {
        while let Ok(event) = self.emitted.try_recv() {
            self.core.event(event);
        }
    }

    fn command(&mut self, command: Command) {
        self.core.command(command);
        self.drain();
    }

    fn event(&mut self, event: SeamEvent) {
        self.core.event(event);
    }

    /// Advance the fake clock, then dispatch every attempt that became due.
    fn advance_and_poll(&mut self, by: Duration) {
        self.clock.advance(by);
        self.core.poll_timers(self.clock.now());
        self.drain();
    }

    fn snapshot(&self) -> ProjectSnapshot {
        self.core.snapshot()
    }

    fn process(&self, name: &str) -> ProcessSnapshot {
        self.snapshot()
            .named(name)
            .cloned()
            .unwrap_or_else(|| panic!("process {name} missing from snapshot"))
    }
}

fn spawned(process: &str, run: u64) -> SeamEvent {
    SeamEvent::Spawned {
        process_id: ProcessId::new(process_index(process)),
        run_id: RunId::new(run),
        root_pid: None,
    }
}

fn exited(process: &str, run: u64, code: Option<i32>) -> SeamEvent {
    SeamEvent::Exited {
        process_id: ProcessId::new(process_index(process)),
        run_id: RunId::new(run),
        code,
    }
}

fn readiness(process: &str, run: u64, passing: bool, diagnostic: Option<String>) -> SeamEvent {
    SeamEvent::Readiness {
        process_id: ProcessId::new(process_index(process)),
        run_id: RunId::new(run),
        passing,
        diagnostic,
    }
}

/// Test processes are registered in this order: api, db, worker, setup.
/// A Process identity is its stable position in the Project.
fn process_index(name: &str) -> u32 {
    match name {
        "api" => 0,
        "db" => 1,
        "worker" => 2,
        "setup" => 3,
        other => panic!("unknown test process {other}"),
    }
}

fn four_process_project() -> EffectiveProject {
    EffectiveProject::new(vec![
        service("api"),
        service("db"),
        service("worker"),
        simple("setup", ProcessKind::OneShot, Enabled::Yes, Autostart::No),
    ])
    .expect("unique names")
}

#[test]
fn duplicate_names_are_rejected() {
    let error = EffectiveProject::new(vec![service("api"), service("api")])
        .expect_err("duplicates must fail");
    assert_eq!(
        error,
        crate::model::ProjectError::DuplicateName("api".into())
    );
}

#[test]
fn start_allocates_run_identity_and_records_intent() {
    let mut h = Harness::new(four_process_project());
    h.command(Command::Start("api".into()));

    assert_eq!(h.process("api").desired, DesiredState::Running);
    assert_eq!(h.process("api").lifecycle, Lifecycle::Starting);
    assert_eq!(h.process("api").current_run, Some(1));
    assert!(matches!(
        h.runtime.intents().as_slice(),
        [Intent::Start { process_id, run_id }]
            if *process_id == ProcessId::new(process_index("api"))
                && *run_id == RunId::new(1)
    ));
}

#[test]
fn spawn_event_makes_service_running_without_a_probe() {
    let mut h = Harness::new(four_process_project());
    h.command(Command::Start("api".into()));
    h.event(spawned("api", 1));

    assert_eq!(h.process("api").lifecycle, Lifecycle::Running);
}

#[test]
fn start_is_rejected_while_a_run_is_active_or_desired_running() {
    let mut h = Harness::new(four_process_project());
    h.command(Command::Start("api".into()));
    h.command(Command::Start("api".into()));

    assert_eq!(h.runtime.intents().len(), 1);
    assert_eq!(h.process("api").current_run, Some(1));
}

#[test]
fn disabled_processes_cannot_start() {
    let project = EffectiveProject::new(vec![
        simple("api", ProcessKind::Service, Enabled::Yes, Autostart::No),
        simple("db", ProcessKind::Service, Enabled::No, Autostart::Yes),
    ])
    .expect("unique names");
    let mut h = Harness::new(project);
    h.command(Command::StartAutostart);
    h.command(Command::Start("db".into()));

    assert_eq!(h.runtime.intents(), Vec::new());
    assert_eq!(h.process("db").lifecycle, Lifecycle::Idle);
    assert!(!h.process("db").enabled);
}

#[test]
fn autostart_starts_only_enabled_autostart_processes() {
    let project = EffectiveProject::new(vec![
        service("api"),
        simple("db", ProcessKind::Service, Enabled::Yes, Autostart::No),
        simple("worker", ProcessKind::Service, Enabled::No, Autostart::Yes),
    ])
    .expect("unique names");
    let mut h = Harness::new(project);
    h.command(Command::StartAutostart);

    let started: Vec<_> = h.runtime.intents();
    assert_eq!(started.len(), 1);
    assert_eq!(h.process("api").desired, DesiredState::Running);
    assert_eq!(h.process("db").desired, DesiredState::Stopped);
    assert_eq!(h.process("worker").desired, DesiredState::Stopped);
}

#[test]
fn snapshots_surface_enabled_and_autostart_flags() {
    let project = EffectiveProject::new(vec![
        service("api"),
        simple("db", ProcessKind::Service, Enabled::Yes, Autostart::No),
        simple("worker", ProcessKind::Service, Enabled::No, Autostart::Yes),
        simple("cron", ProcessKind::Service, Enabled::No, Autostart::No),
    ])
    .expect("unique names");
    let h = Harness::new(project);

    let api = h.process("api");
    assert!(api.enabled && api.autostart);
    let db = h.process("db");
    assert!(db.enabled && !db.autostart);
    let worker = h.process("worker");
    assert!(!worker.enabled && worker.autostart);
    let cron = h.process("cron");
    assert!(!cron.enabled && !cron.autostart);
}

#[test]
fn a_mixed_project_leaves_manual_and_disabled_processes_available() {
    let project = EffectiveProject::new(vec![
        service("api"),
        simple("setup", ProcessKind::OneShot, Enabled::Yes, Autostart::No),
        simple("debug", ProcessKind::Service, Enabled::No, Autostart::No),
    ])
    .expect("unique names");
    let mut h = Harness::new(project);

    // Autostart touches only the enabled autostart Process.
    h.command(Command::StartAutostart);
    let setup = h.process("setup");
    assert_eq!(setup.lifecycle, Lifecycle::Idle);
    assert_eq!(h.process("debug").lifecycle, Lifecycle::Idle);

    // A disabled Process stays visible and cannot start even manually.
    h.command(Command::Start("debug".into()));
    let debug = h.process("debug");
    assert!(!debug.enabled);
    assert_eq!(debug.desired, DesiredState::Stopped);
    assert_eq!(debug.current_run, None);

    // An enabled non-autostart Process starts only by an explicit Start.
    h.command(Command::Start("setup".into()));
    assert_eq!(h.process("setup").desired, DesiredState::Running);
    assert_eq!(h.process("setup").current_run, Some(1));
}

#[test]
fn manual_stop_finishes_as_stopped_without_failure() {
    let mut h = Harness::new(four_process_project());
    h.command(Command::Start("api".into()));
    // The stop intent is visible before any scripted completion arrives.
    h.core.command(Command::Stop("api".into()));

    assert_eq!(h.process("api").desired, DesiredState::Stopped);

    assert_eq!(h.process("api").lifecycle, Lifecycle::Stopping);
    h.drain();

    let stopped_intents: Vec<_> = h
        .runtime
        .intents()
        .iter()
        .copied()
        .filter(|intent| matches!(intent, Intent::Stop { .. }))
        .collect();
    assert!(matches!(
        stopped_intents.as_slice(),
        [Intent::Stop { process_id, run_id }]
            if *process_id == ProcessId::new(process_index("api"))
                && *run_id == RunId::new(1)
    ));

    assert_eq!(h.process("api").lifecycle, Lifecycle::Stopped);
    assert_eq!(h.process("api").current_run, None);
    assert_eq!(h.process("api").failure, None);
}

#[test]
fn unexpected_exit_stays_visible_as_a_failure() {
    let mut h = Harness::new(four_process_project());
    h.command(Command::Start("api".into()));
    h.event(spawned("api", 1));
    h.event(exited("api", 1, Some(3)));

    assert_eq!(
        h.process("api").failure.expect("failure is visible").detail,
        "exited unexpectedly with code 3"
    );
}

#[test]
fn every_new_attempt_receives_the_next_per_process_run_id() {
    let mut h = Harness::new(four_process_project());
    h.command(Command::Start("api".into()));
    h.event(spawned("api", 1));
    h.command(Command::Stop("api".into()));
    h.command(Command::Start("api".into()));

    assert_eq!(h.process("api").current_run, Some(2));
    assert_eq!(
        h.process("setup").current_run,
        None,
        "run ids are per-Process"
    );
}

#[test]
fn stale_events_cannot_change_current_state() {
    let mut h = Harness::new(four_process_project());
    h.command(Command::Start("api".into()));
    h.event(spawned("api", 1));
    let before = h.snapshot();

    // An older Run's exit, an unknown Run, and an unknown Process are all
    // rejected by the one gate before any state changes.
    h.event(exited("api", 0, Some(9)));
    h.event(exited("db", 1, Some(9)));
    h.event(SeamEvent::Metrics {
        process_id: ProcessId::new(process_index("api")),
        run_id: RunId::new(999),
        cpu_percent: 99.0,
        rss_kib: 1 << 20,
    });
    h.event(SeamEvent::ShutdownComplete {
        process_id: ProcessId::new(99),
        run_id: RunId::new(1),
        confirmed: true,
        detail: None,
    });

    let after = h.snapshot();
    assert_eq!(after.named("api").unwrap().lifecycle, Lifecycle::Running);
    assert_eq!(after.named("api").unwrap().failure, None);
    assert_eq!(after.named("api").unwrap().metrics, None);
    assert_eq!(
        after.named("db"),
        before.named("db"),
        "an unknown Process's stale event changed nothing"
    );
}

#[test]
fn current_run_metrics_update_metadata_and_stop_clears_it() {
    let mut h = Harness::new(four_process_project());
    h.command(Command::Start("api".into()));
    h.event(SeamEvent::Metrics {
        process_id: ProcessId::new(process_index("api")),
        run_id: RunId::new(1),
        cpu_percent: 12.5,
        rss_kib: 2048,
    });

    assert_eq!(h.process("api").metrics.map(|m| m.cpu_percent), Some(12.5));

    h.command(Command::Stop("api".into()));
    assert_eq!(h.process("api").metrics, None);
}

#[test]
fn failed_spawn_ends_the_run_and_allows_retry() {
    let runtime = FakeRuntime::shared();
    runtime.set_fail_spawn(true);
    let mut h = Harness::with(four_process_project(), Arc::clone(&runtime));
    h.command(Command::Start("api".into()));

    assert_eq!(h.process("api").lifecycle, Lifecycle::Stopped);
    assert!(h.process("api").failure.is_some());

    runtime.set_fail_spawn(false);
    h.command(Command::Start("api".into()));
    assert_eq!(h.process("api").current_run, Some(2));
}

#[test]
fn unconfirmed_cleanup_keeps_a_bounded_reason() {
    let runtime = FakeRuntime::shared();
    runtime
        .fail_cleanup
        .store(true, std::sync::atomic::Ordering::Release);
    let mut h = Harness::with(four_process_project(), Arc::clone(&runtime));
    h.command(Command::Start("api".into()));
    h.command(Command::Stop("api".into()));

    assert_eq!(
        h.process("api").failure.expect("reason is visible").detail,
        "scripted cleanup failure"
    );
    // The held Run identity blocks any replacement Run; only the retry
    // through Stop can release it.
    assert!(h.process("api").current_run.is_some());
    assert_eq!(h.process("api").lifecycle, Lifecycle::Stopped);

    h.command(Command::Start("api".into()));
    assert_eq!(h.process("api").lifecycle, Lifecycle::Stopped);
    assert!(h.process("api").current_run.is_some());
}

#[test]
fn stop_retry_releases_an_unconfirmed_cleanup() {
    let runtime = FakeRuntime::shared();
    runtime
        .fail_cleanup
        .store(true, std::sync::atomic::Ordering::Release);
    let mut h = Harness::with(four_process_project(), Arc::clone(&runtime));
    h.command(Command::Start("api".into()));
    h.command(Command::Stop("api".into()));
    assert!(h.process("api").failure.is_some());

    // The retry succeeds: the Run identity frees and a replacement may
    // start; the failure summary stays visible until the next Run begins.
    runtime
        .fail_cleanup
        .store(false, std::sync::atomic::Ordering::Release);
    h.command(Command::Stop("api".into()));
    assert_eq!(h.process("api").current_run, None);
    assert_eq!(h.process("api").lifecycle, Lifecycle::Stopped);
    assert!(h.process("api").failure.is_some());

    // A replacement Run may start now and receives the next Run ID.
    h.command(Command::Start("api".into()));
    assert!(h.process("api").current_run.is_some());
    assert!(h.process("api").failure.is_none());
}

#[test]
fn stop_all_targets_only_desired_running_processes() {
    let mut h = Harness::new(four_process_project());
    h.command(Command::Start("api".into()));
    h.command(Command::Start("db".into()));
    h.command(Command::StopAll);

    let stops: Vec<_> = h
        .runtime
        .intents()
        .iter()
        .copied()
        .filter(|intent| matches!(intent, Intent::Stop { .. }))
        .collect();
    assert_eq!(stops.len(), 2);
    assert_eq!(h.process("api").desired, DesiredState::Stopped);
    assert_eq!(h.process("db").desired, DesiredState::Stopped);
    assert_eq!(h.process("setup").desired, DesiredState::Stopped);
}

#[test]
fn snapshots_do_not_alias_supervisor_state() {
    let h = Harness::new(four_process_project());
    let mut snap = h.snapshot();
    snap.processes[0].name.clear();
    snap.processes[0].desired = DesiredState::Running;

    assert_eq!(h.process("api").name, "api");
    assert_eq!(h.process("api").desired, DesiredState::Stopped);
}

#[test]
fn unknown_commands_are_ignored() {
    let mut h = Harness::new(four_process_project());
    h.command(Command::Start("missing".into()));
    h.command(Command::Stop("missing".into()));
    assert!(h.runtime.intents().is_empty());
}

#[cfg(test)]
mod readiness;

mod one_shot_lifecycle;

mod service_lifecycle;

#[cfg(test)]
mod dependency_scheduling {
    use super::*;

    fn start_intents(h: &Harness) -> Vec<(ProcessId, RunId)> {
        h.runtime
            .intents()
            .iter()
            .filter_map(|intent| match intent {
                Intent::Start { process_id, run_id } => Some((*process_id, *run_id)),
                Intent::Stop { .. } => None,
            })
            .collect()
    }

    #[test]
    fn starting_a_process_schedules_transitive_dependencies_too() {
        let project = EffectiveProject::new(vec![
            depending_on("api", &["db"]),
            depending_on("db", &["worker"]),
            service("worker"),
        ])
        .expect("unique names");
        let mut h = Harness::new(project);
        h.command(Command::Start("api".into()));

        // The whole transitive chain desires Running; the Process without
        // unsatisfied dependencies of its own starts first.
        for name in ["api", "db", "worker"] {
            assert_eq!(h.process(name).desired, DesiredState::Running, "{name}");
        }
        assert_eq!(h.process("worker").current_run, Some(1));
        assert_eq!(
            h.process("db").blocked_reason.as_deref(),
            Some("worker: started")
        );

        // Each satisfied condition releases the next dependent.
        h.event(spawned("worker", 1));
        assert_eq!(h.process("db").current_run, Some(1));
        h.event(spawned("db", 1));
        assert_eq!(h.process("api").current_run, Some(1));
        assert_eq!(start_intents(&h).len(), 3);
    }

    #[test]
    fn a_dependent_waits_while_its_dependency_has_not_spawned() {
        let project = EffectiveProject::new(vec![depending_on("api", &["db"]), service("db")])
            .expect("unique names");
        let mut h = Harness::new(project);
        h.command(Command::Start("api".into()));

        // Only the dependency is scheduled; the dependent waits visibly.
        let api = h.process("api");
        assert_eq!(api.lifecycle, Lifecycle::Waiting);
        assert_eq!(api.blocked_reason.as_deref(), Some("db: started"));
        assert_eq!(api.current_run, None);
        // Only the dependency was scheduled; the dependent is still waiting.
        assert_eq!(
            start_intents(&h),
            vec![(ProcessId::new(process_index("db")), RunId::new(1))]
        );

        // The dependency's active Run satisfies `started` and releases the
        // dependent, whose Run identity comes after the dependency's.
        h.event(spawned("db", 1));
        assert_eq!(
            start_intents(&h),
            vec![
                (ProcessId::new(process_index("db")), RunId::new(1)),
                (ProcessId::new(process_index("api")), RunId::new(1))
            ]
        );
        assert_eq!(h.process("api").current_run, Some(1));
    }

    #[test]
    fn a_stopping_or_ended_dependency_does_not_satisfy_a_new_dependent() {
        let project = EffectiveProject::new(vec![depending_on("api", &["db"]), service("db")])
            .expect("unique names");
        let mut h = Harness::new(project);
        h.command(Command::Start("db".into()));
        // Hold db in its bounded shutdown across the next command.
        h.core.command(Command::Stop("db".into()));
        assert_eq!(h.process("db").lifecycle, Lifecycle::Stopping);

        h.command(Command::Start("api".into()));
        let api = h.process("api");
        assert_eq!(api.lifecycle, Lifecycle::Waiting);
        assert_eq!(api.blocked_reason.as_deref(), Some("db: started"));
        // Only db ever started, twice: its original Run and the automatic
        // restart once the stopped Run ended while it desires Running.
        assert_eq!(
            start_intents(&h),
            vec![
                (ProcessId::new(process_index("db")), RunId::new(1)),
                (ProcessId::new(process_index("db")), RunId::new(2))
            ]
        );
        // db ends, restarts automatically because it desires Running again,
        // and the dependent starts only once the new Run is active.
        h.drain();
        assert_eq!(h.process("db").current_run, Some(2));
        assert_eq!(h.process("api").lifecycle, Lifecycle::Waiting);
        h.event(spawned("db", 2));
        assert_eq!(h.process("api").lifecycle, Lifecycle::Starting);
        assert_eq!(h.process("api").current_run, Some(1));
    }

    #[test]
    fn a_disabled_dependency_blocks_visibly_without_auto_enable() {
        let project = EffectiveProject::new(vec![
            depending_on("api", &["db"]),
            simple("db", ProcessKind::Service, Enabled::No, Autostart::Yes),
        ])
        .expect("unique names");
        let mut h = Harness::new(project);
        h.command(Command::Start("api".into()));
        h.command(Command::StartAutostart);

        let api = h.process("api");
        assert_eq!(api.lifecycle, Lifecycle::Waiting);
        assert_eq!(api.blocked_reason.as_deref(), Some("db: disabled"));
        assert_eq!(api.current_run, None);
        // Disabled stays disabled: no desire, no Run.
        assert_eq!(h.process("db").desired, DesiredState::Stopped);
        assert_eq!(h.process("db").lifecycle, Lifecycle::Idle);
        assert!(h.runtime.intents().is_empty());
    }

    #[test]
    fn a_running_dependent_keeps_running_when_its_dependency_stops() {
        let project = EffectiveProject::new(vec![depending_on("api", &["db"]), service("db")])
            .expect("unique names");
        let mut h = Harness::new(project);
        h.command(Command::Start("api".into()));
        h.event(spawned("db", 1));
        h.event(spawned("api", 1));

        h.command(Command::Stop("db".into()));

        assert_eq!(h.process("db").lifecycle, Lifecycle::Stopped);
        assert_eq!(h.process("api").desired, DesiredState::Running);
        assert_eq!(h.process("api").lifecycle, Lifecycle::Running);
        assert_eq!(h.process("api").current_run, Some(1));
    }

    #[test]
    fn autostart_respects_the_same_recursive_scheduling() {
        let project = EffectiveProject::new(vec![
            simple("cron", ProcessKind::Service, Enabled::Yes, Autostart::No),
            {
                let mut spec = depending_on("api", &["db"]);
                spec.autostart = Autostart::Yes;
                spec
            },
            service("db"),
        ])
        .expect("unique names");
        let mut h = Harness::new(project);
        h.command(Command::StartAutostart);

        assert_eq!(h.process("cron").lifecycle, Lifecycle::Idle);
        for name in ["api", "db"] {
            assert_eq!(h.process(name).desired, DesiredState::Running, "{name}");
        }
        assert_eq!(start_intents(&h).len(), 2);

        // The dependent still starts only through the same condition.
        h.event(spawned("db", 1));
        assert_eq!(h.process("api").current_run, Some(1));
    }
}

#[cfg(test)]
mod threaded {
    use super::*;
    use crate::supervisor::start_with;

    #[test]
    fn the_task_wrapper_serves_snapshots_until_the_handle_drops() {
        let handle = start_with(
            four_process_project(),
            Box::new(FakeRuntime::default()),
            Box::new(FakeProbes::default()),
            Arc::new(FakeClock::new()),
            crate::geometry::TerminalGeometry::DEFAULT,
        );
        // Wait for the initial snapshot through the bounded public request.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let snapshot = loop {
            if let Some(snapshot) = handle.snapshot() {
                break snapshot;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "snapshot was not served in time"
            );
        };
        assert_eq!(snapshot.processes.len(), 4);
        assert_eq!(snapshot.processes[0].name, "api");

        handle.command(Command::StartAutostart);

        // Events reach the same serialized task from adapter threads.
        handle.deliver_event(spawned("api", 1));
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let Some(snapshot) = handle.snapshot() else {
                panic!("snapshot was not served in time");
            };
            if snapshot.named("api").unwrap().lifecycle == Lifecycle::Running {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "event was not applied in time"
            );
        }
        handle.stop_task();
    }
}
