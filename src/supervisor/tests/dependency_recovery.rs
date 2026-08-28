//! Dependency recovery through the Supervisor seam. These tests prove that
//! waiting Processes react to dependency events without another user command
//! or a timer poll.

use super::*;

fn one_shot_depending_on(
    name: &str,
    dependencies: &[&str],
    condition: DependencyCondition,
) -> ProcessSpec {
    let mut spec = simple(name, ProcessKind::OneShot, Enabled::Yes, Autostart::No);
    spec.dependencies = dependencies
        .iter()
        .map(|dependency| crate::model::DependencySpec {
            name: (*dependency).to_string(),
            condition,
        })
        .collect();
    spec
}

fn completed_recovery_project() -> EffectiveProject {
    EffectiveProject::new(vec![
        depending_completed_on("api", &["setup"]),
        service("db"),
        service("worker"),
        simple("setup", ProcessKind::OneShot, Enabled::Yes, Autostart::No),
    ])
    .expect("unique names")
}

fn chain_project() -> EffectiveProject {
    EffectiveProject::new(vec![
        depending_completed_on("api", &["setup"]),
        service("db"),
        simple("worker", ProcessKind::OneShot, Enabled::Yes, Autostart::No),
        one_shot_depending_on(
            "setup",
            &["worker"],
            DependencyCondition::CompletedSuccessfully,
        ),
    ])
    .expect("unique names")
}

fn start_count(h: &Harness, process: &str) -> usize {
    h.runtime
        .intents()
        .iter()
        .filter(|intent| {
            matches!(
                intent,
                Intent::Start { process_id, .. }
                    if *process_id == ProcessId::new(process_index(process))
            )
        })
        .count()
}

#[test]
fn rerunning_a_failed_dependency_releases_an_existing_waiter_after_repeated_failures() {
    let mut h = Harness::new(completed_recovery_project());
    h.command(Command::Start("api".into()));

    assert_eq!(h.process("setup").current_run, Some(1));
    assert_eq!(h.process("api").lifecycle, Lifecycle::Waiting);
    assert_eq!(
        h.process("api").blocked_reason.as_deref(),
        Some("setup: completed_successfully")
    );

    h.event(spawned("setup", 1));
    h.event(finished("setup", 1, Some(7)));
    assert_eq!(h.process("setup").lifecycle, Lifecycle::Stopped);
    assert_eq!(
        h.process("api").blocked_reason.as_deref(),
        Some("setup: completed_successfully (exited with code 7)")
    );

    // Rerun invalidates the failed Run before the replacement finishes.
    h.command(Command::Rerun("setup".into()));
    assert_eq!(h.process("setup").current_run, Some(2));
    assert_eq!(h.process("setup").failure, None);
    assert_eq!(h.process("api").current_run, None);

    h.event(spawned("setup", 2));
    h.event(finished("setup", 2, Some(8)));
    assert_eq!(h.process("api").lifecycle, Lifecycle::Waiting);
    assert_eq!(
        h.process("api").blocked_reason.as_deref(),
        Some("setup: completed_successfully (exited with code 8)")
    );

    // A later successful result reevaluates the existing waiter directly.
    h.command(Command::Rerun("setup".into()));
    h.event(spawned("setup", 3));
    h.event(finished("setup", 3, Some(0)));

    assert_eq!(h.process("setup").lifecycle, Lifecycle::Done);
    assert_eq!(h.process("api").current_run, Some(1));
    assert_eq!(h.process("api").lifecycle, Lifecycle::Starting);
    assert_eq!(start_count(&h, "api"), 1);
}

#[test]
fn stopping_a_waiting_dependent_during_recovery_keeps_its_dependency_scheduled() {
    let mut h = Harness::new(completed_recovery_project());
    h.command(Command::Start("api".into()));
    assert_eq!(h.process("setup").current_run, Some(1));

    h.command(Command::Stop("api".into()));
    let api = h.process("api");
    assert_eq!(api.desired, DesiredState::Stopped);
    assert_eq!(api.lifecycle, Lifecycle::Stopped);
    assert_eq!(api.current_run, None);
    assert_eq!(h.process("setup").current_run, Some(1));
    assert!(!h.runtime.intents().iter().any(|intent| {
        matches!(intent, Intent::Stop { process_id, .. }
            if *process_id == ProcessId::new(process_index("setup")))
    }));

    h.event(spawned("setup", 1));
    h.event(finished("setup", 1, Some(0)));

    assert_eq!(h.process("setup").lifecycle, Lifecycle::Done);
    assert_eq!(h.process("api").lifecycle, Lifecycle::Stopped);
    assert_eq!(h.process("api").current_run, None);
    assert_eq!(start_count(&h, "api"), 0);
}

#[test]
fn repeated_dependency_recovery_unblocks_a_chain_in_dependency_order() {
    let mut h = Harness::new(chain_project());
    h.command(Command::Start("api".into()));

    assert_eq!(h.process("worker").current_run, Some(1));
    assert_eq!(h.process("setup").lifecycle, Lifecycle::Waiting);
    assert_eq!(h.process("api").lifecycle, Lifecycle::Waiting);

    h.event(spawned("worker", 1));
    h.event(finished("worker", 1, Some(7)));
    assert_eq!(h.process("setup").lifecycle, Lifecycle::Waiting);
    assert_eq!(
        h.process("setup").blocked_reason.as_deref(),
        Some("worker: completed_successfully (exited with code 7)")
    );

    h.command(Command::Rerun("worker".into()));
    h.event(spawned("worker", 2));
    h.event(finished("worker", 2, Some(8)));
    assert_eq!(h.process("setup").lifecycle, Lifecycle::Waiting);
    assert_eq!(
        h.process("setup").blocked_reason.as_deref(),
        Some("worker: completed_successfully (exited with code 8)")
    );

    h.command(Command::Rerun("worker".into()));
    h.event(spawned("worker", 3));
    h.event(finished("worker", 3, Some(0)));
    assert_eq!(h.process("setup").current_run, Some(1));
    assert_eq!(h.process("api").current_run, None);

    h.event(spawned("setup", 1));
    h.event(finished("setup", 1, Some(0)));
    assert_eq!(h.process("api").current_run, Some(1));

    let starts: Vec<_> = h
        .runtime
        .intents()
        .iter()
        .filter_map(|intent| match intent {
            Intent::Start { process_id, .. } => Some(*process_id),
            Intent::Stop { .. } | Intent::Cancel { .. } => None,
        })
        .collect();
    assert_eq!(
        starts,
        vec![
            ProcessId::new(process_index("worker")),
            ProcessId::new(process_index("worker")),
            ProcessId::new(process_index("worker")),
            ProcessId::new(process_index("setup")),
            ProcessId::new(process_index("api")),
        ]
    );
}

#[test]
fn restarting_a_service_dependency_does_not_rerun_a_completed_dependent_one_shot() {
    let project = EffectiveProject::new(vec![
        service("api"),
        service("db"),
        service("worker"),
        one_shot_depending_on("setup", &["db"], DependencyCondition::Started),
    ])
    .expect("unique names");
    let mut h = Harness::new(project);

    h.command(Command::Start("setup".into()));
    h.event(spawned("db", 1));
    h.event(spawned("setup", 1));
    h.event(finished("setup", 1, Some(0)));
    assert_eq!(h.process("setup").lifecycle, Lifecycle::Done);

    h.command(Command::Restart("db".into()));

    assert_eq!(h.process("db").current_run, Some(2));
    assert_eq!(h.process("setup").lifecycle, Lifecycle::Done);
    assert_eq!(h.process("setup").current_run, None);
    assert_eq!(start_count(&h, "setup"), 1);
}
