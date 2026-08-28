//! Supervisor behavior tests. They drive the highest seam only: semantic
//! commands, typed seam events, observable runtime intent, and immutable
//! snapshots — never internal state-machine fields.

use std::sync::Arc;
use std::time::Duration;

use super::ProjectSnapshot;
use super::seam::{AttemptId, FinishedRun, WorkId};
use super::support::{FakeClock, FakeProbes, FakeRuntime, Intent};
use super::{
    Command, Core, DesiredState, FailureKind, Lifecycle, ProcessSnapshot, ReadinessCheckKind,
    ReadinessState, RunExitDisposition, SeamEvent, SeamSender,
};
use crate::model::{
    Autostart, CommandForm, DependencyCondition, EffectiveProject, Enabled, InputPolicy,
    ProcessKind, ProcessSpec, ReadinessCheck, ReadinessConfig, ReadinessProbe, ShellConfig,
    TerminalMode,
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
        success_exit_codes: vec![0],
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
        checks: vec![ReadinessCheck {
            probe: ReadinessProbe::Tcp {
                host: "127.0.0.1".into(),
                port: 1,
            },
            initial_delay: Duration::ZERO,
            interval: Duration::from_secs(1),
            timeout: Duration::from_millis(100),
            success_threshold: 1,
            failure_threshold: 1,
        }],
        startup_timeout: None,
    });
    spec
}

fn configured_readiness_project(
    initial_delay: Duration,
    success_threshold: u32,
    failure_threshold: u32,
) -> EffectiveProject {
    let mut process = probed_service("api");
    let readiness = process.readiness.as_mut().expect("the probe exists");
    let check = &mut readiness.checks[0];
    check.initial_delay = initial_delay;
    check.success_threshold = success_threshold;
    check.failure_threshold = failure_threshold;
    EffectiveProject::new(vec![process]).expect("unique names")
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

/// A Process that starts after a One-shot ends, regardless of its exit result.
fn depending_exited_on(name: &str, dependencies: &[&str]) -> ProcessSpec {
    let mut spec = depending_on(name, dependencies);
    for dependency in &mut spec.dependencies {
        dependency.condition = DependencyCondition::Exited;
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

    /// Start the runtime's spawn reporting: from now on every start sends
    /// its `Spawned` report straight through the core's event path, and
    /// `command`'s drain delivers it before the test reads the next
    /// snapshot.
    fn report_spawns(&mut self) {
        self.runtime.set_report_spawn(true);
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

fn start_probed(h: &mut Harness) {
    h.command(Command::Start("api".into()));
    h.event(spawned("api", 1));
}

fn spawned(process: &str, run: u64) -> SeamEvent {
    SeamEvent::Spawned {
        process_id: ProcessId::new(process_index(process)),
        run_id: RunId::new(run),
        root_pid: None,
    }
}

fn finished(process: &str, run: u64, code: Option<i32>) -> SeamEvent {
    SeamEvent::Finished(FinishedRun {
        process_id: ProcessId::new(process_index(process)),
        run_id: RunId::new(run),
        exit_code: code,
        intentional_stop: false,
        cleanup_confirmed: true,
        detail: None,
        remaining_pids: Vec::new(),
    })
}

fn readiness(process: &str, run: u64, passing: bool, diagnostic: Option<String>) -> SeamEvent {
    readiness_attempt(process, run, 1, passing, diagnostic)
}

fn readiness_attempt(
    process: &str,
    run: u64,
    attempt: u64,
    passing: bool,
    diagnostic: Option<String>,
) -> SeamEvent {
    SeamEvent::Readiness {
        process_id: ProcessId::new(process_index(process)),
        run_id: RunId::new(run),
        work_id: WorkId::new(run),
        attempt_id: AttemptId::new(attempt),
        passing,
        diagnostic,
    }
}

#[test]
fn shell_intent_uses_the_project_launcher_and_appends_the_expression() {
    let mut process = simple("api", ProcessKind::Service, Enabled::Yes, Autostart::No);
    process.command = CommandForm::Shell {
        text: "printf shell-ok".to_string(),
    };
    let project = EffectiveProject::with_shell(
        vec![process],
        ShellConfig {
            program: "/bin/fish".into(),
            args: vec!["--private", "-c"]
                .into_iter()
                .map(Into::into)
                .collect(),
        },
    )
    .expect("the shell test project is valid");
    let harness = Harness::new(project);
    let intent = harness.core.build_intent(0, RunId::new(1));

    assert_eq!(intent.program, std::ffi::OsString::from("/bin/fish"));
    assert_eq!(
        intent.args,
        [
            std::ffi::OsString::from("--private"),
            std::ffi::OsString::from("-c"),
            std::ffi::OsString::from("printf shell-ok"),
        ]
    );
}

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
fn snapshots_carry_each_process_stable_session_identity() {
    let snapshot = Harness::new(four_process_project()).snapshot();

    assert_eq!(
        snapshot
            .processes
            .iter()
            .map(|process| process.process_id)
            .collect::<Vec<_>>(),
        vec![
            ProcessId::new(0),
            ProcessId::new(1),
            ProcessId::new(2),
            ProcessId::new(3),
        ]
    );
    assert_eq!(
        snapshot.named("worker").map(|process| process.process_id),
        Some(ProcessId::new(2))
    );
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
        [Intent::Stop {
            process_id,
            run_id,
            ..
        }]
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
    h.event(finished("api", 1, Some(3)));

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
    h.event(finished("api", 0, Some(9)));
    h.event(finished("db", 1, Some(9)));
    h.event(SeamEvent::Metrics {
        process_id: ProcessId::new(process_index("api")),
        run_id: RunId::new(999),
        cpu_percent: 99.0,
        rss_kib: 1 << 20,
    });
    h.event(SeamEvent::Finished(FinishedRun {
        process_id: ProcessId::new(99),
        run_id: RunId::new(1),
        exit_code: None,
        intentional_stop: false,
        cleanup_confirmed: true,
        detail: None,
        remaining_pids: Vec::new(),
    }));

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
#[cfg(test)]
mod startup_timeout;

mod diagnostics;
mod one_shot_lifecycle;
mod one_shot_rerun;
mod project_shutdown;

mod service_lifecycle;

#[cfg(test)]
mod dependency_scheduling;

#[cfg(test)]
mod threaded;
