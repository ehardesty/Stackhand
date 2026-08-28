//! Configuration: one YAML version 1 file becomes one validated
//! [`EffectiveProject`] or a structured error before any Process starts.
//!
//! The resolver applies selected profiles to the canonical base file before
//! lowering the result into the validated Project model.

mod env;
mod file;
mod paths;
mod profile;
mod readiness;

#[cfg(test)]
mod exit_tests;
#[cfg(test)]
mod local_tests;
#[cfg(test)]
mod path_tests;
#[cfg(test)]
mod profile_tests;
#[cfg(test)]
mod schema_tests;

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use serde_yaml::Value;

use anyhow::Context;

use self::env::{build_process_environment, load_files, validate_shapes};
use self::file::{
    CommandFile, ConfigFile, DependencyEntry, ProcessEntry, ProcessFile, RestartFile, SettingsFile,
    TerminalFile,
};
use self::profile::{apply_local_override, apply_profiles};

use crate::model::{
    Autostart, CommandForm, DependencyCondition, DependencySpec, EffectiveProject, Enabled,
    InputPolicy, ProcessKind, ProcessSpec, RestartConfig, RestartPolicy, ShellConfig, TerminalMode,
};

const MAX_EXIT_CODE: i32 = 255;
pub const BASE_FILE_NAME: &str = "stackhand.yaml";
pub const LOCAL_FILE_NAME: &str = "stackhand.local.yaml";

/// One request to resolve a Project before the Supervisor starts.
#[derive(Clone, Debug)]
pub enum ResolutionRequest {
    /// Use exactly this Project path. No base-file discovery is performed.
    Explicit {
        path: PathBuf,
        profiles: Vec<String>,
    },
    /// Search for the nearest base file. `None` starts at the current
    /// directory; `Some` is useful for deterministic callers and tests.
    Discover {
        start_dir: Option<PathBuf>,
        profiles: Vec<String>,
    },
}

impl ResolutionRequest {
    pub fn explicit(path: impl Into<PathBuf>) -> Self {
        Self::Explicit {
            path: path.into(),
            profiles: Vec::new(),
        }
    }

    pub fn explicit_with_profiles(
        path: impl Into<PathBuf>,
        profiles: impl IntoIterator<Item = String>,
    ) -> Self {
        Self::Explicit {
            path: path.into(),
            profiles: profiles.into_iter().collect(),
        }
    }

    pub fn discover_with_profiles(profiles: impl IntoIterator<Item = String>) -> Self {
        Self::Discover {
            start_dir: None,
            profiles: profiles.into_iter().collect(),
        }
    }

    #[cfg(test)]
    pub fn discover_from(path: impl Into<PathBuf>) -> Self {
        Self::Discover {
            start_dir: Some(path.into()),
            profiles: Vec::new(),
        }
    }
}

/// The source information selected during one Project resolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolutionSources {
    pub base: PathBuf,
    pub local: Option<PathBuf>,
}

/// One validated Project and the source summary that produced it.
#[derive(Debug)]
pub struct ProjectResolution {
    project: EffectiveProject,
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
    let (base, profiles, local) = match request {
        ResolutionRequest::Explicit { path, profiles } => (
            paths::absolute_normalized(&path)
                .with_context(|| format!("could not resolve Project path {}", path.display()))
                .map_err(config_error)?,
            profiles,
            None,
        ),
        ResolutionRequest::Discover {
            start_dir,
            profiles,
        } => {
            let base = discover_base(start_dir.as_deref())?;
            let local = discover_local_override(&base);
            (base, profiles, local)
        }
    };
    let project = load_file_with_local(&base, &profiles, local.as_deref())?;
    Ok(ProjectResolution {
        project,
        sources: ResolutionSources { base, local },
    })
}

/// Load and validate the Project at `path` through the shared resolver.
/// Relative working directories resolve from the configuration file's
/// directory.
#[allow(dead_code)]
pub fn load(path: &Path) -> Result<EffectiveProject, ConfigError> {
    resolve(ResolutionRequest::explicit(path)).map(ProjectResolution::into_project)
}

/// Resolve and validate a Project without starting the Supervisor. When no
/// path is provided, use the nearest discovered base file.
pub fn validate_project(explicit_path: Option<&Path>) -> Result<PathBuf, ConfigError> {
    validate_project_with_profile(explicit_path, None)
}

/// Resolve and validate a Project with one explicitly selected profile.
pub fn validate_project_with_profile(
    explicit_path: Option<&Path>,
    profile: Option<&str>,
) -> Result<PathBuf, ConfigError> {
    let profiles = profile.into_iter().map(str::to_owned).collect::<Vec<_>>();
    validate_project_with_profiles(explicit_path, &profiles)
}

/// Resolve and validate a Project with profiles selected in CLI order.
pub fn validate_project_with_profiles(
    explicit_path: Option<&Path>,
    profiles: &[String],
) -> Result<PathBuf, ConfigError> {
    validate_project_sources_with_profiles(explicit_path, profiles).map(|sources| sources.base)
}

/// Resolve and validate a Project, returning every selected source in
/// precedence order.
pub fn validate_project_sources_with_profiles(
    explicit_path: Option<&Path>,
    profiles: &[String],
) -> Result<ResolutionSources, ConfigError> {
    let request = explicit_path.map_or_else(
        || ResolutionRequest::discover_with_profiles(profiles.iter().cloned()),
        |path| ResolutionRequest::explicit_with_profiles(path, profiles.iter().cloned()),
    );
    resolve(request).map(|resolution| resolution.sources)
}

#[cfg(test)]
fn load_file(path: &Path, profiles: &[String]) -> Result<EffectiveProject, ConfigError> {
    load_file_with_local(path, profiles, None)
}

fn load_file_with_local(
    path: &Path,
    profiles: &[String],
    local_path: Option<&Path>,
) -> Result<EffectiveProject, ConfigError> {
    let base_dir = path.parent().unwrap_or(Path::new("."));
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("could not read {}", path.display()))
        .map_err(config_error)?;
    let mut document: Value = serde_yaml::from_str(&text)
        .map_err(|error| config_error(anyhow::anyhow!(format_yaml_error(&error))))?;
    let version = read_version(&document)?;
    if version != 1 {
        return Err(config_error(anyhow::anyhow!(
            "unsupported schema version {version}: this Stackhand reads version 1"
        )));
    }
    validate_shapes(&document, "base Project")?;
    let file: ConfigFile = if profiles.is_empty() && local_path.is_none() {
        serde_yaml::from_str(&text)
            .map_err(|error| config_error(anyhow::anyhow!(format_yaml_error(&error))))?
    } else {
        apply_profiles(&mut document, profiles)?;
        if let Some(local_path) = local_path {
            let local_text = std::fs::read_to_string(local_path)
                .with_context(|| format!("could not read local override {}", local_path.display()))
                .map_err(config_error)?;
            let local_document: Value = serde_yaml::from_str(&local_text).map_err(|error| {
                config_error(anyhow::anyhow!(format_local_yaml_error(local_path, &error)))
            })?;
            if let Some(processes) = local_document
                .as_mapping()
                .and_then(|root| root.get(Value::String("processes".to_string())))
            {
                let source = format!("local override '{}'", local_path.display());
                self::env::validate_process_overrides(processes, &source)?;
            }
            apply_local_override(&mut document, &local_document).map_err(|error| ConfigError {
                message: format!("local override '{}': {error}", local_path.display()),
            })?;
        }
        validate_shapes(&document, "merged Project")?;
        let merged = serde_yaml::to_string(&document)
            .map_err(|error| config_error(anyhow::anyhow!(format_yaml_error(&error))))?;
        serde_yaml::from_str(&merged)
            .map_err(|error| config_error(anyhow::anyhow!(format_merged_yaml_error(&error))))?
    };
    let shell = build_shell(file.settings.as_ref())?;
    let project_environment = load_files(
        base_dir,
        file.env_files.as_deref().unwrap_or_default(),
        "Project",
    )?;
    let processes = file
        .processes
        .entries
        .into_iter()
        .map(|entry| {
            let ProcessEntry { key, process } = entry;
            build_spec(&process, key, base_dir, &project_environment)
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

fn read_version(document: &Value) -> Result<u64, ConfigError> {
    let Some(root) = document.as_mapping() else {
        return Err(ConfigError {
            message: "configuration must be a mapping".to_string(),
        });
    };
    let Some(version) = root.get(Value::String("version".to_string())) else {
        return Err(ConfigError {
            message: "configuration must define a numeric version".to_string(),
        });
    };
    serde_yaml::from_value(version.clone()).map_err(|error| ConfigError {
        message: format!("configuration version must be an unsigned integer: {error}"),
    })
}

fn discover_local_override(base: &Path) -> Option<PathBuf> {
    let candidate = base.parent()?.join(LOCAL_FILE_NAME);
    candidate.is_file().then_some(candidate)
}

fn discover_base(start_dir: Option<&Path>) -> Result<PathBuf, ConfigError> {
    let starting_path = match start_dir {
        Some(path) => paths::absolute_normalized(path),
        None => std::env::current_dir()
            .map(paths::normalize)
            .map_err(Into::into),
    }
    .with_context(|| "could not determine the Project discovery directory")
    .map_err(config_error)?;
    let mut directory = starting_path.clone();
    loop {
        let candidate = directory.join(BASE_FILE_NAME);
        if candidate.is_file() {
            return Ok(candidate);
        }
        if !directory.pop() {
            break;
        }
    }
    Err(config_error(anyhow::anyhow!(
        "could not find {BASE_FILE_NAME} from starting directory '{}'; checked that directory and each parent",
        starting_path.display()
    )))
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
    format_yaml_error_with_location(error, true)
}

fn format_merged_yaml_error(error: &serde_yaml::Error) -> String {
    format_yaml_error_with_location(error, false)
}

fn format_local_yaml_error(path: &Path, error: &serde_yaml::Error) -> String {
    format!(
        "invalid local override '{}': {}",
        path.display(),
        format_yaml_error(error)
    )
}

fn format_yaml_error_with_location(error: &serde_yaml::Error, include_location: bool) -> String {
    let detail = error.to_string();
    let detail = match yaml_duplicate_hint(&detail) {
        Some(hint) => hint,
        None => detail,
    };
    let detail = match yaml_replacement_hint(&detail) {
        Some(hint) => format!("{detail}; {hint}"),
        None => detail,
    };
    if include_location && let Some(location) = error.location() {
        return format!(
            "invalid configuration at line {}, column {}: {detail}",
            location.line(),
            location.column(),
        );
    }
    format!("invalid configuration: {detail}")
}

fn yaml_duplicate_hint(detail: &str) -> Option<String> {
    if !detail.contains("duplicate entry with key")
        || !(detail.starts_with("processes:") || detail.contains(".overrides:"))
    {
        return None;
    }
    let name = detail.split_once("key \"")?.1.split_once('"')?.0;
    Some(format!("duplicate Process name '{name}'"))
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
    let Some(program) = shell.program.as_deref() else {
        return Err(ConfigError {
            message: "settings.shell.program is required".to_string(),
        });
    };
    if program.trim().is_empty() {
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
        program: std::ffi::OsString::from(program),
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

fn resolve_direct_program(
    command: &mut CommandForm,
    base_dir: &Path,
    context: &str,
) -> Result<(), String> {
    if let CommandForm::Direct { program, .. } = command {
        paths::resolve_program(program, base_dir, context)?;
    }
    Ok(())
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

fn build_spec(
    process: &ProcessFile,
    name: String,
    base_dir: &Path,
    project_environment: &BTreeMap<String, String>,
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
    let mut command_form =
        match build_command_form(process.command.as_ref(), process.shell.as_deref()) {
            Ok(command) => command,
            Err(detail) => return fail(format!("Process '{name}': {detail}")),
        };
    if let Err(detail) = resolve_direct_program(&mut command_form, base_dir, "command") {
        return fail(format!("Process '{name}': {detail}"));
    }
    let working_dir = match process.cwd.as_deref() {
        Some(directory) => paths::resolve_directory(base_dir, directory, "working directory")
            .map_err(|detail| ConfigError {
                message: format!("Process '{name}': {detail}"),
            })?,
        None => base_dir.to_path_buf(),
    };
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
    let environment = build_process_environment(process, &name, base_dir, project_environment)?;
    Ok(ProcessSpec {
        name,
        kind,
        enabled: Enabled::flag(process.enabled.unwrap_or(true)),
        autostart: Autostart::flag(process.autostart.unwrap_or(true)),
        success_exit_codes,
        restart,
        command: command_form,
        working_dir,
        env: environment.values,
        env_remove: environment.removals,
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
    let condition = entry
        .value
        .as_ref()
        .and_then(serde_yaml::Value::as_str)
        .ok_or_else(|| {
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
