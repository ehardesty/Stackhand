//! Readiness configuration: one YAML `ready` block becomes one validated
//! [`ReadinessConfig`] or a clear per-Process failure. The block carries
//! exactly one leaf check or an `all` list with child scheduling fields.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::model::{LivenessConfig, ReadinessCheck, ReadinessConfig, ReadinessProbe};

use super::ConfigError;

const DURATION_FORMAT_ERROR: &str = "use a nonnegative whole number with an ms, s, m, or h suffix";
const DEFAULT_INTERVAL: Duration = Duration::from_secs(1);
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_SUCCESS_THRESHOLD: u32 = 1;
const DEFAULT_FAILURE_THRESHOLD: u32 = 1;

#[derive(Clone, Copy)]
struct CheckLabels {
    prefix: &'static str,
    noun: &'static str,
    child_startup_timeout_detail: &'static str,
}

const READINESS_LABELS: CheckLabels = CheckLabels {
    prefix: "ready",
    noun: "readiness",
    child_startup_timeout_detail: "startup_timeout is valid only on the parent 'ready' block",
};
const LIVENESS_LABELS: CheckLabels = CheckLabels {
    prefix: "liveness",
    noun: "liveness",
    child_startup_timeout_detail: "startup_timeout is not supported for liveness",
};

/// One `ready` block: one leaf check or an `all` list with independent
/// scheduling for every child.
pub(super) fn build_readiness(
    process_name: &str,
    file: &ReadinessFile,
    base_dir: &Path,
) -> Result<ReadinessConfig, ConfigError> {
    reject_any_form(process_name, file, READINESS_LABELS)?;
    let startup_timeout = file
        .startup_timeout
        .as_deref()
        .map(|value| {
            positive_duration(
                process_name,
                "startup_timeout",
                Some(value),
                Duration::ZERO,
                READINESS_LABELS,
            )
        })
        .transpose()?;
    let checks = build_checks(process_name, file, base_dir, READINESS_LABELS)?;
    Ok(ReadinessConfig {
        checks,
        startup_timeout,
    })
}

/// One `liveness` block. It shares leaf validation with readiness, but a
/// liveness policy cannot own a startup deadline.
pub(super) fn build_liveness(
    process_name: &str,
    file: &ReadinessFile,
    base_dir: &Path,
) -> Result<LivenessConfig, ConfigError> {
    reject_any_form(process_name, file, LIVENESS_LABELS)?;
    if file.startup_timeout.is_some() {
        return Err(check_error(
            process_name,
            LIVENESS_LABELS,
            "startup_timeout is not supported for liveness",
        ));
    }
    let checks = build_checks(process_name, file, base_dir, LIVENESS_LABELS)?;
    Ok(LivenessConfig { checks })
}

fn build_checks(
    process_name: &str,
    file: &ReadinessFile,
    base_dir: &Path,
    labels: CheckLabels,
) -> Result<Vec<ReadinessCheck>, ConfigError> {
    let checks = if let Some(children) = &file.all {
        if file.tcp.is_some() || file.http.is_some() || file.exec.is_some() || file.log.is_some() {
            return Err(check_error(
                process_name,
                labels,
                "define exactly one of 'tcp', 'http', 'exec', 'log', or 'all'",
            ));
        }
        if file.initial_delay.is_some()
            || file.interval.is_some()
            || file.timeout.is_some()
            || file.success_threshold.is_some()
            || file.failure_threshold.is_some()
        {
            return Err(check_error(
                process_name,
                labels,
                format!(
                    "an 'all' {} check sets scheduling fields on each child, not on the parent",
                    labels.noun
                ),
            ));
        }
        if children.len() < 2 {
            return Err(check_error(
                process_name,
                labels,
                "'all' must contain at least two child checks",
            ));
        }
        children
            .iter()
            .enumerate()
            .map(|(index, child)| {
                build_leaf(process_name, child, Some(index + 1), base_dir, labels)
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        vec![build_leaf(process_name, file, None, base_dir, labels)?]
    };

    Ok(checks)
}

fn build_leaf(
    process_name: &str,
    file: &ReadinessFile,
    child_index: Option<usize>,
    base_dir: &Path,
    labels: CheckLabels,
) -> Result<ReadinessCheck, ConfigError> {
    let fail = |detail: String| Err(check_error_for(process_name, child_index, labels, detail));
    if file.all.is_some() {
        return fail(format!(
            "nested 'all' {} checks are not supported",
            labels.noun
        ));
    }
    if file.any.is_some() {
        return fail(format!("the 'any' {} form is not supported", labels.noun));
    }
    if child_index.is_some() && file.startup_timeout.is_some() {
        return fail(labels.child_startup_timeout_detail.to_string());
    }
    let probe = match (&file.tcp, &file.http, &file.exec, &file.log) {
        (Some(tcp), None, None, None) => {
            if tcp.host.is_empty() {
                return fail("tcp host must not be empty".to_string());
            }
            if tcp.port == 0 {
                return fail("tcp port must be between 1 and 65535".to_string());
            }
            ReadinessProbe::Tcp {
                host: tcp.host.clone(),
                port: tcp.port,
            }
        }
        (None, Some(http), None, None) => {
            let (host, port, path) = parse_http_url(&http.url, labels.noun)
                .map_err(|detail| check_error_for(process_name, child_index, labels, detail))?;
            ReadinessProbe::Http { host, port, path }
        }
        (None, None, Some(exec), None) => {
            build_exec_probe(process_name, exec, child_index, base_dir, labels)?
        }
        (None, None, None, Some(log)) => {
            if log.contains.is_empty() {
                return fail("log contains must not be empty".to_string());
            }
            ReadinessProbe::Log {
                contains: log.contains.clone(),
            }
        }
        _ => {
            return fail("define exactly one of 'tcp', 'http', 'exec', or 'log'".to_string());
        }
    };
    let initial_delay = duration_or_default(
        process_name,
        "initial_delay",
        file.initial_delay.as_deref(),
        Duration::ZERO,
        labels,
    )?;
    let interval = positive_duration(
        process_name,
        "interval",
        file.interval.as_deref(),
        DEFAULT_INTERVAL,
        labels,
    )?;
    let timeout = positive_duration(
        process_name,
        "timeout",
        file.timeout.as_deref(),
        DEFAULT_TIMEOUT,
        labels,
    )?;
    let success_threshold = configured_threshold(
        process_name,
        "success_threshold",
        file.success_threshold,
        DEFAULT_SUCCESS_THRESHOLD,
        labels,
    )?;
    let failure_threshold = configured_threshold(
        process_name,
        "failure_threshold",
        file.failure_threshold,
        DEFAULT_FAILURE_THRESHOLD,
        labels,
    )?;
    Ok(ReadinessCheck {
        probe,
        initial_delay,
        interval,
        timeout,
        success_threshold,
        failure_threshold,
    })
}

fn reject_any_form(
    process_name: &str,
    file: &ReadinessFile,
    labels: CheckLabels,
) -> Result<(), ConfigError> {
    if file.any.is_some() {
        return Err(check_error(
            process_name,
            labels,
            format!(
                "the 'any' {} form is not supported; use 'all' or one leaf check",
                labels.noun
            ),
        ));
    }
    Ok(())
}

fn check_error(process_name: &str, labels: CheckLabels, detail: impl Into<String>) -> ConfigError {
    ConfigError {
        message: format!(
            "Process '{process_name}': {}: {}",
            labels.prefix,
            detail.into()
        ),
    }
}

fn check_error_for(
    process_name: &str,
    child_index: Option<usize>,
    labels: CheckLabels,
    detail: impl Into<String>,
) -> ConfigError {
    let detail = detail.into();
    let detail = child_index
        .map(|index| format!("all child {index}: {detail}"))
        .unwrap_or(detail);
    check_error(process_name, labels, detail)
}

fn build_exec_probe(
    process_name: &str,
    file: &ExecFile,
    child_index: Option<usize>,
    base_dir: &Path,
    labels: CheckLabels,
) -> Result<ReadinessProbe, ConfigError> {
    let command = file.command.as_ref().ok_or_else(|| {
        check_error_for(
            process_name,
            child_index,
            labels,
            "exec requires a 'command'",
        )
    })?;
    let command = super::build_command_form(Some(command), None)
        .map_err(|detail| check_error_for(process_name, child_index, labels, detail))?;
    let working_dir = file
        .working_dir
        .as_deref()
        .map(|directory| {
            let path = PathBuf::from(directory);
            let path = if path.is_absolute() {
                path
            } else {
                base_dir.join(path)
            };
            if path.is_dir() {
                Ok(path)
            } else {
                Err(check_error_for(
                    process_name,
                    child_index,
                    labels,
                    format!("exec working directory '{}' does not exist", path.display()),
                ))
            }
        })
        .transpose()?;
    let env = file
        .env
        .as_ref()
        .map(|entries| {
            entries
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default();
    let success_exit_codes = super::build_success_exit_codes(file.success_exit_codes.clone())
        .map_err(|detail| check_error_for(process_name, child_index, labels, detail))?;
    Ok(ReadinessProbe::Exec {
        command,
        working_dir,
        env,
        success_exit_codes,
    })
}

fn duration_or_default(
    process_name: &str,
    field: &str,
    value: Option<&str>,
    default: Duration,
    labels: CheckLabels,
) -> Result<Duration, ConfigError> {
    value
        .map(|value| {
            parse_duration(value)
                .map_err(|detail| check_error(process_name, labels, format!("{field}: {detail}")))
        })
        .transpose()
        .map(|value| value.unwrap_or(default))
}

fn positive_duration(
    process_name: &str,
    field: &str,
    value: Option<&str>,
    default: Duration,
    labels: CheckLabels,
) -> Result<Duration, ConfigError> {
    let duration = duration_or_default(process_name, field, value, default, labels)?;
    if duration.is_zero() {
        return Err(check_error(
            process_name,
            labels,
            format!("{field} must be greater than zero"),
        ));
    }
    Ok(duration)
}

fn configured_threshold(
    process_name: &str,
    field: &str,
    value: Option<u32>,
    default: u32,
    labels: CheckLabels,
) -> Result<u32, ConfigError> {
    let threshold = value.unwrap_or(default);
    if threshold == 0 {
        return Err(check_error(
            process_name,
            labels,
            format!("{field} must be a positive whole number"),
        ));
    }
    Ok(threshold)
}

/// Parse a nonnegative whole-number duration with one supported suffix.
pub(super) fn parse_duration(value: &str) -> Result<Duration, String> {
    let (digits, nanos_per_unit) = if let Some(digits) = value.strip_suffix("ms") {
        (digits, 1_000_000_u128)
    } else if let Some(digits) = value.strip_suffix('s') {
        (digits, 1_000_000_000_u128)
    } else if let Some(digits) = value.strip_suffix('m') {
        (digits, 60_u128 * 1_000_000_000)
    } else if let Some(digits) = value.strip_suffix('h') {
        (digits, 60_u128 * 60 * 1_000_000_000)
    } else {
        return Err(DURATION_FORMAT_ERROR.to_string());
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(DURATION_FORMAT_ERROR.to_string());
    }
    let amount = digits
        .parse::<u128>()
        .map_err(|_| "the duration overflows the supported range".to_string())?;
    let nanos = amount
        .checked_mul(nanos_per_unit)
        .ok_or_else(|| "the duration overflows the supported range".to_string())?;
    let seconds = nanos / 1_000_000_000;
    if seconds > u64::MAX as u128 {
        return Err("the duration overflows the supported range".to_string());
    }
    Ok(Duration::new(
        seconds as u64,
        (nanos % 1_000_000_000) as u32,
    ))
}

/// Parse one plain-`http` readiness URL into its connect target: host,
/// port (defaulting to 80), and request path (defaulting to `/`). Only
/// what a raw HTTP/1.0 GET can reach is accepted; anything else fails with
/// one clear reason.
fn parse_http_url(url: &str, check_name: &str) -> Result<(String, u16, String), String> {
    let Some(rest) = url.strip_prefix("http://") else {
        return Err(if url.starts_with("https://") {
            format!("https URLs are not supported for {check_name} probes; use a plain http URL")
        } else {
            format!("invalid http URL '{url}': it must start with 'http://'")
        });
    };
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => (rest, "/".to_string()),
    };
    if authority.is_empty() || authority.starts_with('[') || authority.contains('@') {
        return Err(format!(
            "invalid http URL '{url}': the host is missing or unsupported"
        ));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port_text)) => {
            let Ok(port) = port_text.parse::<u16>() else {
                return Err(format!(
                    "invalid http URL '{url}': the port must be between 1 and 65535"
                ));
            };
            if port == 0 {
                return Err(format!(
                    "invalid http URL '{url}': the port must be between 1 and 65535"
                ));
            }
            (host.to_string(), port)
        }
        None => (authority.to_string(), 80),
    };
    if host.is_empty() {
        return Err(format!("invalid http URL '{url}': the host is empty"));
    }
    Ok((host, port, path))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReadinessFile {
    tcp: Option<TcpProbeFile>,
    http: Option<HttpProbeFile>,
    exec: Option<ExecFile>,
    log: Option<LogProbeFile>,
    all: Option<Vec<ReadinessFile>>,
    /// Parsed only to provide a clear unsupported-form diagnostic.
    any: Option<serde_yaml::Value>,
    initial_delay: Option<String>,
    interval: Option<String>,
    timeout: Option<String>,
    success_threshold: Option<u32>,
    failure_threshold: Option<u32>,
    startup_timeout: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TcpProbeFile {
    host: String,
    port: u16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct HttpProbeFile {
    url: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LogProbeFile {
    contains: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecFile {
    command: Option<super::CommandFile>,
    working_dir: Option<String>,
    env: Option<std::collections::BTreeMap<String, String>>,
    success_exit_codes: Option<Vec<i32>>,
}

#[cfg(test)]
#[path = "readiness_tests.rs"]
mod tests;
