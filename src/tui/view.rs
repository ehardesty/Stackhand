use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::terminal::OwnedTerminalSnapshot;

const FOOTER_HEIGHT: u16 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsoleViewMode {
    ChildInput,
    AppCommand,
    Scroll,
    Selection,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsoleWarning {
    PasteRejected,
    InputRejected,
    InputBackpressure,
    OutputTruncated,
    PasteDeliveryFailed,
    ClipboardFailed,
    NothingSelected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConsoleViewState {
    pub mode: ConsoleViewMode,
    pub following: bool,
    pub warning: Option<ConsoleWarning>,
    pub stackhand_mouse_gesture: bool,
}

impl Default for ConsoleViewState {
    fn default() -> Self {
        Self {
            mode: ConsoleViewMode::ChildInput,
            following: true,
            warning: None,
            stackhand_mouse_gesture: false,
        }
    }
}

pub fn console_area(area: Rect) -> Rect {
    let content_height = area.height.saturating_sub(FOOTER_HEIGHT);
    Block::bordered().inner(Rect::new(area.x, area.y, area.width, content_height))
}

pub fn render(frame: &mut Frame<'_>, snapshot: &OwnedTerminalSnapshot, view: ConsoleViewState) {
    let area = frame.area();
    let pane = Rect::new(
        area.x,
        area.y,
        area.width,
        area.height.saturating_sub(FOOTER_HEIGHT),
    );
    frame.render_widget(Block::new().borders(Borders::ALL).title(" Shell "), pane);

    let console = console_area(area);
    let source = snapshot.buffer.area();
    let width = source.width.min(console.width);
    let height = source.height.min(console.height);
    for y in 0..height {
        for x in 0..width {
            frame.buffer_mut()[(console.x + x, console.y + y)] =
                snapshot.buffer[(source.x + x, source.y + y)].clone();
        }
    }

    frame.render_widget(
        Paragraph::new(footer_text(view, snapshot.mouse_tracking))
            .style(Style::default().fg(Color::DarkGray)),
        Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
    );

    if let Some(cursor) = snapshot.cursor {
        let x = console.x.saturating_add(cursor.position.x);
        let y = console.y.saturating_add(cursor.position.y);
        if x < console.right() && y < console.bottom() {
            frame.set_cursor_position((x, y));
        }
    }
}

fn footer_text(view: ConsoleViewState, child_mouse_tracking: bool) -> String {
    if let Some(warning) = view.warning {
        let warning = match warning {
            ConsoleWarning::PasteRejected => {
                "WARNING: paste rejected; no partial bytes sent · Ctrl-A: commands"
            }
            ConsoleWarning::InputRejected => {
                "WARNING: terminal input was rejected; Run is stopping or its queue is full · Ctrl-A: commands"
            }
            ConsoleWarning::InputBackpressure => {
                "WARNING: child input queue is saturated; delivery is bounded · Ctrl-A: commands"
            }
            ConsoleWarning::OutputTruncated => {
                "WARNING: oldest Process output was removed at the history bound · Ctrl-A: commands"
            }
            ConsoleWarning::PasteDeliveryFailed => {
                "WARNING: an admitted paste did not reach the child · Ctrl-A: commands"
            }
            ConsoleWarning::ClipboardFailed => {
                "WARNING: clipboard write failed; terminal session continues · Esc: selection"
            }
            ConsoleWarning::NothingSelected => {
                "WARNING: no terminal text is selected · Esc: selection"
            }
        };
        return format!(
            "{} · {warning}",
            mouse_owner_text(view, child_mouse_tracking)
        );
    }

    let controls = match (view.mode, view.following) {
        (ConsoleViewMode::ChildInput, true) => "Ctrl-A: commands · Ctrl-Q: quit · LIVE",
        (ConsoleViewMode::ChildInput, false) => "Ctrl-A: commands · history view · NOT FOLLOWING",
        (ConsoleViewMode::AppCommand, _) => {
            "PageUp: history · s: selection · f: live tail · Esc: child input"
        }
        (ConsoleViewMode::Scroll, _) => {
            "PageUp/Down: move · f: live tail · history target 64 KiB; page rounding and truncation signal unavailable"
        }
        (ConsoleViewMode::Selection, _) => {
            "Drag: cells · double: word · triple: line · a: all · y: copy · Esc: commands"
        }
    };
    format!(
        "{} · {controls}",
        mouse_owner_text(view, child_mouse_tracking)
    )
}

fn mouse_owner_text(view: ConsoleViewState, child_mouse_tracking: bool) -> &'static str {
    if view.stackhand_mouse_gesture {
        "MOUSE: STACKHAND · active gesture"
    } else if view.mode == ConsoleViewMode::ChildInput && child_mouse_tracking {
        "MOUSE: CHILD · Shift+mouse: Stackhand"
    } else {
        "MOUSE: STACKHAND"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiny_screen_has_safe_console_area() {
        let area = console_area(Rect::new(0, 0, 0, 0));

        assert_eq!(area.width, 0);
        assert_eq!(area.height, 0);
    }

    #[test]
    fn scroll_footer_reports_the_bound_and_missing_truncation_signal() {
        let text = footer_text(
            ConsoleViewState {
                mode: ConsoleViewMode::Scroll,
                following: false,
                warning: None,
                stackhand_mouse_gesture: false,
            },
            false,
        );

        assert!(text.contains("target 64 KiB"));
        assert!(text.contains("page rounding"));
        assert!(text.contains("truncation signal unavailable"));
    }

    #[test]
    fn footer_makes_paste_rejection_visible() {
        let text = footer_text(
            ConsoleViewState {
                mode: ConsoleViewMode::ChildInput,
                following: true,
                warning: Some(ConsoleWarning::PasteRejected),
                stackhand_mouse_gesture: false,
            },
            false,
        );

        assert!(text.contains("WARNING: paste rejected"));
        assert!(text.contains("no partial bytes sent"));
    }

    #[test]
    fn footer_makes_admitted_paste_delivery_failure_visible() {
        let text = footer_text(
            ConsoleViewState {
                mode: ConsoleViewMode::ChildInput,
                following: true,
                warning: Some(ConsoleWarning::PasteDeliveryFailed),
                stackhand_mouse_gesture: false,
            },
            false,
        );
        assert!(text.contains("admitted paste did not reach the child"));
    }

    #[test]
    fn footer_makes_clipboard_failure_visible_and_keeps_selection_controls() {
        let text = footer_text(
            ConsoleViewState {
                mode: ConsoleViewMode::Selection,
                following: false,
                warning: Some(ConsoleWarning::ClipboardFailed),
                stackhand_mouse_gesture: false,
            },
            true,
        );

        assert!(text.contains("clipboard write failed"));
        assert!(text.contains("session continues"));
        assert!(text.contains("MOUSE: STACKHAND"));
    }

    #[test]
    fn footer_shows_child_mouse_ownership_and_the_override() {
        let text = footer_text(ConsoleViewState::default(), true);

        assert!(text.contains("MOUSE: CHILD"));
        assert!(text.contains("Shift+mouse: Stackhand"));
    }

    #[test]
    fn footer_shows_a_captured_stackhand_gesture() {
        let text = footer_text(
            ConsoleViewState {
                stackhand_mouse_gesture: true,
                ..ConsoleViewState::default()
            },
            true,
        );

        assert!(text.starts_with("MOUSE: STACKHAND · active gesture"));
    }
}
