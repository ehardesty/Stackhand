//! Validated byte budgets for terminal and Logs history.

use serde::Deserialize;

pub const MIB: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputSettings {
    pub terminal_scrollback_bytes: usize,
    pub terminal_project_bytes: usize,
    pub per_process_history_bytes: usize,
    pub global_history_bytes: usize,
}

impl Default for OutputSettings {
    fn default() -> Self {
        Self {
            terminal_scrollback_bytes: 16 * MIB,
            terminal_project_bytes: 128 * MIB,
            per_process_history_bytes: 32 * MIB,
            global_history_bytes: 256 * MIB,
        }
    }
}

impl OutputSettings {
    /// Fixed shares reserve capacity for every Process that can use a PTY.
    /// Starting or stopping another Run never reduces existing history.
    pub(crate) fn terminal_limit_for_slots(&self, slots: usize) -> Result<usize, String> {
        if slots == 0 {
            return Ok(self.terminal_scrollback_bytes);
        }
        let headroom = slots
            .checked_mul(MIB)
            .ok_or("terminal Project budget overflow")?;
        let share = self.terminal_project_bytes.saturating_sub(headroom) / slots;
        if share < MIB {
            return Err(format!(
                "settings.output.terminal_project_bytes must allow at least 2 MiB for each of the {slots} Processes that can use a PTY"
            ));
        }
        Ok(self.terminal_scrollback_bytes.min(share))
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct OutputSettingsFile {
    terminal_scrollback_bytes: Option<ByteSize>,
    terminal_project_bytes: Option<ByteSize>,
    per_process_history_bytes: Option<ByteSize>,
    global_history_bytes: Option<ByteSize>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ByteSize {
    Bytes(u64),
    Text(String),
}

impl ByteSize {
    fn resolve(&self) -> Option<usize> {
        let bytes = match self {
            Self::Bytes(value) => *value,
            Self::Text(text) => {
                let text = text.trim();
                let split = text
                    .find(|c: char| !c.is_ascii_digit())
                    .unwrap_or(text.len());
                let value: u64 = text[..split].parse().ok()?;
                let unit = match text[split..].trim() {
                    "" | "B" => 1,
                    "KiB" => 1024,
                    "MiB" => 1024 * 1024,
                    "GiB" => 1024 * 1024 * 1024,
                    _ => return None,
                };
                value.checked_mul(unit)?
            }
        };
        usize::try_from(bytes).ok()
    }
}

impl OutputSettingsFile {
    pub(crate) fn resolve(&self) -> Result<OutputSettings, String> {
        let mut settings = OutputSettings::default();
        for (name, value, target) in [
            (
                "terminal_scrollback_bytes",
                &self.terminal_scrollback_bytes,
                &mut settings.terminal_scrollback_bytes,
            ),
            (
                "terminal_project_bytes",
                &self.terminal_project_bytes,
                &mut settings.terminal_project_bytes,
            ),
            (
                "per_process_history_bytes",
                &self.per_process_history_bytes,
                &mut settings.per_process_history_bytes,
            ),
            (
                "global_history_bytes",
                &self.global_history_bytes,
                &mut settings.global_history_bytes,
            ),
        ] {
            if let Some(value) = value {
                *target = value.resolve().filter(|bytes| *bytes >= MIB).ok_or_else(|| {
                    format!("settings.output.{name} must be at least 1 MiB, in bytes or with a KiB, MiB, or GiB suffix")
                })?;
            }
        }
        if settings.terminal_scrollback_bytes > settings.terminal_project_bytes {
            return Err(
                "settings.output.terminal_scrollback_bytes exceeds terminal_project_bytes".into(),
            );
        }
        if settings.per_process_history_bytes > settings.global_history_bytes {
            return Err(
                "settings.output.per_process_history_bytes exceeds global_history_bytes".into(),
            );
        }
        Ok(settings)
    }
}
