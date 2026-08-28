use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::*;

fn unique_directory(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_nanos();
    let directory = std::env::temp_dir().join(format!("stackhand-local-{label}-{unique}"));
    fs::create_dir_all(&directory).expect("local test directory creates");
    directory
}

fn write_base(directory: &Path, command: &str) -> PathBuf {
    let path = directory.join(BASE_FILE_NAME);
    fs::write(
        &path,
        format!(
            "version: 1\nprocesses:\n  web:\n    command: [/bin/echo, {command}]\nprofiles:\n  profile:\n    overrides:\n      web:\n        command: [/bin/echo, profile]\n"
        ),
    )
    .expect("base configuration writes");
    path
}

#[test]
fn discovered_resolution_uses_only_an_existing_same_directory_override() {
    let directory = unique_directory("discovery");
    let nested = directory.join("nested");
    fs::create_dir_all(&nested).expect("nested directory creates");
    let base = write_base(&directory, "base");

    let without_local = resolve(ResolutionRequest::Discover {
        start_dir: Some(nested.clone()),
        profiles: Vec::new(),
    })
    .expect("discovery without a local override succeeds");
    assert_eq!(without_local.sources.base, base);
    assert_eq!(without_local.sources.local, None);
    assert_eq!(
        without_local.project().processes()[0].command,
        CommandForm::Direct {
            program: "/bin/echo".into(),
            args: vec!["base".into()],
        }
    );

    let local = directory.join(LOCAL_FILE_NAME);
    fs::write(
        &local,
        "processes:\n  web:\n    command: [/bin/echo, local]\n",
    )
    .expect("local override writes");
    let with_local = resolve(ResolutionRequest::Discover {
        start_dir: Some(nested),
        profiles: vec!["profile".to_string()],
    })
    .expect("discovery with a local override succeeds");
    assert_eq!(with_local.sources.base, base);
    assert_eq!(with_local.sources.local, Some(local));
    assert_eq!(
        with_local.project().processes()[0].command,
        CommandForm::Direct {
            program: "/bin/echo".into(),
            args: vec!["local".into()],
        }
    );

    fs::remove_dir_all(directory).ok();
}

#[test]
fn explicit_resolution_never_loads_a_local_override() {
    let directory = unique_directory("explicit");
    let base = write_base(&directory, "base");
    fs::write(
        directory.join(LOCAL_FILE_NAME),
        "processes:\n  web:\n    command: [/bin/echo, local]\n",
    )
    .expect("local override writes");

    let resolution = resolve(ResolutionRequest::explicit_with_profiles(
        &base,
        ["profile".to_string()],
    ))
    .expect("explicit resolution succeeds");
    assert_eq!(resolution.sources.local, None);
    assert_eq!(
        resolution.project().processes()[0].command,
        CommandForm::Direct {
            program: "/bin/echo".into(),
            args: vec!["profile".into()],
        }
    );

    fs::remove_dir_all(directory).ok();
}

#[test]
fn child_and_parent_local_files_are_not_searched() {
    let outside = unique_directory("outside");
    let directory = outside.join("project");
    let child = directory.join("child");
    fs::create_dir_all(&child).expect("project directories create");
    let base = write_base(&directory, "base");
    fs::write(
        child.join(LOCAL_FILE_NAME),
        "processes:\n  web:\n    command: [/bin/echo, child]\n",
    )
    .expect("child local override writes");
    fs::write(
        outside.join(LOCAL_FILE_NAME),
        "processes:\n  web:\n    command: [/bin/echo, parent]\n",
    )
    .expect("parent local override writes");

    let resolution = resolve(ResolutionRequest::Discover {
        start_dir: Some(child),
        profiles: Vec::new(),
    })
    .expect("discovery succeeds");
    assert_eq!(resolution.sources.base, base);
    assert_eq!(resolution.sources.local, None);
    assert_eq!(
        resolution.project().processes()[0].command,
        CommandForm::Direct {
            program: "/bin/echo".into(),
            args: vec!["base".into()],
        }
    );

    fs::remove_dir_all(outside).ok();
}

#[test]
fn local_override_has_profile_and_schema_version_guards() {
    for (label, local, expected) in [
        (
            "profiles",
            "profiles: {}\n",
            "local override cannot define profiles",
        ),
        (
            "version",
            "version: 2\n",
            "local override cannot change schema version",
        ),
        (
            "processes-null",
            "processes:\n",
            "local override processes cannot be null",
        ),
        (
            "process-null",
            "processes:\n  web:\n",
            "must define a complete Process",
        ),
    ] {
        let directory = unique_directory(&format!("invalid-{label}"));
        write_base(&directory, "base");
        fs::write(directory.join(LOCAL_FILE_NAME), local).expect("invalid local writes");
        let error = resolve(ResolutionRequest::Discover {
            start_dir: Some(directory.clone()),
            profiles: Vec::new(),
        })
        .expect_err("invalid local override must fail");
        assert!(error.message.contains(expected), "{error}");
        fs::remove_dir_all(directory).ok();
    }
}

#[test]
fn invalid_local_content_rejects_the_complete_project() {
    let directory = unique_directory("invalid-content");
    write_base(&directory, "base");
    fs::write(
        directory.join(LOCAL_FILE_NAME),
        "processes:\n  web:\n    unknown_field: true\n",
    )
    .expect("invalid local content writes");
    let error = resolve(ResolutionRequest::Discover {
        start_dir: Some(directory.clone()),
        profiles: Vec::new(),
    })
    .expect_err("invalid local content must fail");
    assert!(error.message.contains("unknown field"), "{error}");
    fs::remove_dir_all(directory).ok();
}

#[test]
fn local_null_values_have_the_same_merge_semantics_as_profile_values() {
    let directory = unique_directory("null");
    let base = directory.join(BASE_FILE_NAME);
    fs::write(
        &base,
        "version: 1
processes:
  web:
    command: [/bin/true]
    environment:
      KEEP: keep
      REMOVE: remove
profiles:
  profile:
    overrides:
      web:
        environment:
          PROFILE: profile
",
    )
    .expect("base configuration writes");
    fs::write(
        directory.join(LOCAL_FILE_NAME),
        "processes:
  web:
    environment:
      REMOVE: null
      PROFILE: null
      LOCAL: local
",
    )
    .expect("local override writes");

    let resolution = resolve(ResolutionRequest::Discover {
        start_dir: Some(directory.clone()),
        profiles: vec!["profile".to_string()],
    })
    .expect("local null values merge successfully");
    assert_eq!(
        resolution.project().processes()[0].env,
        [
            ("KEEP".to_string(), "keep".to_string()),
            ("LOCAL".to_string(), "local".to_string()),
        ]
    );
    fs::remove_dir_all(directory).ok();
}
