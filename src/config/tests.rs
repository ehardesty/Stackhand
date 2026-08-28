use super::*;
use std::fs;
use std::time::Duration;

fn write_and_load(label: &str, yaml: &str) -> Result<EffectiveProject, ConfigError> {
    let dir = std::env::temp_dir().join(format!("stackhand-config-{label}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("config directory creates");
    let path = dir.join("stackhand.yaml");
    fs::write(&path, yaml).expect("config writes");
    let project = load(&path);
    let _ = fs::remove_dir_all(&dir);
    project
}

#[test]
fn checked_in_example_projects_load() {
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut paths = fs::read_dir(&examples)
        .expect("the examples directory exists")
        .map(|entry| entry.expect("an example directory entry reads").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "yaml")
        })
        .collect::<Vec<_>>();
    paths.sort();
    assert_eq!(paths.len(), 5, "every documented example is checked");

    for path in paths {
        let project = load(&path).unwrap_or_else(|error| {
            panic!("example Project '{}' must load: {error}", path.display())
        });
        for process in project.processes() {
            assert!(
                matches!(process.command, CommandForm::Direct { .. }),
                "example Process '{}:{}' must not depend on the user's login-shell syntax",
                path.display(),
                process.name
            );
        }
    }
}

#[test]
fn explicit_resolution_returns_the_selected_base_source() {
    let dir = std::env::temp_dir().join("stackhand-config-resolution-source");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("resolution directory creates");
    let path = dir.join("stackhand.yaml");
    fs::write(
        &path,
        "version: 1\nprocesses:\n  - name: web\n    command: {program: /bin/true}\n",
    )
    .expect("resolution config writes");

    let resolution = resolve(ResolutionRequest::explicit(&path)).expect("resolution succeeds");
    assert_eq!(resolution.sources.base, path);
    assert_eq!(resolution.project().processes().len(), 1);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn one_direct_command_service_loads_with_defaults() {
    let project = write_and_load(
            "ok",
            "version: 1\nprocesses:\n  - name: web\n    command:\n      program: /bin/sleep\n      args: [\"1\"]\n",
        )
        .expect("valid config");
    assert_eq!(project.processes().len(), 1);
    let web = &project.processes()[0];
    assert_eq!(web.name, "web");
    assert_eq!(web.kind, ProcessKind::Service);
    assert_eq!(web.enabled, Enabled::Yes);
    assert_eq!(web.autostart, Autostart::Yes);
    assert_eq!(web.success_exit_codes, vec![0]);
    assert_eq!(web.restart.policy, RestartPolicy::Never);
    assert_eq!(web.restart.backoff, Duration::from_secs(2));
    assert_eq!(web.restart.max_restarts, 5);
    assert_eq!(web.terminal_mode, TerminalMode::Pipe);
    assert_eq!(web.input_policy, InputPolicy::Disabled);
}

#[test]
fn restart_policy_and_backoff_load_with_explicit_values() {
    for (policy, expected) in [
        ("never", RestartPolicy::Never),
        ("on_failure", RestartPolicy::OnFailure),
        ("always", RestartPolicy::Always),
    ] {
        let project = write_and_load(
                policy,
                &format!(
                    "version: 1\nprocesses:\n  - name: web\n    restart: {{policy: {policy}, backoff: 1500ms, max_restarts: 3}}\n    command: {{program: /bin/sleep, args: [\"1\"]}}\n"
                ),
            )
            .expect("valid restart settings");
        assert_eq!(project.processes()[0].restart.policy, expected);
        assert_eq!(
            project.processes()[0].restart.backoff,
            Duration::from_millis(1500)
        );
        assert_eq!(project.processes()[0].restart.max_restarts, 3);
    }
}

#[test]
fn restart_budget_defaults_to_five_and_accepts_zero_or_more_retries() {
    let omitted = write_and_load(
        "restart-budget-default",
        "version: 1\nprocesses:\n  - name: web\n    restart: {policy: on_failure}\n    command: {program: /bin/true}\n",
    )
    .expect("an automatic policy may omit its budget");
    assert_eq!(omitted.processes()[0].restart.max_restarts, 5);

    for max_restarts in [0, 1, 12] {
        let project = write_and_load(
            &format!("restart-budget-{max_restarts}"),
            &format!(
                "version: 1\nprocesses:\n  - name: web\n    restart: {{policy: on_failure, max_restarts: {max_restarts}}}\n    command: {{program: /bin/true}}\n"
            ),
        )
        .expect("a nonnegative whole-number budget is valid");
        assert_eq!(project.processes()[0].restart.max_restarts, max_restarts);
    }
}

#[test]
fn invalid_restart_settings_are_rejected() {
    let policy = write_and_load(
            "restart-policy-invalid",
            "version: 1\nprocesses:\n  - name: web\n    restart: {policy: sometimes}\n    command: {program: /bin/true}\n",
        )
        .expect_err("an unknown restart policy must fail");
    assert!(policy.message.contains("restart.policy"));

    let backoff = write_and_load(
            "restart-backoff-zero",
            "version: 1\nprocesses:\n  - name: web\n    restart: {backoff: 0s}\n    command: {program: /bin/true}\n",
        )
        .expect_err("a zero restart backoff must fail");
    assert!(
        backoff
            .message
            .contains("backoff must be greater than zero")
    );

    for (label, value) in [
        ("restart-budget-negative", "-1"),
        ("restart-budget-float", "1.5"),
    ] {
        let error = write_and_load(
            label,
            &format!(
                "version: 1\nprocesses:\n  - name: web\n    restart: {{max_restarts: {value}}}\n    command: {{program: /bin/true}}\n"
            ),
        )
        .expect_err("the restart budget must be a nonnegative whole number");
        assert!(error.message.contains("max_restarts"), "{}", error.message);
    }
}

#[test]
fn always_restart_is_rejected_for_one_shots() {
    let error = write_and_load(
            "restart-one-shot-always",
            "version: 1\nprocesses:\n  - name: setup\n    kind: one-shot\n    restart: {policy: always}\n    command: {program: /bin/true}\n",
        )
        .expect_err("always is invalid for a One-shot");
    assert!(
        error.message.contains("restart.policy 'always'")
            && error.message.contains("valid only for Services"),
        "{}",
        error.message
    );
}

#[test]
fn project_shell_defaults_to_sh_without_login_flags() {
    let project = write_and_load(
        "shell-default",
        "version: 1\nprocesses:\n  - name: web\n    command: {shell: 'printf shell-ok'}\n",
    )
    .expect("the default shell is valid");
    assert_eq!(project.shell().program, std::ffi::OsString::from("/bin/sh"));
    assert_eq!(project.shell().args, [std::ffi::OsString::from("-c")]);
}

#[test]
fn project_shell_accepts_an_explicit_launcher_and_argument_list() {
    let project = write_and_load(
            "shell-explicit",
            "version: 1\nsettings:\n  shell:\n    program: /bin/bash\n    args: [-c]\nprocesses:\n  - name: web\n    command: {shell: 'printf shell-ok'}\n",
        )
        .expect("the explicit shell is valid");
    assert_eq!(
        project.shell().program,
        std::ffi::OsString::from("/bin/bash")
    );
    assert_eq!(project.shell().args, [std::ffi::OsString::from("-c")]);
}

#[test]
fn unusable_project_shell_settings_are_rejected() {
    let empty_program = write_and_load(
        "shell-empty-program",
        "version: 1\nsettings:\n  shell:\n    program: ''\n    args: [-c]\nprocesses: []\n",
    )
    .expect_err("an empty shell program must fail");
    assert!(empty_program.message.contains("program must not be empty"));

    let empty_args = write_and_load(
        "shell-empty-args",
        "version: 1\nsettings:\n  shell:\n    program: /bin/sh\n    args: []\nprocesses: []\n",
    )
    .expect_err("empty shell args must fail");
    assert!(empty_args.message.contains("args must contain"));
}

#[test]
fn relative_working_directories_resolve_from_the_config_directory() {
    let dir = std::env::temp_dir().join("stackhand-config-cwd");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("web")).expect("working directory creates");
    let path = dir.join("stackhand.yaml");
    fs::write(&path, "version: 1\nprocesses:\n  - name: web\n    working_dir: ./web\n    command:\n      program: /bin/true\n")
            .expect("config writes");
    let project = load(&path).expect("valid config");
    let _ = fs::remove_dir_all(&dir);
    let expected = dir.join("web");
    assert_eq!(project.processes()[0].working_dir, expected);
}

#[test]
fn missing_working_directories_are_rejected_with_the_process_name() {
    let error = write_and_load(
            "missing-cwd",
            "version: 1\nprocesses:\n  - name: web\n    working_dir: ./nope\n    command:\n      program: /bin/true\n",
        )
        .expect_err("a missing working directory must fail");
    assert!(
        error.message.contains("Process 'web'") && error.message.contains("working directory"),
        "{}",
        error.message
    );
}

#[test]
fn unsupported_versions_are_rejected() {
    let error =
        write_and_load("version", "version: 2\nprocesses: []\n").expect_err("version 2 must fail");
    assert!(error.message.contains("unsupported schema version 2"));
}

#[test]
fn unknown_fields_are_rejected() {
    let error = write_and_load(
        "unknown",
        "version: 1\nprocesses:\n  - name: web\n    comand:\n      program: /bin/true\n",
    )
    .expect_err("unknown field must fail");
    assert!(error.message.contains("unknown field"), "{}", error.message);
}

#[test]
fn multiple_unique_processes_load_with_their_flags() {
    let project = write_and_load(
        "multi",
        "version: 1
processes:
  - name: web
    command: {program: /bin/sleep, args: [\"1\"]}
  - name: worker
    autostart: false
    command: {program: /bin/sleep, args: [\"1\"]}
  - name: debug
    enabled: false
    command: {program: /bin/sleep, args: [\"1\"]}
",
    )
    .expect("valid config");

    let names: Vec<_> = project
        .processes()
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    assert_eq!(names, ["web", "worker", "debug"]);
    // Unset flags default to true; explicit flags surface as set.
    assert_eq!(project.processes()[0].enabled, Enabled::Yes);
    assert_eq!(project.processes()[0].autostart, Autostart::Yes);
    assert_eq!(project.processes()[1].autostart, Autostart::No);
    assert_eq!(project.processes()[2].enabled, Enabled::No);
}

#[test]
fn keyed_processes_and_dependencies_preserve_order_and_conditions() {
    let project = write_and_load(
        "keyed",
        "version: 1
processes:
  web:
    kind: service
    depends_on:
      started-service: started
      ready-service: ready
      exited-source: exited
      completed-source: completed_successfully
    command: {program: /bin/true}
  started-service:
    command: {program: /bin/true}
  ready-service:
    command: {program: /bin/true}
  exited-source:
    kind: one-shot
    command: {program: /bin/true}
  completed-source:
    kind: one-shot
    command: {program: /bin/true}
",
    )
    .expect("keyed configuration is valid");

    let names: Vec<_> = project
        .processes()
        .iter()
        .map(|process| process.name.as_str())
        .collect();
    assert_eq!(
        names,
        [
            "web",
            "started-service",
            "ready-service",
            "exited-source",
            "completed-source"
        ]
    );
    assert_eq!(
        project.processes()[0].dependencies,
        vec![
            DependencySpec {
                name: "started-service".to_string(),
                condition: DependencyCondition::Started,
            },
            DependencySpec {
                name: "ready-service".to_string(),
                condition: DependencyCondition::Ready,
            },
            DependencySpec {
                name: "exited-source".to_string(),
                condition: DependencyCondition::Exited,
            },
            DependencySpec {
                name: "completed-source".to_string(),
                condition: DependencyCondition::CompletedSuccessfully,
            },
        ]
    );
}

#[test]
fn keyed_process_map_owns_the_process_name() {
    let project = write_and_load(
        "keyed-name",
        "version: 1
processes:
  web:
    command: {program: /bin/true}
",
    )
    .expect("a keyed Process does not need a nested name");
    assert_eq!(project.processes()[0].name, "web");
}

#[test]
fn keyed_process_name_mismatches_are_rejected() {
    let error = write_and_load(
        "keyed-name-mismatch",
        "version: 1
processes:
  web:
    name: api
    command: {program: /bin/true}
",
    )
    .expect_err("a keyed Process cannot declare a different name");
    assert!(
        error
            .message
            .contains("Process map key 'web' does not match its declared name 'api'"),
        "{}",
        error.message
    );
}

#[test]
fn keyed_dependency_name_mismatches_are_rejected() {
    let error = write_and_load(
        "keyed-dependency-name-mismatch",
        "version: 1
processes:
  web:
    depends_on:
      db:
        name: cache
        condition: started
    command: {program: /bin/true}
  db:
    command: {program: /bin/true}
",
    )
    .expect_err("a keyed Dependency cannot declare a different name");
    assert!(
        error
            .message
            .contains("Dependency map key 'db' does not match its declared name 'cache'"),
        "{}",
        error.message
    );
}

#[test]
fn keyed_dependency_conditions_use_the_same_validation_as_legacy_entries() {
    let error = write_and_load(
        "keyed-dependency-condition",
        "version: 1
processes:
  web:
    depends_on:
      db: waiting
    command: {program: /bin/true}
  db:
    command: {program: /bin/true}
",
    )
    .expect_err("an unsupported keyed Dependency condition must fail");
    assert!(
        error.message.contains("unsupported condition 'waiting'"),
        "{}",
        error.message
    );
}

#[test]
fn duplicate_keyed_process_names_are_rejected() {
    let error = write_and_load(
        "keyed-dup",
        "version: 1
processes:
  web:
    command: {program: /bin/true}
  web:
    command: {program: /bin/true}
",
    )
    .expect_err("duplicate keyed names must fail");
    assert!(
        error.message.contains("duplicate Process name 'web'"),
        "{}",
        error.message
    );
}

#[test]
fn duplicate_names_are_rejected() {
    let error = write_and_load(
            "dup",
            "version: 1\nprocesses:\n  - name: web\n    command: {program: /bin/true}\n  - name: web\n    command: {program: /bin/true}\n",
        )
        .expect_err("duplicates must fail");
    assert!(error.message.contains("duplicate Process name 'web'"));
}

#[test]
fn command_forms_are_mutually_exclusive_and_required() {
    let both = write_and_load(
            "both",
            "version: 1\nprocesses:\n  - name: web\n    command:\n      program: /bin/true\n      shell: echo hi\n",
        )
        .expect_err("both forms must fail");
    assert!(both.message.contains("exactly one"), "{}", both.message);

    let neither = write_and_load(
        "neither",
        "version: 1\nprocesses:\n  - name: web\n    command: {}\n",
    )
    .expect_err("no form must fail");
    assert!(neither.message.contains("'program' or 'shell'"));
}

#[test]
fn depends_on_accepts_plain_names_and_condition_mappings() {
    let project = write_and_load(
        "deps-ok",
        "version: 1
processes:
  - name: web
    depends_on: [db, {name: cache, condition: started}]
    command: {program: /bin/sleep, args: [\"1\"]}
  - name: db
    command: {program: /bin/sleep, args: [\"1\"]}
  - name: cache
    command: {program: /bin/sleep, args: [\"1\"]}
",
    )
    .expect("valid dependencies");
    let web = &project.processes()[0];
    assert_eq!(web.dependencies.len(), 2);
    assert_eq!(web.dependencies[0].name, "db");
    assert_eq!(
        web.dependencies[0].condition,
        crate::model::DependencyCondition::Started
    );
    assert_eq!(web.dependencies[1].name, "cache");
    assert_eq!(project.processes()[1].dependencies, Vec::new());
}

#[test]
fn unknown_dependency_references_are_rejected() {
    let error = write_and_load(
        "deps-missing",
        "version: 1
processes:
  - name: web
    depends_on: [db]
    command: {program: /bin/true}
",
    )
    .expect_err("a missing reference must fail");
    assert!(
        error.message.contains("Process 'web'") && error.message.contains("'db'"),
        "{}",
        error.message
    );
}

#[test]
fn dependency_cycles_are_rejected_before_startup() {
    let error = write_and_load(
        "deps-cycle",
        "version: 1
processes:
  - name: web
    depends_on: [worker]
    command: {program: /bin/true}
  - name: worker
    depends_on: [web]
    command: {program: /bin/true}
",
    )
    .expect_err("a cycle must fail");
    assert!(
        error.message.contains("dependency cycle")
            && error.message.contains("web -> worker -> web"),
        "{}",
        error.message
    );
}

#[test]
fn completed_successfully_is_valid_only_on_one_shot_dependencies() {
    let accepted = write_and_load(
        "deps-completed-ok",
        "version: 1
processes:
  - name: web
    kind: one-shot
    depends_on: [{name: setup, condition: completed_successfully}]
    command: {program: /bin/true}
  - name: setup
    kind: one-shot
    command: {program: /bin/true}
",
    )
    .expect("a One-shot dependency accepts completed_successfully");
    assert_eq!(
        accepted.processes()[0].dependencies[0].condition,
        crate::model::DependencyCondition::CompletedSuccessfully
    );

    let error = write_and_load(
        "deps-completed-service",
        "version: 1
processes:
  - name: web
    depends_on: [{name: db, condition: completed_successfully}]
    command: {program: /bin/true}
  - name: db
    command: {program: /bin/true}
",
    )
    .expect_err("a Service dependency must reject completed_successfully");
    assert!(
        error.message.contains("'web'")
            && error.message.contains("'db'")
            && error.message.contains("completed_successfully"),
        "{}",
        error.message
    );
}

#[test]
fn ready_condition_is_valid_only_on_service_dependencies() {
    let accepted = write_and_load(
            "deps-ready-ok",
            "version: 1\nprocesses:\n  - name: web\n    depends_on: [{name: db, condition: ready}]\n    command: {program: /bin/true}\n  - name: db\n    command: {program: /bin/true}\n",
        )
        .expect("a Service dependency accepts ready");
    assert_eq!(
        accepted.processes()[0].dependencies[0].condition,
        crate::model::DependencyCondition::Ready
    );

    let error = write_and_load(
            "deps-ready-one-shot",
            "version: 1\nprocesses:\n  - name: web\n    depends_on: [{name: setup, condition: ready}]\n    command: {program: /bin/true}\n  - name: setup\n    kind: one-shot\n    command: {program: /bin/true}\n",
        )
        .expect_err("a One-shot dependency must reject ready");
    assert!(error.message.contains("'ready'"), "{}", error.message);
}

#[test]
fn invalid_depends_on_entries_are_rejected() {
    let unknown_field = write_and_load(
            "deps-field",
            "version: 1\nprocesses:\n  - name: web\n    depends_on: [{name: db, when: started}]\n    command: {program: /bin/true}\n  - name: db\n    command: {program: /bin/true}\n",
        )
        .expect_err("an unknown field must fail");
    assert!(unknown_field.message.contains("unknown field 'when'"));

    let missing_name = write_and_load(
            "deps-noname",
            "version: 1\nprocesses:\n  - name: web\n    depends_on: [{condition: started}]\n    command: {program: /bin/true}\n",
        )
        .expect_err("a mapping without a name must fail");
    assert!(missing_name.message.contains("requires 'name'"));

    let not_a_name = write_and_load(
            "deps-scalar",
            "version: 1\nprocesses:\n  - name: web\n    depends_on: [7]\n    command: {program: /bin/true}\n",
        )
        .expect_err("a scalar entry must fail");
    assert!(
        not_a_name
            .message
            .contains("Process name or a {name, condition} mapping")
    );
}

#[test]
fn canonical_process_fields_lower_into_the_effective_process() {
    let project = write_and_load(
        "canonical-process",
        "version: 1
processes:
  web:
    kind: service
    cwd: .
    environment:
      PORT: \"4000\"
    terminal:
      mode: pty
      input: focused
    command: [/bin/printf, \"hello world\", \"%s\\n\"]
",
    )
    .expect("canonical Process fields are valid");
    let web = &project.processes()[0];
    assert_eq!(
        web.working_dir,
        std::env::temp_dir().join("stackhand-config-canonical-process")
    );
    assert_eq!(web.env, vec![("PORT".to_string(), "4000".to_string())]);
    assert_eq!(web.terminal_mode, TerminalMode::Pty);
    assert_eq!(web.input_policy, InputPolicy::Focused);
    assert_eq!(
        web.command,
        CommandForm::Direct {
            program: "/bin/printf".into(),
            args: vec!["hello world".into(), "%s\n".into()],
        }
    );
}

#[test]
fn canonical_shell_uses_the_project_shell_configuration() {
    let project = write_and_load(
        "canonical-shell",
        "version: 1
settings:
  shell:
    program: /bin/bash
    args: [-lc]
processes:
  web:
    shell: printf canonical-shell
",
    )
    .expect("canonical shell fields are valid");
    assert_eq!(
        project.processes()[0].command,
        CommandForm::Shell {
            text: "printf canonical-shell".to_string()
        }
    );
    assert_eq!(
        project.shell().program,
        std::ffi::OsString::from("/bin/bash")
    );
    assert_eq!(project.shell().args, [std::ffi::OsString::from("-lc")]);
}

#[test]
fn canonical_command_and_shell_are_mutually_exclusive() {
    let error = write_and_load(
        "canonical-command-shell",
        "version: 1
processes:
  web:
    command: [/bin/true]
    shell: echo conflict
",
    )
    .expect_err("a Process must select one command form");
    assert!(
        error.message.contains("Process 'web'")
            && error.message.contains("exactly one")
            && error.message.contains("command")
            && error.message.contains("shell"),
        "{}",
        error.message
    );
}

#[test]
fn invalid_canonical_command_is_rejected_before_startup() {
    let error = write_and_load(
        "canonical-command-empty",
        "version: 1
processes:
  web:
    command: []
",
    )
    .expect_err("a canonical command needs a program");
    assert!(
        error.message.contains("Process 'web'") && error.message.contains("command must contain"),
        "{}",
        error.message
    );
}

#[test]
fn invalid_kinds_terminal_modes_and_input_policies_are_rejected() {
    for (label, field) in [
        ("kind", "kind: cron"),
        ("terminal", "terminal: serial"),
        ("input", "input: always"),
    ] {
        let error = write_and_load(
                label,
                &format!("version: 1\nprocesses:\n  - name: web\n    {field}\n    command:\n      program: /bin/true\n"),
            )
            .expect_err("invalid value must fail");
        assert!(
            error.message.contains("invalid"),
            "{field}: {}",
            error.message
        );
    }
}
