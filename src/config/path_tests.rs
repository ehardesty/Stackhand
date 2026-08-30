use super::*;
use crate::config::load;
use crate::model::{CommandForm, ReadinessProbe};
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_directory(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_nanos();
    let directory = std::env::temp_dir().join(format!("stackhand-config-paths-{label}-{unique}"));
    fs::create_dir_all(&directory).expect("test directory creates");
    directory
}

#[test]
fn relative_program_and_probe_paths_use_the_base_project_directory() {
    let dir = unique_directory("relative");
    fs::create_dir_all(dir.join("bin")).expect("program directory creates");
    fs::create_dir_all(dir.join("checks")).expect("probe directory creates");
    fs::create_dir_all(dir.join("process-cwd")).expect("process cwd creates");
    fs::create_dir_all(dir.join("probe-cwd")).expect("probe cwd creates");
    fs::write(dir.join("bin/process"), "#!/bin/sh\n").expect("process program writes");
    fs::write(dir.join("checks/probe"), "#!/bin/sh\n").expect("probe program writes");
    let path = dir.join("stackhand.yaml");
    fs::write(
        &path,
        "version: 1\nprocesses:\n  web:\n    cwd: process-cwd\n    command: [./bin/process]\n    ready:\n      exec:\n        command: [./checks/probe]\n        cwd: probe-cwd\n",
    )
    .expect("relative path configuration writes");

    let project = load(&path).expect("relative paths resolve");
    let process = &project.processes()[0];
    assert_eq!(process.working_dir, dir.join("process-cwd"));
    assert_eq!(
        process.command,
        CommandForm::Direct {
            program: dir.join("bin/process").into_os_string(),
            args: Vec::new(),
        }
    );
    let Some(ReadinessProbe::Exec {
        command,
        working_dir,
        ..
    }) = process
        .readiness
        .as_ref()
        .and_then(|readiness| readiness.checks.first())
        .map(|check| &check.probe)
    else {
        panic!("the Process has an exec readiness probe");
    };
    assert_eq!(
        command,
        &CommandForm::Direct {
            program: dir.join("checks/probe").into_os_string(),
            args: Vec::new(),
        }
    );
    assert_eq!(working_dir, &Some(dir.join("probe-cwd")));

    fs::remove_dir_all(dir).ok();
}

#[test]
fn process_profiles_and_local_overrides_keep_the_base_path_anchor() {
    let dir = unique_directory("layers");
    fs::create_dir_all(dir.join("bin")).expect("program directory creates");
    fs::create_dir_all(dir.join("profile-cwd")).expect("profile cwd creates");
    fs::create_dir_all(dir.join("local-cwd")).expect("local cwd creates");
    fs::write(dir.join("bin/profile"), "#!/bin/sh\n").expect("profile program writes");
    fs::write(dir.join("bin/local"), "#!/bin/sh\n").expect("local program writes");
    let path = dir.join("stackhand.yaml");
    fs::write(
        &path,
        "version: 1\nprocesses:\n  web:\n    command: [/usr/bin/true]\n    profiles:\n      profile:\n        cwd: profile-cwd\n        command: [./bin/profile]\n",
    )
    .expect("profile configuration writes");
    fs::write(
        dir.join("stackhand.local.yaml"),
        "processes:\n  web:\n    profiles:\n      profile:\n        cwd: local-cwd\n        command: [./bin/local]\n",
    )
    .expect("local configuration writes");

    let resolution = resolve(ResolutionRequest::Discover {
        start_dir: Some(dir.join("nested")),
        profile: Some("profile".to_string()),
    })
    .expect("profile and local paths resolve");
    let process = &resolution.project().processes()[0];
    assert_eq!(process.working_dir, dir.join("local-cwd"));
    assert_eq!(
        process.command,
        CommandForm::Direct {
            program: dir.join("bin/local").into_os_string(),
            args: Vec::new(),
        }
    );

    fs::remove_dir_all(dir).ok();
}

#[test]
fn missing_relative_programs_fail_before_startup() {
    let dir = unique_directory("missing");
    let path = dir.join("stackhand.yaml");
    fs::write(
        &path,
        "version: 1\nprocesses:\n  web:\n    command: [./bin/missing]\n",
    )
    .expect("missing program configuration writes");

    let error = load(&path).expect_err("a missing relative program must fail");
    assert!(error.message.contains("Process 'web'"), "{error}");
    assert!(error.message.contains("./bin/missing"), "{error}");
    assert!(
        error
            .message
            .contains(&dir.join("bin/missing").display().to_string())
    );

    fs::remove_dir_all(dir).ok();
}

#[test]
fn missing_relative_probe_programs_fail_before_startup() {
    let dir = unique_directory("missing-probe");
    let path = dir.join("stackhand.yaml");
    fs::write(
        &path,
        "version: 1\nprocesses:\n  web:\n    command: [/usr/bin/true]\n    ready:\n      exec:\n        command: [./checks/missing]\n",
    )
    .expect("missing probe configuration writes");

    let error = load(&path).expect_err("a missing relative probe program must fail");
    assert!(
        error.message.contains("Process 'web': ready: exec"),
        "{error}"
    );
    assert!(error.message.contains("./checks/missing"), "{error}");
    assert!(
        error
            .message
            .contains(&dir.join("checks/missing").display().to_string())
    );

    fs::remove_dir_all(dir).ok();
}

#[test]
fn bare_program_names_remain_available_to_path_lookup() {
    let dir = unique_directory("bare");
    let path = dir.join("stackhand.yaml");
    fs::write(
        &path,
        "version: 1\nprocesses:\n  web:\n    command: [sh, -c, 'exit 0']\n",
    )
    .expect("bare program configuration writes");

    let project = load(&path).expect("bare program names do not need a local file");
    assert_eq!(
        project.processes()[0].command,
        CommandForm::Direct {
            program: "sh".into(),
            args: vec!["-c".into(), "exit 0".into()],
        }
    );

    fs::remove_dir_all(dir).ok();
}
