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
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

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
            let ProcessEntry { key, mut process } = entry;
            let declared_name = process.name.take();
            let name = match (key, declared_name) {
                (Some(key), None) => key,
                (Some(key), Some(declared_name)) if key == declared_name => key,
                (Some(key), Some(declared_name)) => {
                    return Err(config_error(anyhow::anyhow!(
                        "Process map key '{key}' does not match its declared name '{declared_name}'"
                    )));
                }
                (None, Some(name)) => name,
                (None, None) => {
                    return Err(config_error(anyhow::anyhow!(
                        "each Process entry must define a 'name'"
                    )));
                }
            };
            build_spec(&process, name, base_dir)
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
        liveness,
    })
}

/// One `depends_on` entry: a legacy sequence entry or a canonical keyed
/// entry whose map key is the dependency Process name.
fn build_dependency(
    process_name: &str,
    index: usize,
    entry: &DependencyEntry,
) -> Result<DependencySpec, ConfigError> {
    let fail = |detail: String| Err(dependency_error(process_name, index, detail));
    let (name, condition) = match entry.key.as_deref() {
        Some(name) => keyed_dependency_parts(name, &entry.value, &fail)?,
        None => legacy_dependency_parts(&entry.value, &fail)?,
    };
    let condition = match condition.as_deref() {
        None | Some("started") => DependencyCondition::Started,
        Some("ready") => DependencyCondition::Ready,
        // Kind honesty is enforced later against the full Process list: a
        // One-shot dependency supports `exited` and
        // `completed_successfully`, a Service dependency supports `ready`.
        Some("exited") => DependencyCondition::Exited,
        Some("completed_successfully") => DependencyCondition::CompletedSuccessfully,
        Some(other) => {
            return Err(dependency_error(
                process_name,
                index,
                format!(
                    "unsupported condition '{other}' (this Stackhand supports 'started', 'ready', 'exited', and 'completed_successfully')"
                ),
            ));
        }
    };
    Ok(DependencySpec { name, condition })
}

fn dependency_error(process_name: &str, index: usize, detail: String) -> ConfigError {
    ConfigError {
        message: format!("Process '{process_name}': invalid depends_on entry {index}: {detail}"),
    }
}

fn legacy_dependency_parts(
    entry: &serde_yaml::Value,
    fail: &impl Fn(String) -> Result<(String, Option<String>), ConfigError>,
) -> Result<(String, Option<String>), ConfigError> {
    match entry {
        serde_yaml::Value::String(name) => Ok((name.clone(), None)),
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
                Some(name) => Ok((name, condition)),
                None => fail("a mapping entry requires 'name'".to_string()),
            }
        }
        other => fail(format!(
            "use a Process name or a {{name, condition}} mapping, got {other:?}"
        )),
    }
}

fn keyed_dependency_parts(
    name: &str,
    value: &serde_yaml::Value,
    fail: &impl Fn(String) -> Result<(String, Option<String>), ConfigError>,
) -> Result<(String, Option<String>), ConfigError> {
    match value {
        serde_yaml::Value::String(condition) => Ok((name.to_string(), Some(condition.clone()))),
        serde_yaml::Value::Mapping(map) => {
            let mut declared_name = None;
            let mut condition = None;
            for (key, value) in map {
                let serde_yaml::Value::String(key) = key else {
                    return fail(format!("mapping keys must be strings, got {key:?}"));
                };
                match key.as_str() {
                    "name" => match value.as_str() {
                        Some(value) => declared_name = Some(value),
                        None => return fail("'name' must be a string".to_string()),
                    },
                    "condition" => match value.as_str() {
                        Some(value) => condition = Some(value.to_string()),
                        None => return fail("'condition' must be a string".to_string()),
                    },
                    other => return fail(format!("unknown field '{other}'")),
                }
            }
            if let Some(declared_name) = declared_name
                && declared_name != name
            {
                return fail(format!(
                    "Dependency map key '{name}' does not match its declared name '{declared_name}'"
                ));
            }
            Ok((name.to_string(), condition))
        }
        other => fail(format!(
            "the condition for Dependency '{name}' must be a string, got {other:?}"
        )),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    version: u64,
    #[serde(default)]
    processes: ProcessCollection,
    settings: Option<SettingsFile>,
}

#[derive(Default)]
struct ProcessCollection {
    entries: Vec<ProcessEntry>,
}

struct ProcessEntry {
    key: Option<String>,
    process: ProcessFile,
}

impl<'de> Deserialize<'de> for ProcessCollection {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ProcessCollectionVisitor;

        impl<'de> Visitor<'de> for ProcessCollectionVisitor {
            type Value = ProcessCollection;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a sequence or name-keyed mapping of Processes")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut entries = Vec::new();
                while let Some(process) = sequence.next_element::<ProcessFile>()? {
                    entries.push(ProcessEntry { key: None, process });
                }
                Ok(ProcessCollection { entries })
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut entries = Vec::new();
                let mut names = HashSet::new();
                while let Some(name) = map.next_key::<String>()? {
                    if !names.insert(name.clone()) {
                        return Err(de::Error::custom(format!(
                            "duplicate Process name '{name}'"
                        )));
                    }
                    entries.push(ProcessEntry {
                        key: Some(name),
                        process: map.next_value::<ProcessFile>()?,
                    });
                }
                Ok(ProcessCollection { entries })
            }
        }

        deserializer.deserialize_any(ProcessCollectionVisitor)
    }
}

struct DependencyCollection {
    entries: Vec<DependencyEntry>,
}

struct DependencyEntry {
    key: Option<String>,
    value: serde_yaml::Value,
}

impl<'de> Deserialize<'de> for DependencyCollection {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct DependencyCollectionVisitor;

        impl<'de> Visitor<'de> for DependencyCollectionVisitor {
            type Value = DependencyCollection;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a sequence or name-keyed mapping of Dependencies")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut entries = Vec::new();
                while let Some(value) = sequence.next_element::<serde_yaml::Value>()? {
                    entries.push(DependencyEntry { key: None, value });
                }
                Ok(DependencyCollection { entries })
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut entries = Vec::new();
                let mut names = HashSet::new();
                while let Some(name) = map.next_key::<String>()? {
                    if !names.insert(name.clone()) {
                        return Err(de::Error::custom(format!(
                            "duplicate Dependency name '{name}'"
                        )));
                    }
                    entries.push(DependencyEntry {
                        key: Some(name),
                        value: map.next_value::<serde_yaml::Value>()?,
                    });
                }
                Ok(DependencyCollection { entries })
            }
        }

        deserializer.deserialize_any(DependencyCollectionVisitor)
    }
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
    name: Option<String>,
    kind: Option<String>,
    enabled: Option<bool>,
    autostart: Option<bool>,
    working_dir: Option<String>,
    env: Option<std::collections::BTreeMap<String, String>>,
    terminal: Option<String>,
    input: Option<String>,
    success_exit_codes: Option<Vec<i32>>,
    depends_on: Option<DependencyCollection>,
    ready: Option<readiness::ReadinessFile>,
    liveness: Option<readiness::ReadinessFile>,
    restart: Option<RestartFile>,
    command: CommandFile,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RestartFile {
    policy: Option<String>,
    backoff: Option<String>,
    max_restarts: Option<u32>,
    on_unhealthy: Option<bool>,
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
