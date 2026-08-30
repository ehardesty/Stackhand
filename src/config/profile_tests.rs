use std::fs;

use super::*;

fn write_and_load(
    label: &str,
    yaml: &str,
    profile: Option<&str>,
) -> Result<EffectiveProject, ConfigError> {
    let dir = std::env::temp_dir().join(format!("stackhand-config-{label}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("config directory creates");
    let path = dir.join("stackhand.yaml");
    fs::write(&path, yaml).expect("config writes");
    let project = load_file(&path, profile);
    let _ = fs::remove_dir_all(&dir);
    project
}

#[test]
fn one_process_profile_is_selected_and_missing_names_fall_back_to_base() {
    let mut project = write_and_load(
        "process-profile-selection",
        "version: 1
processes:
  api:
    command: [/bin/echo, base-api]
    profiles:
      local:
        command: [/bin/echo, local-api]
      cloud:
        command: [/bin/echo, cloud-api]
  worker:
    command: [/bin/echo, base-worker]
    profiles:
      local:
        command: [/bin/echo, local-worker]
",
        Some("cloud"),
    )
    .expect("all Process Profiles are valid");

    assert_eq!(project.process_profile_names(), ["cloud", "local"]);
    assert_eq!(project.selected_process_profile(), Some("cloud"));
    assert_eq!(project.process_profile(0), Some("cloud"));
    assert_eq!(project.process_profile(1), None);
    assert_eq!(direct_args(&project.processes()[0]), ["cloud-api"]);
    assert_eq!(direct_args(&project.processes()[1]), ["base-worker"]);

    assert!(project.select_process_profile(Some("local")));
    assert_eq!(project.process_profile(0), Some("local"));
    assert_eq!(project.process_profile(1), Some("local"));
    assert_eq!(direct_args(&project.processes()[0]), ["local-api"]);
    assert_eq!(direct_args(&project.processes()[1]), ["local-worker"]);

    assert!(!project.select_process_profile(Some("missing")));
    assert_eq!(project.selected_process_profile(), Some("local"));
}

#[test]
fn a_process_profile_override_wins_and_base_is_reserved_for_the_base_spec() {
    let mut project = write_and_load(
        "process-profile-overrides",
        "version: 1
processes:
  pinned:
    profile: local
    command: [/bin/echo, base-pinned]
    profiles:
      local:
        command: [/bin/echo, local-pinned]
      cloud:
        command: [/bin/echo, cloud-pinned]
  base-pinned:
    profile: base
    command: [/bin/echo, base-fixed]
    profiles:
      local:
        command: [/bin/echo, local-fixed]
",
        Some("cloud"),
    )
    .expect("Process Profile overrides are valid");

    assert_eq!(project.process_profile(0), Some("local"));
    assert_eq!(project.process_profile(1), None);
    assert_eq!(direct_args(&project.processes()[0]), ["local-pinned"]);
    assert_eq!(direct_args(&project.processes()[1]), ["base-fixed"]);

    assert!(project.select_process_profile(None));
    assert_eq!(project.process_profile(0), Some("local"));
    assert_eq!(project.process_profile(1), None);
}

#[test]
fn enabled_defaults_to_true_and_a_process_profile_can_disable_a_process() {
    let project = write_and_load(
        "process-profile-enabled",
        "version: 1
processes:
  api:
    command: [/usr/bin/true]
    profiles:
      cloud:
        environment:
          MODE: cloud
  local-storage:
    command: [/usr/bin/true]
    profiles:
      cloud:
        enabled: false
",
        Some("cloud"),
    )
    .expect("enabled is an allowed Process Profile field");

    assert_eq!(project.processes()[0].enabled, Enabled::Yes);
    assert_eq!(project.processes()[1].enabled, Enabled::No);
}

#[test]
fn a_process_profile_replaces_the_complete_dependency_mapping() {
    let mut project = write_and_load(
        "process-profile-dependencies",
        "version: 1
processes:
  api:
    depends_on:
      local-db: ready
    command: [/usr/bin/true]
    profiles:
      cloud:
        depends_on:
          cloud-login: completed_successfully
  local-db:
    kind: service
    command: [/usr/bin/true]
  cloud-login:
    kind: one-shot
    command: [/usr/bin/true]
",
        None,
    )
    .expect("every selectable Dependency graph is valid");

    let base_dependencies = project
        .resolved_dependencies(0)
        .map(|(_, dependency)| dependency.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(base_dependencies, ["local-db"]);

    assert!(project.select_process_profile(Some("cloud")));
    let cloud_dependencies = project
        .resolved_dependencies(0)
        .map(|(_, dependency)| dependency.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(cloud_dependencies, ["cloud-login"]);
}

#[test]
fn every_selectable_profile_dependency_graph_is_validated() {
    let error = write_and_load(
        "process-profile-dependency-cycle",
        "version: 1
processes:
  api:
    command: [/usr/bin/true]
    profiles:
      cloud:
        depends_on: {worker: started}
  worker:
    command: [/usr/bin/true]
    profiles:
      cloud:
        depends_on: {api: started}
",
        None,
    )
    .expect_err("an invalid unselected profile must fail before startup");

    assert!(
        error
            .message
            .contains("Process Profile 'cloud' produces an invalid Project")
            && error.message.contains("api -> worker -> api"),
        "{error}"
    );
}

#[test]
fn process_profiles_reject_reserved_names_forbidden_fields_and_invalid_specs() {
    for (label, profile, expected) in [
        (
            "base-name",
            "base:\n        command: [/usr/bin/true]",
            "reserved",
        ),
        (
            "topology",
            "local:\n        autostart: false",
            "cannot change field 'autostart'",
        ),
        (
            "invalid-command",
            "local:\n        command: []",
            "command must contain",
        ),
    ] {
        let error = write_and_load(
            &format!("process-profile-{label}"),
            &format!(
                "version: 1\nprocesses:\n  api:\n    command: [/usr/bin/true]\n    profiles:\n      {profile}\n"
            ),
            None,
        )
        .expect_err("every invalid Process Profile must fail before startup");
        assert!(error.message.contains(expected), "{label}: {error}");
    }
}

#[test]
fn top_level_profiles_are_rejected() {
    let error = write_and_load(
        "top-level-profile",
        "version: 1
processes:
  api:
    command: [/usr/bin/true]
profiles:
  cloud: {}
",
        None,
    )
    .expect_err("top-level profiles are not part of the schema");

    assert!(
        error.message.contains("unknown field `profiles`"),
        "{error}"
    );
}

#[test]
fn an_unknown_selected_process_profile_is_rejected() {
    let error = write_and_load(
        "unknown-process-profile",
        "version: 1
processes:
  api:
    command: [/usr/bin/true]
    profiles:
      local: {}
",
        Some("missing"),
    )
    .expect_err("the selected Process Profile must exist on at least one Process");

    assert!(
        error.message.contains("unknown Process Profile 'missing'"),
        "{error}"
    );
}

fn direct_args(process: &ProcessSpec) -> Vec<&str> {
    let CommandForm::Direct { args, .. } = &process.command else {
        panic!("the test Process uses a direct command");
    };
    args.iter()
        .map(|arg| arg.to_str().expect("UTF-8 test arg"))
        .collect()
}
