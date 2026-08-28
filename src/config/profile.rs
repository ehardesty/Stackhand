use serde_yaml::{Mapping, Value};

use super::ConfigError;
use super::env::validate_process_overrides;

/// Apply the explicitly selected profiles to one raw configuration document.
///
/// Raw YAML is merged before typed deserialization. This keeps the merge rules
/// uniform for every mapping, scalar, and sequence in the configuration.
pub(super) fn apply_profiles(
    document: &mut Value,
    requested_profiles: &[String],
) -> Result<(), ConfigError> {
    let profiles = document
        .as_mapping()
        .and_then(|root| root.get(yaml_key("profiles")))
        .and_then(Value::as_mapping)
        .cloned();

    for requested in requested_profiles {
        let profile = profiles
            .as_ref()
            .and_then(|profiles| profiles.get(yaml_key(requested)))
            .cloned()
            .ok_or_else(|| ConfigError {
                message: format!("unknown profile '{requested}'"),
            })?;
        apply_profile(document, &profile, requested)?;
    }
    normalize_process_entries(document);
    Ok(())
}

/// Keep an empty keyed Process entry representable until typed validation can
/// report which required command form is missing.
fn normalize_process_entries(document: &mut Value) {
    let Some(processes) = document
        .as_mapping_mut()
        .and_then(|root| root.get_mut(yaml_key("processes")))
        .and_then(Value::as_mapping_mut)
    else {
        return;
    };
    for process in processes.values_mut() {
        if process.is_null() {
            *process = Value::Mapping(Mapping::new());
        }
    }
}

/// Apply one same-directory local override with the same merge rules as a
/// profile. A local file may repeat the schema version, but it cannot select
/// profiles or replace a complete Process with null.
pub(super) fn apply_local_override(document: &mut Value, local: &Value) -> Result<(), ConfigError> {
    let Some(root) = local.as_mapping() else {
        return Err(ConfigError {
            message: "local override must be a mapping".to_string(),
        });
    };
    if root.contains_key(yaml_key("profiles")) {
        return Err(ConfigError {
            message: "local override cannot define profiles".to_string(),
        });
    }
    if root.get(yaml_key("processes")).is_some_and(Value::is_null) {
        return Err(ConfigError {
            message: "local override processes cannot be null".to_string(),
        });
    }
    if let Some(version) = root.get(yaml_key("version")) {
        let version =
            serde_yaml::from_value::<u64>(version.clone()).map_err(|error| ConfigError {
                message: format!("local override version must be an unsigned integer: {error}"),
            })?;
        if version != 1 {
            return Err(ConfigError {
                message: format!("local override cannot change schema version from 1 to {version}"),
            });
        }
    }
    reject_null_processes(root)?;
    normalize_process_entries(document);
    merge_yaml(document, local.clone());
    normalize_process_entries(document);
    Ok(())
}

fn reject_null_processes(root: &Mapping) -> Result<(), ConfigError> {
    let Some(processes) = root.get(yaml_key("processes")).and_then(Value::as_mapping) else {
        return Ok(());
    };
    for (name, process) in processes {
        if process.is_null() {
            let name = name.as_str().unwrap_or("<non-string>");
            return Err(ConfigError {
                message: format!("local override Process '{name}' must define a complete Process"),
            });
        }
    }
    Ok(())
}

fn apply_profile(
    document: &mut Value,
    profile: &Value,
    requested: &str,
) -> Result<(), ConfigError> {
    let profile = profile.as_mapping().ok_or_else(|| ConfigError {
        message: format!("profile '{requested}' must be a mapping"),
    })?;
    let enable = profile_names(profile, "enable", requested)?;
    let disable = profile_names(profile, "disable", requested)?;

    if let Some(settings) = profile.get(yaml_key("settings")) {
        merge_root_field(document, "settings", settings.clone())?;
    }
    if let Some(overrides) = profile.get(yaml_key("overrides")) {
        validate_process_overrides(overrides, &format!("profile '{requested}'"))?;
        apply_process_overrides(document, overrides, requested)?;
    }

    reject_repeated_process_mentions(&enable, &disable, requested)?;
    set_process_enabled(document, &enable, true, requested)?;
    set_process_enabled(document, &disable, false, requested)?;
    Ok(())
}

fn profile_names(
    profile: &Mapping,
    field: &str,
    requested: &str,
) -> Result<Vec<String>, ConfigError> {
    profile
        .get(yaml_key(field))
        .map(|value| {
            serde_yaml::from_value(value.clone()).map_err(|error| ConfigError {
                message: format!(
                    "profile '{requested}' {field} must be a list of Process names: {error}"
                ),
            })
        })
        .transpose()
        .map(|names| names.unwrap_or_default())
}

fn reject_repeated_process_mentions(
    enable: &[String],
    disable: &[String],
    requested: &str,
) -> Result<(), ConfigError> {
    let mut mentioned = std::collections::HashSet::new();
    for name in enable.iter().chain(disable) {
        if !mentioned.insert(name) {
            return Err(ConfigError {
                message: format!("profile '{requested}' mentions Process '{name}' more than once"),
            });
        }
    }
    Ok(())
}

fn apply_process_overrides(
    document: &mut Value,
    overrides: &Value,
    requested: &str,
) -> Result<(), ConfigError> {
    let Some(overrides) = overrides.as_mapping() else {
        if overrides.is_null() {
            return Ok(());
        }
        return Err(ConfigError {
            message: format!(
                "profile '{requested}' overrides must be a name-keyed mapping of Processes"
            ),
        });
    };

    let root = document.as_mapping_mut().ok_or_else(|| ConfigError {
        message: "configuration must be a mapping".to_string(),
    })?;
    if !root.contains_key(yaml_key("processes")) {
        root.insert(yaml_key("processes"), Value::Mapping(Mapping::new()));
    }
    let processes = root
        .get_mut(yaml_key("processes"))
        .and_then(Value::as_mapping_mut)
        .ok_or_else(|| ConfigError {
            message: "processes must be a name-keyed mapping of Processes".to_string(),
        })?;

    for (name, overlay) in overrides {
        let Some(name) = name.as_str() else {
            return Err(ConfigError {
                message: format!("profile '{requested}' overrides must use string Process names"),
            });
        };
        if overlay.is_null() {
            return Err(ConfigError {
                message: format!(
                    "profile '{requested}' Process '{name}' must define a complete Process"
                ),
            });
        }
        let name = yaml_key(name);
        if let Some(base) = processes.get_mut(name.clone()) {
            merge_yaml(base, overlay.clone());
        } else {
            let mut process = Value::Mapping(Mapping::new());
            merge_yaml(&mut process, overlay.clone());
            processes.insert(name, process);
        }
    }
    Ok(())
}

fn set_process_enabled(
    document: &mut Value,
    names: &[String],
    enabled: bool,
    requested: &str,
) -> Result<(), ConfigError> {
    if names.is_empty() {
        return Ok(());
    }
    let processes = document
        .as_mapping_mut()
        .and_then(|root| root.get_mut(yaml_key("processes")))
        .and_then(Value::as_mapping_mut)
        .ok_or_else(|| ConfigError {
            message: format!(
                "profile '{requested}' cannot change Process enablement because processes is not a mapping"
            ),
        })?;

    for name in names {
        let Some(process) = processes.get_mut(yaml_key(name)) else {
            return Err(ConfigError {
                message: format!("profile '{requested}' references unknown Process '{name}'"),
            });
        };
        let Some(process) = process.as_mapping_mut() else {
            return Err(ConfigError {
                message: format!("Process '{name}' must define a mapping"),
            });
        };
        process.insert(yaml_key("enabled"), Value::Bool(enabled));
    }
    Ok(())
}

fn merge_root_field(document: &mut Value, field: &str, overlay: Value) -> Result<(), ConfigError> {
    let root = document.as_mapping_mut().ok_or_else(|| ConfigError {
        message: "configuration must be a mapping".to_string(),
    })?;
    if overlay.is_null() {
        root.remove(yaml_key(field));
        return Ok(());
    }
    if let Some(base) = root.get_mut(yaml_key(field)) {
        merge_yaml(base, overlay);
    } else {
        let mut base = Value::Mapping(Mapping::new());
        merge_yaml(&mut base, overlay);
        root.insert(yaml_key(field), base);
    }
    Ok(())
}

fn merge_yaml(base: &mut Value, overlay: Value) {
    match overlay {
        Value::Mapping(overlay) => match base {
            Value::Mapping(base) => merge_mapping(base, overlay),
            base => {
                let mut merged = Mapping::new();
                merge_mapping(&mut merged, overlay);
                *base = Value::Mapping(merged);
            }
        },
        overlay => *base = overlay,
    }
}

fn merge_mapping(base: &mut Mapping, overlay: Mapping) {
    for (key, value) in overlay {
        if key == yaml_key("environment") && value.is_mapping() {
            merge_environment_mapping(base, key, value);
            continue;
        }
        if value.is_null() {
            base.remove(&key);
            continue;
        }
        if let Some(existing) = base.get_mut(&key) {
            merge_yaml(existing, value);
        } else {
            let mut merged = Value::Mapping(Mapping::new());
            merge_yaml(&mut merged, value);
            base.insert(key, merged);
        }
    }
}

/// Keep environment nulls as tombstones until the effective Process is built.
/// A profile or local layer must be able to remove a value that came from an
/// environment file or the parent process, not only a value in the YAML map.
fn merge_environment_mapping(base: &mut Mapping, key: Value, overlay: Value) {
    let Value::Mapping(overlay) = overlay else {
        unreachable!("the environment overlay was checked as a mapping");
    };
    let Some(existing) = base.get_mut(&key) else {
        base.insert(key, Value::Mapping(overlay));
        return;
    };
    let Value::Mapping(existing) = existing else {
        *existing = Value::Mapping(overlay);
        return;
    };
    for (name, value) in overlay {
        if value.is_null() {
            existing.insert(name, Value::Null);
        } else if let Some(current) = existing.get_mut(&name) {
            merge_yaml(current, value);
        } else {
            existing.insert(name, value);
        }
    }
}

fn yaml_key(value: &str) -> Value {
    Value::String(value.to_owned())
}
