use super::*;
use std::fs;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
fn explicit_resolution_returns_the_selected_base_source() {
    let dir = std::env::temp_dir().join("stackhand-config-resolution-source");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("resolution directory creates");
    let path = dir.join("stackhand.yaml");
    fs::write(
        &path,
        "version: 1\nprocesses:\n  web:\n    command: [/usr/bin/true]\n",
    )
    .expect("resolution config writes");

    let resolution = resolve(ResolutionRequest::explicit(&path)).expect("resolution succeeds");
    assert_eq!(resolution.sources.base, path);
    assert_eq!(resolution.project().processes().len(), 1);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn discovery_uses_the_nearest_base_file_without_git_state() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("stackhand-config-discovery-{unique}"));
    let nested = root.join("nested").join("deep");
    fs::create_dir_all(&nested).expect("discovery directories create");
    let config = "version: 1\nprocesses:\n  web:\n    command: [/usr/bin/true]\n";
    fs::write(root.join(BASE_FILE_NAME), config).expect("root config writes");
    fs::write(nested.join(BASE_FILE_NAME), config).expect("nested config writes");

    let resolution =
        resolve(ResolutionRequest::discover_from(&nested)).expect("discovery succeeds");
    assert_eq!(resolution.sources.base, nested.join(BASE_FILE_NAME));

    let missing_root = std::env::temp_dir().join(format!("stackhand-config-missing-{unique}"));
    let missing_start = missing_root.join("nested");
    fs::create_dir_all(&missing_start).expect("missing discovery directory creates");
    let error = resolve(ResolutionRequest::discover_from(&missing_start))
        .expect_err("missing base config must fail");
    assert!(error.message.contains(BASE_FILE_NAME), "{error}");
    assert!(
        error.message.contains(&missing_start.display().to_string()),
        "{error}"
    );

    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&missing_root);
}

#[test]
fn one_direct_command_service_loads_with_defaults() {
    let project = write_and_load(
        "ok",
        "version: 1\nprocesses:\n  web:\n    command: [/bin/sleep, \"1\"]\n",
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
    assert_eq!(web.terminal_mode, TerminalMode::Pty);
    assert_eq!(web.input_policy, InputPolicy::Focused);
}

#[test]
fn pipe_mode_disables_input_by_default() {
    let project = write_and_load(
        "explicit-pipe",
        "version: 1\nprocesses:\n  web:\n    terminal: {mode: pipe}\n    command: [/bin/sleep, \"1\"]\n",
    )
    .expect("explicit pipe config is valid");
    let web = &project.processes()[0];

    assert_eq!(web.terminal_mode, TerminalMode::Pipe);
    assert_eq!(web.input_policy, InputPolicy::Disabled);
}

#[test]
fn pty_input_can_be_disabled_explicitly() {
    let project = write_and_load(
        "disabled-pty-input",
        "version: 1\nprocesses:\n  web:\n    terminal: {mode: pty, input: disabled}\n    command: [/bin/sleep, \"1\"]\n",
    )
    .expect("explicitly disabled PTY input is valid");
    let web = &project.processes()[0];

    assert_eq!(web.terminal_mode, TerminalMode::Pty);
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
                    "version: 1\nprocesses:\n  web:\n    restart: {{policy: {policy}, backoff: 1500ms, max_restarts: 3}}\n    command: [/bin/sleep, \"1\"]\n"
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
        "version: 1\nprocesses:\n  web:\n    restart: {policy: on_failure}\n    command: [/usr/bin/true]\n",
    )
    .expect("an automatic policy may omit its budget");
    assert_eq!(omitted.processes()[0].restart.max_restarts, 5);

    for max_restarts in [0, 1, 12] {
        let project = write_and_load(
            &format!("restart-budget-{max_restarts}"),
            &format!(
                "version: 1\nprocesses:\n  web:\n    restart: {{policy: on_failure, max_restarts: {max_restarts}}}\n    command: [/usr/bin/true]\n"
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
            "version: 1\nprocesses:\n  web:\n    restart: {policy: sometimes}\n    command: [/usr/bin/true]\n",
        )
        .expect_err("an unknown restart policy must fail");
    assert!(policy.message.contains("restart.policy"));

    let backoff = write_and_load(
        "restart-backoff-zero",
        "version: 1\nprocesses:\n  web:\n    restart: {backoff: 0s}\n    command: [/usr/bin/true]\n",
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
                "version: 1\nprocesses:\n  web:\n    restart: {{max_restarts: {value}}}\n    command: [/usr/bin/true]\n"
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
            "version: 1\nprocesses:\n  setup:\n    kind: one-shot\n    restart: {policy: always}\n    command: [/usr/bin/true]\n",
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
        "version: 1\nprocesses:\n  web:\n    shell: 'printf shell-ok'\n",
    )
    .expect("the default shell is valid");
    assert_eq!(project.shell().program, std::ffi::OsString::from("/bin/sh"));
    assert_eq!(project.shell().args, [std::ffi::OsString::from("-c")]);
}

#[test]
fn port_discovery_is_project_wide_and_defaults_off() {
    let disabled = write_and_load(
        "port-discovery-default",
        "version: 1\nprocesses:\n  web:\n    command: [/usr/bin/true]\n",
    )
    .expect("the default Project is valid");
    assert!(!disabled.port_discovery());

    let enabled = write_and_load(
        "port-discovery-enabled",
        "version: 1\nsettings:\n  port_discovery: true\nprocesses:\n  web:\n    command: [/usr/bin/true]\n",
    )
    .expect("the enabled Project is valid");
    assert!(enabled.port_discovery());
}

#[test]
fn project_shell_accepts_an_explicit_launcher_and_argument_list() {
    let project = write_and_load(
            "shell-explicit",
            "version: 1\nsettings:\n  shell:\n    program: /bin/bash\n    args: [-c]\nprocesses:\n  web:\n    shell: 'printf shell-ok'\n",
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
        "version: 1\nsettings:\n  shell:\n    program: ''\n    args: [-c]\nprocesses: {}\n",
    )
    .expect_err("an empty shell program must fail");
    assert!(empty_program.message.contains("program must not be empty"));

    let empty_args = write_and_load(
        "shell-empty-args",
        "version: 1\nsettings:\n  shell:\n    program: /bin/sh\n    args: []\nprocesses: {}\n",
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
    fs::write(
        &path,
        "version: 1\nprocesses:\n  web:\n    cwd: ./web\n    command: [/usr/bin/true]\n",
    )
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
        "version: 1\nprocesses:\n  web:\n    cwd: ./nope\n    command: [/usr/bin/true]\n",
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
        write_and_load("version", "version: 2\nprocesses: {}\n").expect_err("version 2 must fail");
    assert!(error.message.contains("unsupported schema version 2"));
}

#[test]
fn unknown_fields_are_rejected() {
    let error = write_and_load(
        "unknown",
        "version: 1\nprocesses:\n  web:\n    comand:\n      program: /usr/bin/true\n",
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
  web:
    command: [/bin/sleep, \"1\"]
  worker:
    autostart: false
    command: [/bin/sleep, \"1\"]
  debug:
    enabled: false
    command: [/bin/sleep, \"1\"]
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
    command: [/usr/bin/true]
  started-service:
    command: [/usr/bin/true]
  ready-service:
    command: [/usr/bin/true]
  exited-source:
    kind: one-shot
    command: [/usr/bin/true]
  completed-source:
    kind: one-shot
    command: [/usr/bin/true]
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
    command: [/usr/bin/true]
",
    )
    .expect("a keyed Process does not need a nested name");
    assert_eq!(project.processes()[0].name, "web");
}

#[test]
fn temporary_nested_process_names_name_the_canonical_map_key() {
    let error = write_and_load(
        "temporary-process-name",
        "version: 1
processes:
  web:
    name: api
    command: [/usr/bin/true]
",
    )
    .expect_err("a Process name must not be nested in a keyed Process");
    assert!(
        error.message.contains("unknown field `name`")
            && error
                .message
                .contains("put the Process name in the `processes` map key instead"),
        "{}",
        error.message
    );
}

#[test]
fn temporary_nested_dependency_names_name_the_canonical_map_value() {
    let error = write_and_load(
        "temporary-dependency-name",
        "version: 1
processes:
  web:
    depends_on:
      db:
        name: cache
        condition: started
    command: [/usr/bin/true]
  db:
    command: [/usr/bin/true]
",
    )
    .expect_err("a Dependency condition must be a string");
    assert!(
        error
            .message
            .contains("condition for Dependency 'db' must be a string")
            && error.message.contains("use 'dependency-name: condition'"),
        "{}",
        error.message
    );
}

#[test]
fn keyed_dependency_conditions_use_canonical_validation() {
    let error = write_and_load(
        "keyed-dependency-condition",
        "version: 1
processes:
  web:
    depends_on:
      db: waiting
    command: [/usr/bin/true]
  db:
    command: [/usr/bin/true]
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
    command: [/usr/bin/true]
  web:
    command: [/usr/bin/true]
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
            "version: 1\nprocesses:\n  web:\n    command: [/usr/bin/true]\n  web:\n    command: [/usr/bin/true]\n",
        )
        .expect_err("duplicates must fail");
    assert!(error.message.contains("duplicate Process name 'web'"));
}

#[test]
fn command_forms_are_mutually_exclusive_and_required() {
    let both = write_and_load(
        "both",
        "version: 1\nprocesses:\n  web:\n    command: [/usr/bin/true]\n    shell: echo hi\n",
    )
    .expect_err("both forms must fail");
    assert!(both.message.contains("exactly one"), "{}", both.message);

    let neither = write_and_load("neither", "version: 1\nprocesses:\n  web:\n")
        .expect_err("no form must fail");
    assert!(neither.message.contains("'command' or 'shell'"));
}

#[test]
fn depends_on_accepts_plain_names_and_condition_mappings() {
    let project = write_and_load(
        "deps-ok",
        "version: 1
processes:
  web:
    depends_on: {db: started, cache: started}
    command: [/bin/sleep, \"1\"]
  db:
    command: [/bin/sleep, \"1\"]
  cache:
    command: [/bin/sleep, \"1\"]
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
  web:
    depends_on: {db: started}
    command: [/usr/bin/true]
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
  web:
    depends_on: {worker: started}
    command: [/usr/bin/true]
  worker:
    depends_on: {web: started}
    command: [/usr/bin/true]
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
  web:
    kind: one-shot
    depends_on: {setup: completed_successfully}
    command: [/usr/bin/true]
  setup:
    kind: one-shot
    command: [/usr/bin/true]
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
  web:
    depends_on: {db: completed_successfully}
    command: [/usr/bin/true]
  db:
    command: [/usr/bin/true]
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
            "version: 1\nprocesses:\n  web:\n    depends_on: {db: ready}\n    command: [/usr/bin/true]\n  db:\n    command: [/usr/bin/true]\n",
        )
        .expect("a Service dependency accepts ready");
    assert_eq!(
        accepted.processes()[0].dependencies[0].condition,
        crate::model::DependencyCondition::Ready
    );

    let error = write_and_load(
            "deps-ready-one-shot",
            "version: 1\nprocesses:\n  web:\n    depends_on: {setup: ready}\n    command: [/usr/bin/true]\n  setup:\n    kind: one-shot\n    command: [/usr/bin/true]\n",
        )
        .expect_err("a One-shot dependency must reject ready");
    assert!(error.message.contains("'ready'"), "{}", error.message);
}

#[test]
fn temporary_dependency_collections_name_the_canonical_mapping() {
    let error = write_and_load(
        "temporary-dependency-list",
        "version: 1
processes:
  web:
    depends_on:
      - db
    command: [/usr/bin/true]
  db:
    command: [/usr/bin/true]
",
    )
    .expect_err("a Dependency collection must be keyed");
    assert!(
        error
            .message
            .contains("depends_on must be a name-keyed mapping")
            && error
                .message
                .contains("use 'depends_on: {process-name: condition}'"),
        "{}",
        error.message
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
    command: [/usr/bin/printf, \"hello world\", \"%s\\n\"]
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
            program: "/usr/bin/printf".into(),
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
    command: [/usr/bin/true]
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
    for (label, fields) in [
        ("kind", "kind: cron"),
        ("terminal", "terminal: {mode: serial}"),
        ("input", "terminal: {mode: pipe, input: always}"),
    ] {
        let error = write_and_load(
            label,
            &format!(
                "version: 1\nprocesses:\n  web:\n    {fields}\n    command: [/usr/bin/true]\n"
            ),
        )
        .expect_err("invalid value must fail");
        assert!(
            error.message.contains("invalid"),
            "{fields}: {}",
            error.message
        );
    }
}

#[test]
fn process_groups_define_visual_order_and_leave_unlisted_processes_for_other() {
    let project = write_and_load(
        "groups",
        "version: 1
groups:
  Infrastructure: [database, cache]
  Application: [api]
processes:
  api: {command: [/usr/bin/true]}
  database: {command: [/usr/bin/true]}
  worker: {command: [/usr/bin/true]}
  cache: {command: [/usr/bin/true]}
",
    )
    .expect("valid Process Groups load");

    let names = project
        .processes()
        .iter()
        .map(|process| process.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["database", "cache", "api", "worker"]);
    let groups = (0..project.processes().len())
        .map(|index| project.process_group(index))
        .collect::<Vec<_>>();
    assert_eq!(
        groups,
        [
            Some("Infrastructure"),
            Some("Infrastructure"),
            Some("Application"),
            None,
        ]
    );
}

#[test]
fn invalid_process_group_membership_is_rejected() {
    for (label, groups, expected) in [
        ("unknown", "Infrastructure: [missing]", "is not configured"),
        (
            "duplicate",
            "Infrastructure: [web]\n  Application: [web]",
            "more than one Process Group",
        ),
        (
            "empty",
            "Infrastructure: []",
            "must contain at least one Process",
        ),
        ("other", "Other: [web]", "reserved for ungrouped Processes"),
        (
            "padded-other",
            "' Other ': [web]",
            "reserved for ungrouped Processes",
        ),
    ] {
        let error = write_and_load(
            &format!("group-{label}"),
            &format!(
                "version: 1\ngroups:\n  {groups}\nprocesses:\n  web: {{command: [/usr/bin/true]}}\n"
            ),
        )
        .expect_err("invalid Process Group membership must fail");
        assert!(error.message.contains(expected), "{label}: {error}");
    }
}
