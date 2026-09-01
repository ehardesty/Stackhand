//! Logs search and representation state used inside `ProcessLogs`.
//!
//! This implementation hides search editing, bounded result refresh, eviction
//! invalidation, match navigation, and the active Terminal/Logs representation.
//! The retained output module remains the source of text.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::output::{LogMatch, LogSearch, RetainedOutput};

const SEARCH_QUERY_BYTES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SearchDialogView {
    pub(crate) query: String,
    pub(crate) result: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutputRepresentation {
    Terminal,
    Logs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LogViewAction {
    Ignored,
    Changed,
    Pause,
    ShowMatch(LogMatch),
    Follow,
    Copy,
}

#[derive(Clone, Debug)]
pub(crate) struct LogView {
    representation: OutputRepresentation,
    editor: Option<String>,
    search_origin: Option<OutputRepresentation>,
    query: String,
    search: LogSearch,
    current: Option<usize>,
    generation: Option<u64>,
}

impl Default for LogView {
    fn default() -> Self {
        Self {
            representation: OutputRepresentation::Terminal,
            editor: None,
            search_origin: None,
            query: String::new(),
            search: LogSearch::default(),
            current: None,
            generation: None,
        }
    }
}

impl LogView {
    pub(crate) fn representation(&self, has_terminal: bool) -> OutputRepresentation {
        if has_terminal {
            self.representation
        } else {
            OutputRepresentation::Logs
        }
    }

    pub(crate) fn is_editing(&self) -> bool {
        self.editor.is_some()
    }

    pub(crate) fn current_match(&self) -> Option<LogMatch> {
        self.current
            .and_then(|index| self.search.matches.get(index).copied())
    }

    pub(crate) fn has_search(&self) -> bool {
        self.editor
            .as_ref()
            .map_or(!self.query.is_empty(), |editor| !editor.is_empty())
    }

    pub(crate) fn dialog(&self) -> Option<SearchDialogView> {
        self.editor.as_ref().map(|query| SearchDialogView {
            query: query.clone(),
            result: self.match_result(),
        })
    }

    /// Append a host paste to the active search field. Control characters do
    /// not become part of a literal query, and the same byte bound as typed
    /// input applies without splitting UTF-8 characters.
    pub(crate) fn paste_search(&mut self, text: &str, output: &RetainedOutput) -> LogViewAction {
        let Some(editor) = self.editor.as_mut() else {
            return LogViewAction::Ignored;
        };
        for character in text.chars().filter(|character| !character.is_control()) {
            if editor.len() + character.len_utf8() > SEARCH_QUERY_BYTES {
                break;
            }
            editor.push(character);
        }
        self.preview_search(output)
    }

    /// Handle one Logs command. Process-list focus always owns command keys.
    /// Logs focus also owns them because there is no child input to protect.
    pub(crate) fn handle_key(
        &mut self,
        key: KeyEvent,
        command_context: bool,
        has_terminal: bool,
        output: &RetainedOutput,
    ) -> LogViewAction {
        if key.kind != KeyEventKind::Press {
            return if self.editor.is_some() {
                LogViewAction::Changed
            } else {
                LogViewAction::Ignored
            };
        }
        if self.editor.is_some() {
            return self.edit_search(key, output);
        }
        let logs_active = self.representation(has_terminal) == OutputRepresentation::Logs;
        let opens_search = key.code == KeyCode::Char('/')
            || (key.code == KeyCode::Char('f')
                && key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT));
        if opens_search && (command_context || logs_active) {
            self.search_origin = Some(self.representation(has_terminal));
            self.representation = OutputRepresentation::Logs;
            self.editor = Some(String::new());
            self.search = LogSearch::default();
            self.current = None;
            return LogViewAction::Pause;
        }
        if (!command_context && !logs_active)
            || key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return LogViewAction::Ignored;
        }
        match key.code {
            KeyCode::Char('l') if has_terminal => {
                self.representation = match self.representation {
                    OutputRepresentation::Terminal => OutputRepresentation::Logs,
                    OutputRepresentation::Logs => OutputRepresentation::Terminal,
                };
                LogViewAction::Changed
            }
            KeyCode::Enter | KeyCode::F(3)
                if logs_active
                    && self.has_search()
                    && key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.move_match(-1, output)
            }
            KeyCode::Enter | KeyCode::F(3) if logs_active && self.has_search() => {
                self.move_match(1, output)
            }
            KeyCode::Char('n') if logs_active => self.move_match(1, output),
            KeyCode::Char('N') if logs_active => self.move_match(-1, output),
            KeyCode::Char('f')
                if self.representation(has_terminal) == OutputRepresentation::Logs =>
            {
                self.current = None;
                LogViewAction::Follow
            }
            KeyCode::Char('c' | 'y')
                if self.representation(has_terminal) == OutputRepresentation::Logs =>
            {
                LogViewAction::Copy
            }
            _ => LogViewAction::Ignored,
        }
    }

    pub(crate) fn refresh(&mut self, output: &RetainedOutput) -> bool {
        if self.generation == Some(output.generation) {
            return false;
        }
        let previous = self.current_match();
        self.generation = Some(output.generation);
        let query = self.editor.as_deref().unwrap_or(&self.query);
        self.search = output.search(query);
        self.current = previous.and_then(|wanted| {
            self.search
                .matches
                .iter()
                .position(|found| *found == wanted)
        });
        true
    }

    pub(crate) fn status(&self) -> Option<String> {
        if let Some(editor) = &self.editor {
            return Some(format!("Search: {editor}_"));
        }
        if self.query.is_empty() {
            return None;
        }
        Some(format!("Search: {} · {}", self.query, self.match_result()))
    }

    fn match_result(&self) -> String {
        let query = self.editor.as_deref().unwrap_or(&self.query);
        if query.is_empty() {
            return "Type a word or phrase".to_string();
        }
        let count = self.search.matches.len();
        if count == 0 {
            return "No matches".to_string();
        }
        let position = self.current.map_or(1, |index| index + 1);
        let suffix = if self.search.limited { "+" } else { "" };
        format!("Match {position} of {count}{suffix}")
    }

    fn edit_search(&mut self, key: KeyEvent, output: &RetainedOutput) -> LogViewAction {
        match key.code {
            KeyCode::Esc => self.cancel_search_edit(output),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cancel_search_edit(output)
            }
            KeyCode::Enter => {
                self.query = self.editor.take().unwrap_or_default();
                self.search_origin = None;
                self.rebuild_current_query(output)
            }
            KeyCode::Backspace => {
                self.editor.as_mut().expect("editor exists").pop();
                self.preview_search(output)
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let editor = self.editor.as_mut().expect("editor exists");
                if editor.len() + character.len_utf8() <= SEARCH_QUERY_BYTES {
                    editor.push(character);
                }
                self.preview_search(output)
            }
            _ => LogViewAction::Changed,
        }
    }

    fn preview_search(&mut self, output: &RetainedOutput) -> LogViewAction {
        self.rebuild_current_query(output)
    }

    fn cancel_search_edit(&mut self, output: &RetainedOutput) -> LogViewAction {
        self.editor = None;
        if let Some(origin) = self.search_origin.take() {
            self.representation = origin;
        }
        self.rebuild_current_query(output)
    }

    fn rebuild_current_query(&mut self, output: &RetainedOutput) -> LogViewAction {
        let query = self.editor.as_deref().unwrap_or(&self.query);
        self.generation = Some(output.generation);
        self.search = output.search(query);
        self.current = (!self.search.matches.is_empty()).then_some(0);
        self.current_match()
            .map(LogViewAction::ShowMatch)
            .unwrap_or(LogViewAction::Changed)
    }

    fn move_match(&mut self, direction: isize, output: &RetainedOutput) -> LogViewAction {
        self.refresh(output);
        if self.search.matches.is_empty() {
            self.current = None;
            return LogViewAction::Changed;
        }
        let len = self.search.matches.len();
        let current = self
            .current
            .unwrap_or(if direction < 0 { 0 } else { len - 1 });
        self.current = Some(if direction < 0 {
            current.checked_sub(1).unwrap_or(len - 1)
        } else {
            (current + 1) % len
        });
        LogViewAction::ShowMatch(self.current_match().expect("nonempty result"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::OutputViews;
    use crate::runtime::OutputStream;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn logs_focus_can_start_edit_and_cancel_search() {
        let output = OutputViews::new(1).for_process(0).unwrap().snapshot();
        let mut view = LogView::default();

        assert_eq!(
            view.handle_key(key(KeyCode::Char('/')), false, false, &output),
            LogViewAction::Pause
        );
        assert!(view.is_editing());
        assert_eq!(
            view.dialog(),
            Some(SearchDialogView {
                query: String::new(),
                result: "Type a word or phrase".to_string(),
            })
        );
        assert_eq!(view.status().as_deref(), Some("Search: _"));

        assert_eq!(
            view.handle_key(key(KeyCode::Esc), false, false, &output),
            LogViewAction::Changed
        );
        assert!(!view.is_editing());

        view.handle_key(key(KeyCode::Char('/')), false, false, &output);
        assert_eq!(
            view.handle_key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                false,
                false,
                &output,
            ),
            LogViewAction::Changed
        );
        assert!(!view.is_editing());
    }

    #[test]
    fn cancel_search_restores_the_terminal_view_that_opened_it() {
        let output = OutputViews::new(1).for_process(0).unwrap().snapshot();
        let mut view = LogView::default();

        assert_eq!(
            view.handle_key(key(KeyCode::Char('/')), true, true, &output),
            LogViewAction::Pause
        );
        assert_eq!(view.representation(true), OutputRepresentation::Logs);

        view.handle_key(key(KeyCode::Esc), true, true, &output);

        assert!(!view.is_editing());
        assert_eq!(view.representation(true), OutputRepresentation::Terminal);
    }

    #[test]
    fn terminal_focus_keeps_slash_for_the_child_until_logs_are_active() {
        let output = OutputViews::new(1).for_process(0).unwrap().snapshot();
        let mut view = LogView::default();

        assert_eq!(
            view.handle_key(key(KeyCode::Char('/')), false, true, &output),
            LogViewAction::Ignored
        );
        view.handle_key(key(KeyCode::Char('l')), true, true, &output);
        assert_eq!(view.representation(true), OutputRepresentation::Logs);
        assert_eq!(
            view.handle_key(key(KeyCode::Char('/')), false, true, &output),
            LogViewAction::Pause
        );
        assert!(view.is_editing());
        view.handle_key(key(KeyCode::Esc), false, true, &output);

        assert_eq!(
            view.handle_key(key(KeyCode::Char('l')), false, true, &output),
            LogViewAction::Changed
        );
        assert_eq!(view.representation(true), OutputRepresentation::Terminal);
    }

    #[test]
    fn ctrl_f_opens_search_in_logs_but_stays_child_input_in_terminal() {
        let output = OutputViews::new(1).for_process(0).unwrap().snapshot();
        let ctrl_f = KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL);
        let mut terminal = LogView::default();
        assert_eq!(
            terminal.handle_key(ctrl_f, false, true, &output),
            LogViewAction::Ignored
        );

        let mut logs = LogView::default();
        assert_eq!(
            logs.handle_key(ctrl_f, true, false, &output),
            LogViewAction::Pause
        );
        assert!(logs.is_editing());
    }

    #[test]
    fn search_paste_filters_controls_and_preserves_utf8_at_the_byte_limit() {
        let output = OutputViews::new(1).for_process(0).unwrap().snapshot();
        let mut view = LogView::default();
        view.handle_key(key(KeyCode::Char('/')), true, false, &output);

        assert!(matches!(
            view.paste_search("one\ntwo €", &output),
            LogViewAction::Changed
        ));
        assert_eq!(
            view.dialog().map(|dialog| dialog.query),
            Some("onetwo €".to_string())
        );
        assert!(matches!(
            view.paste_search(&"x".repeat(SEARCH_QUERY_BYTES), &output),
            LogViewAction::Changed
        ));
        let editor = view.editor.as_ref().unwrap();
        assert!(editor.len() <= SEARCH_QUERY_BYTES);
        assert!(editor.is_char_boundary(editor.len()));
    }

    #[test]
    fn search_switches_to_logs_and_navigates_literal_matches() {
        let output = OutputViews::new(1).for_process(0).unwrap();
        output.append_at(1, OutputStream::Stdout, 0, b"hit\nmiss\nhit\n".to_vec());
        let snapshot = output.snapshot();
        let mut view = LogView::default();

        assert_eq!(
            view.handle_key(key(KeyCode::Char('/')), true, true, &snapshot),
            LogViewAction::Pause
        );
        view.handle_key(key(KeyCode::Char('h')), true, true, &snapshot);
        assert!(
            view.current_match().is_some(),
            "search must highlight and navigate while the query is typed"
        );
        view.handle_key(key(KeyCode::Char('i')), true, true, &snapshot);
        view.handle_key(key(KeyCode::Char('t')), true, true, &snapshot);
        assert!(matches!(
            view.handle_key(key(KeyCode::Enter), true, true, &snapshot),
            LogViewAction::ShowMatch(_)
        ));
        assert_eq!(view.representation(true), OutputRepresentation::Logs);
        assert_eq!(view.status().as_deref(), Some("Search: hit · Match 1 of 2"));
        view.handle_key(key(KeyCode::Enter), true, true, &snapshot);
        assert_eq!(view.status().as_deref(), Some("Search: hit · Match 2 of 2"));
        view.handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            true,
            true,
            &snapshot,
        );
        assert_eq!(view.status().as_deref(), Some("Search: hit · Match 1 of 2"));
    }

    #[test]
    fn eviction_clears_a_match_that_no_longer_exists() {
        let output = OutputViews::new(1).for_process(0).unwrap();
        output.append_at(1, OutputStream::Stdout, 0, b"needle\n".to_vec());
        let mut view = LogView::default();
        let first = output.snapshot();
        view.handle_key(key(KeyCode::Char('/')), true, true, &first);
        for character in "needle".chars() {
            view.handle_key(key(KeyCode::Char(character)), true, true, &first);
        }
        view.handle_key(key(KeyCode::Enter), true, true, &first);
        output.append_at(
            1,
            OutputStream::Stdout,
            1,
            vec![b'x'; crate::output::RETAINED_BYTES],
        );
        let later = output.snapshot();
        assert!(view.refresh(&later));
        assert!(view.current_match().is_none());
        assert!(view.status().unwrap().contains("No matches"));
    }
}
