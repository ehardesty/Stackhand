//! Configuration: one YAML version 1 file becomes one validated
//! [`EffectiveProject`] or a structured error before any Process starts.
//!
//! Profiles, overlays, environment files, and interpolation are deferred;
//! Milestone 1 supports one base configuration.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

use crate::model::{
    Autostart, CommandForm, EffectiveProject, Enabled, InputPolicy, ProcessKind, ProcessSpec,
    TerminalMode,
};

/// Load and validate the Project at `path`. Relative working directories
/// resolve from the configuration file's directory.
pub fn load(path: &Path) -> Result<EffectiveProject, ConfigError> {
    let base_dir = path.parent().unwrap_or(Path::new("."));
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("could not read {}", path.display()))
        .map_err(config_error)?;
    let file: ConfigFile = serde_yaml::from_str(&text)
        .map_err(|error| config_error(anyhow::anyhow!(format_yaml_error(&error))))?;
    if file.version != 1 {
        return Err(config_error(anyhow::anyhow!(
            "unsupported schema version {}: this Stackhand reads version 1",
            file.version
        )));
    }
    let processes = file
        .processes
        .iter()
        .map(|process| build_spec(process, base_dir))
        .collect::<Result<Vec<_>, ConfigError>>()?;
    EffectiveProject::new(processes).map_err(|error| match error {
        crate::model::ProjectError::DuplicateName(name) => {
            config_error(anyhow::anyhow!("duplicate Process name '{name}'"))
        }
    })
}

fn config_error(error: anyhow::Error) -> ConfigError {
    ConfigError {
        message: format!("{error:#}"),
    }
}

/// One bounded, user-facing configuration failure.
#[derive(Debug, Clone)]
pub struct ConfigError {
    pub message: String,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ConfigError {}

fn format_yaml_error(error: &serde_yaml::Error) -> String {
    match error.location() {
        Some(location) => format!(
            "invalid configuration at line {}, column {}: {}",
            location.line(),
            location.column(),
            error
        ),
        None => format!("invalid configuration: {error}"),
    }
}

fn build_spec(process: &ProcessFile, base_dir: &Path) -> Result<ProcessSpec, ConfigError> {
    let name = process.name.clone();
    let fail = |message: String| Err(ConfigError { message });
    let kind = match process.kind.as_deref() {
        None | Some("service") => ProcessKind::Service,
        Some("one-shot") => ProcessKind::OneShot,
        Some(other) => {
            return fail(format!(
                "Process '{name}': invalid kind '{other}' (use 'service' or 'one-shot')"
            ));
        }
    };
    let command_form = match (&process.command.program, &process.command.shell) {
        (Some(program), None) => CommandForm::Direct {
            program: std::ffi::OsString::from(program),
            args: process
                .command
                .args
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(std::ffi::OsString::from)
                .collect(),
        },
        (None, Some(shell)) => CommandForm::Shell {
            text: shell.clone(),
        },
        (Some(_), Some(_)) => {
            return fail(format!(
                "Process '{name}': define exactly one of 'program' or 'shell'"
            ));
        }
        (None, None) => {
            return fail(format!(
                "Process '{name}': define 'program' or 'shell' under 'command'"
            ));
        }
    };
    let working_dir = match &process.working_dir {
        Some(dir) => {
            let candidate = PathBuf::from(dir);
            if candidate.is_absolute() {
                candidate
            } else {
                base_dir.join(candidate)
            }
        }
        None => base_dir.to_path_buf(),
    };
    if !working_dir.is_dir() {
        return fail(format!(
            "Process '{name}': working directory '{}' does not exist",
            working_dir.display()
        ));
    }
    let terminal_mode = match process.terminal.as_deref() {
        None | Some("pipe") => TerminalMode::Pipe,
        Some("pty") => TerminalMode::Pty,
        Some(other) => {
            return fail(format!(
                "Process '{name}': invalid terminal mode '{other}' (use 'pipe' or 'pty')"
            ));
        }
    };
    let input_policy = match process.input.as_deref() {
        None | Some("disabled") => InputPolicy::Disabled,
        Some("focused") => InputPolicy::Focused,
        Some(other) => {
            return fail(format!(
                "Process '{name}': invalid input policy '{other}' (use 'focused' or 'disabled')"
            ));
        }
    };
    Ok(ProcessSpec {
        name,
        kind,
        enabled: Enabled::flag(process.enabled.unwrap_or(true)),
        autostart: Autostart::flag(process.autostart.unwrap_or(true)),
        command: command_form,
        working_dir,
        env: process
            .env
            .as_ref()
            .map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default(),
        terminal_mode,
        input_policy,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    version: u64,
    #[serde(default)]
    processes: Vec<ProcessFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessFile {
    name: String,
    kind: Option<String>,
    enabled: Option<bool>,
    autostart: Option<bool>,
    working_dir: Option<String>,
    env: Option<std::collections::BTreeMap<String, String>>,
    terminal: Option<String>,
    input: Option<String>,
    command: CommandFile,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandFile {
    program: Option<String>,
    args: Option<Vec<String>>,
    shell: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
    fn one_direct_command_service_loads_with_defaults() {
        let project = write_and_load(
            "ok",
            "version: 1\nprocesses:\n  - name: web\n    command:\n      program: /bin/sleep\n      args: [\"1\"]\n",
        )
        .expect("valid config");
        assert_eq!(project.processes().len(), 1);
        let web = &project.processes()[0];
        assert_eq!(web.name, "web");
        assert_eq!(web.kind, ProcessKind::Service);
        assert_eq!(web.enabled, Enabled::Yes);
        assert_eq!(web.autostart, Autostart::Yes);
        assert_eq!(web.terminal_mode, TerminalMode::Pipe);
        assert_eq!(web.input_policy, InputPolicy::Disabled);
    }

    #[test]
    fn relative_working_directories_resolve_from_the_config_directory() {
        let dir = std::env::temp_dir().join("stackhand-config-cwd");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("web")).expect("working directory creates");
        let path = dir.join("stackhand.yaml");
        fs::write(&path, "version: 1\nprocesses:\n  - name: web\n    working_dir: ./web\n    command:\n      program: /bin/true\n")
            .expect("config writes");
        let project = load(&path).expect("valid config");
        let _ = fs::remove_dir_all(&dir);
        let expected = std::env::temp_dir()
            .join("stackhand-config-cwd")
            .join("web");
        assert_eq!(project.processes()[0].working_dir, expected);
    }

    #[test]
    fn missing_working_directories_are_rejected_with_the_process_name() {
        let error = write_and_load(
            "missing-cwd",
            "version: 1\nprocesses:\n  - name: web\n    working_dir: ./nope\n    command:\n      program: /bin/true\n",
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
        let error = write_and_load("version", "version: 2\nprocesses: []\n")
            .expect_err("version 2 must fail");
        assert!(error.message.contains("unsupported schema version 2"));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let error = write_and_load(
            "unknown",
            "version: 1\nprocesses:\n  - name: web\n    comand:\n      program: /bin/true\n",
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
  - name: web
    command: {program: /bin/sleep, args: [\"1\"]}
  - name: worker
    autostart: false
    command: {program: /bin/sleep, args: [\"1\"]}
  - name: debug
    enabled: false
    command: {program: /bin/sleep, args: [\"1\"]}
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
    fn duplicate_names_are_rejected() {
        let error = write_and_load(
            "dup",
            "version: 1\nprocesses:\n  - name: web\n    command: {program: /bin/true}\n  - name: web\n    command: {program: /bin/true}\n",
        )
        .expect_err("duplicates must fail");
        assert!(error.message.contains("duplicate Process name 'web'"));
    }

    #[test]
    fn command_forms_are_mutually_exclusive_and_required() {
        let both = write_and_load(
            "both",
            "version: 1\nprocesses:\n  - name: web\n    command:\n      program: /bin/true\n      shell: echo hi\n",
        )
        .expect_err("both forms must fail");
        assert!(both.message.contains("exactly one"), "{}", both.message);

        let neither = write_and_load(
            "neither",
            "version: 1\nprocesses:\n  - name: web\n    command: {}\n",
        )
        .expect_err("no form must fail");
        assert!(neither.message.contains("'program' or 'shell'"));
    }

    #[test]
    fn invalid_kinds_terminal_modes_and_input_policies_are_rejected() {
        for (label, field) in [
            ("kind", "kind: cron"),
            ("terminal", "terminal: serial"),
            ("input", "input: always"),
        ] {
            let error = write_and_load(
                label,
                &format!("version: 1\nprocesses:\n  - name: web\n    {field}\n    command:\n      program: /bin/true\n"),
            )
            .expect_err("invalid value must fail");
            assert!(
                error.message.contains("invalid"),
                "{field}: {}",
                error.message
            );
        }
    }
}
