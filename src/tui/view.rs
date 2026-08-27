use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::terminal::OwnedTerminalSnapshot;

use super::theme::TERMINAL_THEME;

const FOOTER_HEIGHT: u16 = 1;

/// The active keyboard scope. Stackhand starts in the Process list so
/// navigation and lifecycle keys work immediately. Only Console sends
/// unbound keys to an input-enabled PTY. Copy owns text-selection keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsoleViewMode {
    ProcessList,
    Console,
    Copy,
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
    InputDisabled,
    PipeReadOnly,
    SelectionUnavailable,
}

/// Which console pane the selected Process currently renders. The footer
/// and the key routing distinguish the pane kinds: only a terminal pane
/// can receive child input, mouse tracking, and text selection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConsolePaneKind {
    #[default]
    Terminal,
    Pipe,
    Empty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConsoleViewState {
    pub mode: ConsoleViewMode,
    pub following: bool,
    pub warning: Option<ConsoleWarning>,
    pub stackhand_mouse_gesture: bool,
    pub pane: ConsolePaneKind,
}

impl Default for ConsoleViewState {
    fn default() -> Self {
        Self {
            mode: ConsoleViewMode::ProcessList,
            following: true,
            warning: None,
            stackhand_mouse_gesture: false,
            pane: ConsolePaneKind::default(),
        }
    }
}

/// One projected Process list row. Labels are projections of structured
/// snapshot state, never stored authoritative strings.
pub struct ProcessRowView {
    pub name: String,
    pub status: String,
    /// Compact aggregate CPU percentage, when the current Run has one.
    pub cpu: Option<String>,
    /// Compact aggregate resident memory, when the current Run has one.
    pub memory: Option<String>,
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

/// One line of retained pipe output for rendering. The marker flag keeps
/// Run-marker identity that flattening to plain strings would lose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PipeLine {
    pub text: String,
    pub marker: bool,
}

/// The selected pipe-mode Process's retained output, tail-following: the
/// newest lines fill the pane and older lines stay in the module, not in
/// render state. Run markers render dimmed so attempts stay distinguishable.
fn render_pipe_console(frame: &mut Frame<'_>, lines: &[PipeLine], pane: Rect) {
    if pane.height == 0 {
        return;
    }
    let start = lines.len().saturating_sub(pane.height as usize);
    let tail = &lines[start..];
    let rows: Vec<ratatui::text::Line<'_>> = tail
        .iter()
        .map(|line| {
            if line.marker {
                ratatui::text::Line::styled(line.text.as_str(), TERMINAL_THEME.secondary_text())
            } else {
                ratatui::text::Line::from(line.text.as_str())
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(rows), pane);
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
    pipe_lines: Option<&[PipeLine]>,
    view: ConsoleViewState,
    selected_header: &str,
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
            ratatui::text::Line::styled(row_line(row, list.width.saturating_sub(2) as usize), style)
        })
        .collect();
    let list_border = if view.mode == ConsoleViewMode::ProcessList {
        TERMINAL_THEME.focus_border()
    } else {
        TERMINAL_THEME.inactive_border()
    };
    frame.render_widget(
        ratatui::widgets::List::new(list_rows).block(
            Block::new()
                .borders(Borders::ALL)
                .border_style(list_border)
                .title(" Processes "),
        ),
        list,
    );
    // The header names the selected Process and its live Run identity; it
    // is a projection of the immutable Supervisor snapshot, never stored
    // UI state.
    let header = if selected_header.is_empty() {
        " Console ".to_string()
    } else {
        format!(" Console · {selected_header} ")
    };
    let console_border = match view.mode {
        ConsoleViewMode::ProcessList => TERMINAL_THEME.inactive_border(),
        ConsoleViewMode::Console => TERMINAL_THEME.focus_border(),
        ConsoleViewMode::Copy => TERMINAL_THEME.copy_border(),
    };
    frame.render_widget(
        Block::bordered().border_style(console_border).title(header),
        console_pane,
    );

    let mouse_tracking = console_snapshot.is_some_and(|snap| snap.mouse_tracking);
    if let Some(lines) = pipe_lines {
        render_pipe_console(frame, lines, console_inner);
    } else {
        blit_console(
            frame,
            console_snapshot.unwrap_or(&EMPTY_CONSOLE),
            console_inner,
        );
    }
    frame.render_widget(
        Paragraph::new(footer_text(view, mouse_tracking))
            .style(TERMINAL_THEME.footer(view.warning.is_some())),
        footer,
    );
    // A pipe console has no terminal cursor. Console focus shows the child
    // cursor, while Copy mode shows the terminal-owned keyboard copy cursor.
    if matches!(view.mode, ConsoleViewMode::Console | ConsoleViewMode::Copy)
        && pipe_lines.is_none()
        && let Some(cursor) = console_snapshot.and_then(|snap| snap.cursor)
    {
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

/// One Process row as text. The compact metric cells stay optional: when
/// the terminal is too narrow for them, the row degrades to name and
/// status instead of pushing the essential fields out.
fn row_line(row: &ProcessRowView, usable_width: usize) -> String {
    let base = format!(" {} · {} ", row.name, row.status);
    let Some(cpu) = &row.cpu else {
        return base;
    };
    let Some(memory) = &row.memory else {
        return base;
    };
    let full = format!("{base} · {cpu} {memory}");
    if full.chars().count() <= usable_width {
        full
    } else {
        base
    }
}

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
    let focus = match view.mode {
        ConsoleViewMode::ProcessList => "FOCUS: PROCESSES",
        ConsoleViewMode::Console => "FOCUS: CONSOLE",
        ConsoleViewMode::Copy => "MODE: COPY",
    };
    if let Some(warning) = view.warning {
        let warning = match warning {
            ConsoleWarning::PasteRejected => {
                "WARNING: paste rejected; focus an input-enabled console first"
            }
            ConsoleWarning::InputRejected => {
                "WARNING: terminal input was rejected; Run is stopping or its queue is full"
            }
            ConsoleWarning::InputBackpressure => {
                "WARNING: child input queue is saturated; delivery is bounded"
            }
            ConsoleWarning::OutputTruncated => {
                "WARNING: oldest Process output was removed at the history bound"
            }
            ConsoleWarning::PasteDeliveryFailed => {
                "WARNING: an admitted paste did not reach the child"
            }
            ConsoleWarning::ClipboardFailed => {
                "WARNING: clipboard write failed; selection remains available"
            }
            ConsoleWarning::NothingSelected => {
                "WARNING: no terminal text is selected · v: start selection"
            }
            ConsoleWarning::InputDisabled => {
                "WARNING: input is not enabled for this Process · Ctrl-A: Process list"
            }
            ConsoleWarning::PipeReadOnly => {
                "WARNING: pipe output is read-only · Ctrl-A: Process list"
            }
            ConsoleWarning::SelectionUnavailable => {
                "WARNING: text selection is available only in a PTY console"
            }
        };
        return format!(
            "{} · {focus} · {warning}",
            mouse_owner_text(view, child_mouse_tracking)
        );
    }

    let controls = match view.mode {
        ConsoleViewMode::ProcessList => {
            "j/k or ↑↓: select · s: start · x: stop · r: restart · v: copy · Ctrl-A: console · q: quit"
        }
        ConsoleViewMode::Console => match view.pane {
            ConsolePaneKind::Terminal => "keys: child · Ctrl-A, then v: copy · Ctrl-Q: quit",
            ConsolePaneKind::Pipe => "read-only output · Ctrl-A: Process list · Ctrl-Q: quit",
            ConsolePaneKind::Empty => "no active Run · Ctrl-A: Process list · Ctrl-Q: quit",
        },
        ConsoleViewMode::Copy => {
            "h/j/k/l or arrows: move · v: select/unselect · c/y: copy · a: all · q/Esc: exit"
        }
    };
    let tail = if view.following {
        "LIVE"
    } else {
        "NOT FOLLOWING"
    };
    format!(
        "{} · {focus} · {controls} · {tail}",
        mouse_owner_text(view, child_mouse_tracking)
    )
}

fn mouse_owner_text(view: ConsoleViewState, child_mouse_tracking: bool) -> &'static str {
    if view.stackhand_mouse_gesture {
        "MOUSE: STACKHAND · selecting"
    } else if view.mode == ConsoleViewMode::Console && child_mouse_tracking {
        "MOUSE: CHILD · Shift+drag: copy"
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
            cpu: None,
            memory: None,
            selected,
        }
    }

    fn row_with_metrics(
        name: &str,
        status: &str,
        cpu: &str,
        memory: &str,
        selected: bool,
    ) -> ProcessRowView {
        ProcessRowView {
            name: name.to_string(),
            status: status.to_string(),
            cpu: Some(cpu.to_string()),
            memory: Some(memory.to_string()),
            selected,
        }
    }

    /// Render one frame into a test buffer for assertions. Small but tall
    /// enough that three Process rows stay inside the capped list band.
    fn rendered(rows: &[ProcessRowView]) -> ratatui::buffer::Buffer {
        render_rows_at(rows, 60, 18)
    }

    fn render_rows_at(rows: &[ProcessRowView], width: u16, height: u16) -> ratatui::buffer::Buffer {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_project(frame, rows, None, None, ConsoleViewState::default(), "");
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
    fn secondary_chrome_uses_the_legible_terminal_palette_role() {
        let rows = [row("web", "Ready", true), row("db", "Stopped", false)];
        let buffer = rendered(&rows);
        let (list, console, footer) = project_layout(buffer.area, rows.len());

        assert_eq!(buffer[(list.x, list.y)].fg, ratatui::style::Color::Cyan);
        assert_eq!(
            buffer[(console.x, console.y)].fg,
            ratatui::style::Color::Gray
        );
        assert_eq!(buffer[(footer.x, footer.y)].fg, ratatui::style::Color::Gray);
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
    fn metric_cells_render_when_they_fit_the_width() {
        let rows = [
            row_with_metrics("web", "Ready", "3.2%", "184M", true),
            row_with_metrics("worker", "Ready", "12%", "2G", false),
        ];
        let text = buffer_text(&rendered(&rows));
        assert!(text.contains("3.2%"), "{text:?}");
        assert!(text.contains("184M"), "{text:?}");
        assert!(text.contains("12%"), "{text:?}");
    }

    #[test]
    fn metric_cells_degrade_on_a_narrow_layout() {
        let rows = [
            row_with_metrics("web", "Ready", "3.2%", "184M", true),
            row_with_metrics("worker", "Waiting (setup: started)", "12%", "2G", false),
        ];
        // A narrow terminal keeps the essential name and status; the
        // optional metric cells drop out instead of crowding them off.
        let text = buffer_text(&render_rows_at(&rows, 20, 18));
        assert!(text.contains("web"), "{text:?}");
        assert!(text.contains("Ready"), "{text:?}");
        assert!(
            !text.contains("3.2%"),
            "the metric cell must degrade: {text:?}"
        );
        assert!(
            !text.contains("184M"),
            "the memory cell must degrade: {text:?}"
        );
    }

    #[test]
    fn missing_metrics_render_without_metric_cells() {
        let rows = [row("web", "Ready", true), row("worker", "Stopped", false)];
        let text = buffer_text(&render_rows_at(&rows, 40, 18));
        assert!(text.contains("web"), "{text:?}");
        assert!(text.contains("Stopped"), "{text:?}");
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
    fn copy_mode_positions_the_terminal_owned_keyboard_cursor() {
        let backend = ratatui::backend::TestBackend::new(40, 12);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let snapshot = OwnedTerminalSnapshot {
            buffer: ratatui::buffer::Buffer::empty(Rect::new(0, 0, 40, 12)),
            cursor: Some(crate::terminal::OwnedCursorState {
                position: ratatui::layout::Position::new(2, 1),
                shape: crate::terminal::CursorShape::Block,
                blinking: false,
            }),
            mouse_tracking: false,
        };
        let mut console_inner = Rect::default();

        terminal
            .draw(|frame| {
                console_inner = render_project(
                    frame,
                    &[],
                    Some(&snapshot),
                    None,
                    ConsoleViewState {
                        mode: ConsoleViewMode::Copy,
                        ..ConsoleViewState::default()
                    },
                    "",
                );
            })
            .unwrap();

        assert_eq!(
            terminal.backend().cursor_position(),
            ratatui::layout::Position::new(console_inner.x + 2, console_inner.y + 1)
        );
    }

    #[test]
    fn scrolling_keeps_process_list_focus_and_reports_live_state() {
        let text = footer_text(
            ConsoleViewState {
                mode: ConsoleViewMode::ProcessList,
                following: false,
                warning: None,
                stackhand_mouse_gesture: false,
                pane: ConsolePaneKind::default(),
            },
            false,
        );

        assert!(text.contains("FOCUS: PROCESSES"), "{text}");
        assert!(text.contains("NOT FOLLOWING"), "{text}");
        assert!(text.contains("j/k or ↑↓: select"), "{text}");
    }

    #[test]
    fn footer_makes_paste_rejection_visible() {
        let text = footer_text(
            ConsoleViewState {
                mode: ConsoleViewMode::Console,
                following: true,
                warning: Some(ConsoleWarning::PasteRejected),
                stackhand_mouse_gesture: false,
                pane: ConsolePaneKind::default(),
            },
            false,
        );

        assert!(text.contains("WARNING: paste rejected"));
        assert!(text.contains("focus an input-enabled console"));
    }

    #[test]
    fn footer_makes_admitted_paste_delivery_failure_visible() {
        let text = footer_text(
            ConsoleViewState {
                mode: ConsoleViewMode::Console,
                following: true,
                warning: Some(ConsoleWarning::PasteDeliveryFailed),
                stackhand_mouse_gesture: false,
                pane: ConsolePaneKind::default(),
            },
            false,
        );
        assert!(text.contains("admitted paste did not reach the child"));
    }

    #[test]
    fn footer_makes_clipboard_failure_visible_and_keeps_selection_controls() {
        let text = footer_text(
            ConsoleViewState {
                mode: ConsoleViewMode::Copy,
                following: false,
                warning: Some(ConsoleWarning::ClipboardFailed),
                stackhand_mouse_gesture: false,
                pane: ConsolePaneKind::default(),
            },
            true,
        );

        assert!(text.contains("clipboard write failed"));
        assert!(text.contains("selection remains available"));
        assert!(text.contains("MOUSE: STACKHAND"));
    }

    #[test]
    fn footer_shows_child_mouse_ownership_and_the_override() {
        let text = footer_text(
            ConsoleViewState {
                mode: ConsoleViewMode::Console,
                ..ConsoleViewState::default()
            },
            true,
        );

        assert!(text.contains("MOUSE: CHILD"));
        assert!(text.contains("Shift+drag: copy"));
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

        assert!(text.starts_with("MOUSE: STACKHAND · selecting"));
    }

    #[test]
    fn footer_labels_the_focus_and_the_pane() {
        let console_footer = footer_text(
            ConsoleViewState {
                mode: ConsoleViewMode::Console,
                ..ConsoleViewState::default()
            },
            false,
        );
        assert!(
            console_footer.contains("FOCUS: CONSOLE"),
            "{console_footer}"
        );
        assert!(
            console_footer.contains("Ctrl-A, then v: copy"),
            "{console_footer}"
        );
        assert!(console_footer.contains("LIVE"), "{console_footer}");

        let list_footer = footer_text(
            ConsoleViewState {
                mode: ConsoleViewMode::ProcessList,
                ..ConsoleViewState::default()
            },
            false,
        );
        assert!(list_footer.contains("FOCUS: PROCESSES"), "{list_footer}");

        let pipe_footer = footer_text(
            ConsoleViewState {
                mode: ConsoleViewMode::Console,
                pane: ConsolePaneKind::Pipe,
                ..ConsoleViewState::default()
            },
            false,
        );
        assert!(pipe_footer.contains("read-only output"), "{pipe_footer}");

        let empty_footer = footer_text(
            ConsoleViewState {
                mode: ConsoleViewMode::Console,
                pane: ConsolePaneKind::Empty,
                ..ConsoleViewState::default()
            },
            false,
        );
        assert!(empty_footer.contains("no active Run"), "{empty_footer}");
    }

    #[test]
    fn footer_makes_input_rejections_visible() {
        for (warning, needle) in [
            (ConsoleWarning::InputDisabled, "input is not enabled"),
            (ConsoleWarning::PipeReadOnly, "pipe output is read-only"),
            (
                ConsoleWarning::SelectionUnavailable,
                "text selection is available only in a PTY console",
            ),
        ] {
            let state = ConsoleViewState {
                warning: Some(warning),
                ..ConsoleViewState::default()
            };
            let text = footer_text(state, false);
            assert!(text.contains("WARNING:"), "{text}");
            assert!(text.contains(needle), "{text}");
        }
    }
}
