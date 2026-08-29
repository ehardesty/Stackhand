use std::path::Path;

use super::ConfigError;

/// Add the configuration layer that produced a later validation failure.
pub(super) fn with_source(error: ConfigError, source: &str) -> ConfigError {
    ConfigError {
        message: format!("{} (source: {source})", error.message),
    }
}

pub(super) fn format_yaml_error(path: &Path, error: &serde_yaml::Error) -> String {
    format_yaml_error_for_source(
        error,
        format!("invalid configuration '{}'", path.display()),
        true,
    )
}

pub(super) fn format_merged_yaml_error(
    path: &Path,
    layer: &str,
    error: &serde_yaml::Error,
) -> String {
    format_yaml_error_for_source(
        error,
        format!(
            "invalid merged Project after {layer} (base Project '{}')",
            path.display()
        ),
        false,
    )
}

pub(super) fn format_local_yaml_error(path: &Path, error: &serde_yaml::Error) -> String {
    format_yaml_error_for_source(
        error,
        format!("invalid local override '{}'", path.display()),
        true,
    )
}

fn format_yaml_error_for_source(
    error: &serde_yaml::Error,
    source: String,
    include_location: bool,
) -> String {
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
            "{source} at line {}, column {}: {detail}",
            location.line(),
            location.column(),
        );
    }
    format!("{source}: {detail}")
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
