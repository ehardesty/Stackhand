use serde_yaml::{Mapping, Value};

use super::ConfigError;

/// Apply one same-directory local override. A local file may repeat the schema
/// version, but it cannot replace a complete Process with null.
pub(super) fn apply_local_override(document: &mut Value, local: &Value) -> Result<(), ConfigError> {
    let Some(root) = local.as_mapping() else {
        return Err(ConfigError {
            message: "local override must be a mapping".to_string(),
        });
    };
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

pub(super) fn merge_yaml(base: &mut Value, overlay: Value) {
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
/// A Process Profile or local override must be able to remove a value that came
/// from an environment file or the parent process.
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
