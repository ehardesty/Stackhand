use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
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

/// One projected Process list row. Labels are projections of structured
/// snapshot state, never stored authoritative strings.
pub struct ProcessRowView {
    pub name: String,
    pub status: String,
    pub selected: bool,
}

/// The one owner of the Project layout rule: a Process list above the
/// selected console pane, one footer line below. Rendering and interaction
/// geometry both derive from this.
pub fn project_layout(area: Rect, process_rows: usize) -> (Rect, Rect, Rect) {
    let list_height = (process_rows as u16 + 2)
        .max(3)
        .min(area.height / 3)
        .min(area.height);
    let list = Rect::new(area.x, area.y, area.width, list_height);
    let console_height = area
        .height
        .saturating_sub(list.height)
        .saturating_sub(FOOTER_HEIGHT)
        .max(1);
    let console_outer = Rect::new(area.x, area.y + list.height, area.width, console_height);
    let footer = Rect::new(
        area.x,
        area.bottom().saturating_sub(1),
        area.width,
        FOOTER_HEIGHT,
    );
    (list, console_outer, footer)
}

/// The inner area of one bordered pane.
pub fn pane_inner(pane: Rect) -> Rect {
    ratatui::widgets::Block::bordered().inner(pane)
}

/// The PTY geometry matching the console pane for a Project with this many
/// Process rows, using the current terminal size.
pub fn project_console_geometry(process_rows: usize) -> crate::geometry::TerminalGeometry {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let (_, pane, _) = project_layout(Rect::new(0, 0, cols, rows), process_rows);
    crate::geometry::TerminalGeometry::from_pane(pane_inner(pane))
}

pub fn render_project(
    frame: &mut Frame<'_>,
    rows: &[ProcessRowView],
    console_snapshot: Option<&OwnedTerminalSnapshot>,
    view: ConsoleViewState,
) -> Rect {
    let area = frame.area();
    let (list, console_pane, footer) = project_layout(area, rows.len());
    let console_inner = pane_inner(console_pane);

    let list_rows: Vec<ratatui::text::Line<'_>> = rows
        .iter()
        .map(|row| {
            let style = if row.selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            ratatui::text::Line::styled(format!(" {} · {} ", row.name, row.status), style)
        })
        .collect();
    frame.render_widget(
        ratatui::widgets::List::new(list_rows)
            .block(Block::new().borders(Borders::ALL).title(" Processes ")),
        list,
    );
    frame.render_widget(Block::bordered().title(" Console "), console_pane);

    let mouse_tracking = console_snapshot.is_some_and(|snap| snap.mouse_tracking);
    blit_console(
        frame,
        console_snapshot.unwrap_or(&EMPTY_CONSOLE),
        console_inner,
    );
    frame.render_widget(
        Paragraph::new(footer_text(view, mouse_tracking))
            .style(Style::default().fg(Color::DarkGray)),
        footer,
    );
    if let Some(cursor) = console_snapshot.and_then(|snap| snap.cursor) {
        let x = console_inner.x.saturating_add(cursor.position.x);
        let y = console_inner.y.saturating_add(cursor.position.y);
        if x < console_inner.right() && y < console_inner.bottom() {
            frame.set_cursor_position((x, y));
        }
    }
    console_inner
}

static EMPTY_CONSOLE: std::sync::LazyLock<OwnedTerminalSnapshot> =
    std::sync::LazyLock::new(|| OwnedTerminalSnapshot {
        buffer: ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 1, 1)),
        cursor: None,
        mouse_tracking: false,
    });

fn blit_console(frame: &mut Frame<'_>, snapshot: &OwnedTerminalSnapshot, console: Rect) {
    let source = snapshot.buffer.area();
    let width = source.width.min(console.width);
    let height = source.height.min(console.height);
    for y in 0..height {
        for x in 0..width {
            frame.buffer_mut()[(console.x + x, console.y + y)] =
                snapshot.buffer[(source.x + x, source.y + y)].clone();
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
            "PageUp: history · s: selection · j/k or ↑↓: select Process · f: live tail · Esc: child input"
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

    fn row(name: &str, status: &str, selected: bool) -> ProcessRowView {
        ProcessRowView {
            name: name.to_string(),
            status: status.to_string(),
            selected,
        }
    }

    /// Render one frame into a test buffer for assertions. Small but tall
    /// enough that three Process rows stay inside the capped list band.
    fn rendered(rows: &[ProcessRowView]) -> ratatui::buffer::Buffer {
        let backend = ratatui::backend::TestBackend::new(60, 18);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_project(frame, rows, None, ConsoleViewState::default());
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
        buffer.content().iter().map(|cell| cell.symbol()).collect()
    }

    #[test]
    fn every_process_row_shows_its_name_and_status_label() {
        let rows = [
            row("web", "Ready", true),
            row("worker", "Stopped", false),
            row("cron", "Disabled", false),
        ];
        let text = buffer_text(&rendered(&rows));

        assert!(text.contains(" Processes "), "{text:?}");
        for row in &rows {
            assert!(text.contains(&row.name), "{} missing: {text:?}", row.name);
            assert!(
                text.contains(&row.status),
                "{} missing: {text:?}",
                row.status
            );
        }
    }

    #[test]
    fn exactly_the_selected_row_is_reversed() {
        let rows = [row("web", "Ready", false), row("db", "Starting", true)];
        let buffer = rendered(&rows);

        // The list body starts inside its border on row 1.
        let web_reversed = buffer[(1, 1)].modifier.contains(Modifier::REVERSED);
        let db_reversed = buffer[(1, 2)].modifier.contains(Modifier::REVERSED);
        assert!(!web_reversed);
        assert!(db_reversed);
    }

    #[test]
    fn a_single_row_keeps_one_valid_selection() {
        let rows = [row("only", "Stopped", true)];
        let buffer = rendered(&rows);

        assert!(buffer[(1, 1)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn the_console_pane_stays_visible_on_small_screens() {
        let (_, console, footer) = project_layout(Rect::new(0, 0, 20, 6), 8);

        // The list is capped at a third of the height; what remains stays
        // visible above the footer.
        assert!(console.height >= 1);
        assert_eq!(console.height + footer.height + 2, 6);
        assert_eq!(footer.height, FOOTER_HEIGHT);
    }

    #[test]
    fn resize_geometry_uses_the_console_pane_not_the_full_screen() {
        let area = Rect::new(0, 0, 100, 30);
        let (_, pane, _) = project_layout(area, 4);
        let geometry = crate::geometry::TerminalGeometry::from_pane(pane_inner(pane));

        // The list band is excluded from what the child believes its size
        // to be.
        assert!(geometry.rows() < 30);
        assert_eq!(geometry.cols(), 98);
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
