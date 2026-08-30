use ratatui::style::{Color, Modifier, Style};

/// Semantic colors for Stackhand-owned UI. ANSI named colors let each
/// terminal palette choose the exact RGB values while these roles keep
/// sufficient separation from the terminal background.
pub(super) struct Theme {
    focus: Color,
    copy: Color,
    secondary: Color,
    info: Color,
    success: Color,
    warning: Color,
    error: Color,
}

/// Semantic color roles for Process lifecycle labels. The text label remains
/// the primary signal; color makes a dense list faster to scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LifecycleTone {
    Muted,
    Info,
    Success,
    Warning,
    Error,
}

pub(super) const TERMINAL_THEME: Theme = Theme {
    focus: Color::Cyan,
    copy: Color::Yellow,
    secondary: Color::Gray,
    info: Color::Cyan,
    success: Color::Green,
    warning: Color::Yellow,
    error: Color::Red,
};

impl Theme {
    pub(super) fn focus_border(&self) -> Style {
        Style::default().fg(self.focus)
    }

    pub(super) fn copy_border(&self) -> Style {
        Style::default().fg(self.copy)
    }

    pub(super) fn inactive_border(&self) -> Style {
        Style::default().fg(self.secondary)
    }

    pub(super) fn secondary_text(&self) -> Style {
        Style::default().fg(self.secondary)
    }

    pub(super) fn search_match(&self) -> Style {
        Style::default()
            .fg(self.copy)
            .add_modifier(Modifier::BOLD | Modifier::REVERSED)
    }

    pub(super) fn selection(&self) -> Style {
        Style::default().add_modifier(Modifier::REVERSED)
    }

    pub(super) fn lifecycle(&self, tone: LifecycleTone) -> Style {
        let color = match tone {
            LifecycleTone::Muted => self.secondary,
            LifecycleTone::Info => self.info,
            LifecycleTone::Success => self.success,
            LifecycleTone::Warning => self.warning,
            LifecycleTone::Error => self.error,
        };
        Style::default().fg(color)
    }

    pub(super) fn footer(&self, warning: bool) -> Style {
        Style::default().fg(if warning {
            self.warning
        } else {
            self.secondary
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_theme_keeps_secondary_content_out_of_bright_black() {
        assert_eq!(TERMINAL_THEME.secondary_text().fg, Some(Color::Gray));
        assert_ne!(TERMINAL_THEME.secondary_text().fg, Some(Color::DarkGray));
    }

    #[test]
    fn focus_copy_and_warning_roles_stay_distinct() {
        assert_eq!(TERMINAL_THEME.focus_border().fg, Some(Color::Cyan));
        assert_eq!(TERMINAL_THEME.copy_border().fg, Some(Color::Yellow));
        assert_eq!(TERMINAL_THEME.footer(true).fg, Some(Color::Yellow));
    }

    #[test]
    fn lifecycle_roles_use_terminal_palette_colors() {
        assert_eq!(
            TERMINAL_THEME.lifecycle(LifecycleTone::Muted).fg,
            Some(Color::Gray)
        );
        assert_eq!(
            TERMINAL_THEME.lifecycle(LifecycleTone::Info).fg,
            Some(Color::Cyan)
        );
        assert_eq!(
            TERMINAL_THEME.lifecycle(LifecycleTone::Success).fg,
            Some(Color::Green)
        );
        assert_eq!(
            TERMINAL_THEME.lifecycle(LifecycleTone::Warning).fg,
            Some(Color::Yellow)
        );
        assert_eq!(
            TERMINAL_THEME.lifecycle(LifecycleTone::Error).fg,
            Some(Color::Red)
        );
    }
}
