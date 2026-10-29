use super::*;

fn bare(name: &str) -> ProcessSpec {
    ProcessSpec {
        name: name.to_string(),
        kind: ProcessKind::Service,
        enabled: Enabled::Yes,
        autostart: Autostart::No,
        success_exit_codes: vec![0],
        restart: RestartConfig::default(),
        command: CommandForm::Direct {
            program: "sleep".into(),
            args: vec!["1".into()],
        },
        working_dir: std::env::temp_dir(),
        env: Vec::new(),
        env_remove: Vec::new(),
        terminal_mode: TerminalMode::Pipe,
        input_policy: InputPolicy::Disabled,
        dependencies: Vec::new(),
        readiness: None,
        liveness: None,
    }
}

fn depending_on(name: &str, dependencies: &[&str]) -> ProcessSpec {
    let mut spec = bare(name);
    spec.dependencies = dependencies
        .iter()
        .map(|dependency| DependencySpec {
            name: (*dependency).to_string(),
            condition: DependencyCondition::Started,
        })
        .collect();
    spec
}

#[test]
fn startup_process_list_row_count_includes_group_headings() {
    let mut project = EffectiveProject::new(vec![
        bare("database"),
        bare("cache"),
        bare("api"),
        bare("worker"),
    ])
    .expect("the Project is valid");
    project.process_groups = vec![
        Some("Infrastructure".to_string()),
        Some("Infrastructure".to_string()),
        Some("Application".to_string()),
        None,
    ];

    assert_eq!(project.process_list_row_count(), 7);
}

#[test]
fn unknown_dependency_references_are_rejected() {
    let error = EffectiveProject::new(vec![depending_on("api", &["db"]), bare("worker")])
        .expect_err("a missing reference must fail");
    assert_eq!(
        error,
        ProjectError::UnknownDependency {
            process: "api".into(),
            dependency: "db".into()
        }
    );
}

#[test]
fn dependency_cycles_are_rejected_with_the_cycle_path() {
    let error = EffectiveProject::new(vec![
        depending_on("api", &["db"]),
        depending_on("db", &["worker"]),
        depending_on("worker", &["api"]),
    ])
    .expect_err("a cycle must fail");
    assert_eq!(
        error,
        ProjectError::DependencyCycle(vec![
            "api".into(),
            "db".into(),
            "worker".into(),
            "api".into()
        ])
    );
}

#[test]
fn a_self_dependency_is_a_cycle() {
    let error = EffectiveProject::new(vec![depending_on("api", &["api"])])
        .expect_err("a self-dependency must fail");
    assert_eq!(
        error,
        ProjectError::DependencyCycle(vec!["api".into(), "api".into()])
    );
}

#[test]
fn completed_successfully_requires_a_one_shot_dependency() {
    let mut setup = bare("setup");
    setup.kind = ProcessKind::OneShot;
    let mut api = depending_on("api", &["setup"]);
    api.dependencies[0].condition = DependencyCondition::CompletedSuccessfully;

    EffectiveProject::new(vec![api.clone(), setup])
        .expect("a One-shot dependency supports completed_successfully");

    let error = EffectiveProject::new(vec![api, bare("setup")])
        .expect_err("a Service dependency must reject completed_successfully");
    assert_eq!(
        error,
        ProjectError::InvalidCondition {
            process: "api".into(),
            dependency: "setup".into(),
            condition: "completed_successfully".into(),
        }
    );
}

#[test]
fn acyclic_shared_dependencies_are_accepted() {
    let project = EffectiveProject::new(vec![
        depending_on("api", &["db"]),
        depending_on("worker", &["db"]),
        bare("db"),
    ])
    .expect("shared dependencies are valid");
    assert_eq!(project.processes().len(), 3);
}

#[test]
fn resolved_dependencies_keep_configuration_order_and_public_names() {
    let project = EffectiveProject::new(vec![
        depending_on("api", &["cache", "db"]),
        bare("db"),
        bare("cache"),
    ])
    .expect("the graph is valid");

    let dependencies = project
        .resolved_dependencies(project.process_index("api").expect("api is configured"))
        .map(|(index, dependency)| (index, dependency.name.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(dependencies, [(2, "cache"), (1, "db")]);
    assert_eq!(project.processes()[0].name, "api");
}
