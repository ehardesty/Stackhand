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

const MARKER_SCRIPT: &str = "printf 'fixture-marker-12345\\n'; exec sleep 60";

#[test]
fn one_configured_service_runs_end_to_end() {
    let dir = unique_dir("project");
    let nested = dir.join("web");
    fs::create_dir_all(&nested).expect("working directory creates");
    let config = format!(
        "version: 1\n\
         processes:\n\
         \x20 - name: hello\n\
         \x20   kind: service\n\
         \x20   terminal: pty\n\
         \x20   input: focused\n\
         \x20   working_dir: ./web\n\
         \x20   command:\n\
         \x20     program: /bin/sh\n\
         \x20     args: [\"-c\", \"{}\"]\n",
        MARKER_SCRIPT.replace('"', "\\\"")
    );
    let config_path = dir.join("stackhand.yaml");
    fs::write(&config_path, config).expect("config writes");

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
    assert!(stdout.contains("fixture-output-ok"), "{stdout}");
    assert!(stdout.contains("fixture-shutdown-ok"), "{stdout}");

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
