use std::fs;

use super::*;

fn write_and_load_with_profile(
    label: &str,
    yaml: &str,
    profile: Option<&str>,
) -> Result<EffectiveProject, ConfigError> {
    let dir = std::env::temp_dir().join(format!("stackhand-config-{label}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("config directory creates");
    let path = dir.join("stackhand.yaml");
    fs::write(&path, yaml).expect("config writes");
    let project = match profile {
        Some(profile) => load_file(&path, Some(profile)),
        None => load(&path),
    };
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
