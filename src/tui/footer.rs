use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph},
};
use ratatui_interact::{
    components::{Select, SelectAction, SelectState, handle_select_key, handle_select_mouse},
    traits::ClickRegion,
};

use crate::log_view::SearchDialogView;

use super::theme::TERMINAL_THEME;
use super::view::{ConsolePaneKind, ConsoleViewMode, ConsoleViewState, ConsoleWarning};

/// One action named in the visible footer or Search Logs dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VisibleAction {
    ApplyProfile,
    ChooseProfile,
    ClearSelection,
    CloseProfileMenu,
    Copy,
    EnterCopy,
    ExitCopy,
    FocusProcesses,
    Follow,
    LifecycleStart,
    LifecycleStop,
    LifecycleRestart,
    OpenLifecycleMenu,
    OpenProfiles,
    Quit,
    SearchCancel,
    SearchEdit,
    SearchNext,
    SearchPrevious,
    SearchSubmit,
    SelectAll,
    StartAnyway,
    ToggleSelection,
    ToggleView,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VisibleActionEvent {
    Ignored,
    Changed,
    Selected(VisibleAction),
}

#[derive(Clone, Debug)]
struct ActionRegion {
    area: Rect,
    action: VisibleAction,
}

struct Segment {
    text: String,
    action: Option<VisibleAction>,
    style: Option<Style>,
}

impl Segment {
    fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            action: None,
            style: None,
        }
    }

    fn styled(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            action: None,
            style: Some(style),
        }
    }

    fn action(text: impl Into<String>, action: VisibleAction) -> Self {
        Self {
            text: text.into(),
            action: Some(action),
            style: Some(TERMINAL_THEME.link()),
        }
    }
}

/// Render-owned hit regions for every visible action and the lifecycle menu.
///
/// Regions are rebuilt from the rendered labels each frame. Input therefore
/// cannot drift from clipped or conditional footer content.
pub(crate) struct VisibleActions {
    regions: Vec<ActionRegion>,
    modal_area: Option<Rect>,
    lifecycle_state: SelectState,
    lifecycle_trigger: Rect,
    lifecycle_regions: Vec<ClickRegion<SelectAction>>,
}

impl Default for VisibleActions {
    fn default() -> Self {
        Self {
            regions: Vec::new(),
            modal_area: None,
            lifecycle_state: SelectState::new(3),
            lifecycle_trigger: Rect::default(),
            lifecycle_regions: Vec::new(),
        }
    }
}

impl VisibleActions {
    pub(crate) fn is_menu_open(&self) -> bool {
        self.lifecycle_state.is_open
    }

    pub(crate) fn close_menu(&mut self) {
        self.lifecycle_state.close();
    }

    pub(crate) fn handle_key(&mut self, key: &KeyEvent) -> VisibleActionEvent {
        if !self.is_menu_open() {
            return VisibleActionEvent::Ignored;
        }
        let code = match key.code {
            KeyCode::Char('j') => KeyCode::Down,
            KeyCode::Char('k') => KeyCode::Up,
            code => code,
        };
        let translated = KeyEvent::new(code, key.modifiers);
        let previous_highlight = self.lifecycle_state.highlighted_index;
        if let Some(action) = handle_select_key(&translated, &mut self.lifecycle_state) {
            return self.map_lifecycle_action(action);
        }
        if previous_highlight != self.lifecycle_state.highlighted_index {
            VisibleActionEvent::Changed
        } else {
            VisibleActionEvent::Ignored
        }
    }

    pub(crate) fn handle_mouse(&mut self, mouse: &MouseEvent) -> VisibleActionEvent {
        if let Some(action) = handle_select_mouse(
            mouse,
            &mut self.lifecycle_state,
            self.lifecycle_trigger,
            &self.lifecycle_regions,
        ) {
            return self.map_lifecycle_action(action);
        }
        if self.is_menu_open() && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            self.close_menu();
            return VisibleActionEvent::Changed;
        }
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return VisibleActionEvent::Ignored;
        }
        if let Some(action) = self
            .regions
            .iter()
            .find(|region| contains(region.area, mouse.column, mouse.row))
            .map(|region| region.action)
        {
            return if action == VisibleAction::OpenLifecycleMenu {
                self.lifecycle_state.toggle();
                VisibleActionEvent::Changed
            } else {
                VisibleActionEvent::Selected(action)
            };
        }
        if self
            .modal_area
            .is_some_and(|area| contains(area, mouse.column, mouse.row))
        {
            VisibleActionEvent::Changed
        } else {
            VisibleActionEvent::Ignored
        }
    }

    pub(crate) fn render_footer(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        screen: Rect,
        view: ConsoleViewState,
        child_mouse_tracking: bool,
    ) {
        self.regions.clear();
        self.modal_area = None;
        let line = self.action_line(
            area,
            footer_segments(view, child_mouse_tracking),
            TERMINAL_THEME.footer(view.warning.is_some()),
        );
        frame.render_widget(Paragraph::new(line), area);

        self.lifecycle_trigger = self
            .regions
            .iter()
            .find(|region| region.action == VisibleAction::OpenLifecycleMenu)
            .map_or(Rect::default(), |region| region.area);
        self.lifecycle_regions = if self.is_menu_open() && self.lifecycle_trigger.width > 0 {
            let menu_width = 22.min(screen.width);
            let menu_x = self
                .lifecycle_trigger
                .x
                .min(screen.right().saturating_sub(menu_width));
            let menu_anchor = Rect::new(
                menu_x,
                self.lifecycle_trigger.y,
                menu_width,
                self.lifecycle_trigger.height,
            );
            Select::new(&["Start", "Stop", "Restart / rerun"], &self.lifecycle_state)
                .render_dropdown(frame, menu_anchor, screen)
        } else {
            Vec::new()
        };
    }

    pub(crate) fn render_search_dialog(
        &mut self,
        frame: &mut Frame<'_>,
        dialog: &SearchDialogView,
        area: Rect,
    ) {
        let width = area.width.saturating_sub(4).min(64);
        let height = area.height.saturating_sub(2).min(7);
        if width < 20 || height < 5 {
            return;
        }
        let dialog_area = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        self.modal_area = Some(dialog_area);
        frame.render_widget(Clear, dialog_area);
        let block = Block::bordered()
            .border_style(TERMINAL_THEME.focus_border())
            .title(" Search Logs ");
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled("Search terms", TERMINAL_THEME.secondary_text()),
                Line::from(format!("{}_", dialog.query)),
            ]),
            inner,
        );
        if inner.height > 3 {
            let action_area = Rect::new(inner.x, inner.y + 3, inner.width, 1);
            let line = self.action_line(
                action_area,
                vec![
                    Segment::styled(&dialog.result, TERMINAL_THEME.secondary_text()),
                    Segment::text("  ·  "),
                    Segment::action("Enter: Search", VisibleAction::SearchSubmit),
                    Segment::text("  ·  "),
                    Segment::action("Esc: Cancel", VisibleAction::SearchCancel),
                ],
                Style::default(),
            );
            frame.render_widget(Paragraph::new(line), action_area);
        }
    }

    fn action_line(
        &mut self,
        area: Rect,
        segments: Vec<Segment>,
        base_style: Style,
    ) -> Line<'static> {
        let mut column = area.x;
        let mut spans = Vec::with_capacity(segments.len());
        for segment in segments {
            let width = Line::from(segment.text.clone()).width() as u16;
            if let Some(action) = segment.action {
                let visible_width = area.right().saturating_sub(column).min(width);
                if visible_width > 0 {
                    self.regions.push(ActionRegion {
                        area: Rect::new(column, area.y, visible_width, area.height),
                        action,
                    });
                }
            }
            let style = segment.style.unwrap_or(base_style);
            spans.push(Span::styled(segment.text, style));
            column = column.saturating_add(width);
        }
        Line::from(spans)
    }

    fn map_lifecycle_action(&self, action: SelectAction) -> VisibleActionEvent {
        match action {
            SelectAction::Select(0) => VisibleActionEvent::Selected(VisibleAction::LifecycleStart),
            SelectAction::Select(1) => VisibleActionEvent::Selected(VisibleAction::LifecycleStop),
            SelectAction::Select(2) => {
                VisibleActionEvent::Selected(VisibleAction::LifecycleRestart)
            }
            SelectAction::Select(_) => VisibleActionEvent::Ignored,
            SelectAction::Open | SelectAction::Close | SelectAction::Focus => {
                VisibleActionEvent::Changed
            }
        }
    }
}

#[cfg(test)]
pub(super) fn footer_text(view: ConsoleViewState, child_mouse_tracking: bool) -> String {
    footer_segments(view, child_mouse_tracking)
        .into_iter()
        .map(|segment| segment.text)
        .collect()
}

fn footer_segments(view: ConsoleViewState, child_mouse_tracking: bool) -> Vec<Segment> {
    if view.search_editing {
        return vec![
            Segment::text("SEARCH · "),
            Segment::action("Enter: Search", VisibleAction::SearchSubmit),
            Segment::text(" · "),
            Segment::action("Esc: Cancel", VisibleAction::SearchCancel),
        ];
    }

    let focus = match view.mode {
        ConsoleViewMode::ProcessList => "FOCUS: PROCESSES",
        ConsoleViewMode::Console => "FOCUS: CONSOLE",
        ConsoleViewMode::Copy => "MODE: COPY",
    };
    let mut segments = vec![
        Segment::text(mouse_owner_text(view, child_mouse_tracking)),
        Segment::text(" · "),
        Segment::text(focus),
        Segment::text(" · "),
    ];
    if let Some(warning) = view.warning {
        append_warning(&mut segments, warning);
        return segments;
    }
    if view.logs_selection {
        return vec![
            Segment::text("MODE: SELECT LOGS · drag: adjust · "),
            Segment::action("c/y: copy", VisibleAction::Copy),
            Segment::text(" · "),
            Segment::action("Esc: clear", VisibleAction::ClearSelection),
        ];
    }
    if view.search_active {
        return vec![
            Segment::text("SEARCH · "),
            Segment::action("Ctrl-F or /: edit", VisibleAction::SearchEdit),
            Segment::text(" · "),
            Segment::action("Enter/F3: next", VisibleAction::SearchNext),
            Segment::text(" · "),
            Segment::action(
                "Shift+Enter/Shift+F3: previous",
                VisibleAction::SearchPrevious,
            ),
        ];
    }

    match view.mode {
        ConsoleViewMode::ProcessList => append_process_list_controls(&mut segments, view),
        ConsoleViewMode::Console if view.pane == ConsolePaneKind::Terminal => {
            segments.push(Segment::text("keys: child · "));
            segments.push(Segment::action(
                "Ctrl-A, then v: copy",
                VisibleAction::EnterCopy,
            ));
            segments.push(Segment::text(" · "));
            segments.push(Segment::action("Ctrl-Q: quit", VisibleAction::Quit));
        }
        ConsoleViewMode::Console => append_logs_controls(&mut segments, view),
        ConsoleViewMode::Copy => {
            segments.push(Segment::text("h/j/k/l or arrows: move · "));
            segments.push(Segment::action(
                "v: select/unselect",
                VisibleAction::ToggleSelection,
            ));
            segments.push(Segment::text(" · "));
            segments.push(Segment::action("c/y: copy", VisibleAction::Copy));
            segments.push(Segment::text(" · "));
            segments.push(Segment::action("a: all", VisibleAction::SelectAll));
            segments.push(Segment::text(" · "));
            segments.push(Segment::action("q/Esc: exit", VisibleAction::ExitCopy));
        }
    }
    segments.push(Segment::text(if view.following {
        " · LIVE"
    } else {
        " · PAUSED"
    }));
    segments
}

fn append_warning(segments: &mut Vec<Segment>, warning: ConsoleWarning) {
    match warning {
        ConsoleWarning::PasteRejected => segments.push(Segment::text(
            "WARNING: paste rejected; focus an input-enabled console first",
        )),
        ConsoleWarning::InputRejected => segments.push(Segment::text(
            "WARNING: terminal input was rejected; Run is stopping or its queue is full",
        )),
        ConsoleWarning::InputBackpressure => segments.push(Segment::text(
            "WARNING: child input queue is saturated; delivery is bounded",
        )),
        ConsoleWarning::OutputTruncated => segments.push(Segment::text(
            "WARNING: oldest Process output was removed at the history bound",
        )),
        ConsoleWarning::PasteDeliveryFailed => segments.push(Segment::text(
            "WARNING: an admitted paste did not reach the child",
        )),
        ConsoleWarning::ClipboardFailed => segments.push(Segment::text(
            "WARNING: clipboard write failed; selection remains available",
        )),
        ConsoleWarning::NothingSelected => {
            segments.push(Segment::text("WARNING: no terminal text is selected · "));
            segments.push(Segment::action(
                "v: start selection",
                VisibleAction::ToggleSelection,
            ));
        }
        ConsoleWarning::NoLogsToCopy => {
            segments.push(Segment::text("WARNING: no Logs text is available to copy"))
        }
        ConsoleWarning::InputDisabled => {
            segments.push(Segment::text(
                "WARNING: input is not enabled for this Process · ",
            ));
            segments.push(Segment::action(
                "Ctrl-A: Process list",
                VisibleAction::FocusProcesses,
            ));
        }
        ConsoleWarning::LogsCommandOnly => {
            segments.push(Segment::text(
                "WARNING: Logs accepts commands, not Process input · ",
            ));
            segments.push(Segment::action(
                "Ctrl-F or /: search",
                VisibleAction::SearchEdit,
            ));
            segments.push(Segment::text(" · "));
            segments.push(Segment::action(
                "Esc: Processes",
                VisibleAction::FocusProcesses,
            ));
        }
        ConsoleWarning::SelectionUnavailable => segments.push(Segment::text(
            "WARNING: terminal selection is available only in Terminal view",
        )),
        ConsoleWarning::LinkOpenFailed => segments.push(Segment::text(
            "WARNING: could not open the port in a browser",
        )),
    }
}

fn append_process_list_controls(segments: &mut Vec<Segment>, view: ConsoleViewState) {
    if view.profile_menu_open {
        segments.push(Segment::text("↑/↓: select · "));
        segments.push(Segment::action(
            "Enter: choose",
            VisibleAction::ChooseProfile,
        ));
        segments.push(Segment::text(" · "));
        segments.push(Segment::action(
            "Esc: close",
            VisibleAction::CloseProfileMenu,
        ));
        return;
    }
    segments.push(Segment::text("j/k: select · "));
    if view.profiles_available {
        segments.push(Segment::action("p: profiles", VisibleAction::OpenProfiles));
        segments.push(Segment::text(" · "));
    }
    if view.profile_changes_pending {
        segments.push(Segment::action(
            "R: apply profile",
            VisibleAction::ApplyProfile,
        ));
        segments.push(Segment::text(" · "));
    }
    segments.push(Segment::action(
        "s/x/r: lifecycle",
        VisibleAction::OpenLifecycleMenu,
    ));
    if view.start_anyway_available {
        segments.push(Segment::text(" · "));
        segments.push(Segment::action(
            "S: start anyway",
            VisibleAction::StartAnyway,
        ));
    }
    segments.push(Segment::text(" · "));
    segments.push(Segment::action("l: view", VisibleAction::ToggleView));
    segments.push(Segment::text(" · "));
    segments.push(Segment::action(
        "Ctrl-F or /: search",
        VisibleAction::SearchEdit,
    ));
    segments.push(Segment::text(" · "));
    segments.push(Segment::action("q: quit", VisibleAction::Quit));
}

fn append_logs_controls(segments: &mut Vec<Segment>, view: ConsoleViewState) {
    segments.push(Segment::text("↑↓/j/k: scroll · PgUp/PgDn: page · "));
    segments.push(Segment::action(
        "Ctrl-F or /: search",
        VisibleAction::SearchEdit,
    ));
    segments.push(Segment::text(" · "));
    segments.push(Segment::action("f: live", VisibleAction::Follow));
    segments.push(Segment::text(" · "));
    segments.push(Segment::action("c/y: copy", VisibleAction::Copy));
    if view.terminal_available {
        segments.push(Segment::text(" · "));
        segments.push(Segment::action("l: terminal", VisibleAction::ToggleView));
    }
    segments.push(Segment::text(" · "));
    segments.push(Segment::action(
        "Esc: Processes",
        VisibleAction::FocusProcesses,
    ));
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

fn contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    fn click(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn direct_footer_action_uses_its_rendered_text_region() {
        let mut actions = VisibleActions::default();
        let mut terminal = Terminal::new(TestBackend::new(100, 5)).unwrap();
        terminal
            .draw(|frame| {
                actions.render_footer(
                    frame,
                    Rect::new(0, 4, 100, 1),
                    frame.area(),
                    ConsoleViewState::default(),
                    false,
                );
            })
            .unwrap();
        let line = (0..100)
            .map(|column| terminal.backend().buffer()[(column, 4)].symbol())
            .collect::<String>();
        let start = line.find("l: view").unwrap() as u16;

        assert_eq!(
            actions.handle_mouse(&click(start, 4)),
            VisibleActionEvent::Selected(VisibleAction::ToggleView)
        );
    }

    #[test]
    fn search_dialog_actions_are_clickable_and_own_the_dialog_surface() {
        let mut actions = VisibleActions::default();
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|frame| {
                actions.render_footer(
                    frame,
                    Rect::new(0, 19, 80, 1),
                    frame.area(),
                    ConsoleViewState {
                        search_editing: true,
                        ..ConsoleViewState::default()
                    },
                    false,
                );
                actions.render_search_dialog(
                    frame,
                    &SearchDialogView {
                        query: "timeout".to_string(),
                        result: "Match 1 of 2".to_string(),
                    },
                    Rect::new(0, 4, 80, 15),
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let search_position = (0..20)
            .find_map(|row| {
                let line = (0..80)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>();
                line.find("Enter: Search")
                    .map(|column| (column as u16, row))
            })
            .unwrap();

        assert_eq!(
            actions.handle_mouse(&click(search_position.0, search_position.1)),
            VisibleActionEvent::Selected(VisibleAction::SearchSubmit)
        );
        assert_eq!(
            actions.handle_mouse(&click(10, 10)),
            VisibleActionEvent::Changed
        );
    }

    #[test]
    fn lifecycle_footer_action_opens_a_mouse_selectable_menu() {
        let mut actions = VisibleActions::default();
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        terminal
            .draw(|frame| {
                actions.render_footer(
                    frame,
                    Rect::new(0, 11, 100, 1),
                    frame.area(),
                    ConsoleViewState::default(),
                    false,
                );
            })
            .unwrap();
        let line = (0..100)
            .map(|column| terminal.backend().buffer()[(column, 11)].symbol())
            .collect::<String>();
        let start = line.find("s/x/r: lifecycle").unwrap() as u16;
        assert_eq!(
            actions.handle_mouse(&click(start, 11)),
            VisibleActionEvent::Changed
        );
        terminal
            .draw(|frame| {
                actions.render_footer(
                    frame,
                    Rect::new(0, 11, 100, 1),
                    frame.area(),
                    ConsoleViewState::default(),
                    false,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let restart_row = (0..12)
            .find(|row| {
                (0..100)
                    .map(|column| buffer[(column, *row)].symbol())
                    .collect::<String>()
                    .contains("Restart / rerun")
            })
            .unwrap();

        assert_eq!(
            actions.handle_mouse(&click(start, restart_row)),
            VisibleActionEvent::Selected(VisibleAction::LifecycleRestart)
        );
    }
}
