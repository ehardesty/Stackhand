use std::fs;

use super::*;

fn write_and_load_with_profile(
    label: &str,
    yaml: &str,
    profile: Option<&str>,
) -> Result<EffectiveProject, ConfigError> {
    let profiles = profile.into_iter().collect::<Vec<_>>();
    write_and_load_with_profiles(label, yaml, &profiles)
}

fn write_and_load_with_profiles(
    label: &str,
    yaml: &str,
    profiles: &[&str],
) -> Result<EffectiveProject, ConfigError> {
    let dir = std::env::temp_dir().join(format!("stackhand-config-{label}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("config directory creates");
    let path = dir.join("stackhand.yaml");
    fs::write(&path, yaml).expect("config writes");
    let selected = profiles
        .iter()
        .map(|profile| (*profile).to_owned())
        .collect::<Vec<_>>();
    let project = load_file(&path, &selected);
    let _ = fs::remove_dir_all(&dir);
    project
}

#[test]
fn one_profile_replaces_fields_enables_processes_and_adds_processes() {
    let project = write_and_load_with_profile(
        "profile-merge",
        "version: 1
processes:
  web:
    enabled: false
    autostart: false
    command: [/bin/true]
  worker:
    command: [/bin/true]
profiles:
  local:
    enable: [web]
    disable: [worker]
    overrides:
      web:
        environment: {MODE: local}
        command: [/bin/echo, profile]
      added:
        kind: one-shot
        autostart: false
        command: [/bin/true]
",
        Some("local"),
    )
    .expect("the selected profile is valid");

    let names = project
        .processes()
        .iter()
        .map(|process| process.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["web", "worker", "added"]);

    let web = &project.processes()[0];
    assert_eq!(web.enabled, Enabled::Yes);
    assert_eq!(web.autostart, Autostart::No);
    assert_eq!(project.processes()[1].enabled, Enabled::No);
    assert_eq!(project.processes()[1].autostart, Autostart::Yes);
    assert_eq!(web.env, [("MODE".to_string(), "local".to_string())]);
    assert_eq!(
        web.command,
        CommandForm::Direct {
            program: std::ffi::OsString::from("/bin/echo"),
            args: vec![std::ffi::OsString::from("profile")],
        }
    );
    assert_eq!(project.processes()[2].kind, ProcessKind::OneShot);
    assert_eq!(project.processes()[2].autostart, Autostart::No);
}

#[test]
fn a_profile_can_replace_project_shell_settings() {
    let project = write_and_load_with_profile(
        "profile-settings",
        "version: 1
processes:
  web:
    shell: printf profile-shell
profiles:
  local:
    settings:
      shell:
        program: /bin/bash
        args: [-lc]
",
        Some("local"),
    )
    .expect("profile settings are valid");
    assert_eq!(
        project.shell().program,
        std::ffi::OsString::from("/bin/bash")
    );
    assert_eq!(project.shell().args, [std::ffi::OsString::from("-lc")]);
}

#[test]
fn profile_selection_is_explicit_and_unknown_names_fail() {
    let yaml = "version: 1
processes:
  web:
    command: [/bin/true]
profiles:
  local:
    overrides:
      added: {}
";
    let base = write_and_load_with_profile("profile-no-implicit-selection", yaml, None)
        .expect("the base configuration does not select a profile");
    assert_eq!(base.processes().len(), 1);

    let error = write_and_load_with_profile("profile-unknown", yaml, Some("missing"))
        .expect_err("an unknown profile must fail");
    assert!(
        error.message.contains("unknown profile 'missing'"),
        "{error}"
    );

    let error = write_and_load_with_profile("profile-invalid-added-process", yaml, Some("local"))
        .expect_err("an incomplete profile Process must fail validation");
    assert!(
        error.message.contains("Process 'added'")
            && error
                .message
                .contains("exactly one of 'command' or 'shell'"),
        "{error}"
    );
}

#[test]
fn profiles_cannot_define_profiles_or_change_the_schema_version() {
    for (label, field) in [("nested", "profiles: {}"), ("version", "version: 2")] {
        let error = write_and_load_with_profile(
            &format!("profile-forbidden-{label}"),
            &format!("version: 1\nprocesses: {{}}\nprofiles:\n  local:\n    {field}\n"),
            Some("local"),
        )
        .expect_err("forbidden profile field must fail");
        assert!(
            error.message.contains("unknown field")
                && error.message.contains(field.split(':').next().unwrap()),
            "{label}: {error}"
        );
    }
}

#[test]
fn ordered_profiles_deep_merge_maps_and_replace_scalars_and_lists() {
    let yaml = "version: 1
settings:
  shell:
    program: /bin/sh
    args: [-c]
processes:
  web:
    enabled: true
    command: [/bin/echo, base]
    environment:
      BASE: base
    success_exit_codes: [0, 1]
profiles:
  first:
    disable: [web]
    settings:
      shell:
        args: [-lc]
    overrides:
      web:
        command: [/bin/echo, first]
        environment:
          FIRST: first
        success_exit_codes: [2, 3]
      first-added:
        command: [/bin/true]
  second:
    enable: [web]
    settings:
      shell:
        program: /bin/bash
    overrides:
      web:
        command: [/bin/echo, second]
        environment:
          SECOND: second
        success_exit_codes: [7]
      second-added:
        command: [/bin/true]
";

    let forward =
        write_and_load_with_profiles("profile-ordered-forward", yaml, &["first", "second"])
            .expect("profiles merge in CLI order");
    let names = forward
        .processes()
        .iter()
        .map(|process| process.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["web", "first-added", "second-added"]);
    let web = &forward.processes()[0];
    assert_eq!(web.enabled, Enabled::Yes);
    assert_eq!(web.success_exit_codes, [7]);
    assert_eq!(
        web.command,
        CommandForm::Direct {
            program: std::ffi::OsString::from("/bin/echo"),
            args: vec![std::ffi::OsString::from("second")],
        }
    );
    assert_eq!(
        web.env,
        [
            ("BASE".to_string(), "base".to_string()),
            ("FIRST".to_string(), "first".to_string()),
            ("SECOND".to_string(), "second".to_string()),
        ]
    );
    assert_eq!(
        forward.shell().program,
        std::ffi::OsString::from("/bin/bash")
    );
    assert_eq!(forward.shell().args, [std::ffi::OsString::from("-lc")]);

    let reverse =
        write_and_load_with_profiles("profile-ordered-reverse", yaml, &["second", "first"])
            .expect("the reverse profile order is also valid");
    assert_eq!(reverse.processes()[0].enabled, Enabled::No);
    assert_eq!(reverse.processes()[0].success_exit_codes, [2, 3]);
    assert_eq!(
        reverse.processes()[0].command,
        CommandForm::Direct {
            program: std::ffi::OsString::from("/bin/echo"),
            args: vec![std::ffi::OsString::from("first")],
        }
    );
}

#[test]
fn null_clears_optional_fields_and_named_map_entries() {
    let project = write_and_load_with_profiles(
        "profile-null-clears",
        "version: 1
processes:
  db:
    kind: one-shot
    command: [/bin/true]
  web:
    command: [/bin/true]
    environment:
      KEEP: keep
      REMOVE: remove
    depends_on:
      db: started
    terminal: {mode: pty, input: focused}
    ready:
      tcp: {host: 127.0.0.1, port: 5432}
    liveness:
      log: {contains: healthy}
    restart: {policy: on_failure, backoff: 1s, max_restarts: 2}
profiles:
  clean:
    overrides:
      web:
        environment:
          REMOVE: null
          ADD: add
        depends_on:
          db: null
        terminal: null
        ready: null
        liveness: null
        restart: null
",
        &["clean"],
    )
    .expect("null values clear optional configuration");
    let web = &project.processes()[1];
    assert_eq!(
        web.env,
        [
            ("ADD".to_string(), "add".to_string()),
            ("KEEP".to_string(), "keep".to_string()),
        ]
    );
    assert!(web.dependencies.is_empty());
    assert!(web.readiness.is_none());
    assert!(web.liveness.is_none());
    assert_eq!(web.restart, RestartConfig::default());
    assert_eq!(web.terminal_mode, TerminalMode::Pipe);
    assert_eq!(web.input_policy, InputPolicy::Disabled);
}

#[test]
fn base_environment_nulls_remove_inherited_environment_values() {
    let project = write_and_load_with_profiles(
        "base-null-environment",
        "version: 1
processes:
  web:
    command: [/bin/true]
    environment:
      MISSING: null
profiles:
  clean:
    overrides:
      web:
        environment:
          MISSING: null
",
        &[],
    )
    .expect("base environment nulls are removal instructions");
    assert!(project.processes()[0].env.is_empty());
    assert_eq!(project.processes()[0].env_remove, ["MISSING"]);
}

#[test]
fn null_cannot_define_a_profile_process_or_required_command() {
    let complete_process = write_and_load_with_profiles(
        "profile-null-process",
        "version: 1
processes:
  web:
    command: [/bin/true]
profiles:
  bad:
    overrides:
      added: null
",
        &["bad"],
    )
    .expect_err("a null profile Process is not complete");
    assert!(
        complete_process
            .message
            .contains("Process 'added' must define a complete Process"),
        "{complete_process}"
    );

    let required_field = write_and_load_with_profiles(
        "profile-null-command",
        "version: 1
processes:
  web:
    command: [/bin/true]
profiles:
  bad:
    overrides:
      web:
        command: null
",
        &["bad"],
    )
    .expect_err("a required command form cannot be cleared");
    assert!(
        required_field
            .message
            .contains("Process 'web': define exactly one of 'command' or 'shell'"),
        "{required_field}"
    );
}

#[test]
fn dependencies_are_checked_after_profile_merge_and_enablement_does_not_remove_them() {
    let repaired = write_and_load_with_profiles(
        "profile-graph-repair",
        "version: 1
processes:
  web:
    command: [/bin/true]
    depends_on: {cache: started}
profiles:
  repair:
    overrides:
      cache:
        command: [/bin/true]
",
        &["repair"],
    )
    .expect("a profile can add a missing dependency before graph validation");
    assert_eq!(repaired.processes().len(), 2);
    assert_eq!(repaired.processes()[0].dependencies[0].name, "cache");

    let base_error = write_and_load_with_profiles(
        "profile-graph-base-error",
        "version: 1
processes:
  web:
    command: [/bin/true]
    depends_on: {cache: started}
profiles:
  repair:
    overrides:
      cache:
        command: [/bin/true]
",
        &[],
    )
    .expect_err("the unmerged graph must fail");
    assert!(
        base_error.message.contains("dependency 'cache'"),
        "{base_error}"
    );

    let disabled = write_and_load_with_profiles(
        "profile-graph-disabled",
        "version: 1
processes:
  cache:
    command: [/bin/true]
  web:
    command: [/bin/true]
    depends_on: {cache: started}
profiles:
  no-cache:
    disable: [cache]
",
        &["no-cache"],
    )
    .expect("a disabled Process remains available to Dependencies");
    assert_eq!(disabled.processes()[0].enabled, Enabled::No);
    assert_eq!(disabled.processes()[1].dependencies[0].name, "cache");
}

#[test]
fn profile_changes_can_repair_kind_checks_and_create_cycles() {
    let repaired_kind = write_and_load_with_profiles(
        "profile-kind-repair",
        "version: 1
processes:
  setup:
    command: [/bin/true]
    depends_on: {db: completed_successfully}
  db:
    command: [/bin/true]
profiles:
  repair:
    overrides:
      db:
        kind: one-shot
",
        &["repair"],
    )
    .expect("kind validation uses the merged Process definitions");
    assert_eq!(repaired_kind.processes()[1].kind, ProcessKind::OneShot);

    let cycle = write_and_load_with_profiles(
        "profile-cycle",
        "version: 1
processes:
  first:
    command: [/bin/true]
  second:
    command: [/bin/true]
profiles:
  loop:
    overrides:
      first:
        depends_on: {second: started}
      second:
        depends_on: {first: started}
",
        &["loop"],
    )
    .expect_err("cycle validation uses the merged Dependency graph");
    assert!(cycle.message.contains("dependency cycle"), "{cycle}");

    let broken = write_and_load_with_profiles(
        "profile-graph-break",
        "version: 1
processes:
  db:
    command: [/bin/true]
  web:
    command: [/bin/true]
    depends_on: {db: started}
profiles:
  break:
    overrides:
      web:
        depends_on:
          db: null
          missing: started
",
        &["break"],
    )
    .expect_err("graph validation must run after null Dependency removal");
    assert!(broken.message.contains("dependency 'missing'"), "{broken}");
}
