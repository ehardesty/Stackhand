//! Configuration: one YAML version 1 file becomes one validated
//! [`EffectiveProject`] or a structured error before any Process starts.
//!
//! Profiles, overlays, and environment files are added by later milestones;
//! version 1 currently accepts one canonical base configuration.

mod file;
mod readiness;

#[cfg(test)]
mod exit_tests;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Context;

use self::file::{
    CommandFile, ConfigFile, DependencyEntry, ProcessEntry, ProcessFile, RestartFile, SettingsFile,
    TerminalFile,
};

use crate::model::{
    Autostart, CommandForm, DependencyCondition, DependencySpec, EffectiveProject, Enabled,
    InputPolicy, ProcessKind, ProcessSpec, RestartConfig, RestartPolicy, ShellConfig, TerminalMode,
};

const MAX_EXIT_CODE: i32 = 255;

/// One request to resolve a Project before the Supervisor starts.
#[derive(Clone, Debug)]
pub struct ResolutionRequest {
    /// An explicit base Project path. Discovery is intentionally added by a
    /// later milestone rather than being hidden in this request.
    pub project_path: PathBuf,
}

impl ResolutionRequest {
    pub fn explicit(path: impl Into<PathBuf>) -> Self {
        Self {
            project_path: path.into(),
        }
    }
}

/// The source information selected during one Project resolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolutionSources {
    pub base: PathBuf,
}

/// One validated Project and the source summary that produced it.
#[derive(Debug)]
pub struct ProjectResolution {
    project: EffectiveProject,
    #[allow(dead_code)]
    pub sources: ResolutionSources,
}

impl ProjectResolution {
    #[allow(dead_code)]
    pub fn project(&self) -> &EffectiveProject {
        &self.project
    }

    pub fn into_project(self) -> EffectiveProject {
        self.project
    }
}

/// Resolve and validate one Project before any Process starts.
pub fn resolve(request: ResolutionRequest) -> Result<ProjectResolution, ConfigError> {
    let path = request.project_path;
    let base = absolute_normalized_path(&path)
        .with_context(|| format!("could not resolve Project path {}", path.display()))
        .map_err(config_error)?;
    let project = load_file(&base)?;
    Ok(ProjectResolution {
        project,
        sources: ResolutionSources { base },
    })
}

/// Load and validate the Project at `path` through the shared resolver.
/// Relative working directories resolve from the configuration file's
/// directory.
#[allow(dead_code)]
pub fn load(path: &Path) -> Result<EffectiveProject, ConfigError> {
    resolve(ResolutionRequest::explicit(path)).map(ProjectResolution::into_project)
}

fn load_file(path: &Path) -> Result<EffectiveProject, ConfigError> {
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
        .entries
        .into_iter()
        .map(|entry| {
            let ProcessEntry { key, process } = entry;
            build_spec(&process, key, base_dir)
        })
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
        crate::model::ProjectError::LivenessOnOneShot { process } => config_error(
            anyhow::anyhow!(
                "Process '{process}': liveness is valid only on Services; a One-shot cannot have ongoing health checks"
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

fn absolute_normalized_path(path: &Path) -> anyhow::Result<PathBuf> {
    let current_dir = std::env::current_dir()?;
    Ok(normalize_path(if path.is_absolute() {
        path.to_path_buf()
    } else {
        current_dir.join(path)
    }))
}

fn normalize_path(path: PathBuf) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
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
    let location = detail
        .split_once(": unknown field")
        .map(|(location, _)| location)
        .unwrap_or_default();
    let process_fields = location
        .strip_prefix("processes.")
        .is_some_and(|path| !path.contains('.'));
    let exec_fields = location.ends_with(".ready.exec") || location.ends_with(".liveness.exec");

    if detail.contains("unknown field `readiness`") {
        Some("use `ready` instead")
    } else if detail.contains("unknown field `interval_ms`") {
        Some("use `interval` instead")
    } else if detail.contains("unknown field `timeout_ms`") {
        Some("use `timeout` instead")
    } else if detail.contains("unknown field `working_dir`") && (process_fields || exec_fields) {
        Some("use `cwd` instead")
    } else if detail.contains("unknown field `env`") && (process_fields || exec_fields) {
        Some("use `environment` instead")
    } else if detail.contains("unknown field `input`") && process_fields {
        Some("put `input` under the `terminal` mapping instead")
    } else if detail.contains("unknown field `name`") && process_fields {
        Some("put the Process name in the `processes` map key instead")
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
    let max_restarts = file
        .and_then(|file| file.max_restarts)
        .unwrap_or(defaults.max_restarts);
    let on_unhealthy = file
        .and_then(|file| file.on_unhealthy)
        .unwrap_or(defaults.on_unhealthy);
    Ok(RestartConfig {
        policy,
        backoff,
        max_restarts,
        on_unhealthy,
    })
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

fn build_command_form(
    command: Option<&CommandFile>,
    shell: Option<&str>,
) -> Result<CommandForm, String> {
    match (command, shell) {
        (Some(_), Some(_)) => Err("define exactly one of 'command' or 'shell'".to_string()),
        (Some(CommandFile::Direct(values)), None) => build_direct_command(values),
        (None, Some(text)) if text.trim().is_empty() => {
            Err("shell expression must not be empty".to_string())
        }
        (None, Some(text)) => Ok(CommandForm::Shell {
            text: text.to_string(),
        }),
        (None, None) => Err("define exactly one of 'command' or 'shell'".to_string()),
    }
}

fn build_direct_command(values: &[serde_yaml::Value]) -> Result<CommandForm, String> {
    let Some(program) = values.first().and_then(serde_yaml::Value::as_str) else {
        return Err("command must contain a non-empty program as its first item".to_string());
    };
    let program = nonempty_program(program)?;
    let args = values
        .iter()
        .skip(1)
        .enumerate()
        .map(|(index, value)| {
            value
                .as_str()
                .map(std::ffi::OsString::from)
                .ok_or_else(|| format!("command argument {index} must be a string"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(CommandForm::Direct { program, args })
}

fn nonempty_program(program: &str) -> Result<std::ffi::OsString, String> {
    if program.is_empty() {
        return Err("command program must not be empty".to_string());
    }
    Ok(std::ffi::OsString::from(program))
}

fn build_terminal_settings(
    terminal: Option<&TerminalFile>,
) -> Result<(TerminalMode, InputPolicy), String> {
    let (mode, terminal_input) = match terminal {
        None => (None, None),
        Some(TerminalFile::Settings(settings)) => {
            (settings.mode.as_deref(), settings.input.as_deref())
        }
    };
    let terminal_mode = match mode {
        None | Some("pipe") => TerminalMode::Pipe,
        Some("pty") => TerminalMode::Pty,
        Some(other) => {
            return Err(format!(
                "invalid terminal mode '{other}' (use 'pipe' or 'pty')"
            ));
        }
    };
    let input_policy = match terminal_input {
        None | Some("disabled") => InputPolicy::Disabled,
        Some("focused") => InputPolicy::Focused,
        Some(other) => Err(format!(
            "invalid input policy '{other}' (use 'focused' or 'disabled')"
        ))?,
    };
    Ok((terminal_mode, input_policy))
}

fn build_environment(process: &ProcessFile) -> Vec<(String, String)> {
    process
        .environment
        .as_ref()
        .map(|entries| {
            entries
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default()
}

fn build_spec(
    process: &ProcessFile,
    name: String,
    base_dir: &Path,
) -> Result<ProcessSpec, ConfigError> {
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
    let command_form = match build_command_form(process.command.as_ref(), process.shell.as_deref())
    {
        Ok(command) => command,
        Err(detail) => return fail(format!("Process '{name}': {detail}")),
    };
    let working_dir = match process.cwd.as_deref() {
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
    let (terminal_mode, input_policy) = match build_terminal_settings(process.terminal.as_ref()) {
        Ok(settings) => settings,
        Err(detail) => return fail(format!("Process '{name}': {detail}")),
    };
    let dependencies = match &process.depends_on {
        None => Vec::new(),
        Some(entries) => entries
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| build_dependency(&name, index, entry))
            .collect::<Result<Vec<_>, ConfigError>>()?,
    };
    let readiness = match &process.ready {
        None => None,
        Some(file) => Some(readiness::build_readiness(&name, file, base_dir)?),
    };
    let liveness = match &process.liveness {
        None => None,
        Some(file) => Some(readiness::build_liveness(&name, file, base_dir)?),
    };
    let success_exit_codes = match build_success_exit_codes(process.success_exit_codes.clone()) {
        Ok(codes) => codes,
        Err(detail) => return fail(format!("Process '{name}': {detail}")),
    };
    let restart = build_restart(&name, process.restart.as_ref())?;
    let env = build_environment(process);
    Ok(ProcessSpec {
        name,
        kind,
        enabled: Enabled::flag(process.enabled.unwrap_or(true)),
        autostart: Autostart::flag(process.autostart.unwrap_or(true)),
        success_exit_codes,
        restart,
        command: command_form,
        working_dir,
        env,
        terminal_mode,
        input_policy,
        dependencies,
        readiness,
        liveness,
    })
}

/// One canonical `depends_on` entry. The map key is the dependency Process
/// name and its value is the dependency condition.
fn build_dependency(
    process_name: &str,
    index: usize,
    entry: &DependencyEntry,
) -> Result<DependencySpec, ConfigError> {
    let condition = entry.value.as_str().ok_or_else(|| {
        dependency_error(
            process_name,
            index,
            format!(
                "condition for Dependency '{}' must be a string; use 'dependency-name: condition'",
                entry.key
            ),
        )
    })?;
    let condition = match condition {
        "started" => DependencyCondition::Started,
        "ready" => DependencyCondition::Ready,
        // Kind honesty is enforced later against the full Process list: a
        // One-shot dependency supports `exited` and
        // `completed_successfully`, a Service dependency supports `ready`.
        "exited" => DependencyCondition::Exited,
        "completed_successfully" => DependencyCondition::CompletedSuccessfully,
        other => {
            return Err(dependency_error(
                process_name,
                index,
                format!(
                    "unsupported condition '{other}' (this Stackhand supports 'started', 'ready', 'exited', and 'completed_successfully')"
                ),
            ));
        }
    };
    Ok(DependencySpec {
        name: entry.key.clone(),
        condition,
    })
}

fn dependency_error(process_name: &str, index: usize, detail: String) -> ConfigError {
    ConfigError {
        message: format!("Process '{process_name}': invalid depends_on entry {index}: {detail}"),
    }
}

#[cfg(test)]
mod tests;
