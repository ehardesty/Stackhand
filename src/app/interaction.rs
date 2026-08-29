//! Interactive Project state.
//!
//! This module owns the state that changes together when the user interacts
//! with the selected Process: focus, warnings, Process selection, and every
//! per-Process Logs view. The event loop supplies immutable Supervisor and
//! output facts and performs the returned Supervisor commands.

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;

use crate::console::{ConsoleInteraction, LifecycleCommand};
use crate::log_view::OutputRepresentation;
use crate::output::RetainedOutput;
use crate::process_logs::{LogsInput, ProcessLogs};
use crate::supervisor::{Command, ConsoleView, Consoles, ProcessSnapshot, ProjectSnapshot};
use crate::terminal::{OwnedTerminalSnapshot, TerminalEvent};
use crate::tui::{
    ConsolePaneKind, ConsoleViewState, ConsoleWarning, PipeLine, pane_inner, process_row_at,
    project_layout,
};

/// The selected Process's visible output owner.
pub(crate) enum SelectedPane<'a> {
    Terminal(ConsoleView),
    Logs(&'a RetainedOutput),
    Empty,
}

/// One result from routing a terminal input event through application state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InputResult {
    Ignored,
    Changed,
    Quit,
}

/// Immutable values needed to render one frame.
pub(super) struct InteractionFrame {
    pub(super) terminal: Option<OwnedTerminalSnapshot>,
    pub(super) lines: Option<Vec<PipeLine>>,
    pub(super) logs_status: Option<String>,
    pub(super) logs_editing: bool,
    pub(super) representation: OutputRepresentation,
    pub(super) view: ConsoleViewState,
}

/// Effects produced when queued list commands are applied to a snapshot.
pub(crate) struct ProjectUpdate {
    pub(super) changed: bool,
    pub(crate) commands: Vec<Command>,
}

#[derive(Default)]
pub(crate) struct ProjectInteraction {
    console: ConsoleInteraction,
    selected: usize,
    logs: Vec<ProcessLogs>,
    truncation: Option<(usize, bool)>,
}

impl ProjectInteraction {
    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    pub(super) fn poll_requests(&mut self) -> bool {
        self.console.poll_requests()
    }

    pub(super) fn warn(&mut self, warning: ConsoleWarning) {
        self.console.warn(warning);
    }

    /// Reconcile queued user commands with the latest immutable Project
    /// snapshot. Selection and Process commands are applied in one place.
    pub(crate) fn update_project(&mut self, snapshot: &ProjectSnapshot) -> ProjectUpdate {
        self.selected = self
            .selected
            .min(snapshot.processes.len().saturating_sub(1));
        self.logs
            .resize_with(snapshot.processes.len(), ProcessLogs::default);
        let changed = self
            .console
            .apply_selection_moves(&mut self.selected, snapshot.processes.len());
        let process = &snapshot.processes[self.selected];
        let commands = self
            .console
            .take_lifecycle_commands()
            .into_iter()
            .map(|request| match request {
                LifecycleCommand::Start => Command::Start(process.name.clone()),
                LifecycleCommand::Stop => Command::Stop(process.name.clone()),
                LifecycleCommand::Restart if process.kind == crate::model::ProcessKind::OneShot => {
                    Command::Rerun(process.name.clone())
                }
                LifecycleCommand::Restart => Command::Restart(process.name.clone()),
            })
            .collect();
        ProjectUpdate { changed, commands }
    }

    pub(crate) fn refresh_logs(&mut self, output: &RetainedOutput) -> bool {
        self.logs[self.selected].refresh(output)
    }

    pub(crate) fn pane<'a>(
        &self,
        consoles: &Consoles,
        process: &ProcessSnapshot,
        retained: &'a RetainedOutput,
    ) -> SelectedPane<'a> {
        let representation = self.logs[self.selected]
            .representation(process.terminal_mode == crate::model::TerminalMode::Pty);
        if representation == OutputRepresentation::Logs
            || process.terminal_mode == crate::model::TerminalMode::Pipe
        {
            return SelectedPane::Logs(retained);
        }
        let Some(run_id) = process.current_run else {
            return SelectedPane::Empty;
        };
        consoles
            .view_process(process.process_id, run_id)
            .map(SelectedPane::Terminal)
            .unwrap_or(SelectedPane::Empty)
    }

    /// Apply output-owner facts and project the selected pane into the shared
    /// focus state. This also emits a truncation warning once per transition.
    pub(crate) fn update_pane(&mut self, pane: &SelectedPane<'_>) -> bool {
        let mut changed = false;
        match pane {
            SelectedPane::Terminal(view) => {
                self.truncation = None;
                while let Some(event) = view.poll_event() {
                    match event {
                        TerminalEvent::Failed(_) => {
                            self.console.warn(ConsoleWarning::InputRejected)
                        }
                        TerminalEvent::InputBackpressure { .. } => {
                            self.console.warn(ConsoleWarning::InputBackpressure);
                            changed = true;
                        }
                        TerminalEvent::OutputTruncated => {
                            self.console.warn(ConsoleWarning::OutputTruncated);
                            changed = true;
                        }
                        TerminalEvent::Exited | TerminalEvent::StateChanged => changed = true,
                    }
                }
                self.console.set_pane(ConsolePaneKind::Terminal);
            }
            SelectedPane::Logs(retained) => {
                let (index, was) = self.truncation.take().unwrap_or((usize::MAX, false));
                if retained.truncated && (index != self.selected || !was) {
                    self.console.warn(ConsoleWarning::OutputTruncated);
                }
                self.truncation = Some((self.selected, retained.truncated));
                self.console.set_pane(ConsolePaneKind::Pipe);
                self.console
                    .set_following(self.logs[self.selected].following());
            }
            SelectedPane::Empty => {
                self.truncation = None;
                self.console.set_pane(ConsolePaneKind::Empty);
            }
        }
        changed
    }

    pub(super) fn terminal_is_dirty(pane: &SelectedPane<'_>) -> bool {
        matches!(pane, SelectedPane::Terminal(view) if view.is_dirty())
    }

    pub(super) fn frame(
        &mut self,
        pane: &SelectedPane<'_>,
        retained: &RetainedOutput,
        pane_rows: usize,
        process_has_terminal: bool,
    ) -> InteractionFrame {
        let representation = self.logs[self.selected].representation(process_has_terminal);
        let terminal = match pane {
            SelectedPane::Terminal(view) => view.snapshot(),
            SelectedPane::Logs(_) | SelectedPane::Empty => None,
        };
        let mut view = self.console.view();
        let (lines, logs_status, logs_editing) = match pane {
            SelectedPane::Logs(_) => {
                let frame = self.logs[self.selected].frame(retained, pane_rows);
                self.console.set_following(frame.following);
                view.search_editing = frame.editing;
                view.search_active = frame.search_active;
                view.logs_selection = frame.has_selection;
                view.logs_scrollbar = frame.scrollbar;
                (Some(frame.lines), frame.status, frame.editing)
            }
            SelectedPane::Terminal(_) | SelectedPane::Empty => (None, None, false),
        };
        InteractionFrame {
            terminal,
            lines,
            logs_status,
            logs_editing,
            representation,
            view,
        }
    }

    pub(crate) fn route_input(
        &mut self,
        event: Event,
        repeats: usize,
        pane: &SelectedPane<'_>,
        process: &ProcessSnapshot,
        retained: &RetainedOutput,
        console_area: Rect,
    ) -> InputResult {
        if let Event::Key(key) = event
            && should_quit(
                key,
                self.console.view(),
                self.logs[self.selected].is_search_editing(),
            )
        {
            return InputResult::Quit;
        }
        match event {
            Event::Key(key) => self.route_key(key, pane, process, retained, console_area.height),
            Event::Paste(data) if self.logs[self.selected].is_search_editing() => {
                self.console.clear_warning();
                if self.logs[self.selected].paste_search(&data, retained) {
                    self.console
                        .set_following(self.logs[self.selected].following());
                    InputResult::Changed
                } else {
                    InputResult::Ignored
                }
            }
            Event::Paste(data) => {
                match pane {
                    SelectedPane::Terminal(view)
                        if self.console.accepts_child_input(process.input_focused) =>
                    {
                        view.with(|session| self.console.handle_paste(&data, session));
                    }
                    SelectedPane::Terminal(_) if !process.input_focused => {
                        self.console.warn(ConsoleWarning::InputDisabled);
                    }
                    SelectedPane::Terminal(_) | SelectedPane::Logs(_) | SelectedPane::Empty => {
                        self.console.warn(ConsoleWarning::PasteRejected);
                    }
                }
                InputResult::Changed
            }
            Event::FocusGained | Event::FocusLost => {
                let SelectedPane::Terminal(view) = pane else {
                    return InputResult::Ignored;
                };
                let gained = matches!(event, Event::FocusGained);
                let delivered = if process.input_focused {
                    view.with(|session| session.send_focus(gained).is_ok())
                        .unwrap_or(false)
                } else {
                    self.console.warn(ConsoleWarning::InputDisabled);
                    false
                };
                if !delivered && process.input_focused {
                    self.console.warn(ConsoleWarning::InputRejected);
                }
                if delivered {
                    InputResult::Ignored
                } else {
                    InputResult::Changed
                }
            }
            Event::Mouse(mouse) => {
                if self.route_mouse(mouse, pane, process, repeats) {
                    InputResult::Changed
                } else {
                    InputResult::Ignored
                }
            }
            Event::Resize(_, _) => InputResult::Ignored,
        }
    }

    fn route_key(
        &mut self,
        key: KeyEvent,
        pane: &SelectedPane<'_>,
        process: &ProcessSnapshot,
        retained: &RetainedOutput,
        pane_rows: u16,
    ) -> InputResult {
        let has_terminal = process.terminal_mode == crate::model::TerminalMode::Pty;
        match self.logs[self.selected].handle_key(
            key,
            self.console.view().mode == crate::tui::ConsoleViewMode::ProcessList,
            has_terminal,
            retained,
            usize::from(pane_rows.max(1)),
        ) {
            LogsInput::Changed => {
                self.console.clear_warning();
                self.console
                    .set_following(self.logs[self.selected].following());
                return InputResult::Changed;
            }
            LogsInput::Copy(text) => {
                self.console.clear_warning();
                self.console.copy_logs(text);
                return InputResult::Changed;
            }
            LogsInput::Ignored => {}
        }
        let changed = match pane {
            SelectedPane::Terminal(view) => view
                .with(|session| {
                    self.console.route_pane_key(
                        ConsolePaneKind::Terminal,
                        process.input_focused,
                        key,
                        Some(session),
                        &mut self.logs[self.selected],
                        pane_rows.max(1),
                    )
                })
                .unwrap_or(false),
            SelectedPane::Logs(_) | SelectedPane::Empty => {
                let kind = if matches!(pane, SelectedPane::Logs(_)) {
                    ConsolePaneKind::Pipe
                } else {
                    ConsolePaneKind::Empty
                };
                self.console.route_pane_key(
                    kind,
                    process.input_focused,
                    key,
                    None,
                    &mut self.logs[self.selected],
                    pane_rows.max(1),
                )
            }
        };
        if changed {
            InputResult::Changed
        } else {
            InputResult::Ignored
        }
    }

    fn route_mouse(
        &mut self,
        mouse: MouseEvent,
        pane: &SelectedPane<'_>,
        process: &ProcessSnapshot,
        repeats: usize,
    ) -> bool {
        let process_count = self.logs.len();
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let (list, console_outer, _) = project_layout(Rect::new(0, 0, cols, rows), process_count);
        let console_inner = pane_inner(console_outer);
        let child_tracks_mouse = process.input_focused
            && matches!(pane, SelectedPane::Terminal(view) if view.mouse_tracking());

        if matches!(pane, SelectedPane::Logs(_))
            && self.logs[self.selected].scrollbar_gesture_active()
            && matches!(mouse.kind, MouseEventKind::Drag(_) | MouseEventKind::Up(_))
            && let SelectedPane::Logs(retained) = pane
        {
            return self.console.handle_read_only_mouse(
                mouse,
                console_inner,
                repeats,
                &mut self.logs[self.selected],
                retained,
            );
        }
        if self.console.mouse_gesture_active()
            && matches!(mouse.kind, MouseEventKind::Drag(_) | MouseEventKind::Up(_))
            && let SelectedPane::Terminal(view) = pane
        {
            return view
                .with(|session| {
                    (0..repeats).fold(false, |changed, _| {
                        self.console
                            .handle_mouse(mouse, console_inner, child_tracks_mouse, session)
                            || changed
                    })
                })
                .unwrap_or(false);
        }
        if rect_contains(list, mouse.column, mouse.row) {
            if mouse_changes_focus(mouse.kind) {
                match pane {
                    SelectedPane::Terminal(view) => {
                        view.with(|session| self.console.focus_process_list(Some(session)));
                    }
                    SelectedPane::Logs(_) | SelectedPane::Empty => {
                        self.console.focus_process_list(None)
                    }
                }
            }
            let previous = self.selected;
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(index) =
                        process_row_at(list, mouse.row, process_count, self.selected)
                    {
                        self.selected = index;
                        self.console.clear_pane_warning();
                    }
                }
                MouseEventKind::ScrollUp => self.selected = self.selected.saturating_sub(repeats),
                MouseEventKind::ScrollDown => {
                    self.selected = self
                        .selected
                        .saturating_add(repeats)
                        .min(process_count.saturating_sub(1));
                }
                _ => {}
            }
            return previous != self.selected || matches!(mouse.kind, MouseEventKind::Down(_));
        }
        if !rect_contains(console_outer, mouse.column, mouse.row) {
            return false;
        }
        match pane {
            SelectedPane::Terminal(view) => view
                .with(|session| {
                    if mouse_starts_console_focus(mouse.kind, self.console.view().mode) {
                        self.console.focus_console(Some(session));
                    }
                    (0..repeats).fold(false, |changed, _| {
                        self.console
                            .handle_mouse(mouse, console_inner, child_tracks_mouse, session)
                            || changed
                    })
                })
                .unwrap_or(false),
            SelectedPane::Logs(retained) => {
                if mouse_starts_console_focus(mouse.kind, self.console.view().mode) {
                    self.console.focus_console(None);
                }
                self.console.handle_read_only_mouse(
                    mouse,
                    console_inner,
                    repeats,
                    &mut self.logs[self.selected],
                    retained,
                ) || mouse_changes_focus(mouse.kind)
            }
            SelectedPane::Empty => {
                if mouse_starts_console_focus(mouse.kind, self.console.view().mode) {
                    self.console.focus_console(None);
                }
                mouse_changes_focus(mouse.kind)
            }
        }
    }
}

pub(super) fn should_quit(key: KeyEvent, view: ConsoleViewState, search_editing: bool) -> bool {
    key.kind == KeyEventKind::Press
        && is_quit(key, view)
        && (!search_editing
            || key
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL))
}

pub(super) fn is_quit(key: KeyEvent, view: ConsoleViewState) -> bool {
    key.code == KeyCode::Char('q')
        && (key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL)
            || (view.mode == crate::tui::ConsoleViewMode::ProcessList && key.modifiers.is_empty()))
}

pub(super) fn mouse_changes_focus(kind: MouseEventKind) -> bool {
    matches!(kind, MouseEventKind::Down(_))
}

pub(super) fn mouse_starts_console_focus(
    kind: MouseEventKind,
    mode: crate::tui::ConsoleViewMode,
) -> bool {
    mouse_changes_focus(kind) && mode == crate::tui::ConsoleViewMode::ProcessList
}

fn rect_contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyEvent, KeyModifiers};

    use super::*;
    use crate::supervisor::{DesiredState, Lifecycle, ProcessId, RestartBudgetStatus};

    fn process(name: &str, kind: crate::model::ProcessKind, process_id: u32) -> ProcessSnapshot {
        ProcessSnapshot {
            process_id: ProcessId::new(process_id),
            name: name.to_string(),
            kind,
            enabled: true,
            autostart: true,
            input_focused: false,
            desired: DesiredState::Running,
            lifecycle: Lifecycle::Running,
            terminal_mode: crate::model::TerminalMode::Pipe,
            current_run: Some(1),
            root_pid: None,
            run_started_at_ms: Some(0),
            failure: None,
            metrics: None,
            blocked_reason: None,
            readiness: None,
            liveness: None,
            restart_backoff: None,
            automatic_restart_budget: RestartBudgetStatus {
                automatic_retries_used: 0,
                max_restarts: 5,
                exhausted: false,
            },
            recent_runs: Vec::new(),
        }
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn selection_and_lifecycle_commands_cross_one_state_seam() {
        let snapshot = ProjectSnapshot {
            processes: vec![
                process("api", crate::model::ProcessKind::Service, 0),
                process("setup", crate::model::ProcessKind::OneShot, 1),
            ],
            now_ms: 0,
            shutdown: None,
        };
        let output = crate::output::OutputViews::new(2);
        let first = output.for_process(0).unwrap().snapshot();
        let second = output.for_process(1).unwrap().snapshot();
        let mut interaction = ProjectInteraction::default();
        interaction.update_project(&snapshot);

        assert_eq!(
            interaction.route_input(
                key(KeyCode::Char('j')),
                1,
                &SelectedPane::Logs(&first),
                &snapshot.processes[0],
                &first,
                Rect::new(0, 0, 80, 20),
            ),
            InputResult::Changed
        );
        let update = interaction.update_project(&snapshot);
        assert!(update.changed);
        assert_eq!(interaction.selected(), 1);

        interaction.route_input(
            key(KeyCode::Char('r')),
            1,
            &SelectedPane::Logs(&second),
            &snapshot.processes[1],
            &second,
            Rect::new(0, 0, 80, 20),
        );
        let update = interaction.update_project(&snapshot);
        assert_eq!(update.commands, vec![Command::Rerun("setup".to_string())]);
    }
}
