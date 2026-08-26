//! The effective Project model.
//!
//! Configuration (Milestone 1: YAML version 1) parses and validates into one
//! [`EffectiveProject`]. The Supervisor consumes that validated Project and
//! never sees YAML text or diagnostics.

use std::ffi::OsString;
use std::path::PathBuf;

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
#[derive(Clone, Debug)]
pub enum CommandForm {
    /// Run `program` with `args` directly, without shell parsing.
    Direct {
        program: OsString,
        args: Vec<OsString>,
    },
    /// Run `text` through the user's shell.
    Shell { text: String },
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
    /// The dependency One-shot's latest completed Run exited with code zero.
    /// Valid only when the dependency Process is a One-shot.
    CompletedSuccessfully,
}

impl DependencyCondition {
    /// The configuration spelling of this condition.
    pub fn label(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::CompletedSuccessfully => "completed_successfully",
        }
    }
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
    pub command: CommandForm,
    /// Absolute working directory. Relative paths are resolved by
    /// configuration before this model exists.
    pub working_dir: PathBuf,
    pub env: Vec<(String, String)>,
    pub terminal_mode: TerminalMode,
    /// Consumed by TUI input routing (Issue #30); configuration validation
    /// covers it in Issue #23.
    #[allow(dead_code)]
    pub input_policy: InputPolicy,
    /// Startup Dependencies. A Dependency is a startup relationship only;
    /// it never couples lifetimes after the dependent starts.
    pub dependencies: Vec<DependencySpec>,
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
    /// kind; only One-shot dependencies support `completed_successfully`.
    InvalidCondition {
        process: String,
        dependency: String,
        condition: String,
    },
    /// The Dependency graph has a cycle; the path repeats its first name.
    DependencyCycle(Vec<String>),
}

/// The validated set of Processes for one Stackhand session. Process order
/// is configuration order and stays stable for the session.
#[derive(Clone, Debug, Default)]
pub struct EffectiveProject {
    processes: Vec<ProcessSpec>,
}

impl EffectiveProject {
    /// Build one Project, rejecting duplicate Process names and invalid
    /// Dependency graphs before any Process can start.
    pub fn new(processes: Vec<ProcessSpec>) -> Result<Self, ProjectError> {
        let mut seen = std::collections::HashSet::new();
        for spec in &processes {
            if !seen.insert(spec.name.as_str()) {
                return Err(ProjectError::DuplicateName(spec.name.clone()));
            }
        }
        let positions: std::collections::HashMap<&str, usize> = processes
            .iter()
            .enumerate()
            .map(|(index, spec)| (spec.name.as_str(), index))
            .collect();
        for spec in &processes {
            for dependency in &spec.dependencies {
                let Some(&dependency_index) = positions.get(dependency.name.as_str()) else {
                    return Err(ProjectError::UnknownDependency {
                        process: spec.name.clone(),
                        dependency: dependency.name.clone(),
                    });
                };
                if dependency.condition == DependencyCondition::CompletedSuccessfully
                    && processes[dependency_index].kind == ProcessKind::Service
                {
                    return Err(ProjectError::InvalidCondition {
                        process: spec.name.clone(),
                        dependency: dependency.name.clone(),
                        condition: dependency.condition.label().to_string(),
                    });
                }
            }
        }
        find_dependency_cycle(&processes, &positions).map_or(Ok(Self { processes }), |path| {
            Err(ProjectError::DependencyCycle(path))
        })
    }

    pub fn processes(&self) -> &[ProcessSpec] {
        &self.processes
    }
}

/// Find one cycle in the Dependency graph, or `None` when the graph is
/// acyclic. The reported path visits each Process once and repeats its
/// first name at the end. Configuration order decides the search order so
/// failures stay stable.
fn find_dependency_cycle(
    processes: &[ProcessSpec],
    positions: &std::collections::HashMap<&str, usize>,
) -> Option<Vec<String>> {
    // 0 = unvisited, 1 = on the current path, 2 = fully explored.
    let mut state = vec![0u8; processes.len()];
    let mut path: Vec<usize> = Vec::new();

    fn visit(
        index: usize,
        processes: &[ProcessSpec],
        positions: &std::collections::HashMap<&str, usize>,
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
        for dependency in &processes[index].dependencies {
            let next = positions[dependency.name.as_str()];
            if let Some(cycle) = visit(next, processes, positions, state, path) {
                return Some(cycle);
            }
        }
        path.pop();
        state[index] = 2;
        None
    }

    for index in 0..processes.len() {
        if let Some(cycle) = visit(index, processes, positions, &mut state, &mut path) {
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
            command: CommandForm::Direct {
                program: "sleep".into(),
                args: vec!["1".into()],
            },
            working_dir: std::env::temp_dir(),
            env: Vec::new(),
            terminal_mode: TerminalMode::Pipe,
            input_policy: InputPolicy::Disabled,
            dependencies: Vec::new(),
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
}
