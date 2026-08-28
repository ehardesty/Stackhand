//! Configuration: one YAML version 1 file becomes one validated
//! [`EffectiveProject`] or a structured error before any Process starts.
//!
//! Profiles, overlays, environment files, and interpolation are deferred;
//! Milestone 1 supports one base configuration.

mod readiness;

#[cfg(test)]
mod exit_tests;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

use crate::model::{
    Autostart, CommandForm, DependencyCondition, DependencySpec, EffectiveProject, Enabled,
    InputPolicy, ProcessKind, ProcessSpec, RestartConfig, RestartPolicy, ShellConfig, TerminalMode,
};

const MAX_EXIT_CODE: i32 = 255;

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
    let shell = build_shell(file.settings.as_ref())?;
    let processes = file
        .processes
        .iter()
        .map(|process| build_spec(process, base_dir))
        .collect::<Result<Vec<_>, ConfigError>>()?;
    EffectiveProject::with_shell(processes, shell).map_err(|error| match error {
        crate::model::ProjectError::DuplicateName(name) => {
            config_error(anyhow::anyhow!("duplicate Process name '{name}'"))
        }
        crate::model::ProjectError::UnknownDependency {
            process,
            dependency,
        } => config_error(anyhow::anyhow!(
            "Process '{process}': dependency '{dependency}' does not match any configured Process"
        )),
        crate::model::ProjectError::InvalidCondition {
            process,
            dependency,
            condition,
        } => config_error(anyhow::anyhow!(
            "Process '{process}': dependency '{dependency}' cannot use condition '{condition}': 'exited' and 'completed_successfully' are valid only when the dependency Process is a One-shot, and 'ready' only when it is a Service"
        )),
        crate::model::ProjectError::ReadinessOnOneShot { process } => config_error(
            anyhow::anyhow!(
                "Process '{process}': readiness is valid only on Services; a One-shot completes instead of becoming ready"
            ),
        ),
        crate::model::ProjectError::InvalidRestartPolicy { process, policy } => config_error(
            anyhow::anyhow!(
                "Process '{process}': restart.policy '{policy}' is valid only for Services"
            ),
        ),
        crate::model::ProjectError::DependencyCycle(path) => {
            config_error(anyhow::anyhow!("dependency cycle: {}", path.join(" -> ")))
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
    let detail = error.to_string();
    let detail = match yaml_replacement_hint(&detail) {
        Some(hint) => format!("{detail}; {hint}"),
        None => detail,
    };
    match error.location() {
        Some(location) => format!(
            "invalid configuration at line {}, column {}: {detail}",
            location.line(),
            location.column(),
        ),
        None => format!("invalid configuration: {detail}"),
    }
}

fn yaml_replacement_hint(detail: &str) -> Option<&'static str> {
    if detail.contains("unknown field `readiness`") {
        Some("use `ready` instead")
    } else if detail.contains("unknown field `interval_ms`") {
        Some("use `interval` instead")
    } else if detail.contains("unknown field `timeout_ms`") {
        Some("use `timeout` instead")
    } else {
        None
    }
}

fn build_shell(settings: Option<&SettingsFile>) -> Result<ShellConfig, ConfigError> {
    let Some(shell) = settings.and_then(|settings| settings.shell.as_ref()) else {
        return Ok(ShellConfig::default());
    };
    if shell.program.trim().is_empty() {
        return Err(ConfigError {
            message: "settings.shell.program must not be empty".to_string(),
        });
    }
    let args = shell.args.clone().unwrap_or_else(|| vec!["-c".to_string()]);
    if args.is_empty() {
        return Err(ConfigError {
            message: "settings.shell.args must contain the arguments needed to evaluate a shell expression".to_string(),
        });
    }
    Ok(ShellConfig {
        program: std::ffi::OsString::from(&shell.program),
        args: args.into_iter().map(std::ffi::OsString::from).collect(),
    })
}

fn build_restart(
    process_name: &str,
    file: Option<&RestartFile>,
) -> Result<RestartConfig, ConfigError> {
    let defaults = RestartConfig::default();
    let policy = match file.and_then(|file| file.policy.as_deref()) {
        None => defaults.policy,
        Some(value) => RestartPolicy::from_label(value).ok_or_else(|| ConfigError {
            message: format!(
                "Process '{process_name}': restart.policy must be 'never', 'on_failure', or 'always', got '{value}'"
            ),
        })?,
    };
    let backoff = match file.and_then(|file| file.backoff.as_deref()) {
        Some(value) => {
            let duration = readiness::parse_duration(value).map_err(|detail| ConfigError {
                message: format!("Process '{process_name}': restart.backoff: {detail}"),
            })?;
            if duration.is_zero() {
                return Err(ConfigError {
                    message: format!(
                        "Process '{process_name}': restart.backoff must be greater than zero"
                    ),
                });
            }
            duration
        }
        None => defaults.backoff,
    };
    Ok(RestartConfig { policy, backoff })
}

fn build_success_exit_codes(configured: Option<Vec<i32>>) -> Result<Vec<i32>, String> {
    let codes = configured.unwrap_or_else(|| vec![0]);
    if codes.is_empty() {
        return Err("success_exit_codes must contain at least one exit code".to_string());
    }
    let mut seen = HashSet::with_capacity(codes.len());
    for code in &codes {
        if !(0..=MAX_EXIT_CODE).contains(code) {
            return Err(format!(
                "success_exit_codes values must be unique exit codes from 0 through {MAX_EXIT_CODE}"
            ));
        }
        if !seen.insert(*code) {
            return Err(format!(
                "success_exit_codes values must be unique; {code} is repeated"
            ));
        }
    }
    Ok(codes)
}

fn build_command_form(command: &CommandFile) -> Result<CommandForm, String> {
    match (&command.program, &command.shell) {
        (Some(program), None) => Ok(CommandForm::Direct {
            program: std::ffi::OsString::from(program),
            args: command
                .args
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(std::ffi::OsString::from)
                .collect(),
        }),
        (None, Some(shell)) => Ok(CommandForm::Shell {
            text: shell.clone(),
        }),
        (Some(_), Some(_)) => Err("define exactly one of 'program' or 'shell'".to_string()),
        (None, None) => Err("define 'program' or 'shell' under 'command'".to_string()),
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
    let command_form = match build_command_form(&process.command) {
        Ok(command) => command,
        Err(detail) => return fail(format!("Process '{name}': {detail}")),
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
    let dependencies = match &process.depends_on {
        None => Vec::new(),
        Some(entries) => entries
            .iter()
            .enumerate()
            .map(|(index, entry)| build_dependency(&name, index, entry))
            .collect::<Result<Vec<_>, ConfigError>>()?,
    };
    let readiness = match &process.ready {
        None => None,
        Some(file) => Some(readiness::build_readiness(&name, file, base_dir)?),
    };
    let success_exit_codes = match build_success_exit_codes(process.success_exit_codes.clone()) {
        Ok(codes) => codes,
        Err(detail) => return fail(format!("Process '{name}': {detail}")),
    };
    let restart = build_restart(&name, process.restart.as_ref())?;
    Ok(ProcessSpec {
        name,
        kind,
        enabled: Enabled::flag(process.enabled.unwrap_or(true)),
        autostart: Autostart::flag(process.autostart.unwrap_or(true)),
        success_exit_codes,
        restart,
        command: command_form,
        working_dir,
        env: process
            .env
            .as_ref()
            .map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default(),
        terminal_mode,
        input_policy,
        dependencies,
        readiness,
    })
}

/// One `depends_on` entry: a plain Process name (`started` condition) or an
/// explicit `{name, condition}` mapping.
fn build_dependency(
    process_name: &str,
    index: usize,
    entry: &serde_yaml::Value,
) -> Result<DependencySpec, ConfigError> {
    let fail = |detail: String| {
        Err(ConfigError {
            message: format!(
                "Process '{process_name}': invalid depends_on entry {index}: {detail}"
            ),
        })
    };
    let (name, condition) = match entry {
        serde_yaml::Value::String(name) => (name.clone(), None),
        serde_yaml::Value::Mapping(map) => {
            let mut name = None;
            let mut condition = None;
            for (key, value) in map {
                let serde_yaml::Value::String(key) = key else {
                    return fail(format!("mapping keys must be strings, got {key:?}"));
                };
                match key.as_str() {
                    "name" => match value.as_str() {
                        Some(value) => name = Some(value.to_string()),
                        None => return fail("'name' must be a string".to_string()),
                    },
                    "condition" => match value.as_str() {
                        Some(value) => condition = Some(value.to_string()),
                        None => return fail("'condition' must be a string".to_string()),
                    },
                    other => return fail(format!("unknown field '{other}'")),
                }
            }
            match name {
                Some(name) => (name, condition),
                None => return fail("a mapping entry requires 'name'".to_string()),
            }
        }
        other => {
            return fail(format!(
                "use a Process name or a {{name, condition}} mapping, got {other:?}"
            ));
        }
    };
    let condition = match condition.as_deref() {
        None => DependencyCondition::Started,
        Some("started") => DependencyCondition::Started,
        Some("ready") => DependencyCondition::Ready,
        // Kind honesty is enforced later against the full Process list: a
        // One-shot dependency supports `exited` and
        // `completed_successfully`, a Service dependency supports `ready`.
        Some("exited") => DependencyCondition::Exited,
        Some("completed_successfully") => DependencyCondition::CompletedSuccessfully,
        Some(other) => {
            return fail(format!(
                "unsupported condition '{other}' (this Stackhand supports 'started', 'ready', 'exited', and 'completed_successfully')"
            ));
        }
    };
    Ok(DependencySpec { name, condition })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    version: u64,
    #[serde(default)]
    processes: Vec<ProcessFile>,
    settings: Option<SettingsFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsFile {
    shell: Option<ShellFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShellFile {
    program: String,
    args: Option<Vec<String>>,
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
    success_exit_codes: Option<Vec<i32>>,
    depends_on: Option<Vec<serde_yaml::Value>>,
    ready: Option<readiness::ReadinessFile>,
    restart: Option<RestartFile>,
    command: CommandFile,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RestartFile {
    policy: Option<String>,
    backoff: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandFile {
    program: Option<String>,
    args: Option<Vec<String>>,
    shell: Option<String>,
}

#[cfg(test)]
mod tests;
