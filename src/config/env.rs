use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use super::ConfigError;
use super::file::ProcessFile;

/// Load ordered literal environment files for one configuration owner.
/// Later entries replace earlier entries, including entries in the same file.
pub(super) fn load_files(
    base_dir: &Path,
    files: &[String],
    owner: &str,
) -> Result<BTreeMap<String, String>, ConfigError> {
    let mut values = BTreeMap::new();
    for configured in files {
        let path = resolve_path(base_dir, configured);
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

/// Assemble one Process environment from Project files, Process files, and
/// inline values. Each later layer replaces an earlier value.
pub(super) fn build_process_environment(
    process: &ProcessFile,
    name: &str,
    base_dir: &Path,
    project_environment: &BTreeMap<String, String>,
) -> Result<Vec<(String, String)>, ConfigError> {
    let owner = format!("Process '{name}'");
    let process_environment = load_files(
        base_dir,
        process.env_files.as_deref().unwrap_or_default(),
        &owner,
    )?;
    let mut environment = project_environment.clone();
    environment.extend(process_environment);
    if let Some(entries) = process.environment.as_ref() {
        for (key, value) in entries {
            match value {
                Some(value) => {
                    environment.insert(key.clone(), value.clone());
                }
                None => {
                    environment.remove(key);
                }
            }
        }
    }
    Ok(environment.into_iter().collect())
}

/// Resolve one environment-file path against the base Project directory.
fn resolve_path(base_dir: &Path, configured: &str) -> PathBuf {
    let path = Path::new(configured);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    }
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
