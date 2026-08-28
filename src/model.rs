//! The effective Project model.
//!
//! Configuration (Milestone 1: YAML version 1) parses and validates into one
//! [`EffectiveProject`]. The Supervisor consumes that validated Project and
//! never sees YAML text or diagnostics.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

/// How a Process expects to run. A Process is exactly one of these kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessKind {
    /// A Process that stays active until it is stopped.
    Service,
    /// A Process that runs to completion and exits.
    OneShot,
}

/// What the Supervisor should do with a Process when the Project starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Autostart {
    /// Start the Process when the Project starts.
    Yes,
    /// Leave the Process stopped; it remains available for a manual start.
    No,
}

impl Autostart {
    pub fn flag(value: bool) -> Self {
        if value { Self::Yes } else { Self::No }
    }
}

/// Whether configuration enables a Process at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Enabled {
    Yes,
    /// The Process stays visible but cannot start.
    No,
}

impl Enabled {
    pub fn flag(value: bool) -> Self {
        if value { Self::Yes } else { Self::No }
    }
}

/// Exactly one command form for one Process. The two forms are mutually
/// exclusive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandForm {
    /// Run `program` with `args` directly, without shell parsing.
    Direct {
        program: OsString,
        args: Vec<OsString>,
    },
    /// Run `text` through the user's shell.
    Shell { text: String },
}

impl CommandForm {
    /// Resolve this validated command with the Project's shell policy.
    pub(crate) fn resolve(&self, shell: &ShellConfig) -> (OsString, Vec<OsString>) {
        match self {
            Self::Direct { program, args } => (program.clone(), args.clone()),
            Self::Shell { text } => {
                let mut args = shell.args.clone();
                args.push(OsString::from(text));
                (shell.program.clone(), args)
            }
        }
    }
}

/// The terminal transport for one Process's Runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalMode {
    /// Separate stdout and stderr pipes without terminal semantics.
    Pipe,
    /// A pseudo-terminal owned by each Run.
    Pty,
}

/// Whether a Process may receive keyboard input from Stackhand.
///
/// Terminal allocation and input policy stay separate: a PTY-mode Process
/// can have colors without receiving keys.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InputPolicy {
    /// Never deliver child input to this Process.
    #[default]
    Disabled,
    /// Deliver input only while this Process is selected and focused.
    Focused,
}

/// What must hold for one Dependency before a dependent Process starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DependencyCondition {
    /// The dependency Process has an active Run that is Starting or Running.
    Started,
    /// The dependency Service has an active Run whose readiness probe has
    /// passed. Valid only when the dependency Process is a Service.
    Ready,
    /// The dependency One-shot's latest scheduled Run has ended.
    /// Valid only when the dependency Process is a One-shot.
    Exited,
    /// The dependency One-shot's latest completed Run exited with an accepted
    /// success code. Valid only when the dependency Process is a One-shot.
    CompletedSuccessfully,
}

impl DependencyCondition {
    /// The configuration spelling of this condition.
    pub fn label(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Ready => "ready",
            Self::Exited => "exited",
            Self::CompletedSuccessfully => "completed_successfully",
        }
    }
}

/// One bounded network target for a leaf readiness check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadinessProbe {
    /// A TCP connection must succeed against this endpoint.
    Tcp { host: String, port: u16 },
    /// An HTTP GET must return a successful 2xx status from this endpoint,
    /// parsed from the configured URL at configuration time. Redirects are
    /// never followed.
    Http {
        host: String,
        port: u16,
        /// The request path and any query, always starting with `/`.
        path: String,
    },
    /// Run one validated direct or shell command without a PTY. The optional
    /// working directory and environment entries override the Process values.
    Exec {
        command: CommandForm,
        working_dir: Option<PathBuf>,
        env: Vec<(String, String)>,
        success_exit_codes: Vec<i32>,
    },
    /// Observe the current Run's live output for one literal substring.
    Log { contains: String },
}

/// Scheduling and threshold policy for one leaf readiness check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadinessCheck {
    pub probe: ReadinessProbe,
    /// How long to wait after spawn before the first attempt.
    pub initial_delay: Duration,
    /// How long after an attempt completes before the next one may run.
    pub interval: Duration,
    /// How long one attempt may take before it fails as timed out.
    pub timeout: Duration,
    /// Consecutive passing attempts required to become ready.
    pub success_threshold: u32,
    /// Consecutive failing attempts required after readiness to become
    /// failing.
    pub failure_threshold: u32,
}

/// The readiness policy of one Service. A direct check has one leaf in
/// `checks`; an `all` check has one independently scheduled leaf per child.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadinessConfig {
    pub checks: Vec<ReadinessCheck>,
    /// Optional deadline for the complete readiness policy to pass after
    /// spawn. This is one composite deadline, not one deadline per child.
    pub startup_timeout: Option<Duration>,
}

/// One startup Dependency of one Process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DependencySpec {
    pub name: String,
    pub condition: DependencyCondition,
}

/// One configured Process as it will actually run.
#[derive(Clone, Debug)]
pub struct ProcessSpec {
    pub name: String,
    pub kind: ProcessKind,
    pub enabled: Enabled,
    pub autostart: Autostart,
    /// Exit codes that count as success for a One-shot Run.
    pub success_exit_codes: Vec<i32>,
    pub command: CommandForm,
    /// Absolute working directory. Relative paths are resolved by
    /// configuration before this model exists.
    pub working_dir: PathBuf,
    pub env: Vec<(String, String)>,
    pub terminal_mode: TerminalMode,
    /// Consumed by TUI input routing through ProcessSnapshot.input_focused;
    /// configuration validation covers it in Issue #23.
    pub input_policy: InputPolicy,
    /// Startup Dependencies. A Dependency is a startup relationship only;
    /// it never couples lifetimes after the dependent starts.
    pub dependencies: Vec<DependencySpec>,
    /// When set, the Service stays Starting until this probe passes. Valid
    /// on Services only; configuration validation rejects it on One-shots.
    pub readiness: Option<ReadinessConfig>,
}

/// Why one effective Project could not be built.
#[derive(Debug, PartialEq, Eq)]
pub enum ProjectError {
    DuplicateName(String),
    /// A Dependency names a Process that is not configured.
    UnknownDependency {
        process: String,
        dependency: String,
    },
    /// A Dependency condition is not valid for the dependency Process's
    /// kind; only One-shot dependencies support `exited` and
    /// `completed_successfully`, and only Service dependencies support
    /// `ready`.
    InvalidCondition {
        process: String,
        dependency: String,
        condition: String,
    },
    /// A readiness probe was configured on a One-shot; only Services wait
    /// for readiness.
    ReadinessOnOneShot {
        process: String,
    },
    /// The Dependency graph has a cycle; the path repeats its first name.
    DependencyCycle(Vec<String>),
}

/// The launcher used for shell-expression commands in one Project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellConfig {
    /// The interpreter executable, for example `/bin/sh` or `/bin/bash`.
    pub program: OsString,
    /// Arguments placed before the shell expression.
    pub args: Vec<OsString>,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            program: OsString::from("/bin/sh"),
            args: vec![OsString::from("-c")],
        }
    }
}

/// The validated set of Processes for one Stackhand session. Process order
/// is configuration order and stays stable for the session.
#[derive(Clone, Default)]
pub struct EffectiveProject {
    processes: Vec<ProcessSpec>,
    positions: HashMap<String, usize>,
    /// Each inner list follows its Process's configured Dependency order.
    dependency_indices: Vec<Vec<usize>>,
    shell: ShellConfig,
}

impl std::fmt::Debug for EffectiveProject {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EffectiveProject")
            .field("processes", &self.processes)
            .finish()
    }
}

impl EffectiveProject {
    /// Build one Project with the macOS prototype shell fallback.
    pub fn new(processes: Vec<ProcessSpec>) -> Result<Self, ProjectError> {
        Self::with_shell(processes, ShellConfig::default())
    }

    /// Build one Project with a validated shell launcher.
    pub fn with_shell(
        processes: Vec<ProcessSpec>,
        shell: ShellConfig,
    ) -> Result<Self, ProjectError> {
        let mut positions = HashMap::with_capacity(processes.len());
        for (index, spec) in processes.iter().enumerate() {
            if positions.insert(spec.name.clone(), index).is_some() {
                return Err(ProjectError::DuplicateName(spec.name.clone()));
            }
        }

        let mut dependency_indices = Vec::with_capacity(processes.len());
        for spec in &processes {
            let mut resolved = Vec::with_capacity(spec.dependencies.len());
            for dependency in &spec.dependencies {
                let Some(&dependency_index) = positions.get(dependency.name.as_str()) else {
                    return Err(ProjectError::UnknownDependency {
                        process: spec.name.clone(),
                        dependency: dependency.name.clone(),
                    });
                };
                let condition_kind_mismatch = match dependency.condition {
                    DependencyCondition::Exited | DependencyCondition::CompletedSuccessfully => {
                        processes[dependency_index].kind == ProcessKind::Service
                    }
                    DependencyCondition::Ready => {
                        processes[dependency_index].kind == ProcessKind::OneShot
                    }
                    DependencyCondition::Started => false,
                };
                if condition_kind_mismatch {
                    return Err(ProjectError::InvalidCondition {
                        process: spec.name.clone(),
                        dependency: dependency.name.clone(),
                        condition: dependency.condition.label().to_string(),
                    });
                }
                resolved.push(dependency_index);
            }
            if spec.readiness.is_some() && spec.kind == ProcessKind::OneShot {
                return Err(ProjectError::ReadinessOnOneShot {
                    process: spec.name.clone(),
                });
            }
            dependency_indices.push(resolved);
        }

        if let Some(path) = find_dependency_cycle(&processes, &dependency_indices) {
            return Err(ProjectError::DependencyCycle(path));
        }
        Ok(Self {
            processes,
            positions,
            dependency_indices,
            shell,
        })
    }

    pub fn processes(&self) -> &[ProcessSpec] {
        &self.processes
    }

    /// Return the Project's shell launcher for shell-expression commands.
    pub fn shell(&self) -> &ShellConfig {
        &self.shell
    }

    /// Resolve a user-facing Process name to its stable session position.
    pub(crate) fn process_index(&self, name: &str) -> Option<usize> {
        self.positions.get(name).copied()
    }

    /// Iterate one Process's validated Dependencies in configuration order.
    /// Each item keeps the configured name and condition for diagnostics and
    /// adds the stable session position used by Project operations.
    pub(crate) fn resolved_dependencies(
        &self,
        index: usize,
    ) -> impl Iterator<Item = (usize, &DependencySpec)> {
        self.dependency_indices[index]
            .iter()
            .copied()
            .zip(&self.processes[index].dependencies)
    }
}

/// Find one cycle in the Dependency graph, or `None` when the graph is
/// acyclic. The reported path visits each Process once and repeats its
/// first name at the end. Configuration order decides the search order so
/// failures stay stable.
fn find_dependency_cycle(
    processes: &[ProcessSpec],
    dependency_indices: &[Vec<usize>],
) -> Option<Vec<String>> {
    // 0 = unvisited, 1 = on the current path, 2 = fully explored.
    let mut state = vec![0u8; processes.len()];
    let mut path: Vec<usize> = Vec::new();

    fn visit(
        index: usize,
        processes: &[ProcessSpec],
        dependency_indices: &[Vec<usize>],
        state: &mut [u8],
        path: &mut Vec<usize>,
    ) -> Option<Vec<String>> {
        if state[index] == 1 {
            let start = path
                .iter()
                .position(|i| *i == index)
                .expect("cycle node is on the path");
            let names = path[start..]
                .iter()
                .map(|i| processes[*i].name.clone())
                .collect::<Vec<_>>();
            let mut cycle = names;
            cycle.push(processes[index].name.clone());
            return Some(cycle);
        }
        if state[index] == 2 {
            return None;
        }
        state[index] = 1;
        path.push(index);
        for &dependency_index in &dependency_indices[index] {
            if let Some(cycle) = visit(dependency_index, processes, dependency_indices, state, path)
            {
                return Some(cycle);
            }
        }
        path.pop();
        state[index] = 2;
        None
    }

    for index in 0..processes.len() {
        if let Some(cycle) = visit(index, processes, dependency_indices, &mut state, &mut path) {
            return Some(cycle);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bare(name: &str) -> ProcessSpec {
        ProcessSpec {
            name: name.to_string(),
            kind: ProcessKind::Service,
            enabled: Enabled::Yes,
            autostart: Autostart::No,
            success_exit_codes: vec![0],
            command: CommandForm::Direct {
                program: "sleep".into(),
                args: vec!["1".into()],
            },
            working_dir: std::env::temp_dir(),
            env: Vec::new(),
            terminal_mode: TerminalMode::Pipe,
            input_policy: InputPolicy::Disabled,
            dependencies: Vec::new(),
            readiness: None,
        }
    }

    fn depending_on(name: &str, dependencies: &[&str]) -> ProcessSpec {
        let mut spec = bare(name);
        spec.dependencies = dependencies
            .iter()
            .map(|dependency| DependencySpec {
                name: (*dependency).to_string(),
                condition: DependencyCondition::Started,
            })
            .collect();
        spec
    }

    #[test]
    fn unknown_dependency_references_are_rejected() {
        let error = EffectiveProject::new(vec![depending_on("api", &["db"]), bare("worker")])
            .expect_err("a missing reference must fail");
        assert_eq!(
            error,
            ProjectError::UnknownDependency {
                process: "api".into(),
                dependency: "db".into()
            }
        );
    }

    #[test]
    fn dependency_cycles_are_rejected_with_the_cycle_path() {
        let error = EffectiveProject::new(vec![
            depending_on("api", &["db"]),
            depending_on("db", &["worker"]),
            depending_on("worker", &["api"]),
        ])
        .expect_err("a cycle must fail");
        assert_eq!(
            error,
            ProjectError::DependencyCycle(vec![
                "api".into(),
                "db".into(),
                "worker".into(),
                "api".into()
            ])
        );
    }

    #[test]
    fn a_self_dependency_is_a_cycle() {
        let error = EffectiveProject::new(vec![depending_on("api", &["api"])])
            .expect_err("a self-dependency must fail");
        assert_eq!(
            error,
            ProjectError::DependencyCycle(vec!["api".into(), "api".into()])
        );
    }

    #[test]
    fn completed_successfully_requires_a_one_shot_dependency() {
        let mut setup = bare("setup");
        setup.kind = ProcessKind::OneShot;
        let mut api = depending_on("api", &["setup"]);
        api.dependencies[0].condition = DependencyCondition::CompletedSuccessfully;

        EffectiveProject::new(vec![api.clone(), setup])
            .expect("a One-shot dependency supports completed_successfully");

        let error = EffectiveProject::new(vec![api, bare("setup")])
            .expect_err("a Service dependency must reject completed_successfully");
        assert_eq!(
            error,
            ProjectError::InvalidCondition {
                process: "api".into(),
                dependency: "setup".into(),
                condition: "completed_successfully".into(),
            }
        );
    }

    #[test]
    fn acyclic_shared_dependencies_are_accepted() {
        let project = EffectiveProject::new(vec![
            depending_on("api", &["db"]),
            depending_on("worker", &["db"]),
            bare("db"),
        ])
        .expect("shared dependencies are valid");
        assert_eq!(project.processes().len(), 3);
    }

    #[test]
    fn resolved_dependencies_keep_configuration_order_and_public_names() {
        let project = EffectiveProject::new(vec![
            depending_on("api", &["cache", "db"]),
            bare("db"),
            bare("cache"),
        ])
        .expect("the graph is valid");

        let dependencies = project
            .resolved_dependencies(project.process_index("api").expect("api is configured"))
            .map(|(index, dependency)| (index, dependency.name.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(dependencies, [(2, "cache"), (1, "db")]);
        assert_eq!(project.processes()[0].name, "api");
    }
}
