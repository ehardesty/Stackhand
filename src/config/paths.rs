use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Turn a caller-supplied path into an absolute, lexical path.
pub(super) fn absolute_normalized(path: &Path) -> anyhow::Result<PathBuf> {
    let current_dir = std::env::current_dir()?;
    Ok(normalize(if path.is_absolute() {
        path.to_path_buf()
    } else {
        current_dir.join(path)
    }))
}

/// Resolve a configuration path from the base Project directory.
pub(super) fn resolve(base_dir: &Path, configured: &Path) -> PathBuf {
    normalize(if configured.is_absolute() {
        configured.to_path_buf()
    } else {
        base_dir.join(configured)
    })
}

/// Resolve and validate a directory supplied by configuration.
pub(super) fn resolve_directory(
    base_dir: &Path,
    configured: &str,
    context: &str,
) -> Result<PathBuf, String> {
    let path = resolve(base_dir, Path::new(configured));
    if path.is_dir() {
        Ok(path)
    } else {
        Err(format!(
            "{context} '{configured}' resolves to '{}' but does not exist or is not a directory",
            path.display()
        ))
    }
}

/// Resolve and validate a direct program when it contains a path separator.
/// Bare names stay untouched so the process launcher can search `PATH`.
pub(super) fn resolve_program(
    program: &mut OsString,
    base_dir: &Path,
    context: &str,
) -> Result<(), String> {
    let configured = Path::new(program.as_os_str());
    if !has_path_separator(configured) {
        return Ok(());
    }

    let resolved = resolve(base_dir, configured);
    if !resolved.is_file() {
        return Err(format!(
            "{context} program '{}' does not identify a file after resolving to '{}' from the base Project directory",
            configured.display(),
            resolved.display()
        ));
    }
    *program = resolved.into_os_string();
    Ok(())
}

fn has_path_separator(path: &Path) -> bool {
    let text = path.as_os_str().to_string_lossy();
    text.contains(std::path::MAIN_SEPARATOR) || (cfg!(windows) && text.contains('/'))
}

pub(super) fn normalize(path: PathBuf) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}
