use assert_cmd::Command;
use serde_yaml::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_directory(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_nanos();
    let directory = std::env::temp_dir().join(format!("stackhand-cli-show-{label}-{unique}"));
    fs::create_dir_all(&directory).expect("test directory creates");
    directory
}

fn run_show(directory: &Path, path: Option<&Path>, profile: Option<&str>) -> std::process::Output {
    let mut command = Command::cargo_bin("stackhand").expect("Stackhand binary builds");
    command.current_dir(directory).args(["config", "show"]);
    if let Some(path) = path {
        command.arg(path);
    }
    if let Some(profile) = profile {
        command.args(["--profile", profile]);
    }
    command.output().expect("config show runs")
}

fn run_validate(directory: &Path, path: &Path) -> std::process::Output {
    let mut command = Command::cargo_bin("stackhand").expect("Stackhand binary builds");
    command
        .current_dir(directory)
        .args(["config", "validate"])
        .arg(path);
    command.output().expect("config validate runs")
}

fn effective_yaml(output: &std::process::Output) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let yaml = stdout
        .split_once("Effective Project:\n")
        .expect("show output has an effective Project section")
        .1;
    serde_yaml::from_str(yaml).expect("effective output is YAML")
}

#[test]
fn config_show_reports_layers_and_redacts_loaded_environment() {
    let root = unique_directory("layers");
    let nested = root.join("nested");
    fs::create_dir_all(&nested).expect("nested directory creates");
    let base = root.join("stackhand.yaml");
    let local = root.join("stackhand.local.yaml");
    fs::write(
        root.join("project.env"),
        "PROJECT_FILE_SECRET=project-file-secret\n",
    )
    .expect("Project environment writes");
    fs::write(
        root.join("process.env"),
        "PROCESS_FILE_SECRET=process-file-secret\n",
    )
    .expect("Process environment writes");
    fs::write(
        &base,
        "version: 1
env_files: [project.env]
processes:
  web:
    env_files: [process.env]
    command: [/usr/bin/true]
    environment:
      BASE_SECRET: base-secret
    profiles:
      second:
        environment:
          SECOND_SECRET: second-secret
      unused:
        environment:
          UNUSED_SECRET: should-not-appear
",
    )
    .expect("base Project writes");
    fs::write(
        &local,
        "processes:
  web:
    cwd: .
    environment:
      LOCAL_SECRET: local-secret
",
    )
    .expect("local override writes");

    let canonical_root = fs::canonicalize(&root).expect("root canonicalizes");
    let canonical_base = fs::canonicalize(&base).expect("base canonicalizes");
    let canonical_local = fs::canonicalize(&local).expect("local canonicalizes");
    let output = run_show(&nested, None, Some("second"));
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let base_position = stdout
        .find(&canonical_base.display().to_string())
        .expect("base is shown");
    let profile_position = stdout
        .find("  profile: second")
        .expect("selected profile is shown");
    let local_position = stdout
        .find(&canonical_local.display().to_string())
        .expect("local override is shown");
    assert!(base_position < local_position);
    assert!(local_position < profile_position);
    assert!(!stdout.contains("unused"));
    for secret in [
        "project-file-secret",
        "process-file-secret",
        "base-secret",
        "second-secret",
        "local-secret",
        "should-not-appear",
    ] {
        assert!(!stdout.contains(secret), "secret leaked: {secret}");
    }

    let value = effective_yaml(&output);
    let root_mapping = value.as_mapping().expect("effective Project is a mapping");
    assert!(
        root_mapping
            .get(Value::String("profiles".to_string()))
            .is_none()
    );
    assert!(
        root_mapping
            .get(Value::String("env_files".to_string()))
            .is_none()
    );
    let processes = root_mapping
        .get(Value::String("processes".to_string()))
        .and_then(Value::as_mapping)
        .expect("effective processes are a mapping");
    let web = processes
        .get(Value::String("web".to_string()))
        .and_then(Value::as_mapping)
        .expect("effective web Process exists");
    assert_eq!(
        web.get(Value::String("cwd".to_string())),
        Some(&Value::String(canonical_root.display().to_string()))
    );
    assert_eq!(
        web.get(Value::String("command".to_string()))
            .and_then(Value::as_sequence)
            .and_then(|command| command.first()),
        Some(&Value::String("/usr/bin/true".to_string()))
    );
    let environment = web
        .get(Value::String("environment".to_string()))
        .and_then(Value::as_mapping)
        .expect("effective environment is visible");
    for key in [
        "PROJECT_FILE_SECRET",
        "PROCESS_FILE_SECRET",
        "BASE_SECRET",
        "SECOND_SECRET",
        "LOCAL_SECRET",
    ] {
        assert_eq!(
            environment.get(Value::String(key.to_string())),
            Some(&Value::String("<redacted>".to_string())),
            "environment key is redacted: {key}"
        );
    }

    fs::remove_dir_all(root).ok();
}

#[test]
fn config_show_accepts_explicit_paths_and_has_stable_output_without_starting_processes() {
    let root = unique_directory("explicit");
    let current = root.join("current");
    let config = root.join("project.yaml");
    let marker = root.join("started.marker");
    fs::create_dir_all(&current).expect("current directory creates");
    fs::write(
        &config,
        format!(
            "version: 1\nprocesses:\n  web:\n    shell: |\n      : > {}\n    profiles:\n      selected: {{}}\n",
            marker.display()
        ),
    )
    .expect("explicit Project writes");
    fs::write(
        root.join("stackhand.local.yaml"),
        "processes:
  local-only:
    command: [/usr/bin/true]
",
    )
    .expect("local override writes");

    let first = run_show(&current, Some(&config), Some("selected"));
    let second = run_show(&current, Some(&config), Some("selected"));
    assert!(first.status.success(), "{first:?}");
    assert!(second.status.success(), "{second:?}");
    assert_eq!(first.stdout, second.stdout);
    assert!(!marker.exists(), "config show started a Process");
    let stdout = String::from_utf8_lossy(&first.stdout);
    assert!(stdout.contains(&config.display().to_string()), "{stdout}");
    assert!(stdout.contains("  profile: selected"), "{stdout}");
    assert!(!stdout.contains("local override"), "{stdout}");
    assert!(!stdout.contains("local-only"), "{stdout}");
    let _ = effective_yaml(&first);

    fs::remove_dir_all(root).ok();
}

#[test]
fn config_show_reports_a_configured_base_profile_name() {
    let root = unique_directory("base-profile-name");
    let config = root.join("stackhand.yaml");
    fs::write(
        &config,
        "version: 1
base_profile_name: local
processes:
  web:
    command: [/usr/bin/true]
",
    )
    .expect("Project writes");

    let output = run_show(&root, Some(&config), None);
    assert!(output.status.success(), "{output:?}");
    let effective = effective_yaml(&output);
    assert_eq!(
        effective
            .as_mapping()
            .and_then(|root| root.get(Value::String("base_profile_name".to_string())))
            .and_then(Value::as_str),
        Some("local")
    );

    fs::remove_dir_all(root).ok();
}

#[test]
fn config_show_uses_the_same_resolution_failure_as_config_validate() {
    let root = unique_directory("failure");
    let config = root.join("project.yaml");
    fs::write(&config, "version: 2\nprocesses: {}\n").expect("invalid Project writes");

    let show = run_show(&root, Some(&config), None);
    let validate = run_validate(&root, &config);
    assert!(!show.status.success(), "{show:?}");
    assert!(!validate.status.success(), "{validate:?}");
    let show_stderr = String::from_utf8_lossy(&show.stderr);
    let validate_stderr = String::from_utf8_lossy(&validate.stderr);
    assert!(show_stderr.contains("configuration error: unsupported schema version 2"));
    assert!(validate_stderr.contains("configuration error: unsupported schema version 2"));
    assert!(!String::from_utf8_lossy(&show.stdout).contains("Effective Project"));

    fs::remove_dir_all(root).ok();
}
