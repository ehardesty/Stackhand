use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{
    Block, Borders, Cell, Paragraph, Row, Scrollbar, ScrollbarOrientation, ScrollbarState, Table,
    TableState,
};

use crate::terminal::{OwnedTerminalScrollbar, OwnedTerminalSnapshot};

use super::profile_menu::ProjectProfileMenu;
use super::theme::{LifecycleTone, TERMINAL_THEME};

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
    NoLogsToCopy,
    InputDisabled,
    LogsCommandOnly,
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConsoleScrollbar {
    pub position: usize,
    pub content_length: usize,
    pub viewport_length: usize,
}

impl ConsoleScrollbar {
    pub(crate) fn from_terminal(scrollbar: OwnedTerminalScrollbar) -> Option<Self> {
        (scrollbar.total > scrollbar.len && scrollbar.len > 0).then(|| {
            let max_position = scrollbar.total.saturating_sub(scrollbar.len);
            Self {
                position: scrollbar.offset.min(max_position),
                content_length: max_position.saturating_add(1),
                viewport_length: scrollbar.len,
            }
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConsoleViewState {
    pub mode: ConsoleViewMode,
    pub following: bool,
    pub warning: Option<ConsoleWarning>,
    pub stackhand_mouse_gesture: bool,
    pub pane: ConsolePaneKind,
    pub search_editing: bool,
    pub search_active: bool,
    pub logs_selection: bool,
    pub logs_scrollbar: Option<ConsoleScrollbar>,
    pub profile_menu_open: bool,
    pub profile_changes_pending: bool,
    pub start_anyway_available: bool,
}

impl Default for ConsoleViewState {
    fn default() -> Self {
        Self {
            mode: ConsoleViewMode::ProcessList,
            following: true,
            warning: None,
            stackhand_mouse_gesture: false,
            pane: ConsolePaneKind::default(),
            search_editing: false,
            search_active: false,
            logs_selection: false,
            logs_scrollbar: None,
            profile_menu_open: false,
            profile_changes_pending: false,
            start_anyway_available: false,
        }
    }
}

/// One projected Process list row. Labels are projections of structured
/// snapshot state, never stored authoritative strings.
pub struct ProcessRowView {
    pub name: String,
    pub status: String,
    pub(crate) lifecycle_tone: LifecycleTone,
    /// Effective profile text when the Process list needs a Profile column.
    pub profile: Option<String>,
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
    let available_height = area.height.saturating_sub(2).max(1);
    let list_height = (process_rows as u16 + 3)
        .min((area.height / 3).max(4))
        .min(available_height);
    let [list, console_outer, footer] = Layout::vertical([
        Constraint::Length(list_height),
        Constraint::Min(1),
        Constraint::Length(FOOTER_HEIGHT),
    ])
    .areas(area);
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
    /// Retained Logs source location: observation sequence and line index.
    /// Synthetic truncation markers have no source.
    pub source: Option<(u64, usize)>,
    /// Byte offset where normalized output text starts after timestamp and
    /// stream chrome.
    pub content_offset: usize,
    /// Byte range of the current literal match in the formatted line.
    pub highlight: Option<(usize, usize)>,
    /// Byte range selected by a Logs mouse gesture.
    pub selection: Option<(usize, usize)>,
}

/// The selected pipe-mode Process's retained output, tail-following: the
/// newest lines fill the pane and older lines stay in the module, not in
/// render state. Run markers render dimmed so attempts stay distinguishable.
fn render_pipe_console(
    frame: &mut Frame<'_>,
    lines: &[PipeLine],
    scrollbar: Option<ConsoleScrollbar>,
    pane: Rect,
) {
    if pane.height == 0 {
        return;
    }
    let text_pane = if scrollbar.is_some() && pane.width > 1 {
        Rect::new(pane.x, pane.y, pane.width - 1, pane.height)
    } else {
        pane
    };
    let start = lines.len().saturating_sub(text_pane.height as usize);
    let tail = &lines[start..];
    let rows: Vec<ratatui::text::Line<'_>> = tail.iter().map(styled_pipe_line).collect();
    frame.render_widget(Paragraph::new(rows), text_pane);

    if let Some(scrollbar) = scrollbar.filter(|_| pane.width > 1) {
        render_scrollbar(frame, scrollbar, pane);
    }
}

fn render_terminal_console(frame: &mut Frame<'_>, snapshot: &OwnedTerminalSnapshot, console: Rect) {
    blit_console(frame, snapshot, console);
    if console.width > 1
        && let Some(scrollbar) = ConsoleScrollbar::from_terminal(snapshot.scrollbar)
    {
        render_scrollbar(frame, scrollbar, console);
    }
}

fn render_scrollbar(frame: &mut Frame<'_>, scrollbar: ConsoleScrollbar, pane: Rect) {
    let mut state = ScrollbarState::new(scrollbar.content_length)
        .position(scrollbar.position)
        .viewport_content_length(scrollbar.viewport_length);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_style(TERMINAL_THEME.secondary_text())
            .thumb_style(TERMINAL_THEME.focus_border()),
        pane,
        &mut state,
    );
}

fn styled_pipe_line(line: &PipeLine) -> ratatui::text::Line<'_> {
    use ratatui::text::Span;

    let mut breaks = vec![0, line.text.len()];
    for (start, end) in [line.highlight, line.selection].into_iter().flatten() {
        breaks.push(start.min(line.text.len()));
        breaks.push(end.min(line.text.len()));
    }
    breaks.sort_unstable();
    breaks.dedup();
    let spans = breaks
        .windows(2)
        .filter(|range| range[0] < range[1])
        .map(|range| {
            let start = range[0];
            let end = range[1];
            let mut style = if line.marker {
                TERMINAL_THEME.secondary_text()
            } else {
                Style::default()
            };
            if line
                .highlight
                .is_some_and(|(from, to)| start >= from && end <= to)
            {
                style = style.patch(TERMINAL_THEME.search_match());
            }
            if line
                .selection
                .is_some_and(|(from, to)| start >= from && end <= to)
            {
                style = style.patch(TERMINAL_THEME.selection());
            }
            Span::styled(&line.text[start..end], style)
        })
        .collect::<Vec<_>>();
    ratatui::text::Line::from(spans)
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
    process_table_state: &mut TableState,
    console_snapshot: Option<&OwnedTerminalSnapshot>,
    pipe_lines: Option<&[PipeLine]>,
    view: ConsoleViewState,
    process_list_title: &str,
    selected_header: &str,
    profile_menu: &mut ProjectProfileMenu,
) -> Rect {
    let area = frame.area();
    let (list, console_pane, footer) = project_layout(area, rows.len());
    let console_inner = pane_inner(console_pane);

    let profile_trigger = render_process_table(
        frame,
        rows,
        process_table_state,
        list,
        view.mode,
        process_list_title,
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
        render_pipe_console(frame, lines, view.logs_scrollbar, console_inner);
    } else {
        render_terminal_console(
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
    if let Some(trigger) = profile_trigger {
        profile_menu.render(frame, trigger, area);
    }
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
        scrollbar: OwnedTerminalScrollbar::new(1),
    });

const PROCESS_METRICS_MIN_WIDTH: u16 = 52;

fn render_process_table(
    frame: &mut Frame<'_>,
    rows: &[ProcessRowView],
    state: &mut TableState,
    pane: Rect,
    mode: ConsoleViewMode,
    title: &str,
) -> Option<Rect> {
    let inner = pane_inner(pane);
    let show_profile = rows.iter().any(|row| row.profile.is_some());
    let show_metrics = inner.width >= PROCESS_METRICS_MIN_WIDTH + if show_profile { 24 } else { 0 };
    let table_rows = rows.iter().map(|row| {
        let mut cells = vec![
            Cell::from(row.name.as_str()),
            Cell::from(row.status.as_str()).style(TERMINAL_THEME.lifecycle(row.lifecycle_tone)),
        ];
        if show_profile {
            cells.push(Cell::from(row.profile.as_deref().unwrap_or("")));
        }
        if show_metrics {
            cells.push(right_cell(row.cpu.as_deref().unwrap_or("")));
            cells.push(right_cell(row.memory.as_deref().unwrap_or("")));
        }
        Row::new(cells)
    });
    let mut widths = vec![Constraint::Percentage(28), Constraint::Fill(1)];
    if show_profile {
        widths.push(Constraint::Length(22));
    }
    if show_metrics {
        widths.push(Constraint::Length(8));
        widths.push(Constraint::Length(8));
    }
    let mut heading_cells = vec![Cell::from("Process"), Cell::from("Status")];
    if show_profile {
        heading_cells.push(Cell::from("Profile"));
    }
    if show_metrics {
        heading_cells.push(right_cell("CPU"));
        heading_cells.push(right_cell("Memory"));
    }
    let headings =
        Row::new(heading_cells).style(TERMINAL_THEME.secondary_text().add_modifier(Modifier::BOLD));
    let border_style = if mode == ConsoleViewMode::ProcessList {
        TERMINAL_THEME.focus_border()
    } else {
        TERMINAL_THEME.inactive_border()
    };
    let mut table = Table::new(table_rows, widths)
        .column_spacing(1)
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .block(
            Block::new()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(format!(" {title} ")),
        );
    if process_table_header_height(pane) == 1 {
        table = table.header(headings);
    }
    state.select(rows.iter().position(|row| row.selected));
    frame.render_stateful_widget(table, pane, state);
    profile_title_trigger(pane, title)
}

fn profile_title_trigger(pane: Rect, title: &str) -> Option<Rect> {
    let profile_start = title.find("Profile: ")?;
    let affordance_end = title[profile_start..].find(" ▾")? + profile_start + " ▾".len();
    let title_area = Rect::new(
        pane.x.saturating_add(1),
        pane.y,
        pane.width.saturating_sub(2),
        1,
    );
    let trigger_x = title_area
        .x
        .saturating_add(1)
        .saturating_add(Line::from(title[..profile_start].to_owned()).width() as u16);
    let trigger_width = Line::from(title[profile_start..affordance_end].to_owned()).width() as u16;
    let visible_width = title_area.right().saturating_sub(trigger_x);
    (trigger_width > 0 && visible_width > 0).then(|| {
        Rect::new(
            trigger_x,
            title_area.y,
            trigger_width.min(visible_width),
            title_area.height,
        )
    })
}

fn right_cell(text: &str) -> Cell<'_> {
    Cell::from(Line::from(text).right_aligned())
}

fn process_table_header_height(pane: Rect) -> u16 {
    u16::from(pane_inner(pane).height > 1)
}

/// Return the Process row under one terminal cell. The offset comes from the
/// `TableState` that Ratatui updated during the last render.
pub fn process_row_at(pane: Rect, row: u16, process_count: usize, offset: usize) -> Option<usize> {
    let inner = pane_inner(pane);
    let first_row = inner.y.saturating_add(process_table_header_height(pane));
    if row < first_row || row >= inner.bottom() {
        return None;
    }
    let index = offset.saturating_add(usize::from(row - first_row));
    (index < process_count).then_some(index)
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
    if view.search_editing {
        return "MODE: SEARCH · type query · Enter: apply · Esc/Ctrl-C: cancel".to_string();
    }
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
            ConsoleWarning::NoLogsToCopy => "WARNING: no Logs text is available to copy",
            ConsoleWarning::InputDisabled => {
                "WARNING: input is not enabled for this Process · Ctrl-A: Process list"
            }
            ConsoleWarning::LogsCommandOnly => {
                "WARNING: Logs accepts commands, not Process input · /: search · Esc: Processes"
            }
            ConsoleWarning::SelectionUnavailable => {
                "WARNING: terminal selection is available only in Terminal view"
            }
        };
        return format!(
            "{} · {focus} · {warning}",
            mouse_owner_text(view, child_mouse_tracking)
        );
    }
    if view.logs_selection {
        return "MODE: SELECT LOGS · drag: adjust · c/y: copy · Esc: clear".to_string();
    }

    let match_control = if view.search_active {
        " · n/N: match"
    } else {
        ""
    };
    let controls = match view.mode {
        ConsoleViewMode::ProcessList => {
            if view.profile_menu_open {
                "↑/↓: select · Enter: choose · Esc: close".to_string()
            } else {
                let apply_profile = if view.profile_changes_pending {
                    " · R: apply profile"
                } else {
                    ""
                };
                let start_anyway = if view.start_anyway_available {
                    " · S: start anyway"
                } else {
                    ""
                };
                format!(
                    "j/k: select · p: profiles{apply_profile} · s/x/r: lifecycle{start_anyway} · l: view · /: search{match_control} · q: quit"
                )
            }
        }
        ConsoleViewMode::Console => match view.pane {
            ConsolePaneKind::Terminal => {
                "keys: child · Ctrl-A, then v: copy · Ctrl-Q: quit".to_string()
            }
            ConsolePaneKind::Pipe => format!(
                "↑↓/j/k: scroll · PgUp/PgDn: page · /: search{match_control} · f: live · c/y: copy · Esc: Processes"
            ),
        },
        ConsoleViewMode::Copy => {
            "h/j/k/l or arrows: move · v: select/unselect · c/y: copy · a: all · q/Esc: exit"
                .to_string()
        }
    };
    let tail = if view.following { "LIVE" } else { "PAUSED" };
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
    use ratatui::buffer::Buffer;

    use super::*;

    fn row(name: &str, status: &str, selected: bool) -> ProcessRowView {
        ProcessRowView {
            name: name.to_string(),
            status: status.to_string(),
            lifecycle_tone: LifecycleTone::Muted,
            profile: None,
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
            lifecycle_tone: LifecycleTone::Muted,
            profile: None,
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
        let mut process_table_state = TableState::default();
        let mut profile_menu = ProjectProfileMenu::default();
        terminal
            .draw(|frame| {
                render_project(
                    frame,
                    rows,
                    &mut process_table_state,
                    None,
                    None,
                    ConsoleViewState::default(),
                    "Processes",
                    "",
                    &mut profile_menu,
                );
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
        buffer.content().iter().map(|cell| cell.symbol()).collect()
    }

    fn buffer_line(buffer: &ratatui::buffer::Buffer, row: u16) -> String {
        (buffer.area.x..buffer.area.right())
            .map(|column| buffer[(column, row)].symbol())
            .collect()
    }

    fn text_end(line: &str, text: &str) -> Option<usize> {
        line.find(text).map(|start| start + text.len())
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
    fn lifecycle_labels_use_semantic_terminal_palette_roles() {
        let mut rows = [
            row("web", "Ready", false),
            row("worker", "Waiting", false),
            row("api", "Failed", false),
        ];
        rows[0].lifecycle_tone = LifecycleTone::Success;
        rows[1].lifecycle_tone = LifecycleTone::Warning;
        rows[2].lifecycle_tone = LifecycleTone::Error;
        let buffer = rendered(&rows);

        for (row_index, label, color) in [
            (2, "Ready", ratatui::style::Color::Green),
            (3, "Waiting", ratatui::style::Color::Yellow),
            (4, "Failed", ratatui::style::Color::Red),
        ] {
            let line = buffer_line(&buffer, row_index);
            let column = line.find(label).expect("status label renders") as u16;
            assert_eq!(buffer[(column, row_index)].fg, color);
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
    fn exactly_the_selected_row_is_reversed_without_replacing_its_state_color() {
        let mut rows = [row("web", "Ready", false), row("db", "Waiting", true)];
        rows[1].lifecycle_tone = LifecycleTone::Warning;
        let buffer = rendered(&rows);

        // The table header occupies row 1; Process rows start on row 2.
        let web_reversed = buffer[(1, 2)].modifier.contains(Modifier::REVERSED);
        let db_reversed = buffer[(1, 3)].modifier.contains(Modifier::REVERSED);
        assert!(!web_reversed);
        assert!(db_reversed);

        let selected = buffer_line(&buffer, 3);
        let status_column = selected.find("Waiting").expect("status renders") as u16;
        assert_eq!(buffer[(status_column, 3)].fg, ratatui::style::Color::Yellow);
        assert!(
            buffer[(status_column, 3)]
                .modifier
                .contains(Modifier::REVERSED)
        );
        assert_eq!(buffer[(1, 3)].fg, ratatui::style::Color::Reset);
    }

    #[test]
    fn metric_cells_render_in_aligned_columns_when_they_fit() {
        let rows = [
            row_with_metrics("web", "Ready", "3.2%", "184M", true),
            row_with_metrics("worker", "Ready", "12%", "2G", false),
        ];
        let buffer = rendered(&rows);
        let header = buffer_line(&buffer, 1);
        let first = buffer_line(&buffer, 2);

        assert_eq!(header.find("Process"), first.find("web"));
        assert_eq!(header.find("Status"), first.find("Ready"));
        assert_eq!(text_end(&header, "CPU"), text_end(&first, "3.2%"));
        assert_eq!(text_end(&header, "Memory"), text_end(&first, "184M"));
    }

    #[test]
    fn profile_column_renders_every_profile_when_present() {
        let mut first = row("api", "Ready", true);
        first.profile = Some("local → cloud-dev".to_string());
        let mut second = row("worker", "Ready", false);
        second.profile = Some("cloud-dev".to_string());

        let text = buffer_text(&rendered(&[first, second]));
        assert!(text.contains("Profile"), "{text:?}");
        assert!(text.contains("local → cloud-dev"), "{text:?}");
        assert!(text.contains("cloud-dev"), "{text:?}");
    }

    #[test]
    fn profile_menu_overlay_keeps_title_affordance_visible() {
        let backend = ratatui::backend::TestBackend::new(60, 18);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let rows = [row("api", "Ready", true)];
        let mut process_table_state = TableState::default();
        let mut profile_menu = ProjectProfileMenu::default();
        profile_menu.sync("base", ["dev"], None);
        profile_menu.toggle();

        terminal
            .draw(|frame| {
                render_project(
                    frame,
                    &rows,
                    &mut process_table_state,
                    None,
                    None,
                    ConsoleViewState {
                        profile_menu_open: true,
                        ..ConsoleViewState::default()
                    },
                    "Processes · Profile: base ▾",
                    "",
                    &mut profile_menu,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let text = buffer_text(buffer);
        assert!(text.contains("Profile: base ▾"), "{text:?}");
        assert!(
            text.contains("dev"),
            "dropdown should overlay the frame: {text:?}"
        );
        assert!(
            buffer_text(buffer).contains("↑/↓: select"),
            "open footer should show menu controls: {text:?}"
        );
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

        assert!(buffer[(1, 2)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn the_selected_process_stays_visible_when_the_table_scrolls() {
        let rows = (0..8)
            .map(|index| row(&format!("process-{index}"), "Ready", index == 7))
            .collect::<Vec<_>>();
        let text = buffer_text(&rendered(&rows));

        assert!(text.contains("process-7"), "{text:?}");
        assert!(!text.contains("process-0"), "{text:?}");
    }

    #[test]
    fn process_table_keeps_ratatui_scroll_state_between_frames() {
        let backend = ratatui::backend::TestBackend::new(60, 18);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut rows = (0..12)
            .map(|index| row(&format!("process-{index}"), "Ready", index == 7))
            .collect::<Vec<_>>();
        let mut state = TableState::default();
        let mut profile_menu = ProjectProfileMenu::default();
        terminal
            .draw(|frame| {
                render_project(
                    frame,
                    &rows,
                    &mut state,
                    None,
                    None,
                    ConsoleViewState::default(),
                    "Processes",
                    "",
                    &mut profile_menu,
                );
            })
            .unwrap();
        assert_eq!(state.offset(), 5);

        rows[7].selected = false;
        rows[6].selected = true;
        terminal
            .draw(|frame| {
                render_project(
                    frame,
                    &rows,
                    &mut state,
                    None,
                    None,
                    ConsoleViewState::default(),
                    "Processes",
                    "",
                    &mut profile_menu,
                );
            })
            .unwrap();
        assert_eq!(state.offset(), 5);
    }

    #[test]
    fn the_console_pane_stays_visible_on_small_screens() {
        let (_, console, footer) = project_layout(Rect::new(0, 0, 20, 6), 8);

        // The table keeps room for its header and one Process while the
        // Console remains visible above the footer.
        assert!(console.height >= 1);
        assert_eq!(console.height + footer.height + 4, 6);
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
    fn logs_current_match_is_visibly_distinct() {
        let backend = ratatui::backend::TestBackend::new(40, 8);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut profile_menu = ProjectProfileMenu::default();
        let lines = [PipeLine {
            text: "00:00:00.000 out: before needle after".to_string(),
            marker: false,
            source: Some((1, 0)),
            content_offset: 18,
            highlight: Some((25, 31)),
            selection: None,
        }];
        let mut process_table_state = TableState::default();
        terminal
            .draw(|frame| {
                render_project(
                    frame,
                    &[],
                    &mut process_table_state,
                    None,
                    Some(&lines),
                    ConsoleViewState {
                        pane: ConsolePaneKind::Pipe,
                        ..ConsoleViewState::default()
                    },
                    "Processes",
                    "Logs · demo · search: /needle · 1/1",
                    &mut profile_menu,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert!(
            buffer
                .content()
                .iter()
                .any(|cell| cell.modifier.contains(Modifier::REVERSED) && cell.symbol() == "n")
        );
    }

    #[test]
    fn terminal_scrollbar_is_rendered_from_ghostty_rows() {
        let backend = ratatui::backend::TestBackend::new(10, 4);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let snapshot = OwnedTerminalSnapshot {
            buffer: Buffer::empty(Rect::new(0, 0, 10, 4)),
            cursor: None,
            mouse_tracking: false,
            scrollbar: OwnedTerminalScrollbar {
                total: 20,
                offset: 10,
                len: 4,
            },
        };

        terminal
            .draw(|frame| render_terminal_console(frame, &snapshot, frame.area()))
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert!(
            (0..4).all(|row| buffer[(9, row)].symbol() != " "),
            "the PTY scrollbar track and thumb must remain visible"
        );
    }

    #[test]
    fn terminal_scrollbar_does_not_replace_the_only_output_column() {
        let backend = ratatui::backend::TestBackend::new(1, 1);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut source = Buffer::empty(Rect::new(0, 0, 1, 1));
        source[(0, 0)].set_symbol("x");
        let snapshot = OwnedTerminalSnapshot {
            buffer: source,
            cursor: None,
            mouse_tracking: false,
            scrollbar: OwnedTerminalScrollbar {
                total: 20,
                offset: 10,
                len: 1,
            },
        };

        terminal
            .draw(|frame| render_terminal_console(frame, &snapshot, frame.area()))
            .unwrap();

        assert_eq!(terminal.backend().buffer()[(0, 0)].symbol(), "x");
    }

    #[test]
    fn logs_scrollbar_uses_a_reserved_right_column() {
        let backend = ratatui::backend::TestBackend::new(10, 4);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let lines = (0..4)
            .map(|index| PipeLine {
                text: format!("line-{index:04}"),
                marker: false,
                source: Some((index, 0)),
                content_offset: 0,
                highlight: None,
                selection: None,
            })
            .collect::<Vec<_>>();

        terminal
            .draw(|frame| {
                render_pipe_console(
                    frame,
                    &lines,
                    Some(ConsoleScrollbar {
                        position: 4,
                        content_length: 5,
                        viewport_length: 4,
                    }),
                    frame.area(),
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(8, 0)].symbol(), "0");
        assert!(
            (0..4).all(|row| buffer[(9, row)].symbol() != " "),
            "the scrollbar track and thumb must remain visible"
        );
    }

    #[test]
    fn logs_mouse_selection_is_rendered_as_selected_text() {
        let line = PipeLine {
            text: "select me".to_string(),
            marker: false,
            source: Some((1, 0)),
            content_offset: 0,
            highlight: None,
            selection: Some((0, 6)),
        };
        let styled = styled_pipe_line(&line);

        assert!(
            styled.spans[0]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert_eq!(styled.spans[0].content, "select");
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
            scrollbar: OwnedTerminalScrollbar::new(12),
        };
        let mut console_inner = Rect::default();
        let mut process_table_state = TableState::default();
        let mut profile_menu = ProjectProfileMenu::default();

        terminal
            .draw(|frame| {
                console_inner = render_project(
                    frame,
                    &[],
                    &mut process_table_state,
                    Some(&snapshot),
                    None,
                    ConsoleViewState {
                        mode: ConsoleViewMode::Copy,
                        ..ConsoleViewState::default()
                    },
                    "Processes",
                    "",
                    &mut profile_menu,
                );
            })
            .unwrap();

        assert_eq!(
            terminal.backend().cursor_position(),
            ratatui::layout::Position::new(console_inner.x + 2, console_inner.y + 1)
        );
    }

    #[test]
    fn apply_profile_control_appears_only_for_pending_changes() {
        let ordinary = footer_text(ConsoleViewState::default(), false);
        assert!(!ordinary.contains("R: apply profile"), "{ordinary}");

        let pending = footer_text(
            ConsoleViewState {
                profile_changes_pending: true,
                ..ConsoleViewState::default()
            },
            false,
        );
        assert!(pending.contains("R: apply profile"), "{pending}");
    }

    #[test]
    fn start_anyway_control_appears_only_for_a_waiting_selection() {
        let ordinary = footer_text(ConsoleViewState::default(), false);
        assert!(!ordinary.contains("S: start anyway"), "{ordinary}");

        let waiting = footer_text(
            ConsoleViewState {
                start_anyway_available: true,
                ..ConsoleViewState::default()
            },
            false,
        );
        assert!(waiting.contains("S: start anyway"), "{waiting}");
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
                search_editing: false,
                search_active: false,
                logs_selection: false,
                logs_scrollbar: None,
                profile_menu_open: false,
                profile_changes_pending: false,
                start_anyway_available: false,
            },
            false,
        );

        assert!(text.contains("FOCUS: PROCESSES"), "{text}");
        assert!(text.contains("PAUSED"), "{text}");
        assert!(text.contains("j/k: select"), "{text}");
    }

    #[test]
    fn search_footer_always_shows_submit_and_cancel_actions() {
        let text = footer_text(
            ConsoleViewState {
                search_editing: true,
                warning: Some(ConsoleWarning::LogsCommandOnly),
                ..ConsoleViewState::default()
            },
            false,
        );

        assert!(text.contains("MODE: SEARCH"), "{text}");
        assert!(text.contains("Enter: apply"), "{text}");
        assert!(text.contains("Esc/Ctrl-C: cancel"), "{text}");
        assert!(!text.contains("WARNING"), "{text}");
    }

    #[test]
    fn footer_hides_match_navigation_without_an_active_search() {
        let text = footer_text(
            ConsoleViewState {
                pane: ConsolePaneKind::Pipe,
                mode: ConsoleViewMode::Console,
                ..ConsoleViewState::default()
            },
            false,
        );

        assert!(!text.contains("n/N: match"), "{text}");

        let active = footer_text(
            ConsoleViewState {
                pane: ConsolePaneKind::Pipe,
                mode: ConsoleViewMode::Console,
                search_active: true,
                ..ConsoleViewState::default()
            },
            false,
        );
        assert!(active.contains("n/N: match"), "{active}");
    }

    #[test]
    fn logs_selection_footer_shows_copy_and_clear_actions() {
        let text = footer_text(
            ConsoleViewState {
                pane: ConsolePaneKind::Pipe,
                mode: ConsoleViewMode::Console,
                logs_selection: true,
                ..ConsoleViewState::default()
            },
            false,
        );

        assert!(text.contains("MODE: SELECT LOGS"), "{text}");
        assert!(text.contains("c/y: copy"), "{text}");
        assert!(text.contains("Esc: clear"), "{text}");
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
                search_editing: false,
                search_active: false,
                logs_selection: false,
                logs_scrollbar: None,
                profile_menu_open: false,
                profile_changes_pending: false,
                start_anyway_available: false,
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
                search_editing: false,
                search_active: false,
                logs_selection: false,
                logs_scrollbar: None,
                profile_menu_open: false,
                profile_changes_pending: false,
                start_anyway_available: false,
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
                search_editing: false,
                search_active: false,
                logs_selection: false,
                logs_scrollbar: None,
                profile_menu_open: false,
                profile_changes_pending: false,
                start_anyway_available: false,
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
        assert!(pipe_footer.contains("↑↓/j/k: scroll"), "{pipe_footer}");
        assert!(pipe_footer.contains("Esc: Processes"), "{pipe_footer}");
    }

    #[test]
    fn footer_makes_input_rejections_visible() {
        for (warning, needle) in [
            (ConsoleWarning::InputDisabled, "input is not enabled"),
            (ConsoleWarning::LogsCommandOnly, "Logs accepts commands"),
            (
                ConsoleWarning::SelectionUnavailable,
                "terminal selection is available only in Terminal view",
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
