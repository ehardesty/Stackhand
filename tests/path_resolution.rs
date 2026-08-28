use assert_cmd::Command;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_directory(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_nanos();
    let directory = std::env::temp_dir().join(format!("stackhand-cli-paths-{label}-{unique}"));
    fs::create_dir_all(&directory).expect("test directory creates");
    directory
}

fn run_validate(current_dir: &Path, explicit: Option<&Path>) -> std::process::Output {
    let mut command = Command::cargo_bin("stackhand").expect("Stackhand binary builds");
    command
        .current_dir(current_dir)
        .args(["config", "validate"]);
    if let Some(path) = explicit {
        command.arg(path);
    }
    command.output().expect("config validate runs")
}

#[test]
fn compiled_validation_uses_one_base_anchor_from_three_current_directories() {
    let base = unique_directory("base");
    let nested = base.join("nested").join("deep");
    let unrelated = unique_directory("unrelated");
    fs::create_dir_all(&nested).expect("nested directories create");
    fs::create_dir_all(base.join("bin")).expect("program directory creates");
    fs::create_dir_all(base.join("work")).expect("working directory creates");
    fs::write(base.join("bin/child"), "#!/bin/sh\n").expect("relative program writes");
    fs::write(base.join("project.env"), "PROJECT=value\n")
        .expect("Project environment file writes");
    fs::write(base.join("process.env"), "PROCESS=value\n")
        .expect("Process environment file writes");
    let config = base.join("stackhand.yaml");
    fs::write(
        &config,
        "version: 1\nenv_files: [project.env]\nprocesses:\n  child:\n    cwd: work\n    env_files: [process.env]\n    command: [./bin/child]\n  path-bare:\n    command: [sh, -c, 'exit 0']\n",
    )
    .expect("path fixture writes");

    let explicit = fs::canonicalize(&config).expect("configuration path canonicalizes");
    let from_base = run_validate(&base, None);
    let from_nested = run_validate(&nested, None);
    let from_unrelated = run_validate(&unrelated, Some(&explicit));
    for output in [&from_base, &from_nested, &from_unrelated] {
        assert!(output.status.success(), "{output:?}");
    }
    assert_eq!(from_base.stdout, from_nested.stdout);
    assert_eq!(from_base.stdout, from_unrelated.stdout);
    assert!(String::from_utf8_lossy(&from_base.stdout).contains(&explicit.display().to_string()));

    fs::remove_dir_all(base).ok();
    fs::remove_dir_all(unrelated).ok();
}
