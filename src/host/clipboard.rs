use crossterm::Command;
use crossterm::clipboard::CopyToClipboard;

use super::output::{HostOutput, OutputSink};

pub(crate) const COPY_LIMIT_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClipboardBackend {
    Auto,
    Native,
    Osc52,
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CopyOutcome {
    Submitted,
    Empty,
    Failed,
    TooLarge,
    Disabled,
}

pub(crate) struct ClipboardService {
    backend: ClipboardBackend,
    native: Option<arboard::Clipboard>,
}

impl ClipboardBackend {
    fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "auto" => Ok(Self::Auto),
            "native" => Ok(Self::Native),
            "osc52" => Ok(Self::Osc52),
            "off" => Ok(Self::Off),
            _ => anyhow::bail!("STACKHAND_CLIPBOARD must be auto, native, osc52, or off"),
        }
    }

    fn from_environment(
        mut value: impl FnMut(&str) -> Option<std::ffi::OsString>,
    ) -> anyhow::Result<Self> {
        let backend = match value("STACKHAND_CLIPBOARD") {
            Some(value) => Self::parse(
                value
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("STACKHAND_CLIPBOARD must be valid UTF-8"))?,
            )?,
            None => Self::Auto,
        };
        let ssh = ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
            .iter()
            .any(|key| value(key).is_some_and(|value| !value.is_empty()));
        let desktop = cfg!(target_os = "macos")
            || cfg!(target_os = "windows")
            || ["DISPLAY", "WAYLAND_DISPLAY"]
                .iter()
                .any(|key| value(key).is_some_and(|value| !value.is_empty()));
        Ok(match backend {
            Self::Auto if ssh || !desktop => Self::Osc52,
            Self::Auto => Self::Native,
            backend => backend,
        })
    }
}

impl ClipboardService {
    pub(crate) fn from_environment() -> anyhow::Result<Self> {
        Ok(Self::new(ClipboardBackend::from_environment(|key| {
            std::env::var_os(key)
        })?))
    }

    fn new(backend: ClipboardBackend) -> Self {
        Self {
            backend,
            native: None,
        }
    }

    pub(crate) fn copy<W: OutputSink>(
        &mut self,
        text: String,
        output: &mut HostOutput<W>,
    ) -> CopyOutcome {
        if text.is_empty() {
            return CopyOutcome::Empty;
        }
        if text.len() > COPY_LIMIT_BYTES {
            return CopyOutcome::TooLarge;
        }
        match self.backend {
            ClipboardBackend::Osc52 => {
                let mut command = String::new();
                CopyToClipboard::to_clipboard_from(text)
                    .write_ansi(&mut command)
                    .expect("writing a clipboard command to a String cannot fail");
                match output.send(command.into_bytes()) {
                    Ok(()) => CopyOutcome::Submitted,
                    Err(_) => CopyOutcome::Failed,
                }
            }
            ClipboardBackend::Native => {
                if self.native.is_none() {
                    self.native = arboard::Clipboard::new().ok();
                }
                match self
                    .native
                    .as_mut()
                    .map(|clipboard| clipboard.set_text(text))
                {
                    Some(Ok(())) => CopyOutcome::Submitted,
                    _ => CopyOutcome::Failed,
                }
            }
            ClipboardBackend::Off => CopyOutcome::Disabled,
            ClipboardBackend::Auto => unreachable!("backend resolved at session startup"),
        }
    }
}

#[cfg(test)]
mod tests;
