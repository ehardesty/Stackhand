use super::*;

use crate::config::{ResolutionRequest, load, resolve};
use crate::model::CommandForm;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_directory(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_nanos();
    let directory = std::env::temp_dir().join(format!("stackhand-env-{label}-{unique}"));
    fs::create_dir_all(&directory).expect("environment test directory creates");
    directory
}

fn environment_files(
    directory: &Path,
    file_names: &[&str],
) -> Result<BTreeMap<String, String>, ConfigError> {
    load_files(
        directory,
        &file_names
            .iter()
            .map(|name| (*name).to_string())
            .collect::<Vec<_>>(),
        "Project",
    )
}

#[test]
fn loads_the_documented_literal_environment_grammar() {
    let directory = unique_directory("grammar");
    let contents = format!(
        r#"# a comment

PLAIN=literal value
export EXPORTED=exported
EMPTY=
EMPTY_SINGLE=''
EMPTY_DOUBLE=""
SINGLE='$HOME; $(echo never); *; # stays literal'
DOUBLE="line\n\t\"$HOME; *\\\r"
TRIMMED=   padded value   {horizontal}
REPEATED=old
REPEATED=new
"#,
        horizontal = "\t",
    );
    fs::write(directory.join("values.env"), contents).expect("environment file writes");

    let values = environment_files(&directory, &["values.env"]).expect("grammar is valid");
    assert_eq!(values.get("PLAIN"), Some(&"literal value".to_string()));
    assert_eq!(values.get("EXPORTED"), Some(&"exported".to_string()));
    assert_eq!(values.get("EMPTY"), Some(&String::new()));
    assert_eq!(values.get("EMPTY_SINGLE"), Some(&String::new()));
    assert_eq!(values.get("EMPTY_DOUBLE"), Some(&String::new()));
    assert_eq!(
        values.get("SINGLE"),
        Some(&"$HOME; $(echo never); *; # stays literal".to_string())
    );
    assert_eq!(
        values.get("DOUBLE"),
        Some(&"line\n\t\"$HOME; *\\\r".to_string())
    );
    assert_eq!(values.get("TRIMMED"), Some(&"padded value".to_string()));
    assert_eq!(values.get("REPEATED"), Some(&"new".to_string()));

    fs::remove_dir_all(directory).ok();
}

#[test]
fn invalid_inline_environment_diagnostics_exclude_the_value() {
    let directory = unique_directory("inline-error");
    let secret = "inline-secret-sentinel";
    let config = directory.join("stackhand.yaml");
    fs::write(
        &config,
        format!(
            "version: 1\nprocesses:\n  child:\n    environment:\n      SECRET_VALUE: [{secret}]\n    command: [/usr/bin/true]\n"
        ),
    )
    .expect("configuration writes");

    let error = load(&config).expect_err("non-string inline environment values fail");
    assert!(error.message.contains("Process 'child'"), "{error}");
    assert!(error.message.contains("base Project"), "{error}");
    assert!(error.message.contains("SECRET_VALUE"), "{error}");
    assert!(!error.message.contains(secret), "{error}");

    fs::remove_dir_all(directory).ok();
}

#[test]
fn process_profile_and_local_environment_errors_name_their_source_without_values() {
    let directory = unique_directory("layered-errors");
    let config = directory.join("stackhand.yaml");
    let profile_secret = "profile-secret-sentinel";
    fs::write(
        &config,
        format!(
            "version: 1\nprocesses:\n  child:\n    command: [/usr/bin/true]\n    profiles:\n      profile:\n        environment:\n          SECRET_VALUE: [{profile_secret}]\n"
        ),
    )
    .expect("profile configuration writes");

    let profile_error = resolve(ResolutionRequest::explicit_with_profile(
        &config,
        Some("profile".to_string()),
    ))
    .expect_err("invalid profile environment values fail");
    assert!(profile_error.message.contains("profile 'profile'"));
    assert!(profile_error.message.contains("SECRET_VALUE"));
    assert!(!profile_error.message.contains(profile_secret));

    fs::write(
        &config,
        "version: 1\nprocesses:\n  child:\n    command: [/usr/bin/true]\n",
    )
    .expect("base configuration writes");
    let local_secret = "local-secret-sentinel";
    let local = directory.join("stackhand.local.yaml");
    fs::write(
        &local,
        format!("processes:\n  child:\n    environment:\n      SECRET_VALUE: [{local_secret}]\n"),
    )
    .expect("local configuration writes");

    let local_error = resolve(ResolutionRequest::Discover {
        start_dir: Some(directory.clone()),
        profile: None,
    })
    .expect_err("invalid local environment values fail");
    assert!(
        local_error
            .message
            .contains(&format!("local override '{}'", local.display()))
    );
    assert!(local_error.message.contains("SECRET_VALUE"));
    assert!(!local_error.message.contains(local_secret));

    fs::remove_dir_all(directory).ok();
}

#[test]
fn later_environment_files_replace_earlier_values() {
    let directory = unique_directory("order");
    fs::write(directory.join("first.env"), "SHARED=first\nFIRST=first\n")
        .expect("first environment file writes");
    fs::write(
        directory.join("second.env"),
        "SHARED=second\nSECOND=second\n",
    )
    .expect("second environment file writes");

    let values = environment_files(&directory, &["first.env", "second.env"])
        .expect("ordered environment files load");
    assert_eq!(values.get("SHARED"), Some(&"second".to_string()));
    assert_eq!(values.get("FIRST"), Some(&"first".to_string()));
    assert_eq!(values.get("SECOND"), Some(&"second".to_string()));

    fs::remove_dir_all(directory).ok();
}

#[test]
fn invalid_keys_and_quotes_identify_the_file_and_line() {
    let cases = [
        ("BAD-KEY=value\n", "invalid environment key"),
        ("NO_EQUALS\n", "expected KEY=VALUE"),
        ("BROKEN='value\n", "unterminated quoted value"),
        ("BROKEN=value\"with-quote\n", "quotes must surround"),
        ("BROKEN=\"value\\q\"\n", "unsupported escape"),
        (
            "BROKEN=\"value\" trailing\n",
            "must end at the closing quote",
        ),
    ];
    for (index, (content, detail)) in cases.into_iter().enumerate() {
        let directory = unique_directory(&format!("invalid-{index}"));
        let path = directory.join("invalid.env");
        fs::write(&path, format!("# header\n{content}")).expect("invalid environment file writes");
        let error = environment_files(&directory, &["invalid.env"])
            .expect_err("invalid environment syntax must fail");
        assert!(
            error.message.contains(&path.display().to_string()),
            "{error}"
        );
        assert!(error.message.contains("line 2"), "{error}");
        assert!(error.message.contains(detail), "{error}");
        fs::remove_dir_all(directory).ok();
    }
}

#[test]
fn missing_and_invalid_utf8_files_are_rejected_with_their_paths() {
    let directory = unique_directory("read-errors");
    let missing = directory.join("missing.env");
    let missing_error = environment_files(&directory, &["missing.env"])
        .expect_err("a missing environment file must fail");
    assert!(
        missing_error
            .message
            .contains(&missing.display().to_string())
    );
    assert!(missing_error.message.contains("could not read"));

    let invalid = directory.join("invalid.env");
    fs::write(&invalid, [0xff, 0xfe, 0xfd]).expect("invalid UTF-8 file writes");
    let invalid_error =
        environment_files(&directory, &["invalid.env"]).expect_err("invalid UTF-8 must fail");
    assert!(
        invalid_error
            .message
            .contains(&invalid.display().to_string())
    );
    assert!(invalid_error.message.contains("invalid UTF-8"));

    fs::remove_dir_all(directory).ok();
}

#[test]
fn profile_and_local_nulls_remove_environment_file_values() {
    let directory = unique_directory("null-layer");
    fs::write(
        directory.join("project.env"),
        "FROM_FILE=project\nLOCAL_FILE=project\n",
    )
    .expect("project environment file writes");
    let config = directory.join("stackhand.yaml");
    fs::write(
        &config,
        r#"version: 1
env_files: [project.env]
processes:
  child:
    command: [/usr/bin/true]
    profiles:
      profile:
        environment:
          FROM_FILE: null
"#,
    )
    .expect("configuration writes");
    fs::write(
        directory.join("stackhand.local.yaml"),
        "processes:\n  child:\n    environment:\n      LOCAL_FILE: null\n",
    )
    .expect("local override writes");

    let resolution = resolve(ResolutionRequest::Discover {
        start_dir: Some(directory.clone()),
        profile: Some("profile".to_string()),
    })
    .expect("profile and local nulls are valid");
    assert!(resolution.project().processes()[0].env.is_empty());

    fs::remove_dir_all(directory).ok();
}

#[test]
fn project_and_process_environment_files_reach_a_real_child_from_the_base_directory() {
    let directory = unique_directory("child");
    let nested = directory.join("config");
    fs::create_dir_all(&nested).expect("nested configuration directory creates");
    let command_substitution_marker = nested.join("command-substitution-marker");
    fs::write(
        directory.join("project-one.env"),
        format!(
            "SHARED=project-one\nPROJECT_ONLY=project\nDOLLAR=$HOME\nSUBSTITUTION=$(touch {})\nGLOB=*.txt\nMETA=semi;echo never\n",
            command_substitution_marker.display()
        ),
    )
    .expect("first project environment file writes");
    fs::write(nested.join("project-two.env"), "SHARED=project-two\n")
        .expect("second project environment file writes");
    fs::write(
        nested.join("process-one.env"),
        "SHARED=process-one\nPROCESS_ONLY=process\n",
    )
    .expect("first Process environment file writes");
    fs::write(nested.join("process-two.env"), "SHARED=process-two\n")
        .expect("second Process environment file writes");

    let config = nested.join("stackhand.yaml");
    fs::write(
        &config,
        r#"version: 1
env_files:
  - ../project-one.env
  - project-two.env
processes:
  child:
    env_files:
      - process-one.env
      - process-two.env
    environment:
      INLINE: inline
    command: [/bin/sh, -c, 'printf "%s|%s|%s|%s|%s|%s|%s|%s" "$SHARED" "$PROJECT_ONLY" "$PROCESS_ONLY" "$INLINE" "$DOLLAR" "$SUBSTITUTION" "$GLOB" "$META"']
"#,
    )
    .expect("configuration writes");

    let project = load(&config).expect("environment files load through configuration");
    let process = &project.processes()[0];
    assert_eq!(
        process.env,
        vec![
            ("DOLLAR".to_string(), "$HOME".to_string()),
            ("GLOB".to_string(), "*.txt".to_string()),
            ("INLINE".to_string(), "inline".to_string()),
            ("META".to_string(), "semi;echo never".to_string()),
            ("PROCESS_ONLY".to_string(), "process".to_string()),
            ("PROJECT_ONLY".to_string(), "project".to_string()),
            ("SHARED".to_string(), "process-two".to_string()),
            (
                "SUBSTITUTION".to_string(),
                format!("$(touch {})", command_substitution_marker.display()),
            ),
        ]
    );

    let CommandForm::Direct { program, args } = &process.command else {
        panic!("the child fixture uses a direct command");
    };
    let mut command = StdCommand::new(program);
    command.args(args).current_dir(&process.working_dir);
    for (key, value) in &process.env {
        command.env(key, value);
    }
    let output = command.output().expect("real child starts");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).expect("child output is UTF-8"),
        format!(
            "process-two|project|process|inline|$HOME|$(touch {})|*.txt|semi;echo never",
            command_substitution_marker.display()
        )
    );
    assert!(!command_substitution_marker.exists());

    fs::remove_dir_all(directory).ok();
}

#[test]
fn layered_environment_precedence_and_removal_reach_a_real_child() {
    let directory = unique_directory("layered");
    fs::write(
        directory.join("project.env"),
        "LAYERED=project-file\nPROJECT_ONLY=project\nREMOVE_PROJECT=project\n",
    )
    .expect("project environment file writes");
    fs::write(
        directory.join("process.env"),
        "LAYERED=process-file\nPROCESS_ONLY=process\nREMOVE_PROCESS=process\n",
    )
    .expect("Process environment file writes");
    let config = directory.join("stackhand.yaml");
    fs::write(
        &config,
        r#"version: 1
env_files: [project.env]
processes:
  child:
    env_files: [process.env]
    environment:
      LAYERED: base
      BASE_ONLY: base
      REMOVE_PARENT: null
      REMOVE_BASE: base
    command: [/bin/sh, -c, 'printf "%s|%s|%s|%s|%s|%s|%s|%s|%s|%s|%s|%s" "$LAYERED" "$PROJECT_ONLY" "$PROCESS_ONLY" "$BASE_ONLY" "$PROFILE_ONLY" "$LOCAL_ONLY" "$PARENT_ONLY" "${REMOVE_PARENT-unset}" "${REMOVE_PROJECT-unset}" "${REMOVE_PROCESS-unset}" "${REMOVE_BASE-unset}" "${REMOVE_PROFILE-unset}"']
    profiles:
      profile:
        environment:
          LAYERED: profile
          REMOVE_PROJECT: null
          REMOVE_BASE: null
          REMOVE_PROFILE: profile
          PROFILE_ONLY: profile
"#,
    )
    .expect("layered configuration writes");
    fs::write(
        directory.join("stackhand.local.yaml"),
        r#"processes:
  child:
    profiles:
      profile:
        environment:
          LAYERED: local
          REMOVE_PROCESS: null
          REMOVE_PROFILE: null
          LOCAL_ONLY: local
"#,
    )
    .expect("local override writes");

    let resolution = resolve(ResolutionRequest::Discover {
        start_dir: Some(directory.clone()),
        profile: Some("profile".to_string()),
    })
    .expect("layered configuration resolves");
    let process = &resolution.project().processes()[0];
    assert_eq!(
        process.env,
        vec![
            ("BASE_ONLY".to_string(), "base".to_string()),
            ("LAYERED".to_string(), "local".to_string()),
            ("LOCAL_ONLY".to_string(), "local".to_string()),
            ("PROCESS_ONLY".to_string(), "process".to_string()),
            ("PROFILE_ONLY".to_string(), "profile".to_string()),
            ("PROJECT_ONLY".to_string(), "project".to_string()),
        ]
    );
    assert_eq!(
        process.env_remove,
        vec![
            "REMOVE_BASE".to_string(),
            "REMOVE_PARENT".to_string(),
            "REMOVE_PROCESS".to_string(),
            "REMOVE_PROFILE".to_string(),
            "REMOVE_PROJECT".to_string(),
        ]
    );

    let CommandForm::Direct { program, args } = &process.command else {
        panic!("the layered fixture uses a direct command");
    };
    let mut command = StdCommand::new(program);
    command.args(args).current_dir(&process.working_dir);
    command.env("LAYERED", "parent");
    command.env("REMOVE_PARENT", "parent");
    command.env("PARENT_ONLY", "parent");
    for (key, value) in &process.env {
        command.env(key, value);
    }
    for key in &process.env_remove {
        command.env_remove(key);
    }
    let output = command.output().expect("layered real child starts");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).expect("child output is UTF-8"),
        "local|project|process|base|profile|local|parent|unset|unset|unset|unset|unset"
    );

    fs::remove_dir_all(directory).ok();
}
