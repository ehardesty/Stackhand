use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde_yaml::Value;

use super::ConfigError;
use super::file::ProcessFile;
use super::paths::resolve;

/// Load ordered literal environment files for one configuration owner.
/// Later entries replace earlier entries, including entries in the same file.
pub(super) fn load_files(
    base_dir: &Path,
    files: &[String],
    owner: &str,
) -> Result<BTreeMap<String, String>, ConfigError> {
    let mut values = BTreeMap::new();
    for configured in files {
        let path = resolve(base_dir, Path::new(configured));
        let bytes = fs::read(&path).map_err(|error| ConfigError {
            message: format!(
                "could not read {owner} environment file '{}': {error}",
                path.display()
            ),
        })?;
        let text = String::from_utf8(bytes).map_err(|error| ConfigError {
            message: format!(
                "invalid UTF-8 in {owner} environment file '{}' at byte {}",
                path.display(),
                error.utf8_error().valid_up_to()
            ),
        })?;
        for (line_index, line) in text.lines().enumerate() {
            if let Some((key, value)) = parse_line(line).map_err(|detail| ConfigError {
                message: format!(
                    "invalid {owner} environment file '{}' at line {}: {detail}",
                    path.display(),
                    line_index + 1
                ),
            })? {
                values.insert(key, value);
            }
        }
    }
    Ok(values)
}

/// The resolved changes to one child environment. Values not listed in
/// `removals` remain inherited from the Supervisor's parent process.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct ResolvedEnvironment {
    pub(super) values: Vec<(String, String)>,
    pub(super) removals: Vec<String>,
}

/// Assemble one Process environment from Project files, Process files, and
/// inline values. Each later layer replaces an earlier value. Null inline
/// values are retained as removals so the runtime can remove them from the
/// inherited parent environment.
pub(super) fn build_process_environment(
    process: &ProcessFile,
    name: &str,
    base_dir: &Path,
    project_environment: &BTreeMap<String, String>,
) -> Result<ResolvedEnvironment, ConfigError> {
    let owner = format!("Process '{name}'");
    let process_environment = load_files(
        base_dir,
        process.env_files.as_deref().unwrap_or_default(),
        &owner,
    )?;
    let mut environment = project_environment.clone();
    environment.extend(process_environment);
    let mut removals = BTreeSet::new();
    if let Some(entries) = process.environment.as_ref() {
        for (key, value) in entries {
            match value {
                Some(value) => {
                    environment.insert(key.clone(), value.clone());
                    removals.remove(key);
                }
                None => {
                    environment.remove(key);
                    removals.insert(key.clone());
                }
            }
        }
    }
    Ok(ResolvedEnvironment {
        values: environment.into_iter().collect(),
        removals: removals.into_iter().collect(),
    })
}

/// Validate environment maps before typed deserialization so diagnostics do
/// not echo a malformed environment value.
pub(super) fn validate_shapes(document: &Value, source: &str) -> Result<(), ConfigError> {
    let Some(processes) = document
        .as_mapping()
        .and_then(|root| root.get(Value::String("processes".to_string())))
        .and_then(Value::as_mapping)
    else {
        return Ok(());
    };
    validate_processes(processes, source)
}

/// Validate a name-keyed Process mapping from a profile or local override.
pub(super) fn validate_process_overrides(
    processes: &Value,
    source: &str,
) -> Result<(), ConfigError> {
    let Some(processes) = processes.as_mapping() else {
        return Ok(());
    };
    validate_processes(processes, source)
}

fn validate_processes(processes: &serde_yaml::Mapping, source: &str) -> Result<(), ConfigError> {
    for (name, process) in processes {
        let Some(name) = name.as_str() else {
            continue;
        };
        validate_values(process, name, source)?;
    }
    Ok(())
}

fn validate_values(value: &Value, process_name: &str, source: &str) -> Result<(), ConfigError> {
    match value {
        Value::Mapping(mapping) => {
            if let Some(environment) = mapping.get(Value::String("environment".to_string())) {
                match environment {
                    Value::Mapping(entries) => {
                        for (name, value) in entries {
                            let name = name.as_str().unwrap_or("<non-string>");
                            if !value.is_null() && value.as_str().is_none() {
                                return Err(ConfigError {
                                    message: format!(
                                        "Process '{process_name}': {source} environment variable '{name}' must be a string or null"
                                    ),
                                });
                            }
                        }
                    }
                    Value::Null => {}
                    _ => {
                        return Err(ConfigError {
                            message: format!(
                                "Process '{process_name}': {source} environment must be a mapping"
                            ),
                        });
                    }
                }
            }
            for child in mapping.values() {
                validate_values(child, process_name, source)?;
            }
        }
        Value::Sequence(values) => {
            for child in values {
                validate_values(child, process_name, source)?;
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) | Value::Tagged(_) => {}
    }
    Ok(())
}

fn parse_line(line: &str) -> Result<Option<(String, String)>, String> {
    let line = trim_horizontal(line);
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    let line = strip_export_prefix(line);
    let Some(separator) = line.find('=') else {
        return Err("expected KEY=VALUE".to_string());
    };
    let key = trim_horizontal(&line[..separator]);
    validate_key(key)?;
    let value = parse_value(&line[separator + 1..])?;
    if value.contains('\0') {
        return Err("value must not contain a NUL character".to_string());
    }
    Ok(Some((key.to_string(), value)))
}

fn strip_export_prefix(line: &str) -> &str {
    let Some(rest) = line.strip_prefix("export") else {
        return line;
    };
    if rest.starts_with(' ') || rest.starts_with('\t') {
        trim_horizontal_start(rest)
    } else {
        line
    }
}

fn validate_key(key: &str) -> Result<(), String> {
    let mut characters = key.chars();
    let Some(first) = characters.next() else {
        return Err("environment key must not be empty".to_string());
    };
    if !(first == '_' || first.is_ascii_alphabetic())
        || !characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
    {
        return Err(format!(
            "invalid environment key '{key}'; use ASCII letters, digits, and '_' with a letter or '_' first"
        ));
    }
    Ok(())
}

fn parse_value(raw: &str) -> Result<String, String> {
    let raw = trim_horizontal(raw);
    if raw.is_empty() {
        return Ok(String::new());
    }
    let quote = raw
        .chars()
        .next()
        .expect("a non-empty value has a first character");
    if quote == '\'' || quote == '"' {
        return parse_quoted_value(raw, quote);
    }
    if raw.contains(['\'', '"']) {
        return Err("quotes must surround the complete value".to_string());
    }
    Ok(raw.to_string())
}

fn parse_quoted_value(raw: &str, quote: char) -> Result<String, String> {
    let mut value = String::new();
    let mut index = quote.len_utf8();
    while index < raw.len() {
        let character = raw[index..]
            .chars()
            .next()
            .expect("a UTF-8 slice has a first character");
        if character == quote {
            let trailing = trim_horizontal(&raw[index + character.len_utf8()..]);
            if !trailing.is_empty() {
                return Err("quoted value must end at the closing quote".to_string());
            }
            return Ok(value);
        }
        if quote == '"' && character == '\\' {
            let escape_start = index;
            index += character.len_utf8();
            let Some(escaped) = raw[index..].chars().next() else {
                return Err("double-quoted value ends with an escape".to_string());
            };
            let replacement = match escaped {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                '\\' => '\\',
                '"' => '"',
                _ => {
                    return Err(format!(
                        "unsupported escape '\\{}' in double-quoted value",
                        &raw[escape_start..index + escaped.len_utf8()]
                    ));
                }
            };
            value.push(replacement);
            index += escaped.len_utf8();
        } else {
            value.push(character);
            index += character.len_utf8();
        }
    }
    Err("unterminated quoted value".to_string())
}

fn trim_horizontal(value: &str) -> &str {
    trim_horizontal_start(value).trim_end_matches([' ', '\t'])
}

fn trim_horizontal_start(value: &str) -> &str {
    value.trim_start_matches([' ', '\t'])
}

#[cfg(test)]
#[path = "env_tests.rs"]
mod tests;
