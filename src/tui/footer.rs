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

use unicode_width::UnicodeWidthChar;

use crate::log_view::SearchDialogView;

use super::theme::{ModeTone, TERMINAL_THEME};
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
    FocusConsole,
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
    /// Show the other console view and focus it, as a console click does.
    SwitchView,
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

#[derive(Clone)]
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

    /// Forget the previous frame's regions. Call once before rendering any
    /// visible action.
    pub(crate) fn begin_frame(&mut self) {
        self.regions.clear();
        self.modal_area = None;
    }

    /// Build the console's top-border title: Terminal and Logs tabs, then
    /// the selected Process header. The inactive tab is a visible action.
    pub(crate) fn console_title(
        &mut self,
        pane: Rect,
        view: ConsoleViewState,
        header: &str,
    ) -> Line<'static> {
        let focused = view.mode != ConsoleViewMode::ProcessList;
        let tab = |label: &str, active: bool| {
            let text = format!(" {label} ");
            if active {
                Segment::styled(text, TERMINAL_THEME.active_tab(focused))
            } else {
                Segment::action(text, VisibleAction::SwitchView)
            }
        };
        let logs_active = view.pane == ConsolePaneKind::Pipe;
        let mut segments = Vec::new();
        if view.terminal_available {
            segments.push(tab("Terminal", !logs_active));
            segments.push(Segment::text("│"));
        }
        segments.push(tab("Logs", logs_active));
        // Block titles start one cell after the corner and stop before the
        // opposite corner, which keeps one border cell after the header.
        let title_row = Rect::new(
            pane.x.saturating_add(1),
            pane.y,
            pane.width.saturating_sub(3),
            1,
        );
        let room = usize::from(title_row.width).saturating_sub(segments_width(&segments) + 2);
        if !header.is_empty() && room > 0 {
            segments.push(Segment::text(format!(" {} ", fit_text(header, room))));
        }
        self.action_line(title_row, segments, TERMINAL_THEME.secondary_text())
    }

    pub(crate) fn render_footer(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        screen: Rect,
        view: ConsoleViewState,
        child_mouse_tracking: bool,
    ) {
        let line = self.action_line(
            area,
            footer_segments(view, child_mouse_tracking, area.width),
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

/// One footer hint. Lower priorities survive longer when the footer is
/// narrower than its content, so essential exits stay visible on small
/// terminals.
struct Hint {
    segment: Segment,
    priority: Priority,
}

/// How long a hint survives as the footer narrows. Essential hints are
/// clipped rather than hidden.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Priority {
    Essential,
    Useful,
    Extra,
}

use Priority::{Essential, Extra, Useful};

impl Hint {
    fn action(text: impl Into<String>, action: VisibleAction, priority: Priority) -> Self {
        Self {
            segment: Segment::action(text, action),
            priority,
        }
    }

    fn text(text: impl Into<String>, priority: Priority) -> Self {
        Self {
            segment: Segment::text(text),
            priority,
        }
    }
}

/// The footer's projection of keyboard ownership: one mode badge, ranked
/// hints, and an optional right-aligned follow state.
struct FooterContent {
    badge: &'static str,
    tone: ModeTone,
    hints: Vec<Hint>,
    trailing: Option<&'static str>,
}

const SEPARATOR: &str = " · ";
const TRAILING_GAP: usize = 2;

impl FooterContent {
    /// Drop the least important hints until the content fits. Equal
    /// priorities drop from the end, so the visible order never changes.
    /// The follow state goes last; essential hints are clipped, not hidden.
    fn fit(mut self, width: u16) -> Self {
        while self.width() > usize::from(width) {
            let Some(drop) = self
                .hints
                .iter()
                .enumerate()
                .filter(|(_, hint)| hint.priority > Essential)
                .max_by_key(|(index, hint)| (hint.priority, *index))
                .map(|(index, _)| index)
            else {
                self.trailing = None;
                break;
            };
            self.hints.remove(drop);
        }
        self
    }

    fn width(&self) -> usize {
        segments_width(&self.leading_segments())
            + self
                .trailing
                .map_or(0, |trailing| TRAILING_GAP + text_width(trailing))
    }

    /// The badge and hints, in rendered order.
    fn leading_segments(&self) -> Vec<Segment> {
        let mut segments = vec![
            Segment::styled(format!(" {} ", self.badge), TERMINAL_THEME.badge(self.tone)),
            Segment::text(" "),
        ];
        for (index, hint) in self.hints.iter().enumerate() {
            if index > 0 {
                segments.push(Segment::text(SEPARATOR));
            }
            segments.push(hint.segment.clone());
        }
        segments
    }

    fn into_segments(self, width: u16) -> Vec<Segment> {
        let mut segments = self.leading_segments();
        if let Some(trailing) = self.trailing {
            let gap = usize::from(width)
                .saturating_sub(segments_width(&segments) + text_width(trailing))
                .max(TRAILING_GAP);
            segments.push(Segment::text(" ".repeat(gap)));
            segments.push(Segment::text(trailing));
        }
        segments
    }
}

fn segments_width(segments: &[Segment]) -> usize {
    segments
        .iter()
        .map(|segment| text_width(&segment.text))
        .sum()
}

fn text_width(text: &str) -> usize {
    Line::from(text.to_owned()).width()
}

/// Shorten `text` to `width` display cells, ending with `…` when cut.
pub(super) fn fit_text(text: &str, width: usize) -> String {
    if text_width(text) <= width {
        return text.to_owned();
    }
    let mut kept = String::new();
    let mut used = 0;
    for character in text.chars() {
        let cells = character.width().unwrap_or(0);
        if used + cells + 1 > width {
            break;
        }
        kept.push(character);
        used += cells;
    }
    kept.push('…');
    kept
}

#[cfg(test)]
pub(super) fn footer_text(view: ConsoleViewState, child_mouse_tracking: bool) -> String {
    footer_content(view, child_mouse_tracking)
        .into_segments(0)
        .into_iter()
        .map(|segment| segment.text)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn footer_segments(view: ConsoleViewState, child_mouse_tracking: bool, width: u16) -> Vec<Segment> {
    footer_content(view, child_mouse_tracking)
        .fit(width)
        .into_segments(width)
}

fn footer_content(view: ConsoleViewState, child_mouse_tracking: bool) -> FooterContent {
    let (badge, tone) = mode_badge(view);
    let mut content = FooterContent {
        badge,
        tone,
        hints: Vec::new(),
        trailing: None,
    };
    if view.search_editing {
        content.hints = vec![
            Hint::action("Enter: search", VisibleAction::SearchSubmit, Essential),
            Hint::action("Esc: cancel", VisibleAction::SearchCancel, Essential),
        ];
        return content;
    }
    if let Some(owner) = mouse_owner_text(view, child_mouse_tracking) {
        content.hints.push(Hint::text(owner, Essential));
    }
    if let Some(warning) = view.warning {
        append_warning(&mut content.hints, warning);
        return content;
    }
    if view.logs_selection {
        content.hints.extend([
            Hint::text("drag: adjust", Extra),
            Hint::action("c/y: copy", VisibleAction::Copy, Essential),
            Hint::action("Esc: clear", VisibleAction::ClearSelection, Essential),
        ]);
        return content;
    }
    let hints = &mut content.hints;
    match view.mode {
        ConsoleViewMode::ProcessList => append_process_list_controls(hints, view),
        ConsoleViewMode::Console if view.pane == ConsolePaneKind::Terminal => {
            hints.push(Hint::action(
                "Ctrl-A: Processes",
                VisibleAction::FocusProcesses,
                Essential,
            ));
            hints.push(Hint::action(
                "Ctrl-A l: Logs",
                VisibleAction::ToggleView,
                Essential,
            ));
            hints.push(Hint::action(
                "Ctrl-A v: copy",
                VisibleAction::EnterCopy,
                Useful,
            ));
            hints.push(Hint::action("Ctrl-Q: quit", VisibleAction::Quit, Extra));
        }
        ConsoleViewMode::Console => append_logs_controls(hints, view),
        ConsoleViewMode::Copy => {
            hints.push(Hint::text("h/j/k/l: move", Extra));
            hints.push(Hint::action(
                "v: select",
                VisibleAction::ToggleSelection,
                Useful,
            ));
            hints.push(Hint::action("c/y: copy", VisibleAction::Copy, Essential));
            hints.push(Hint::action("a: all", VisibleAction::SelectAll, Extra));
            hints.push(Hint::action(
                "Esc: exit",
                VisibleAction::ExitCopy,
                Essential,
            ));
        }
    }
    content.trailing = Some(if view.following { "LIVE" } else { "PAUSED" });
    content
}

/// Name the keyboard owner. The badge answers the question the footer must
/// always answer: where will the next key go?
fn mode_badge(view: ConsoleViewState) -> (&'static str, ModeTone) {
    if view.search_editing {
        return ("FIND", ModeTone::Modal);
    }
    if view.logs_selection {
        return ("SELECT", ModeTone::Modal);
    }
    match view.mode {
        ConsoleViewMode::ProcessList => ("PROCESSES", ModeTone::Focus),
        ConsoleViewMode::Console if view.pane == ConsolePaneKind::Pipe => ("LOGS", ModeTone::Focus),
        ConsoleViewMode::Console if view.input_enabled => ("INPUT", ModeTone::Input),
        ConsoleViewMode::Console => ("TERMINAL", ModeTone::Focus),
        ConsoleViewMode::Copy => ("COPY", ModeTone::Modal),
    }
}

fn append_warning(hints: &mut Vec<Hint>, warning: ConsoleWarning) {
    let (message, follow_ups): (&str, &[(&str, VisibleAction)]) = match warning {
        ConsoleWarning::PasteRejected => (
            "WARNING: paste rejected; focus an input-enabled console first",
            &[],
        ),
        ConsoleWarning::InputRejected => (
            "WARNING: terminal input was rejected; Run is stopping or its queue is full",
            &[],
        ),
        ConsoleWarning::InputBackpressure => (
            "WARNING: child input queue is saturated; delivery is bounded",
            &[],
        ),
        ConsoleWarning::OutputTruncated => (
            "Earlier Logs expired at the history limit; recent output continues",
            &[],
        ),
        ConsoleWarning::PasteDeliveryFailed => {
            ("WARNING: an admitted paste did not reach the child", &[])
        }
        ConsoleWarning::ClipboardFailed => (
            "WARNING: clipboard write failed; selection remains available",
            &[],
        ),
        ConsoleWarning::NothingSelected => (
            "WARNING: no terminal text is selected",
            &[("v: start selection", VisibleAction::ToggleSelection)],
        ),
        ConsoleWarning::NoLogsToCopy => ("WARNING: no Logs text is available to copy", &[]),
        ConsoleWarning::InputDisabled => (
            "WARNING: input is not enabled for this Process",
            &[("Ctrl-A: Processes", VisibleAction::FocusProcesses)],
        ),
        ConsoleWarning::LogsCommandOnly => (
            "WARNING: Logs accepts commands, not Process input",
            &[
                ("Ctrl-F /: find", VisibleAction::SearchEdit),
                ("Esc: Processes", VisibleAction::FocusProcesses),
            ],
        ),
        ConsoleWarning::SelectionUnavailable => (
            "WARNING: terminal selection is available only in Terminal view",
            &[],
        ),
        ConsoleWarning::LinkOpenFailed => ("WARNING: could not open the port in a browser", &[]),
    };
    hints.push(Hint::text(message, Essential));
    hints.extend(
        follow_ups
            .iter()
            .map(|(label, action)| Hint::action(*label, *action, Essential)),
    );
}

fn append_process_list_controls(hints: &mut Vec<Hint>, view: ConsoleViewState) {
    if view.profile_menu_open {
        hints.push(Hint::text("↑/↓: select", Useful));
        hints.push(Hint::action(
            "Enter: choose",
            VisibleAction::ChooseProfile,
            Essential,
        ));
        hints.push(Hint::action(
            "Esc: close",
            VisibleAction::CloseProfileMenu,
            Essential,
        ));
        return;
    }
    hints.push(Hint::text("j/k: select", Extra));
    if view.profile_changes_pending {
        hints.push(Hint::action(
            "R: apply profile",
            VisibleAction::ApplyProfile,
            Essential,
        ));
    }
    if view.start_anyway_available {
        hints.push(Hint::action(
            "S: start anyway",
            VisibleAction::StartAnyway,
            Essential,
        ));
    }
    hints.push(Hint::action(
        "s/x/r: lifecycle",
        VisibleAction::OpenLifecycleMenu,
        Useful,
    ));
    append_view_toggle(hints, view);
    hints.push(Hint::action(
        "Tab: console",
        VisibleAction::FocusConsole,
        Useful,
    ));
    append_find_controls(hints, view);
    if view.profiles_available {
        hints.push(Hint::action(
            "p: profiles",
            VisibleAction::OpenProfiles,
            Extra,
        ));
    }
    hints.push(Hint::action("q: quit", VisibleAction::Quit, Extra));
}

fn append_logs_controls(hints: &mut Vec<Hint>, view: ConsoleViewState) {
    hints.push(Hint::text("j/k PgUp/PgDn: scroll", Extra));
    append_find_controls(hints, view);
    append_view_toggle(hints, view);
    hints.push(Hint::action(
        "Esc: Processes",
        VisibleAction::FocusProcesses,
        Essential,
    ));
    if !view.following {
        hints.push(Hint::action("f: live", VisibleAction::Follow, Useful));
    }
    hints.push(Hint::action("c/y: copy", VisibleAction::Copy, Extra));
}

fn append_find_controls(hints: &mut Vec<Hint>, view: ConsoleViewState) {
    hints.push(Hint::action(
        "Ctrl-F /: find",
        VisibleAction::SearchEdit,
        if view.search_active {
            Useful
        } else {
            Essential
        },
    ));
    if view.search_active {
        hints.push(Hint::action(
            "n: next",
            VisibleAction::SearchNext,
            Essential,
        ));
        hints.push(Hint::action(
            "N: previous",
            VisibleAction::SearchPrevious,
            Useful,
        ));
    }
}

/// Name the view a toggle would show, not the toggle itself.
fn append_view_toggle(hints: &mut Vec<Hint>, view: ConsoleViewState) {
    if !view.terminal_available {
        return;
    }
    let label = match view.pane {
        ConsolePaneKind::Terminal => "l: Logs",
        ConsolePaneKind::Pipe => "l: Terminal",
    };
    hints.push(Hint::action(label, VisibleAction::ToggleView, Essential));
}

/// Mouse ownership matters only when it differs from the default: the
/// child owns the mouse, or a Stackhand gesture is in progress.
fn mouse_owner_text(view: ConsoleViewState, child_mouse_tracking: bool) -> Option<&'static str> {
    if view.stackhand_mouse_gesture {
        Some("MOUSE: STACKHAND · selecting")
    } else if view.mode == ConsoleViewMode::Console && child_mouse_tracking {
        Some("MOUSE: CHILD · Shift+drag: copy")
    } else {
        None
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
                    ConsoleViewState {
                        terminal_available: true,
                        ..ConsoleViewState::default()
                    },
                    false,
                );
            })
            .unwrap();
        let line = (0..100)
            .map(|column| terminal.backend().buffer()[(column, 4)].symbol())
            .collect::<String>();
        let start = line[..line.find("l: Logs").unwrap()].chars().count() as u16;

        assert_eq!(
            actions.handle_mouse(&click(start, 4)),
            VisibleActionEvent::Selected(VisibleAction::ToggleView)
        );
    }

    fn fitted_text(view: ConsoleViewState, width: u16) -> String {
        footer_segments(view, false, width)
            .into_iter()
            .map(|segment| segment.text)
            .collect()
    }

    #[test]
    fn narrow_footers_drop_optional_hints_before_essential_exits() {
        let view = ConsoleViewState {
            terminal_available: true,
            profile_changes_pending: true,
            profiles_available: true,
            ..ConsoleViewState::default()
        };
        let wide = fitted_text(view, 200);
        assert!(wide.contains("q: quit"), "{wide}");
        assert!(wide.contains("p: profiles"), "{wide}");

        for width in [60, 80] {
            let narrow = fitted_text(view, width);
            assert!(
                Line::from(narrow.clone()).width() <= usize::from(width),
                "{narrow}"
            );
            assert!(narrow.starts_with(" PROCESSES "), "{narrow}");
            for essential in ["R: apply profile", "l: Logs", "Ctrl-F /: find"] {
                assert!(narrow.contains(essential), "{width}: {narrow}");
            }
            assert!(!narrow.contains("q: quit"), "{width}: {narrow}");
        }
        assert!(fitted_text(view, 80).ends_with("LIVE"));
    }

    #[test]
    fn follow_state_yields_only_when_essential_hints_overflow() {
        let view = ConsoleViewState {
            mode: ConsoleViewMode::Console,
            input_enabled: true,
            terminal_available: true,
            ..ConsoleViewState::default()
        };
        let essentials = " INPUT  Ctrl-A: Processes · Ctrl-A l: Logs";
        let fits = Line::from(essentials).width() as u16 + 6;
        assert!(fitted_text(view, fits).ends_with("LIVE"));
        assert!(!fitted_text(view, fits - 1).contains("LIVE"));
    }

    #[test]
    fn terminal_input_footer_always_names_the_exit_and_the_logs_view() {
        let view = ConsoleViewState {
            mode: ConsoleViewMode::Console,
            input_enabled: true,
            terminal_available: true,
            ..ConsoleViewState::default()
        };
        let narrow = fitted_text(view, 50);
        assert!(narrow.starts_with(" INPUT "), "{narrow}");
        assert!(narrow.contains("Ctrl-A: Processes"), "{narrow}");
        assert!(narrow.contains("Ctrl-A l: Logs"), "{narrow}");
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
                actions.begin_frame();
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
        let start = line[..line.find("s/x/r: lifecycle").unwrap()]
            .chars()
            .count() as u16;
        assert_eq!(
            actions.handle_mouse(&click(start, 11)),
            VisibleActionEvent::Changed
        );
        terminal
            .draw(|frame| {
                actions.begin_frame();
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
            actions.handle_mouse(&click(start + 2, restart_row)),
            VisibleActionEvent::Selected(VisibleAction::LifecycleRestart)
        );
    }
}
