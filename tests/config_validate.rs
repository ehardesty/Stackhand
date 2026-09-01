use assert_cmd::Command;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_directory(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_nanos();
    let directory = std::env::temp_dir().join(format!("stackhand-cli-{label}-{unique}"));
    fs::create_dir_all(&directory).expect("test directory creates");
    directory
}

fn write_valid_project(path: &Path) {
    fs::write(
        path,
        "version: 1\nprocesses:\n  web:\n    command: [/usr/bin/true]\n",
    )
    .expect("project config writes");
}

fn run_validate(directory: &Path, path: Option<&Path>) -> std::process::Output {
    run_validate_with_profile(directory, path, None)
}

fn run_validate_with_profile(
    directory: &Path,
    path: Option<&Path>,
    profile: Option<&str>,
) -> std::process::Output {
    let mut command = Command::cargo_bin("stackhand").expect("Stackhand binary builds");
    command.current_dir(directory).args(["config", "validate"]);
    if let Some(path) = path {
        command.arg(path);
    }
    if let Some(profile) = profile {
        command.args(["--profile", profile]);
    }
    command.output().expect("config validate runs")
}

fn run_validate_with_arguments(
    directory: &Path,
    path: Option<&Path>,
    arguments: &[&str],
) -> std::process::Output {
    let mut command = Command::cargo_bin("stackhand").expect("Stackhand binary builds");
    command.current_dir(directory).args(["config", "validate"]);
    if let Some(path) = path {
        command.arg(path);
    }
    command.args(arguments);
    command.output().expect("config validate runs")
}

#[test]
fn config_validate_discovers_the_nearest_base_file() {
    let root = unique_directory("discover");
    let nested = root.join("nested").join("deep");
    fs::create_dir_all(&nested).expect("nested directories create");
    write_valid_project(&root.join("stackhand.yaml"));
    write_valid_project(&nested.join("stackhand.yaml"));

    let output = run_validate(&nested, None);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&nested.join("stackhand.yaml").display().to_string()),
        "{stdout}"
    );

    fs::remove_dir_all(root).ok();
}

#[test]
fn config_validate_uses_an_explicit_path_without_discovery() {
    let root = unique_directory("explicit");
    let current = root.join("current");
    let explicit = root.join("other").join("project.yaml");
    fs::create_dir_all(&current).expect("current directory creates");
    fs::create_dir_all(explicit.parent().expect("explicit parent exists"))
        .expect("explicit directory creates");
    fs::write(
        current.join("stackhand.yaml"),
        "version: 2\nprocesses: {}\n",
    )
    .expect("discovered config writes");
    write_valid_project(&explicit);
    let explicit_local = explicit
        .parent()
        .expect("explicit parent exists")
        .join("stackhand.local.yaml");
    fs::write(
        &explicit_local,
        "processes:\n  local-only:\n    command: [/usr/bin/true]\n",
    )
    .expect("explicit local override writes");

    let output = run_validate(&current, Some(&explicit));
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(&explicit.display().to_string()), "{stdout}");
    assert!(!stdout.contains(&current.join("stackhand.yaml").display().to_string()));
    assert!(!stdout.contains(&explicit_local.display().to_string()));

    fs::remove_dir_all(root).ok();
}

#[test]
fn config_validate_reports_discovered_sources_in_precedence_order() {
    let root = unique_directory("sources");
    let nested = root.join("nested");
    fs::create_dir_all(&nested).expect("nested directory creates");
    let base = root.join("stackhand.yaml");
    let local = root.join("stackhand.local.yaml");
    write_valid_project(&base);
    fs::write(&local, "processes:\n  web:\n    command: [/usr/bin/true]\n")
        .expect("local override writes");

    let output = run_validate(&nested, None);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let base_position = stdout
        .find(&base.display().to_string())
        .expect("base source is reported");
    let local_position = stdout
        .find(&local.display().to_string())
        .expect("local source is reported");
    assert!(base_position < local_position, "{stdout}");

    fs::remove_dir_all(root).ok();
}

#[test]
fn config_validate_reports_a_missing_base_file_and_starting_directory() {
    let root = unique_directory("missing");
    let start = root.join("nested");
    fs::create_dir_all(&start).expect("starting directory creates");

    let output = run_validate(&start, None);
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("stackhand.yaml"), "{stderr}");
    assert!(stderr.contains(&start.display().to_string()), "{stderr}");

    fs::remove_dir_all(root).ok();
}

#[test]
fn config_validate_rejects_extra_paths_with_a_failure_status() {
    let root = unique_directory("extra-paths");
    let first = root.join("first.yaml");
    let second = root.join("second.yaml");
    write_valid_project(&first);
    write_valid_project(&second);

    let mut command = Command::cargo_bin("stackhand").expect("Stackhand binary builds");
    let output = command
        .current_dir(&root)
        .args(["config", "validate"])
        .arg(&first)
        .arg(&second)
        .output()
        .expect("config validate runs");
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("at most one Project path"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    fs::remove_dir_all(root).ok();
}

#[test]
fn config_validate_does_not_start_a_process() {
    let root = unique_directory("invalid");
    let marker = root.join("started.marker");
    let config = root.join("project.yaml");
    let shell = format!(": > {}", marker.display());
    fs::write(
        &config,
        format!("version: 1\nprocesses:\n  web:\n    shell: \"{shell}\"\n"),
    )
    .expect("invalid-test config writes");

    let output = run_validate(&root, Some(&config));
    assert!(output.status.success(), "{output:?}");
    assert!(!marker.exists(), "config validate started a Process");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(&config.display().to_string()),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );

    fs::remove_dir_all(root).ok();
}

#[test]
fn config_validate_keeps_invalid_schema_errors_on_stderr() {
    let root = unique_directory("invalid-schema");
    let config = root.join("project.yaml");
    fs::write(&config, "version: 2\nprocesses: {}\n").expect("invalid config writes");

    let output = run_validate(&root, Some(&config));
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unsupported schema version 2"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    fs::remove_dir_all(root).ok();
}

#[test]
fn config_validate_validates_the_selected_profile() {
    let root = unique_directory("profile");
    let config = root.join("project.yaml");
    fs::write(
        &config,
        "version: 1\nprocesses:\n  web:\n    command: [/usr/bin/true]\n    profiles:\n      local:\n        command: [/usr/bin/true, 1]\n",
    )
    .expect("profile config writes");

    let output = run_validate_with_profile(&root, Some(&config), Some("local"));
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("Process 'web': command argument 0 must be a string"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    fs::remove_dir_all(root).ok();
}

#[test]
fn config_validate_rejects_an_unknown_profile_name() {
    let root = unique_directory("unknown-profile");
    let config = root.join("project.yaml");
    fs::write(
        &config,
        "version: 1\nprocesses:\n  web:\n    command: [/usr/bin/true]\n    profiles:\n      local: {}\n",
    )
    .expect("profile config writes");

    let output = run_validate_with_profile(&root, Some(&config), Some("missing"));
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unknown Project Profile 'missing'"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    fs::remove_dir_all(root).ok();
}

#[test]
fn config_validate_rejects_an_invalid_unselected_profile_graph() {
    let root = unique_directory("profile-invalid-graph");
    let config = root.join("project.yaml");
    fs::write(
        &config,
        "version: 1\nprocesses:\n  api:\n    command: [/usr/bin/true]\n    profiles:\n      cloud:\n        depends_on: {worker: started}\n  worker:\n    command: [/usr/bin/true]\n    profiles:\n      cloud:\n        depends_on: {api: started}\n",
    )
    .expect("profile config writes");

    let output = run_validate(&root, Some(&config));
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Process Profile 'cloud' produces an invalid Project")
            && stderr.contains("api -> worker -> api"),
        "{stderr}"
    );

    fs::remove_dir_all(root).ok();
}

#[test]
fn config_validate_accepts_an_unselected_valid_process_profile() {
    let root = unique_directory("profile-explicit");
    let config = root.join("project.yaml");
    fs::write(
        &config,
        "version: 1\nprocesses:\n  web:\n    command: [/usr/bin/true]\n    profiles:\n      local:\n        environment:\n          MODE: local\n",
    )
    .expect("profile config writes");

    let base = run_validate(&root, Some(&config));
    assert!(base.status.success(), "{base:?}");
    let selected = run_validate_with_profile(&root, Some(&config), Some("local"));
    assert!(selected.status.success(), "{selected:?}");

    fs::remove_dir_all(root).ok();
}

#[test]
fn config_validate_does_not_print_environment_values() {
    let root = unique_directory("environment-redaction");
    let config = root.join("project.yaml");
    let environment = root.join("project.env");
    const SECRET: &str = "config-validation-secret-sentinel";
    fs::write(&environment, format!("BAD-KEY={SECRET}\n"))
        .expect("invalid environment file writes");
    fs::write(
        &config,
        "version: 1\nenv_files: [project.env]\nprocesses:\n  web:\n    command: [/usr/bin/true]\n",
    )
    .expect("configuration writes");

    let invalid = run_validate(&root, Some(&config));
    assert!(!invalid.status.success(), "{invalid:?}");
    assert!(!String::from_utf8_lossy(&invalid.stdout).contains(SECRET));
    assert!(!String::from_utf8_lossy(&invalid.stderr).contains(SECRET));
    assert!(
        String::from_utf8_lossy(&invalid.stderr).contains("BAD-KEY")
            && String::from_utf8_lossy(&invalid.stderr).contains("line 1")
    );

    fs::remove_dir_all(root).ok();
}

#[test]
fn config_validate_rejects_more_than_one_profile_option() {
    let root = unique_directory("profile-order");
    let config = root.join("project.yaml");
    fs::write(
        &config,
        "version: 1
processes:
  web:
    command: [/usr/bin/true]
    profiles:
      first: {}
      second: {}
",
    )
    .expect("Process Profile config writes");

    let output = run_validate_with_arguments(
        &root,
        Some(&config),
        &["--profile", "first", "--profile", "second"],
    );
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--profile can be specified only once"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    fs::remove_dir_all(root).ok();
}
