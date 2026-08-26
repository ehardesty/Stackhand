use std::fs;
use std::path::PathBuf;
use std::process::Command as StdCommand;
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("stackhand-fixture-{label}-{nanos}"));
    fs::create_dir_all(&dir).expect("fixture directory creates");
    dir
}

/// Prints the direct-command marker, then proves inline environment reached
/// the child through `$FIXTURE_TOKEN`, then stays alive as a Service.
const MARKER_SCRIPT: &str = "printf 'fixture-marker-12345\\n'; printf 'fixture-token-%s\\n' \"$FIXTURE_TOKEN\"; exec sleep 60";

/// One pipeline whose output only exists because both stages ran; then the
/// Run stays alive as a Service.
const SHELL_SCRIPT: &str = "echo fixture-pipeline-lower | tr a-z A-Z; exec sleep 60";

fn fixture_config() -> String {
    format!(
        "version: 1\n\
         processes:\n\
         \x20 - name: hello\n\
         \x20   kind: service\n\
         \x20   terminal: pty\n\
         \x20   input: focused\n\
         \x20   working_dir: ./web\n\
         \x20   env:\n\
         \x20     FIXTURE_TOKEN: stackhand-env-ok\n\
         \x20   command:\n\
         \x20     program: /bin/sh\n\
         \x20     args: [\"-c\", \"{}\"]\n\
         \x20 - name: shelled\n\
         \x20   kind: service\n\
         \x20   terminal: pty\n\
         \x20   input: focused\n\
         \x20   command:\n\
         \x20     shell: {}\n",
        MARKER_SCRIPT.replace('"', "\\\""),
        SHELL_SCRIPT,
    )
}

#[test]
fn one_configured_service_runs_end_to_end() {
    let dir = unique_dir("project");
    let nested = dir.join("web");
    fs::create_dir_all(&nested).expect("working directory creates");
    let config_path = dir.join("stackhand.yaml");
    fs::write(&config_path, fixture_config()).expect("config writes");

    let output = StdCommand::new(env!("CARGO_BIN_EXE_stackhand"))
        .arg("--fixture-project")
        .arg(&config_path)
        .output()
        .expect("the fixture binary runs");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "fixture failed: {stdout} {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("fixture-started-ok"), "{stdout}");
    // The fixture prints this checkpoint only after the direct command's
    // marker, the inline-environment token, and the shell pipeline's
    // transformed output all reached their consoles.
    assert!(stdout.contains("fixture-output-ok"), "{stdout}");
    assert!(stdout.contains("fixture-shutdown-ok"), "{stdout}");

    fs::remove_dir_all(&dir).ok();
}

/// Broken YAML must fail before any Process starts: the fixture prints its
/// startup checkpoint only after every Process reaches its active lifecycle,
/// so an absent checkpoint plus a failed exit means zero Processes ran.
#[test]
fn an_unknown_field_starts_nothing_and_fails_clearly() {
    let dir = unique_dir("unknown-field");
    let config_path = dir.join("stackhand.yaml");
    fs::write(
        &config_path,
        "version: 1\nprocesses:\n  - name: hello\n    comand:\n      program: /bin/true\n",
    )
    .expect("config writes");

    let output = StdCommand::new(env!("CARGO_BIN_EXE_stackhand"))
        .arg("--fixture-project")
        .arg(&config_path)
        .output()
        .expect("the fixture binary runs");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown field"),
        "diagnostic was not clear: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("fixture-started-ok"), "{stdout}");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn duplicate_process_names_start_nothing_and_fail_clearly() {
    let dir = unique_dir("duplicate-name");
    let config_path = dir.join("stackhand.yaml");
    fs::write(
        &config_path,
        "version: 1\nprocesses:\n  - name: hello\n    command: {program: /bin/true}\n  - name: hello\n    command: {program: /bin/true}\n",
    )
    .expect("config writes");

    let output = StdCommand::new(env!("CARGO_BIN_EXE_stackhand"))
        .arg("--fixture-project")
        .arg(&config_path)
        .output()
        .expect("the fixture binary runs");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("duplicate Process name 'hello'"),
        "diagnostic was not clear: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("fixture-started-ok"), "{stdout}");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_invalid_project_starts_nothing_and_fails_clearly() {
    let dir = unique_dir("invalid");
    let config_path = dir.join("stackhand.yaml");
    fs::write(
        &config_path,
        "version: 2\nprocesses:\n  - name: hello\n    command:\n      program: /bin/true\n",
    )
    .expect("config writes");

    let output = StdCommand::new(env!("CARGO_BIN_EXE_stackhand"))
        .arg("--fixture-project")
        .arg(&config_path)
        .output()
        .expect("the fixture binary runs");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unsupported schema version 2"),
        "diagnostic was not clear: {stderr}"
    );

    fs::remove_dir_all(&dir).ok();
}
