use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};
use ratatui_interact::{
    components::{Select, SelectAction, SelectState, handle_select_key, handle_select_mouse},
    traits::ClickRegion,
};

use unicode_width::UnicodeWidthChar;

use crate::log_view::{LogsPrompt, PromptKind};

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
    FilterEdit,
    FocusConsole,
    FocusProcesses,
    Follow,
    LifecycleStart,
    LifecycleStop,
    LifecycleRestart,
    OpenLifecycleMenu,
    OpenProfiles,
    /// Commit the edited prompt and open the other one.
    PromptSwitch,
    Quit,
    PromptCancel,
    SearchEdit,
    SearchNext,
    SearchPrevious,
    PromptSubmit,
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
    lifecycle_state: SelectState,
    lifecycle_trigger: Rect,
    lifecycle_regions: Vec<ClickRegion<SelectAction>>,
}

impl Default for VisibleActions {
    fn default() -> Self {
        Self {
            regions: Vec::new(),
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
        VisibleActionEvent::Ignored
    }

    /// Forget the previous frame's regions. Call once before rendering any
    /// visible action.
    pub(crate) fn begin_frame(&mut self) {
        self.regions.clear();
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
        segments.push(tab(
            if view.logs_filtered {
                "Logs (filtered)"
            } else {
                "Logs"
            },
            logs_active,
        ));
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

    /// Render the footer. While a search is edited, the footer is the
    /// search prompt and owns the terminal cursor, so no dialog covers the
    /// output that the search highlights.
    pub(crate) fn render_footer(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        screen: Rect,
        view: ConsoleViewState,
        child_mouse_tracking: bool,
        prompt: Option<&LogsPrompt>,
    ) {
        let (segments, cursor) = footer_layout(view, child_mouse_tracking, prompt, area.width);
        let line = self.action_line(
            area,
            segments,
            TERMINAL_THEME.footer(view.warning.is_some()),
        );
        frame.render_widget(Paragraph::new(line), area);
        if let Some(x) = cursor
            .and_then(|offset| u16::try_from(offset).ok())
            .map(|offset| area.x.saturating_add(offset))
            .filter(|x| *x < area.right())
        {
            frame.set_cursor_position((x, area.y));
        }

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
/// clipped rather than hidden. The follow state yields after Useful hints
/// and before Important ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Priority {
    Essential,
    Important,
    Useful,
    Extra,
}

use Priority::{Essential, Extra, Important, Useful};

impl Hint {
    fn styled(text: impl Into<String>, style: Style, priority: Priority) -> Self {
        Self {
            segment: Segment::styled(text, style),
            priority,
        }
    }

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
    /// The first hint is an edited search query that owns the cursor.
    prompt: bool,
}

const SEPARATOR: &str = " · ";
const TRAILING_GAP: usize = 2;

impl FooterContent {
    /// Drop the least important hints until the content fits. Equal
    /// priorities drop from the end, so the visible order never changes.
    fn fit(mut self, width: u16) -> Self {
        while self.width() > usize::from(width) {
            let drop = self
                .hints
                .iter()
                .enumerate()
                .filter(|(_, hint)| hint.priority > Essential)
                .max_by_key(|(index, hint)| (hint.priority, *index));
            match drop {
                Some((index, hint)) if hint.priority >= Useful || self.trailing.is_none() => {
                    self.hints.remove(index);
                }
                _ if self.trailing.is_some() => self.trailing = None,
                _ => break,
            }
        }
        self
    }

    /// The cursor column after the edited query, relative to the footer:
    /// the width of the badge, its gap, and the query hint.
    fn prompt_cursor(&self) -> Option<usize> {
        self.prompt
            .then(|| segments_width(&self.leading_segments()[..3]))
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

/// Shorten `text` to `width` display cells, starting with `…` when cut.
fn fit_text_end(text: &str, width: usize) -> String {
    if text_width(text) <= width {
        return text.to_owned();
    }
    let mut kept = Vec::new();
    let mut used = 1;
    for character in text.chars().rev() {
        let cells = character.width().unwrap_or(0);
        if used + cells > width {
            break;
        }
        kept.push(character);
        used += cells;
    }
    std::iter::once('…').chain(kept.into_iter().rev()).collect()
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
    footer_layout(view, child_mouse_tracking, None, 400)
        .0
        .into_iter()
        .map(|segment| segment.text)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The fitted footer segments and, while a search is edited, the cursor
/// column relative to the footer.
fn footer_layout(
    view: ConsoleViewState,
    child_mouse_tracking: bool,
    prompt: Option<&LogsPrompt>,
    width: u16,
) -> (Vec<Segment>, Option<usize>) {
    let content = footer_content(view, child_mouse_tracking, prompt, width).fit(width);
    let cursor = content.prompt_cursor();
    (content.into_segments(width), cursor)
}

const PROMPT_EXIT: &str = "Esc: cancel";

fn footer_content(
    view: ConsoleViewState,
    child_mouse_tracking: bool,
    prompt: Option<&LogsPrompt>,
    width: u16,
) -> FooterContent {
    let (badge, tone) = mode_badge(view, prompt.map(|prompt| prompt.kind));
    let mut content = FooterContent {
        badge,
        tone,
        hints: Vec::new(),
        trailing: None,
        prompt: prompt.is_some(),
    };
    if let Some(prompt) = prompt {
        // A long query shows its end so the cursor and the exit stay visible.
        let room = usize::from(width).saturating_sub(
            text_width(badge) + 3 + text_width(SEPARATOR) + text_width(PROMPT_EXIT) + 1,
        );
        let (sigil, other) = match prompt.kind {
            PromptKind::Search => ('/', "Tab: filter"),
            PromptKind::Filter => ('&', "Tab: search"),
        };
        let query = fit_text_end(&format!("{sigil}{}", prompt.query), room);
        content.hints = vec![
            Hint::styled(query, TERMINAL_THEME.prompt(), Essential),
            Hint::text(prompt.result.clone(), Useful),
            Hint::action(other, VisibleAction::PromptSwitch, Useful),
            Hint::action("Enter: done", VisibleAction::PromptSubmit, Extra),
            Hint::action(PROMPT_EXIT, VisibleAction::PromptCancel, Essential),
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
                "Ctrl-A /: search Logs",
                VisibleAction::SearchEdit,
                Useful,
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
fn mode_badge(view: ConsoleViewState, prompt: Option<PromptKind>) -> (&'static str, ModeTone) {
    match prompt {
        Some(PromptKind::Search) => return ("SEARCH", ModeTone::Modal),
        Some(PromptKind::Filter) => return ("FILTER", ModeTone::Modal),
        None => {}
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
            "WARNING: Logs is read-only",
            &[
                ("Ctrl-F /: search", VisibleAction::SearchEdit),
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
        Important,
    ));
    append_view_toggle(hints, view);
    hints.push(Hint::action(
        "Tab: console",
        VisibleAction::FocusConsole,
        Useful,
    ));
    append_search_controls(hints, view);
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
    append_search_controls(hints, view);
    append_view_toggle(hints, view);
    hints.push(Hint::action(
        "Esc: Processes",
        VisibleAction::FocusProcesses,
        Essential,
    ));
    if !view.following {
        hints.push(Hint::action("f: live", VisibleAction::Follow, Important));
    }
    hints.push(Hint::action("c/y: copy", VisibleAction::Copy, Extra));
}

fn append_search_controls(hints: &mut Vec<Hint>, view: ConsoleViewState) {
    hints.push(Hint::action(
        "Ctrl-F /: search",
        VisibleAction::SearchEdit,
        if view.match_navigation {
            Useful
        } else {
            Essential
        },
    ));
    hints.push(Hint::action(
        "Ctrl-G &: filter",
        VisibleAction::FilterEdit,
        Important,
    ));
    if view.match_navigation {
        hints.push(Hint::action(
            "n/F3: next",
            VisibleAction::SearchNext,
            Essential,
        ));
        hints.push(Hint::action(
            "N/⇧F3: previous",
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
                    None,
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
        footer_layout(view, false, None, width)
            .0
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
            for essential in ["R: apply profile", "l: Logs", "Ctrl-F /: search"] {
                assert!(narrow.contains(essential), "{width}: {narrow}");
            }
            assert!(!narrow.contains("q: quit"), "{width}: {narrow}");
        }
        let eighty = fitted_text(
            ConsoleViewState {
                terminal_available: true,
                ..ConsoleViewState::default()
            },
            80,
        );
        for important in ["s/x/r: lifecycle", "Ctrl-G &: filter"] {
            assert!(eighty.contains(important), "{eighty}");
        }
        assert!(fitted_text(view, 120).ends_with("LIVE"));
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
        let eighty = fitted_text(view, 80);
        assert!(eighty.contains("Ctrl-A /: search Logs"), "{eighty}");
    }

    #[test]
    fn logs_prompt_replaces_footer_hints_and_owns_the_cursor() {
        let mut actions = VisibleActions::default();
        let mut terminal = Terminal::new(TestBackend::new(60, 3)).unwrap();
        let prompt = LogsPrompt {
            kind: PromptKind::Search,
            query: "timeout".to_string(),
            result: "2/11".to_string(),
        };
        let view = ConsoleViewState {
            warning: Some(ConsoleWarning::LogsCommandOnly),
            ..ConsoleViewState::default()
        };
        terminal
            .draw(|frame| {
                actions.begin_frame();
                actions.render_footer(
                    frame,
                    Rect::new(0, 2, 60, 1),
                    frame.area(),
                    view,
                    false,
                    Some(&prompt),
                );
            })
            .unwrap();
        let line = (0..60)
            .map(|column| terminal.backend().buffer()[(column, 2)].symbol())
            .collect::<String>();
        assert!(line.starts_with(" SEARCH  /timeout · 2/11"), "{line}");
        assert!(!line.contains("WARNING"), "{line}");
        let query_end = line.find("/timeout").unwrap() + "/timeout".len();
        terminal
            .backend_mut()
            .assert_cursor_position((query_end as u16, 2));

        let cancel = line[..line.find("Esc: cancel").unwrap()].chars().count() as u16;
        assert_eq!(
            actions.handle_mouse(&click(cancel + 1, 2)),
            VisibleActionEvent::Selected(VisibleAction::PromptCancel)
        );
    }

    #[test]
    fn a_long_search_query_shows_its_end_and_keeps_the_exit_visible() {
        let prompt = LogsPrompt {
            kind: PromptKind::Search,
            query: "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJ".to_string(),
            result: "no matches".to_string(),
        };
        let (segments, cursor) =
            footer_layout(ConsoleViewState::default(), false, Some(&prompt), 50);
        let line: String = segments.into_iter().map(|segment| segment.text).collect();

        assert!(Line::from(line.clone()).width() <= 50, "{line}");
        assert!(
            line.contains(" …") && line.contains("ABCDEFGHIJ · "),
            "{line}"
        );
        assert!(line.ends_with("Esc: cancel"), "{line}");
        let query_end = line.find('J').unwrap() + 1;
        assert_eq!(cursor, Some(line[..query_end].chars().count()));
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
                    None,
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
                    None,
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
