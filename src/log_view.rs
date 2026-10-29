//! Logs search, filter, and representation state used inside `ProcessLogs`.
//!
//! This implementation hides prompt editing, bounded search and filter
//! refresh, eviction invalidation, match navigation, and the active
//! Terminal/Logs representation. The retained output module remains the
//! source of text; the filter decides which retained lines are shown.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::output::{
    FilterCache, FilteredLines, FilteredOutput, LineFilter, LogMatch, LogSearch, RetainedOutput,
};

const PROMPT_BYTES: usize = 256;

/// What an edited Logs prompt changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PromptKind {
    /// Highlight and visit literal matches.
    Search,
    /// Hide retained lines that do not match.
    Filter,
}

/// The Logs prompt being edited and its live result summary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LogsPrompt {
    pub(crate) kind: PromptKind,
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
    /// The shown lines changed, so a held viewport anchor may be hidden.
    Refiltered,
    Follow,
    Copy,
}

#[derive(Clone, Copy, Debug)]
enum MatchSelection {
    Inactive,
    AwaitingMatch,
    Selected(usize),
}

impl MatchSelection {
    fn index(self) -> Option<usize> {
        match self {
            Self::Selected(index) => Some(index),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
struct Prompt {
    kind: PromptKind,
    text: String,
}

#[derive(Clone, Debug)]
pub(crate) struct LogView {
    representation: OutputRepresentation,
    prompt: Option<Prompt>,
    prompt_origin: Option<OutputRepresentation>,
    query: String,
    filter: String,
    search: LogSearch,
    current: MatchSelection,
    generation: Option<u64>,
    cache: crate::output::SearchCache,
    filter_cache: FilterCache,
    filtered: Option<FilteredLines>,
}

impl Default for LogView {
    fn default() -> Self {
        Self {
            representation: OutputRepresentation::Terminal,
            prompt: None,
            prompt_origin: None,
            query: String::new(),
            filter: String::new(),
            search: LogSearch::default(),
            current: MatchSelection::Inactive,
            generation: None,
            cache: Default::default(),
            filter_cache: Default::default(),
            filtered: None,
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

    /// Switch between Terminal and Logs for a Process that has a terminal.
    pub(crate) fn toggle_representation(&mut self) {
        self.representation = match self.representation {
            OutputRepresentation::Terminal => OutputRepresentation::Logs,
            OutputRepresentation::Logs => OutputRepresentation::Terminal,
        };
    }

    pub(crate) fn is_editing(&self) -> bool {
        self.prompt.is_some()
    }

    pub(crate) fn current_match(&self) -> Option<LogMatch> {
        self.current
            .index()
            .and_then(|index| self.search.matches.get(index).copied())
    }

    pub(crate) fn follow(&mut self) {
        self.current = MatchSelection::Inactive;
    }

    /// The retained lines the viewport shows through the active filter.
    pub(crate) fn shown<'a>(&'a self, output: &'a RetainedOutput) -> FilteredOutput<'a> {
        FilteredOutput {
            output,
            filtered: self.filtered.as_ref(),
        }
    }

    /// A filter hides lines, including one being edited.
    pub(crate) fn is_filtered(&self) -> bool {
        self.filtered.is_some()
    }

    pub(crate) fn has_search(&self) -> bool {
        !self.search_query().is_empty()
    }

    /// A committed search found matches that `n` and `N` can visit.
    pub(crate) fn has_matches(&self) -> bool {
        self.prompt.is_none() && !self.query.is_empty() && !self.search.matches.is_empty()
    }

    pub(crate) fn prompt(&self) -> Option<LogsPrompt> {
        let prompt = self.prompt.as_ref()?;
        Some(LogsPrompt {
            kind: prompt.kind,
            query: prompt.text.clone(),
            result: match prompt.kind {
                PromptKind::Search => self.match_result(),
                PromptKind::Filter => self.filter_result(),
            },
        })
    }

    /// Append a host paste to the edited prompt. Control characters do not
    /// become part of a literal query, and the same byte bound as typed
    /// input applies without splitting UTF-8 characters.
    pub(crate) fn paste_prompt(&mut self, text: &str, output: &RetainedOutput) -> LogViewAction {
        let Some(prompt) = self.prompt.as_mut() else {
            return LogViewAction::Ignored;
        };
        for character in text.chars().filter(|character| !character.is_control()) {
            if prompt.text.len() + character.len_utf8() > PROMPT_BYTES {
                break;
            }
            prompt.text.push(character);
        }
        self.preview(output)
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
            return if self.prompt.is_some() {
                LogViewAction::Changed
            } else {
                LogViewAction::Ignored
            };
        }
        if self.prompt.is_some() {
            return self.edit_prompt(key, output);
        }
        let logs_active = self.representation(has_terminal) == OutputRepresentation::Logs;
        if let Some(kind) = prompt_key(key)
            && (command_context || logs_active)
        {
            self.prompt_origin = Some(self.representation(has_terminal));
            self.representation = OutputRepresentation::Logs;
            return self.open_prompt(kind, false, output);
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
                self.toggle_representation();
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
                self.follow();
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
        let pending =
            self.search.pending || self.filtered.as_ref().is_some_and(FilteredLines::pending);
        if self.generation == Some(output.generation) && !pending {
            return false;
        }
        self.update_results(output);
        true
    }

    /// Recompute results for new output or a changed filter, keeping the
    /// selected match while it is still found.
    fn update_results(&mut self, output: &RetainedOutput) {
        let previous = self.current_match();
        self.recompute(output);
        self.current = match self.current {
            MatchSelection::Inactive => MatchSelection::Inactive,
            MatchSelection::AwaitingMatch if !self.search.matches.is_empty() => {
                MatchSelection::Selected(0)
            }
            MatchSelection::AwaitingMatch => MatchSelection::AwaitingMatch,
            MatchSelection::Selected(_) => previous
                .and_then(|wanted| {
                    self.search
                        .matches
                        .iter()
                        .position(|found| *found == wanted)
                })
                .map_or(MatchSelection::AwaitingMatch, MatchSelection::Selected),
        };
    }

    /// Committed search and filter parts for the Logs border. An edited
    /// prompt shows its own result instead.
    pub(crate) fn status(&self) -> Vec<String> {
        let editing = self.prompt.as_ref().map(|prompt| prompt.kind);
        let mut parts = Vec::new();
        if editing != Some(PromptKind::Search) && !self.query.is_empty() {
            parts.push(format!("/{} {}", self.query, self.match_result()));
        }
        if editing != Some(PromptKind::Filter) && self.filtered.is_some() {
            parts.push(format!("filter {} · {}", self.filter, self.filter_result()));
        }
        parts
    }

    fn search_query(&self) -> &str {
        active_text(&self.prompt, PromptKind::Search, &self.query)
    }

    fn match_result(&self) -> String {
        if self.search_query().is_empty() {
            return "type to search".to_string();
        }
        let count = self.search.matches.len();
        if count == 0 {
            return if self.search.pending {
                "searching older Logs…".into()
            } else {
                "no matches".into()
            };
        }
        let position = self.current.index().map_or(1, |index| index + 1);
        let suffix = if self.search.limited {
            " newest"
        } else if self.search.pending {
            " · searching older Logs…"
        } else {
            ""
        };
        format!("{position} of {count}{suffix}")
    }

    fn filter_result(&self) -> String {
        let Some(filtered) = &self.filtered else {
            return "terms, !exclude".to_string();
        };
        let matched = filtered.matched();
        let lines = if matched == 1 { "line" } else { "lines" };
        if filtered.pending() {
            format!("{matched} {lines} · filtering older Logs…")
        } else {
            format!("{matched} {lines} shown")
        }
    }

    /// Open a prompt. A filter starts from the committed filter. A search
    /// starts empty, as in `less` and Vim, unless `keep_search` carries the
    /// committed query over from the filter prompt.
    fn open_prompt(
        &mut self,
        kind: PromptKind,
        keep_search: bool,
        output: &RetainedOutput,
    ) -> LogViewAction {
        let text = match kind {
            PromptKind::Search if keep_search => self.query.clone(),
            PromptKind::Search => String::new(),
            PromptKind::Filter => self.filter.clone(),
        };
        self.prompt = Some(Prompt { kind, text });
        match kind {
            PromptKind::Search if keep_search => self.rebuild_search(output),
            PromptKind::Search => {
                self.search = LogSearch::default();
                self.current = MatchSelection::Inactive;
                LogViewAction::Pause
            }
            PromptKind::Filter => LogViewAction::Changed,
        }
    }

    fn edit_prompt(&mut self, key: KeyEvent, output: &RetainedOutput) -> LogViewAction {
        let prompt = self.prompt.as_mut().expect("the prompt is open");
        match key.code {
            KeyCode::Esc => self.close_prompt(false, output),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.close_prompt(false, output)
            }
            KeyCode::Enter => self.close_prompt(true, output),
            // Tab keeps what was typed and moves to the other prompt, so a
            // filter and a search can be set in one pass.
            KeyCode::Tab => {
                let kind = prompt.kind;
                let text = std::mem::take(&mut prompt.text);
                self.prompt = None;
                match kind {
                    PromptKind::Search => {
                        self.query = text;
                        self.rebuild_search(output);
                        self.open_prompt(PromptKind::Filter, false, output)
                    }
                    PromptKind::Filter => {
                        self.filter = text;
                        self.update_results(output);
                        self.open_prompt(PromptKind::Search, true, output);
                        LogViewAction::Refiltered
                    }
                }
            }
            KeyCode::Backspace => {
                prompt.text.pop();
                self.preview(output)
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                prompt.text.clear();
                self.preview(output)
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if prompt.text.len() + character.len_utf8() <= PROMPT_BYTES {
                    prompt.text.push(character);
                }
                self.preview(output)
            }
            _ => LogViewAction::Changed,
        }
    }

    /// Close the prompt. Committing keeps its text; cancelling restores the
    /// committed query and the representation that opened the prompt.
    fn close_prompt(&mut self, commit: bool, output: &RetainedOutput) -> LogViewAction {
        let prompt = self.prompt.take().expect("the prompt is open");
        let origin = self.prompt_origin.take();
        if commit {
            match prompt.kind {
                PromptKind::Search => self.query = prompt.text,
                PromptKind::Filter => self.filter = prompt.text,
            }
        } else if let Some(origin) = origin {
            self.representation = origin;
        }
        match prompt.kind {
            PromptKind::Search => self.rebuild_search(output),
            PromptKind::Filter => {
                self.update_results(output);
                LogViewAction::Refiltered
            }
        }
    }

    fn preview(&mut self, output: &RetainedOutput) -> LogViewAction {
        match self.prompt.as_ref().map(|prompt| prompt.kind) {
            Some(PromptKind::Filter) => {
                self.update_results(output);
                LogViewAction::Refiltered
            }
            _ => self.rebuild_search(output),
        }
    }

    /// Recompute the filter and the search over the lines it shows.
    fn recompute(&mut self, output: &RetainedOutput) {
        self.generation = Some(output.generation);
        let filter = LineFilter::parse(active_text(&self.prompt, PromptKind::Filter, &self.filter));
        let query = active_text(&self.prompt, PromptKind::Search, &self.query);
        self.search = self.cache.search_shown(output, query, filter.as_ref());
        self.filtered = filter.map(|filter| self.filter_cache.project(output, &filter));
    }

    fn rebuild_search(&mut self, output: &RetainedOutput) -> LogViewAction {
        self.recompute(output);
        self.current = if self.search_query().is_empty() {
            MatchSelection::Inactive
        } else if self.search.matches.is_empty() {
            MatchSelection::AwaitingMatch
        } else {
            MatchSelection::Selected(0)
        };
        self.current_match()
            .map(LogViewAction::ShowMatch)
            .unwrap_or(LogViewAction::Changed)
    }

    fn move_match(&mut self, direction: isize, output: &RetainedOutput) -> LogViewAction {
        self.refresh(output);
        if self.search.matches.is_empty() {
            self.current = if self.has_search() {
                MatchSelection::AwaitingMatch
            } else {
                MatchSelection::Inactive
            };
            return LogViewAction::Changed;
        }
        let len = self.search.matches.len();
        let current = self
            .current
            .index()
            .unwrap_or(if direction < 0 { 0 } else { len - 1 });
        self.current = MatchSelection::Selected(if direction < 0 {
            current.checked_sub(1).unwrap_or(len - 1)
        } else {
            (current + 1) % len
        });
        LogViewAction::ShowMatch(self.current_match().expect("nonempty result"))
    }
}

/// The edited prompt text of `kind`, or the committed text.
fn active_text<'a>(prompt: &'a Option<Prompt>, kind: PromptKind, committed: &'a str) -> &'a str {
    match prompt {
        Some(prompt) if prompt.kind == kind => &prompt.text,
        _ => committed,
    }
}

/// The prompt a key opens. Search follows editors and `less`: Ctrl-F or
/// `/`. Filter follows `less` (`&`), htop (`\`, F4), and grep (Ctrl-G).
fn prompt_key(key: KeyEvent) -> Option<PromptKind> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Char('/') => Some(PromptKind::Search),
        KeyCode::Char('f') if ctrl && !alt => Some(PromptKind::Search),
        KeyCode::Char('g') if ctrl && !alt => Some(PromptKind::Filter),
        KeyCode::Char('&' | '\\') if !ctrl && !alt => Some(PromptKind::Filter),
        KeyCode::F(4) => Some(PromptKind::Filter),
        _ => None,
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
            view.prompt(),
            Some(LogsPrompt {
                kind: PromptKind::Search,
                query: String::new(),
                result: "type to search".to_string(),
            })
        );
        assert!(
            view.status().is_empty(),
            "the prompt shows an edited search"
        );

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
            view.paste_prompt("one\ntwo €", &output),
            LogViewAction::Changed
        ));
        assert_eq!(
            view.prompt().map(|prompt| prompt.query),
            Some("onetwo €".to_string())
        );
        assert!(matches!(
            view.paste_prompt(&"x".repeat(PROMPT_BYTES), &output),
            LogViewAction::Changed
        ));
        let editor = &view.prompt.as_ref().unwrap().text;
        assert!(editor.len() <= PROMPT_BYTES);
        assert!(editor.is_char_boundary(editor.len()));
    }

    #[test]
    fn search_switches_to_logs_and_navigates_literal_matches() {
        let output = OutputViews::new(1).for_process(0).unwrap();
        output.append_at(1, OutputStream::Stdout, 0, b"hit\nmiss\nhit\n");
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
        assert_eq!(view.status(), ["/hit 1 of 2"]);
        view.handle_key(key(KeyCode::Enter), true, true, &snapshot);
        assert_eq!(view.status(), ["/hit 2 of 2"]);
        view.handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            true,
            true,
            &snapshot,
        );
        assert_eq!(view.status(), ["/hit 1 of 2"]);
    }

    #[test]
    fn eviction_clears_a_match_that_no_longer_exists() {
        let output = OutputViews::with_settings(
            1,
            crate::output_settings::OutputSettings {
                per_process_history_bytes: 128 * 1024,
                global_history_bytes: 1024 * 1024,
                ..Default::default()
            },
        )
        .for_process(0)
        .unwrap();
        output.append_at(1, OutputStream::Stdout, 0, b"needle\n");
        let mut view = LogView::default();
        let first = output.snapshot();
        view.handle_key(key(KeyCode::Char('/')), true, true, &first);
        for character in "needle".chars() {
            view.handle_key(key(KeyCode::Char(character)), true, true, &first);
        }
        view.handle_key(key(KeyCode::Enter), true, true, &first);
        output.append_at(1, OutputStream::Stdout, 1, &vec![b'x'; 128 * 1024]);
        let later = output.snapshot();
        assert!(view.refresh(&later));
        assert!(view.current_match().is_none());
        assert!(view.status()[0].contains("no matches"));
    }
}
