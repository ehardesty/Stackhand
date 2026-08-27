use super::*;

fn start_intents(h: &Harness) -> Vec<(ProcessId, RunId)> {
    h.runtime
        .intents()
        .iter()
        .filter_map(|intent| match intent {
            Intent::Start { process_id, run_id } => Some((*process_id, *run_id)),
            Intent::Stop { .. } | Intent::Cancel { .. } => None,
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
