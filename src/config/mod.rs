//! Configuration: one YAML version 1 file becomes one validated
//! [`EffectiveProject`] or a structured error before any Process starts.
//!
//! The resolver validates named Process Profiles and selects one global
//! profile for future Runs before it lowers the Project model.

mod diagnostics;
mod env;
mod file;
mod paths;
mod profile;
mod readiness;
mod show;

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

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use serde_yaml::Value;

use anyhow::Context;

use self::env::{build_process_environment, load_files, validate_shapes};
use self::file::{
    CommandFile, ConfigFile, DependencyEntry, ProcessEntry, ProcessFile, RestartFile, SettingsFile,
    TerminalFile,
};
use self::profile::{apply_local_override, merge_yaml};

use crate::model::{
    Autostart, CommandForm, DependencyCondition, DependencySpec, EffectiveProject, Enabled,
    InputPolicy, ProcessKind, ProcessSpec, ProjectError, RestartConfig, RestartPolicy, ShellConfig,
    TerminalMode,
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
        profile: Option<String>,
    },
    /// Search for the nearest base file. `None` starts at the current
    /// directory; `Some` is useful for deterministic callers and tests.
    Discover {
        start_dir: Option<PathBuf>,
        profile: Option<String>,
    },
}

impl ResolutionRequest {
    pub fn explicit(path: impl Into<PathBuf>) -> Self {
        Self::Explicit {
            path: path.into(),
            profile: None,
        }
    }

    pub fn explicit_with_profile(path: impl Into<PathBuf>, profile: Option<String>) -> Self {
        Self::Explicit {
            path: path.into(),
            profile,
        }
    }

    pub fn discover_with_profile(profile: Option<String>) -> Self {
        Self::Discover {
            start_dir: None,
            profile,
        }
    }

    #[cfg(test)]
    pub fn discover_from(path: impl Into<PathBuf>) -> Self {
        Self::Discover {
            start_dir: Some(path.into()),
            profile: None,
        }
    }
}

/// The source information selected during one Project resolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolutionSources {
    pub base: PathBuf,
    pub profile: Option<String>,
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
    let (base, profile, local) = match request {
        ResolutionRequest::Explicit { path, profile } => (
            paths::absolute_normalized(&path)
                .with_context(|| format!("could not resolve Project path {}", path.display()))
                .map_err(config_error)?,
            profile,
            None,
        ),
        ResolutionRequest::Discover { start_dir, profile } => {
            let base = discover_base(start_dir.as_deref())?;
            let local = discover_local_override(&base);
            (base, profile, local)
        }
    };
    let project = load_file_with_local(&base, profile.as_deref(), local.as_deref())?;
    Ok(ProjectResolution {
        project,
        sources: ResolutionSources {
            base,
            profile,
            local,
        },
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
    validate_project_sources(explicit_path, profile).map(|sources| sources.base)
}

/// Resolve and validate a Project, returning its selected sources.
pub fn validate_project_sources(
    explicit_path: Option<&Path>,
    profile: Option<&str>,
) -> Result<ResolutionSources, ConfigError> {
    resolve(resolution_request(explicit_path, profile)).map(|resolution| resolution.sources)
}

/// The source summary and redacted normalized YAML for one resolved Project.
#[derive(Debug)]
pub struct EffectiveProjectView {
    pub sources: ResolutionSources,
    pub yaml: String,
}

/// Resolve one Project and render the effective canonical configuration
/// without starting the Supervisor or any Process.
pub fn show_project(
    explicit_path: Option<&Path>,
    profile: Option<&str>,
) -> Result<EffectiveProjectView, ConfigError> {
    let resolution = resolve(resolution_request(explicit_path, profile))?;
    let yaml = show::render(&resolution.project)?;
    Ok(EffectiveProjectView {
        sources: resolution.sources,
        yaml,
    })
}

fn resolution_request(explicit_path: Option<&Path>, profile: Option<&str>) -> ResolutionRequest {
    explicit_path.map_or_else(
        || ResolutionRequest::discover_with_profile(profile.map(str::to_owned)),
        |path| ResolutionRequest::explicit_with_profile(path, profile.map(str::to_owned)),
    )
}

#[cfg(test)]
fn load_file(path: &Path, profile: Option<&str>) -> Result<EffectiveProject, ConfigError> {
    load_file_with_local(path, profile, None)
}

fn load_file_with_local(
    path: &Path,
    profile: Option<&str>,
    local_path: Option<&Path>,
) -> Result<EffectiveProject, ConfigError> {
    let base_dir = path.parent().unwrap_or(Path::new("."));
    let base_source = format!("base Project '{}'", path.display());
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("could not read {}", path.display()))
        .map_err(config_error)?;
    let mut document: Value = serde_yaml::from_str(&text).map_err(|error| {
        config_error(anyhow::anyhow!(diagnostics::format_yaml_error(
            path, &error
        )))
    })?;
    let version =
        read_version(&document).map_err(|error| diagnostics::with_source(error, &base_source))?;
    if version != 1 {
        return Err(diagnostics::with_source(
            config_error(anyhow::anyhow!(
                "unsupported schema version {version}: this Stackhand reads version 1"
            )),
            &base_source,
        ));
    }
    validate_shapes(&document, &base_source)?;
    let mut last_layer = base_source.clone();
    let effective_text = if let Some(local_path) = local_path {
        let local_source = format!("local override '{}'", local_path.display());
        let local_text = std::fs::read_to_string(local_path)
            .with_context(|| format!("could not read {local_source}"))
            .map_err(config_error)?;
        let local_document: Value = serde_yaml::from_str(&local_text).map_err(|error| {
            config_error(anyhow::anyhow!(diagnostics::format_local_yaml_error(
                local_path, &error
            )))
        })?;
        if let Some(processes) = local_document
            .as_mapping()
            .and_then(|root| root.get(Value::String("processes".to_string())))
        {
            self::env::validate_process_overrides(processes, &local_source)?;
        }
        apply_local_override(&mut document, &local_document)
            .map_err(|error| diagnostics::with_source(error, &local_source))?;
        last_layer = local_source;
        validate_shapes(&document, &last_layer)?;
        serde_yaml::to_string(&document).map_err(|error| {
            diagnostics::with_source(
                config_error(anyhow::anyhow!(format!(
                    "could not serialize effective YAML: {error}"
                ))),
                &last_layer,
            )
        })?
    } else {
        text
    };
    let file: ConfigFile = serde_yaml::from_str(&effective_text).map_err(|error| {
        let message = if local_path.is_none() {
            diagnostics::format_yaml_error(path, &error)
        } else {
            diagnostics::format_merged_yaml_error(path, &last_layer, &error)
        };
        config_error(anyhow::anyhow!(message))
    })?;
    let shell = build_shell(file.settings.as_ref())
        .map_err(|error| diagnostics::with_source(error, &last_layer))?;
    let raw_processes = document
        .as_mapping()
        .and_then(|root| root.get(Value::String("processes".to_string())))
        .and_then(Value::as_mapping)
        .expect("typed Process parsing requires a Process mapping");
    let mut process_profile_names = file
        .processes
        .entries
        .iter()
        .flat_map(|entry| {
            entry
                .process
                .profiles
                .as_ref()
                .into_iter()
                .flat_map(|profiles| profiles.keys().cloned())
        })
        .collect::<BTreeSet<_>>();
    let project_profiles = file.profiles.unwrap_or_default();
    if project_profiles.contains_key("base") {
        return Err(diagnostics::with_source(
            ConfigError {
                message: "Project Profile name 'base' is reserved for the base Project".to_string(),
            },
            &last_layer,
        ));
    }
    process_profile_names.extend(project_profiles.keys().cloned());
    let process_profile_names = process_profile_names.into_iter().collect::<Vec<_>>();
    if let Some(profile) = profile
        && !process_profile_names.iter().any(|name| name == profile)
    {
        return Err(diagnostics::with_source(
            ConfigError {
                message: format!("unknown Project Profile '{profile}'"),
            },
            &last_layer,
        ));
    }
    let process_documents = file
        .processes
        .entries
        .into_iter()
        .map(|entry| {
            let raw = raw_processes
                .get(Value::String(entry.key.clone()))
                .cloned()
                .expect("the raw Process mapping matches typed Process entries");
            (entry.key, raw)
        })
        .collect::<Vec<_>>();
    let base_env_files = file.env_files.unwrap_or_default();
    let resolve_profile = |selection: Option<&str>,
                           project_environment: &BTreeMap<String, String>|
     -> Result<(Vec<ProcessSpec>, Vec<Option<String>>), ConfigError> {
        let mut processes = Vec::with_capacity(process_documents.len());
        let mut labels = Vec::with_capacity(process_documents.len());
        for (name, raw) in &process_documents {
            let process = serde_yaml::from_value(raw.clone()).map_err(|error| ConfigError {
                message: format!("Process '{name}': {error}"),
            })?;
            let profiled = build_profiled_process(
                ProcessEntry {
                    key: name.clone(),
                    process,
                },
                raw.clone(),
                base_dir,
                project_environment,
            )?;
            let label = match profiled.profile_override.as_deref() {
                Some("base") => None,
                Some(name) => Some(name),
                None => selection.filter(|name| profiled.profiles.contains_key(*name)),
            };
            processes.push(
                label
                    .and_then(|name| profiled.profiles.get(name))
                    .unwrap_or(&profiled.base)
                    .clone(),
            );
            let project_environment_changes = selection.is_some_and(|name| {
                project_profiles
                    .get(name)
                    .is_some_and(|profile| profile.env_files.is_some())
            });
            labels.push(if project_environment_changes {
                selection.map(str::to_owned)
            } else {
                label.map(str::to_owned)
            });
        }
        Ok((processes, labels))
    };
    let base_environment = load_files(base_dir, &base_env_files, "Project")
        .map_err(|error| diagnostics::with_source(error, &last_layer))?;
    let (base_processes, base_labels) = resolve_profile(None, &base_environment)
        .map_err(|error| diagnostics::with_source(error, &last_layer))?;
    let mut processes_by_profile = BTreeMap::new();
    let mut labels_by_profile = BTreeMap::new();
    for name in &process_profile_names {
        let env_files = project_profiles
            .get(name)
            .and_then(|profile| profile.env_files.as_ref())
            .unwrap_or(&base_env_files);
        let owner = format!("Project Profile '{name}'");
        let environment = load_files(base_dir, env_files, &owner)
            .map_err(|error| diagnostics::with_source(error, &last_layer))?;
        let (processes, labels) = resolve_profile(Some(name), &environment)
            .map_err(|error| diagnostics::with_source(error, &last_layer))?;
        processes_by_profile.insert(name.clone(), processes);
        labels_by_profile.insert(name.clone(), labels);
    }
    EffectiveProject::with_resolved_profiles(
        base_processes,
        base_labels,
        processes_by_profile,
        labels_by_profile,
        profile.map(str::to_owned),
        shell,
    )
    .map_err(|error| {
        let detail = format_project_error(error);
        diagnostics::with_source(config_error(anyhow::anyhow!(detail)), &last_layer)
    })
}

fn format_project_error(error: ProjectError) -> String {
    match error {
        ProjectError::DuplicateName(name) => format!("duplicate Process name '{name}'"),
        ProjectError::UnknownDependency {
            process,
            dependency,
        } => format!(
            "Process '{process}': dependency '{dependency}' does not match any configured Process"
        ),
        ProjectError::InvalidCondition {
            process,
            dependency,
            condition,
        } => format!(
            "Process '{process}': dependency '{dependency}' cannot use condition '{condition}': 'exited' and 'completed_successfully' are valid only when the dependency Process is a One-shot, and 'ready' only when it is a Service"
        ),
        ProjectError::ReadinessOnOneShot { process } => format!(
            "Process '{process}': readiness is valid only on Services; a One-shot completes instead of becoming ready"
        ),
        ProjectError::LivenessOnOneShot { process } => format!(
            "Process '{process}': liveness is valid only on Services; a One-shot cannot have ongoing health checks"
        ),
        ProjectError::InvalidRestartPolicy { process, policy } => {
            format!("Process '{process}': restart.policy '{policy}' is valid only for Services")
        }
        ProjectError::DependencyCycle(path) => {
            format!("dependency cycle: {}", path.join(" -> "))
        }
        ProjectError::InvalidProcessProfileGraph { profile, source } => {
            format!(
                "Process Profile '{profile}' produces an invalid Project: {}",
                format_project_error(*source)
            )
        }
    }
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
        None | Some("pty") => TerminalMode::Pty,
        Some("pipe") => TerminalMode::Pipe,
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

struct ProfiledProcess {
    base: ProcessSpec,
    profiles: BTreeMap<String, ProcessSpec>,
    profile_override: Option<String>,
}

fn build_profiled_process(
    entry: ProcessEntry,
    mut raw: Value,
    base_dir: &Path,
    project_environment: &BTreeMap<String, String>,
) -> Result<ProfiledProcess, ConfigError> {
    const PROFILE_FIELDS: &[&str] = &[
        "command",
        "shell",
        "cwd",
        "env_files",
        "environment",
        "enabled",
        "depends_on",
    ];

    let ProcessEntry { key: name, process } = entry;
    let profile_override = process.profile.clone();
    let patches = process.profiles.clone().unwrap_or_default();
    if raw.is_null() {
        raw = Value::Mapping(serde_yaml::Mapping::new());
    }
    let Some(base_mapping) = raw.as_mapping_mut() else {
        return Err(ConfigError {
            message: format!("Process '{name}' must define a mapping"),
        });
    };
    base_mapping.remove(Value::String("profile".to_string()));
    base_mapping.remove(Value::String("profiles".to_string()));

    let base_file: ProcessFile =
        serde_yaml::from_value(raw.clone()).map_err(|error| ConfigError {
            message: format!("Process '{name}': invalid base definition: {error}"),
        })?;
    let base = build_spec(&base_file, name.clone(), base_dir, project_environment)?;
    let mut profiles = BTreeMap::new();
    for (profile_name, patch) in patches {
        if profile_name == "base" {
            return Err(ConfigError {
                message: format!(
                    "Process '{name}': Process Profile name 'base' is reserved for the base specification"
                ),
            });
        }
        let Some(mapping) = patch.as_mapping() else {
            return Err(ConfigError {
                message: format!(
                    "Process '{name}' profile '{profile_name}' must be a partial mapping"
                ),
            });
        };
        for field in mapping.keys() {
            let Some(field) = field.as_str() else {
                return Err(ConfigError {
                    message: format!(
                        "Process '{name}' profile '{profile_name}' must use string field names"
                    ),
                });
            };
            if !PROFILE_FIELDS.contains(&field) {
                return Err(ConfigError {
                    message: format!(
                        "Process '{name}' profile '{profile_name}' cannot change field '{field}'; Process Profiles can change only command, shell, cwd, env_files, environment, enabled, and depends_on"
                    ),
                });
            }
        }
        let mut environment_check = serde_yaml::Mapping::new();
        environment_check.insert(Value::String(name.clone()), patch.clone());
        self::env::validate_process_overrides(
            &Value::Mapping(environment_check),
            &format!("Process '{name}' profile '{profile_name}'"),
        )?;
        let mut merged = raw.clone();
        merge_process_profile(&mut merged, patch);
        let profile_file: ProcessFile =
            serde_yaml::from_value(merged).map_err(|error| ConfigError {
                message: format!("Process '{name}' profile '{profile_name}': {error}"),
            })?;
        let spec = build_spec(&profile_file, name.clone(), base_dir, project_environment).map_err(
            |error| ConfigError {
                message: format!(
                    "Process '{name}' profile '{profile_name}': {}",
                    error.message
                ),
            },
        )?;
        profiles.insert(profile_name, spec);
    }

    if let Some(profile) = profile_override.as_deref()
        && profile != "base"
        && !profiles.contains_key(profile)
    {
        return Err(ConfigError {
            message: format!(
                "Process '{name}': profile override '{profile}' does not match a named Process Profile"
            ),
        });
    }

    Ok(ProfiledProcess {
        base,
        profiles,
        profile_override,
    })
}

/// Merge one Process Profile. Dependencies form one coherent startup contract,
/// so a profile replaces the complete mapping instead of inheriting entries.
fn merge_process_profile(base: &mut Value, patch: Value) {
    let depends_on = Value::String("depends_on".to_string());
    if patch
        .as_mapping()
        .is_some_and(|mapping| mapping.contains_key(&depends_on))
        && let Some(base_mapping) = base.as_mapping_mut()
    {
        base_mapping.remove(&depends_on);
    }
    merge_yaml(base, patch);
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
