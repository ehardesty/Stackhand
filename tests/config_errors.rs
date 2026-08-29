use assert_cmd::Command;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_directory(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_nanos();
    let directory = std::env::temp_dir().join(format!("stackhand-config-errors-{label}-{unique}"));
    fs::create_dir_all(&directory).expect("configuration test directory creates");
    directory
}

fn yaml_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn marker_process(marker: &Path) -> String {
    let command = format!(": > {}", shell_quote(&marker.to_string_lossy()));
    format!("  marker:\n    shell: {}\n", yaml_quote(&command))
}

fn run_project(directory: &Path, path: Option<&Path>, profiles: &[&str]) -> Output {
    let mut command = Command::cargo_bin("stackhand").expect("Stackhand binary builds");
    command.current_dir(directory);
    if let Some(path) = path {
        command.arg(path);
    }
    for profile in profiles {
        command.args(["--profile", profile]);
    }
    command.output().expect("Project command runs")
}

fn assert_failure_without_start(output: &Output, marker: &Path, expected: &[&str]) -> String {
    assert!(
        !output.status.success(),
        "expected a configuration failure: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    for text in expected {
        assert!(stderr.contains(text), "missing {text:?} in {stderr}");
    }
    assert!(!marker.exists(), "a Process start marker was written");
    stderr
}

#[test]
fn invalid_base_yaml_reports_path_and_location_without_starting_a_process() {
    let root = unique_directory("base-yaml");
    let marker = root.join("started.marker");
    let config = root.join("stackhand.yaml");
    fs::write(
        &config,
        format!(
            "version: 1\nprocesses:\n{}  broken: [\n",
            marker_process(&marker)
        ),
    )
    .expect("invalid base configuration writes");

    let output = run_project(&root, Some(&config), &[]);
    assert_failure_without_start(
        &output,
        &marker,
        &[&config.display().to_string(), "line", "column"],
    );

    fs::remove_dir_all(root).ok();
}

#[test]
fn selected_profile_reports_the_profile_and_effective_process_without_starting() {
    let root = unique_directory("profile");
    let marker = root.join("started.marker");
    let config = root.join("stackhand.yaml");
    fs::write(
        &config,
        format!(
            "version: 1\nprocesses:\n{}profiles:\n  broken:\n    overrides:\n      marker:\n        terminal:\n          mode: invalid-mode\n",
            marker_process(&marker)
        ),
    )
    .expect("profile configuration writes");

    let output = run_project(&root, Some(&config), &["broken"]);
    assert_failure_without_start(
        &output,
        &marker,
        &["profile 'broken'", "Process 'marker'", "terminal mode"],
    );

    fs::remove_dir_all(root).ok();
}

#[test]
fn invalid_local_yaml_reports_override_path_and_location_without_starting() {
    let root = unique_directory("local-yaml");
    let nested = root.join("nested");
    fs::create_dir_all(&nested).expect("nested directory creates");
    let marker = root.join("started.marker");
    let base = root.join("stackhand.yaml");
    let local = root.join("stackhand.local.yaml");
    fs::write(
        &base,
        format!("version: 1\nprocesses:\n{}", marker_process(&marker)),
    )
    .expect("base configuration writes");
    fs::write(&local, "processes:\n  marker: [\n").expect("invalid local override writes");

    let output = run_project(&nested, None, &[]);
    assert_failure_without_start(
        &output,
        &marker,
        &[&local.display().to_string(), "line", "column"],
    );

    fs::remove_dir_all(root).ok();
}

#[test]
fn invalid_environment_file_reports_file_and_line_without_starting() {
    let root = unique_directory("environment");
    let marker = root.join("started.marker");
    let config = root.join("stackhand.yaml");
    let environment = root.join("project.env");
    const SECRET: &str = "configuration-error-secret-sentinel";
    fs::write(&environment, format!("# header\nBAD-KEY={SECRET}\n"))
        .expect("invalid environment file writes");
    fs::write(
        &config,
        format!(
            "version: 1\nenv_files: [project.env]\nprocesses:\n{}",
            marker_process(&marker)
        ),
    )
    .expect("environment configuration writes");

    let output = run_project(&root, Some(&config), &[]);
    let stderr = assert_failure_without_start(
        &output,
        &marker,
        &[&environment.display().to_string(), "line 2", "BAD-KEY"],
    );
    assert!(
        !stderr.contains(SECRET),
        "environment values must stay private"
    );

    fs::remove_dir_all(root).ok();
}

#[test]
fn invalid_path_reports_process_and_configured_path_without_starting() {
    let root = unique_directory("path");
    let marker = root.join("started.marker");
    let config = root.join("stackhand.yaml");
    fs::write(
        &config,
        format!(
            "version: 1\nprocesses:\n{}  worker:\n    command: [./missing/program]\n",
            marker_process(&marker)
        ),
    )
    .expect("path configuration writes");

    let output = run_project(&root, Some(&config), &[]);
    assert_failure_without_start(
        &output,
        &marker,
        &["Process 'worker'", "./missing/program", "missing/program"],
    );

    fs::remove_dir_all(root).ok();
}

#[test]
fn dependency_cycle_reports_affected_processes_without_starting() {
    let root = unique_directory("cycle");
    let marker = root.join("started.marker");
    let config = root.join("stackhand.yaml");
    fs::write(
        &config,
        format!(
            "version: 1\nprocesses:\n{}  first:\n    depends_on:\n      second: started\n    command: [/usr/bin/true]\n  second:\n    depends_on:\n      first: started\n    command: [/usr/bin/true]\n",
            marker_process(&marker)
        ),
    )
    .expect("cycle configuration writes");

    let output = run_project(&root, Some(&config), &[]);
    assert_failure_without_start(&output, &marker, &["dependency cycle", "first", "second"]);

    fs::remove_dir_all(root).ok();
}

#[test]
fn null_process_override_reports_the_layer_and_process_without_starting() {
    let root = unique_directory("null-override");
    let marker = root.join("started.marker");
    let config = root.join("stackhand.yaml");
    fs::write(
        &config,
        format!(
            "version: 1\nprocesses:\n{}profiles:\n  broken:\n    overrides:\n      marker: null\n",
            marker_process(&marker)
        ),
    )
    .expect("null override configuration writes");

    let output = run_project(&root, Some(&config), &["broken"]);
    assert_failure_without_start(
        &output,
        &marker,
        &["profile 'broken'", "Process 'marker'", "complete Process"],
    );

    fs::remove_dir_all(root).ok();
}
