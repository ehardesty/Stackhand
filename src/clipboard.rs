use anyhow::{Context, Result};

/// Write text only after a user copy action.
pub fn write_text(text: String) -> Result<()> {
    let mut clipboard = arboard::Clipboard::new().context("system clipboard is unavailable")?;
    clipboard
        .set_text(text)
        .context("could not write selected text to the system clipboard")
}
