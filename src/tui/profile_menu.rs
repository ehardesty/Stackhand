use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::{Frame, layout::Rect};
use ratatui_interact::{
    components::{Select, SelectAction, SelectState, handle_select_key, handle_select_mouse},
    traits::ClickRegion,
};

/// The result of an interaction with the Project Profile menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProjectProfileMenuAction {
    Ignored,
    Changed,
    Selected(Option<String>),
}

/// State and interaction boundary for the Project Profile menu.
///
/// The application only handles `ProjectProfileMenuAction`; it does not need
/// to know the ratatui-interact state or action types.
pub(crate) struct ProjectProfileMenuState {
    select_state: SelectState,
    option_values: Vec<Option<String>>,
    base_profile_name: String,
    trigger: Rect,
    dropdown_regions: Vec<ClickRegion<SelectAction>>,
}

pub(crate) type ProjectProfileMenu = ProjectProfileMenuState;

impl Default for ProjectProfileMenuState {
    fn default() -> Self {
        Self::new()
    }
}

impl ProjectProfileMenuState {
    pub(crate) fn new() -> Self {
        Self {
            select_state: SelectState::default(),
            option_values: Vec::new(),
            base_profile_name: String::new(),
            trigger: Rect::default(),
            dropdown_regions: Vec::new(),
        }
    }

    /// Rebuild the menu while keeping the selected profile and highlight aligned.
    pub(crate) fn sync<I>(
        &mut self,
        base_profile_name: &str,
        available_profiles: I,
        selected_profile: Option<&str>,
    ) where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        let requested = selected_profile.map(str::to_owned);
        let option_values = std::iter::once(None)
            .chain(
                available_profiles
                    .into_iter()
                    .map(|name| Some(name.as_ref().to_owned())),
            )
            .collect::<Vec<_>>();
        let selected_index = requested
            .as_deref()
            .and_then(|name| {
                option_values
                    .iter()
                    .position(|value| value.as_deref() == Some(name))
            })
            .unwrap_or(0);

        // Preserve the user's highlighted option between event-loop snapshots.
        if self.base_profile_name == base_profile_name
            && self.option_values == option_values
            && self.select_state.selected_index == Some(selected_index)
        {
            return;
        }

        let was_open = self.select_state.is_open;
        let was_focused = self.select_state.focused;
        let was_enabled = self.select_state.enabled;
        self.base_profile_name = base_profile_name.to_owned();
        self.option_values = option_values;
        self.select_state = SelectState::new(self.option_values.len());
        self.select_state.is_open = was_open;
        self.select_state.focused = was_focused;
        self.select_state.enabled = was_enabled;
        self.select_state.selected_index = Some(selected_index);
        self.select_state.highlighted_index = selected_index;
    }

    pub(crate) fn toggle(&mut self) {
        self.select_state.toggle();
    }

    pub(crate) fn close(&mut self) {
        self.select_state.close();
    }

    pub(crate) fn is_open(&self) -> bool {
        self.select_state.is_open
    }

    pub(crate) fn handle_key(&mut self, key: &KeyEvent) -> ProjectProfileMenuAction {
        if matches!(key.code, KeyCode::Char('p') | KeyCode::Char('P')) {
            let was_open = self.is_open();
            self.toggle();
            return if was_open != self.is_open() {
                ProjectProfileMenuAction::Changed
            } else {
                ProjectProfileMenuAction::Ignored
            };
        }

        let code = match key.code {
            KeyCode::Char('j') => KeyCode::Down,
            KeyCode::Char('k') => KeyCode::Up,
            code => code,
        };
        let translated = KeyEvent::new(code, key.modifiers);
        let previous_highlight = self.select_state.highlighted_index;
        let action = handle_select_key(&translated, &mut self.select_state);

        if let Some(action) = action {
            return self.map_select_action(action);
        }
        if previous_highlight != self.select_state.highlighted_index {
            ProjectProfileMenuAction::Changed
        } else {
            ProjectProfileMenuAction::Ignored
        }
    }

    pub(crate) fn handle_mouse(&mut self, mouse: &MouseEvent) -> ProjectProfileMenuAction {
        handle_select_mouse(
            mouse,
            &mut self.select_state,
            self.trigger,
            &self.dropdown_regions,
        )
        .map_or(ProjectProfileMenuAction::Ignored, |action| {
            self.map_select_action(action)
        })
    }

    /// Render only the dropdown overlay. The trigger itself is rendered elsewhere.
    pub(crate) fn render(&mut self, frame: &mut Frame, trigger: Rect, screen: Rect) {
        self.trigger = trigger;
        let labels: Vec<String> = self
            .option_values
            .iter()
            .map(|value| {
                value
                    .as_deref()
                    .unwrap_or(&self.base_profile_name)
                    .to_owned()
            })
            .collect();
        self.dropdown_regions = if self.is_open() {
            Select::new(&labels, &self.select_state).render_dropdown(frame, trigger, screen)
        } else {
            Vec::new()
        };
    }

    fn map_select_action(&self, action: SelectAction) -> ProjectProfileMenuAction {
        match action {
            SelectAction::Select(index) => self.option_values.get(index).cloned().map_or(
                ProjectProfileMenuAction::Ignored,
                ProjectProfileMenuAction::Selected,
            ),
            SelectAction::Open | SelectAction::Close | SelectAction::Focus => {
                ProjectProfileMenuAction::Changed
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn sync_puts_base_first_and_maps_named_selection() {
        let mut menu = ProjectProfileMenuState::new();
        let profiles = vec!["dev".to_owned(), "prod".to_owned()];

        menu.sync("base", &profiles, Some("prod"));

        assert_eq!(
            menu.option_values,
            vec![None, Some("dev".to_owned()), Some("prod".to_owned())]
        );
        assert_eq!(menu.select_state.selected_index, Some(2));
        assert_eq!(menu.select_state.highlighted_index, 2);
    }

    #[test]
    fn keyboard_navigation_and_selection_are_translated() {
        let mut menu = ProjectProfileMenuState::new();
        menu.sync("base", ["dev", "prod"], None);

        assert_eq!(
            menu.handle_key(&KeyEvent::from(KeyCode::Char('p'))),
            ProjectProfileMenuAction::Changed
        );
        assert_eq!(
            menu.handle_key(&KeyEvent::from(KeyCode::Char('j'))),
            ProjectProfileMenuAction::Changed
        );
        // The app syncs each fresh Supervisor snapshot before routing the next
        // input batch. Unchanged data must not reset the highlighted option.
        menu.sync("base", ["dev", "prod"], None);
        assert_eq!(menu.select_state.selected_index, Some(0));
        assert_eq!(
            menu.handle_key(&KeyEvent::from(KeyCode::Enter)),
            ProjectProfileMenuAction::Selected(Some("dev".to_owned()))
        );
        assert!(!menu.is_open());
    }

    #[test]
    fn render_saves_option_regions_for_mouse_selection() {
        let mut menu = ProjectProfileMenuState::new();
        menu.sync("base", ["dev", "prod"], None);
        let trigger = Rect::new(2, 1, 12, 3);
        let screen = Rect::new(0, 0, 40, 20);
        menu.trigger = trigger;
        let click = |column, row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };

        assert_eq!(
            menu.handle_mouse(&click(2, 1)),
            ProjectProfileMenuAction::Changed
        );
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        terminal
            .draw(|frame| menu.render(frame, trigger, screen))
            .unwrap();
        assert_eq!(menu.dropdown_regions.len(), 3);

        assert_eq!(
            menu.handle_mouse(&click(3, 6)),
            ProjectProfileMenuAction::Selected(Some("dev".to_owned()))
        );
        assert_eq!(
            menu.handle_mouse(&click(2, 1)),
            ProjectProfileMenuAction::Changed
        );
        assert_eq!(
            menu.handle_mouse(&click(2, 1)),
            ProjectProfileMenuAction::Changed
        );
        assert!(!menu.is_open());
    }
}
