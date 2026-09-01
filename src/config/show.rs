use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

use serde_yaml::{Mapping, Value};

use crate::model::{
    Autostart, CommandForm, EffectiveProject, Enabled, InputPolicy, LivenessConfig, ProcessKind,
    ProcessSpec, ReadinessCheck, ReadinessConfig, ReadinessProbe, RestartConfig, TerminalMode,
};

use super::ConfigError;

const REDACTED_VALUE: &str = "<redacted>";

/// Render the validated Project as stable canonical YAML without exposing
/// configured environment values.
pub(super) fn render(project: &EffectiveProject) -> Result<String, ConfigError> {
    let mut root = Mapping::new();
    insert(&mut root, "version", number(1));
    if project.base_profile_name() != "base" {
        insert(
            &mut root,
            "base_profile_name",
            string(project.base_profile_name()),
        );
    }

    let mut processes = Mapping::new();
    for process in project.processes() {
        insert(&mut processes, &process.name, process_value(process));
    }
    insert(&mut root, "processes", Value::Mapping(processes));

    let mut settings = Mapping::new();
    let mut shell = Mapping::new();
    insert(
        &mut shell,
        "program",
        os_string(project.shell().program.as_os_str()),
    );
    insert(
        &mut shell,
        "args",
        Value::Sequence(
            project
                .shell()
                .args
                .iter()
                .map(|argument| os_string(argument.as_os_str()))
                .collect(),
        ),
    );
    insert(&mut settings, "shell", Value::Mapping(shell));
    insert(
        &mut settings,
        "port_discovery",
        Value::Bool(project.port_discovery()),
    );
    insert(&mut root, "settings", Value::Mapping(settings));

    serde_yaml::to_string(&Value::Mapping(root)).map_err(|error| ConfigError {
        message: format!("could not render effective Project: {error}"),
    })
}

fn process_value(process: &ProcessSpec) -> Value {
    let mut map = Mapping::new();
    insert(
        &mut map,
        "kind",
        string(match process.kind {
            ProcessKind::Service => "service",
            ProcessKind::OneShot => "one-shot",
        }),
    );
    insert(
        &mut map,
        "enabled",
        boolean(process.enabled == Enabled::Yes),
    );
    insert(
        &mut map,
        "autostart",
        boolean(process.autostart == Autostart::Yes),
    );
    insert(&mut map, "cwd", path(&process.working_dir));
    insert(&mut map, "environment", process_environment(process));
    insert(
        &mut map,
        "terminal",
        terminal_value(process.terminal_mode, process.input_policy),
    );
    insert(
        &mut map,
        "success_exit_codes",
        Value::Sequence(
            process
                .success_exit_codes
                .iter()
                .map(|code| number(*code))
                .collect(),
        ),
    );
    if !process.dependencies.is_empty() {
        insert(&mut map, "depends_on", dependencies(&process.dependencies));
    }
    if let Some(readiness) = &process.readiness {
        insert(&mut map, "ready", readiness_value(readiness));
    }
    if let Some(liveness) = &process.liveness {
        insert(&mut map, "liveness", liveness_value(liveness));
    }
    insert(&mut map, "restart", restart_value(&process.restart));
    insert_command(&mut map, &process.command);
    Value::Mapping(map)
}

fn process_environment(process: &ProcessSpec) -> Value {
    let mut environment = Mapping::new();
    for (key, _) in &process.env {
        insert(&mut environment, key, string(REDACTED_VALUE));
    }
    for key in &process.env_remove {
        insert(&mut environment, key, Value::Null);
    }
    Value::Mapping(environment)
}

fn terminal_value(mode: TerminalMode, input: InputPolicy) -> Value {
    let mut terminal = Mapping::new();
    insert(
        &mut terminal,
        "mode",
        string(match mode {
            TerminalMode::Pipe => "pipe",
            TerminalMode::Pty => "pty",
        }),
    );
    insert(
        &mut terminal,
        "input",
        string(match input {
            InputPolicy::Disabled => "disabled",
            InputPolicy::Focused => "focused",
        }),
    );
    Value::Mapping(terminal)
}

fn dependencies(entries: &[crate::model::DependencySpec]) -> Value {
    let mut dependencies = Mapping::new();
    for dependency in entries {
        insert(
            &mut dependencies,
            &dependency.name,
            string(dependency.condition.label()),
        );
    }
    Value::Mapping(dependencies)
}

fn readiness_value(readiness: &ReadinessConfig) -> Value {
    let mut map = checks_value(&readiness.checks);
    if let Some(startup_timeout) = readiness.startup_timeout {
        insert(&mut map, "startup_timeout", duration(startup_timeout));
    }
    Value::Mapping(map)
}

fn liveness_value(liveness: &LivenessConfig) -> Value {
    Value::Mapping(checks_value(&liveness.checks))
}

fn checks_value(checks: &[ReadinessCheck]) -> Mapping {
    if checks.len() == 1 {
        return mapping_value(checks.first().expect("one check exists"));
    }
    let mut map = Mapping::new();
    insert(
        &mut map,
        "all",
        Value::Sequence(
            checks
                .iter()
                .map(|check| Value::Mapping(mapping_value(check)))
                .collect(),
        ),
    );
    map
}

fn mapping_value(check: &ReadinessCheck) -> Mapping {
    let mut map = probe_value(&check.probe);
    insert(&mut map, "initial_delay", duration(check.initial_delay));
    insert(&mut map, "interval", duration(check.interval));
    insert(&mut map, "timeout", duration(check.timeout));
    insert(
        &mut map,
        "success_threshold",
        number(check.success_threshold),
    );
    insert(
        &mut map,
        "failure_threshold",
        number(check.failure_threshold),
    );
    map
}

fn probe_value(probe: &ReadinessProbe) -> Mapping {
    let mut map = Mapping::new();
    match probe {
        ReadinessProbe::Tcp { host, port } => {
            let mut tcp = Mapping::new();
            insert(&mut tcp, "host", string(host));
            insert(&mut tcp, "port", number(*port));
            insert(&mut map, "tcp", Value::Mapping(tcp));
        }
        ReadinessProbe::Http {
            host,
            port,
            path: request_path,
        } => {
            let mut http = Mapping::new();
            insert(
                &mut http,
                "url",
                string(format!("http://{host}:{port}{request_path}")),
            );
            insert(&mut map, "http", Value::Mapping(http));
        }
        ReadinessProbe::Exec {
            command,
            working_dir,
            env,
            success_exit_codes,
        } => {
            let mut exec = Mapping::new();
            insert_command(&mut exec, command);
            if let Some(working_dir) = working_dir {
                insert(&mut exec, "cwd", path(working_dir));
            }
            if !env.is_empty() {
                let mut environment = Mapping::new();
                for (key, _) in env {
                    insert(&mut environment, key, string(REDACTED_VALUE));
                }
                insert(&mut exec, "environment", Value::Mapping(environment));
            }
            insert(
                &mut exec,
                "success_exit_codes",
                Value::Sequence(
                    success_exit_codes
                        .iter()
                        .map(|code| number(*code))
                        .collect(),
                ),
            );
            insert(&mut map, "exec", Value::Mapping(exec));
        }
        ReadinessProbe::Log { contains } => {
            let mut log = Mapping::new();
            insert(&mut log, "contains", string(contains));
            insert(&mut map, "log", Value::Mapping(log));
        }
    }
    map
}

fn restart_value(restart: &RestartConfig) -> Value {
    let mut map = Mapping::new();
    insert(&mut map, "policy", string(restart.policy.label()));
    insert(&mut map, "backoff", duration(restart.backoff));
    insert(&mut map, "max_restarts", number(restart.max_restarts));
    insert(&mut map, "on_unhealthy", boolean(restart.on_unhealthy));
    Value::Mapping(map)
}

fn insert_command(map: &mut Mapping, command: &CommandForm) {
    match command {
        CommandForm::Direct { program, args } => {
            let mut values = Vec::with_capacity(args.len() + 1);
            values.push(os_string(program.as_os_str()));
            values.extend(args.iter().map(|argument| os_string(argument.as_os_str())));
            insert(map, "command", Value::Sequence(values));
        }
        CommandForm::Shell { text } => {
            insert(map, "shell", string(text));
        }
    }
}

fn insert(map: &mut Mapping, key: &str, value: Value) {
    map.insert(Value::String(key.to_owned()), value);
}

fn string(value: impl Into<String>) -> Value {
    Value::String(value.into())
}

fn os_string(value: &OsStr) -> Value {
    string(value.to_string_lossy().into_owned())
}

fn path(value: &Path) -> Value {
    string(value.display().to_string())
}

fn boolean(value: bool) -> Value {
    Value::Bool(value)
}

fn number<T>(value: T) -> Value
where
    serde_yaml::Number: From<T>,
{
    Value::Number(value.into())
}

fn duration(value: Duration) -> Value {
    let milliseconds = value.as_millis();
    if milliseconds == 0 {
        return string("0ms");
    }
    let (amount, suffix) = if milliseconds.is_multiple_of(3_600_000) {
        (milliseconds / 3_600_000, "h")
    } else if milliseconds.is_multiple_of(60_000) {
        (milliseconds / 60_000, "m")
    } else if milliseconds.is_multiple_of(1_000) {
        (milliseconds / 1_000, "s")
    } else {
        (milliseconds, "ms")
    };
    string(format!("{amount}{suffix}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DependencyCondition, DependencySpec};

    #[test]
    fn duration_uses_the_smallest_canonical_whole_unit() {
        assert_eq!(duration(Duration::ZERO), string("0ms"));
        assert_eq!(duration(Duration::from_millis(1_500)), string("1500ms"));
        assert_eq!(duration(Duration::from_secs(60)), string("1m"));
        assert_eq!(duration(Duration::from_secs(60 * 60)), string("1h"));
    }

    #[test]
    fn dependency_condition_labels_are_canonical() {
        let value = dependencies(&[DependencySpec {
            name: "database".to_string(),
            condition: DependencyCondition::Ready,
        }]);
        let Value::Mapping(value) = value else {
            panic!("dependencies are a mapping");
        };
        assert_eq!(
            value.get(Value::String("database".to_string())),
            Some(&string("ready"))
        );
    }
}
