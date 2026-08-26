//! Supervisor behavior tests. They drive the highest seam only: semantic
//! commands, typed seam events, observable runtime intent, and immutable
//! snapshots — never internal state-machine fields.

use std::sync::Arc;
use std::time::Duration;

use super::ProjectSnapshot;
use super::support::{FakeClock, FakeRuntime, Intent};
use super::{Command, Core, DesiredState, Lifecycle, ProcessSnapshot, SeamEvent, SeamSender};
use crate::model::{
    Autostart, CommandForm, EffectiveProject, Enabled, InputPolicy, ProcessKind, ProcessSpec,
    TerminalMode,
};
use crate::runtime::{ProcessId, RunId};

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
    }
}

struct Harness {
    core: Core,
    runtime: Arc<FakeRuntime>,
    emitted: crossbeam_channel::Receiver<SeamEvent>,
}

impl Harness {
    fn new(project: EffectiveProject) -> Self {
        Self::with(project, Arc::new(FakeRuntime::default()))
    }

    fn with(project: EffectiveProject, runtime: Arc<FakeRuntime>) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let clock = Box::new(FakeClock::new());
        let core = Core::new(
            project,
            Box::new(Arc::clone(&runtime)),
            clock,
            SeamSender::new(tx),
            crate::geometry::TerminalGeometry::DEFAULT,
        );
        Self {
            core,
            runtime,
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
    assert_eq!(h.process("api").current_run, None);
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
mod threaded {
    use super::*;
    use crate::supervisor::start_with;

    #[test]
    fn the_task_wrapper_serves_snapshots_until_the_handle_drops() {
        let handle = start_with(
            four_process_project(),
            Box::new(FakeRuntime::default()),
            Box::new(FakeClock::new()),
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
