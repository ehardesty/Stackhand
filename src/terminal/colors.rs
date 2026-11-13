use libghostty_vt::color::{Palette, PaletteIndex, Rgb};
use libghostty_vt::terminal::Terminal;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HostColorProfile {
    pub(crate) foreground: Option<Rgb>,
    pub(crate) background: Option<Rgb>,
    pub(crate) indexed: [Option<Rgb>; 256],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColorSupport {
    Supported,
    Partial,
    Fallback,
}

impl Default for HostColorProfile {
    fn default() -> Self {
        Self {
            foreground: None,
            background: None,
            indexed: [None; 256],
        }
    }
}

impl HostColorProfile {
    pub(crate) fn support(&self) -> ColorSupport {
        let count = self.indexed.iter().filter(|color| color.is_some()).count()
            + usize::from(self.foreground.is_some())
            + usize::from(self.background.is_some());
        match count {
            258 => ColorSupport::Supported,
            0 => ColorSupport::Fallback,
            _ => ColorSupport::Partial,
        }
    }

    pub(super) fn apply(
        &self,
        terminal: &mut Terminal<'_, '_>,
    ) -> libghostty_vt::error::Result<()> {
        if let Some(color) = self.foreground {
            terminal.set_default_fg_color(Some(color))?;
        }
        if let Some(color) = self.background {
            terminal.set_default_bg_color(Some(color))?;
        }
        let mut palette = Palette::default();
        for (index, color) in self.indexed.iter().enumerate() {
            if let Some(color) = color {
                palette.set(PaletteIndex(index as u8), *color);
            }
        }
        terminal.set_default_color_palette(Some(palette))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
