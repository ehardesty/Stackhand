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
use ratatui::widgets::TableState;

use super::view_model::profile_changes_pending;

use crate::console::{ConsoleInteraction, LifecycleCommand};
use crate::log_view::OutputRepresentation;
use crate::output::RetainedOutput;
use crate::process_logs::{LogsInput, ProcessLogs};
use crate::supervisor::{
    Command, ConsoleView, Consoles, Lifecycle, ProcessSnapshot, ProjectSnapshot,
};
use crate::terminal::{OwnedTerminalSnapshot, TerminalEvent};
use crate::tui::{
    ConsolePaneKind, ConsoleViewState, ConsoleWarning, PipeLine, pane_inner, process_row_at,
    project_layout,
};

/// The selected Process's visible output owner.
pub(crate) enum SelectedPane<'a> {
    Terminal(ConsoleView),
    Logs(&'a RetainedOutput),
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
    process_table: TableState,
    logs: Vec<ProcessLogs>,
    project_commands: Vec<Command>,
    profile_changes_pending: bool,
    start_anyway_available: bool,
    truncation: Option<(usize, bool)>,
}

impl ProjectInteraction {
    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    pub(super) fn process_table_state(&mut self) -> &mut TableState {
        &mut self.process_table
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
        self.profile_changes_pending = profile_changes_pending(snapshot);
        self.selected = self
            .selected
            .min(snapshot.processes.len().saturating_sub(1));
        self.logs
            .resize_with(snapshot.processes.len(), ProcessLogs::default);
        let changed = self
            .console
            .apply_selection_moves(&mut self.selected, snapshot.processes.len());
        let process = &snapshot.processes[self.selected];
        self.start_anyway_available = process.lifecycle == Lifecycle::Waiting;
        let mut commands = std::mem::take(&mut self.project_commands);
        commands.extend(
            self.console
                .take_lifecycle_commands()
                .into_iter()
                .map(|request| match request {
                    LifecycleCommand::Start => Command::Start(process.name.clone()),
                    LifecycleCommand::Stop => Command::Stop(process.name.clone()),
                    LifecycleCommand::Restart
                        if process.kind == crate::model::ProcessKind::OneShot =>
                    {
                        Command::Rerun(process.name.clone())
                    }
                    LifecycleCommand::Restart => Command::Restart(process.name.clone()),
                }),
        );
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
            return SelectedPane::Logs(retained);
        };
        consoles
            .view_process(process.process_id, run_id)
            .map(SelectedPane::Terminal)
            .unwrap_or(SelectedPane::Logs(retained))
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
    ) -> InteractionFrame {
        let representation = match pane {
            SelectedPane::Terminal(_) => OutputRepresentation::Terminal,
            SelectedPane::Logs(_) => OutputRepresentation::Logs,
        };
        let terminal = match pane {
            SelectedPane::Terminal(view) => view.snapshot(),
            SelectedPane::Logs(_) => None,
        };
        let mut view = self.console.view();
        view.profile_changes_pending = self.profile_changes_pending;
        view.start_anyway_available = self.start_anyway_available;
        let (lines, logs_status, logs_editing) = match pane {
            SelectedPane::Logs(_) => {
                let frame = self.logs[self.selected].frame(retained, pane_rows);
                view.following = frame.following;
                view.search_editing = frame.editing;
                view.search_active = frame.search_active;
                view.logs_selection = frame.has_selection;
                view.logs_scrollbar = frame.scrollbar;
                (Some(frame.lines), frame.status, frame.editing)
            }
            SelectedPane::Terminal(_) => (None, None, false),
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
        if let Event::Key(key) = event
            && key.kind == KeyEventKind::Press
            && self.console.view().mode == crate::tui::ConsoleViewMode::ProcessList
            && !key.modifiers.intersects(
                crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::ALT,
            )
        {
            let command = match key.code {
                KeyCode::Char('p') => Some(Command::SelectNextProcessProfile),
                KeyCode::Char('R') if self.profile_changes_pending => {
                    Some(Command::RestartProfiledAutostart)
                }
                KeyCode::Char('S') if process.lifecycle == Lifecycle::Waiting => {
                    Some(Command::StartAnyway(process.name.clone()))
                }
                _ => None,
            };
            if let Some(command) = command {
                self.project_commands.push(command);
                self.console.clear_warning();
                return InputResult::Changed;
            }
        }
        match event {
            Event::Key(key) => self.route_key(key, pane, process, retained, console_area.height),
            Event::Paste(data) if self.logs[self.selected].is_search_editing() => {
                self.console.clear_warning();
                if self.logs[self.selected].paste_search(&data, retained) {
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
                    SelectedPane::Terminal(_) | SelectedPane::Logs(_) => {
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
        // Use the pane that is actually rendered. A PTY Process with no active
        // Run falls back to retained Logs, so treating it as Terminal here
        // would route Logs copy keys to the read-only navigation handler.
        let has_terminal = matches!(pane, SelectedPane::Terminal(_));
        match self.logs[self.selected].handle_key(
            key,
            self.console.view().mode == crate::tui::ConsoleViewMode::ProcessList,
            has_terminal,
            retained,
            usize::from(pane_rows.max(1)),
        ) {
            LogsInput::Changed => {
                self.console.clear_warning();
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
            SelectedPane::Logs(_) => self.console.route_pane_key(
                ConsolePaneKind::Pipe,
                process.input_focused,
                key,
                None,
                &mut self.logs[self.selected],
                pane_rows.max(1),
            ),
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
                    SelectedPane::Logs(_) => self.console.focus_process_list(None),
                }
            }
            let previous = self.selected;
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(index) =
                        process_row_at(list, mouse.row, process_count, self.process_table.offset())
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
            current_profile: None,
            next_profile: None,
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
            selected_profile: None,
            available_profiles: Vec::new(),
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

    #[test]
    fn profile_keys_queue_global_selection_and_pending_apply_commands() {
        let mut snapshot = ProjectSnapshot {
            selected_profile: Some("local".to_string()),
            available_profiles: vec!["cloud-dev".to_string(), "local".to_string()],
            processes: vec![process("api", crate::model::ProcessKind::Service, 0)],
            now_ms: 0,
            shutdown: None,
        };
        let output = crate::output::OutputViews::new(1);
        let retained = output.for_process(0).unwrap().snapshot();
        let pane = SelectedPane::Logs(&retained);
        let mut interaction = ProjectInteraction::default();
        interaction.update_project(&snapshot);

        assert_eq!(
            interaction.route_input(
                key(KeyCode::Char('p')),
                1,
                &pane,
                &snapshot.processes[0],
                &retained,
                Rect::new(0, 0, 80, 20),
            ),
            InputResult::Changed
        );
        assert_eq!(
            interaction.update_project(&snapshot).commands,
            vec![Command::SelectNextProcessProfile]
        );

        snapshot.processes[0].current_run = Some(1);
        snapshot.processes[0].current_profile = Some("local".to_string());
        snapshot.processes[0].next_profile = Some("cloud-dev".to_string());
        interaction.update_project(&snapshot);
        assert_eq!(
            interaction.route_input(
                key(KeyCode::Char('R')),
                1,
                &pane,
                &snapshot.processes[0],
                &retained,
                Rect::new(0, 0, 80, 20),
            ),
            InputResult::Changed
        );
        assert_eq!(
            interaction.update_project(&snapshot).commands,
            vec![Command::RestartProfiledAutostart]
        );
    }

    #[test]
    fn start_anyway_is_available_only_for_the_selected_waiting_process() {
        let mut snapshot = ProjectSnapshot {
            selected_profile: None,
            available_profiles: Vec::new(),
            processes: vec![process("api", crate::model::ProcessKind::Service, 0)],
            now_ms: 0,
            shutdown: None,
        };
        let output = crate::output::OutputViews::new(1);
        let retained = output.for_process(0).unwrap().snapshot();
        let pane = SelectedPane::Logs(&retained);
        let mut interaction = ProjectInteraction::default();
        interaction.update_project(&snapshot);

        assert_eq!(
            interaction.route_input(
                key(KeyCode::Char('S')),
                1,
                &pane,
                &snapshot.processes[0],
                &retained,
                Rect::new(0, 0, 80, 20),
            ),
            InputResult::Ignored
        );

        snapshot.processes[0].lifecycle = Lifecycle::Waiting;
        snapshot.processes[0].blocked_reason = Some("db: ready".to_string());
        interaction.update_project(&snapshot);
        assert_eq!(
            interaction.route_input(
                key(KeyCode::Char('S')),
                1,
                &pane,
                &snapshot.processes[0],
                &retained,
                Rect::new(0, 0, 80, 20),
            ),
            InputResult::Changed
        );
        assert_eq!(
            interaction.update_project(&snapshot).commands,
            vec![Command::StartAnyway("api".to_string())]
        );
    }

    #[test]
    fn retained_logs_are_labeled_as_logs_when_a_terminal_is_not_available() {
        let output = crate::output::OutputViews::new(1);
        let retained = output.for_process(0).unwrap().snapshot();
        let pane = SelectedPane::Logs(&retained);
        let mut interaction = ProjectInteraction {
            logs: vec![ProcessLogs::default()],
            ..Default::default()
        };

        let frame = interaction.frame(&pane, &retained, 20);

        assert_eq!(frame.representation, OutputRepresentation::Logs);
    }

    #[test]
    fn pty_logs_fallback_routes_copy_to_the_logs_handler() {
        let output = crate::output::OutputViews::new(1);
        output.for_process(0).unwrap().append_at(
            1,
            crate::runtime::OutputStream::Stdout,
            0,
            b"quadrant log\n".to_vec(),
        );
        let retained = output.for_process(0).unwrap().snapshot();
        let mut process = process("api", crate::model::ProcessKind::Service, 0);
        process.terminal_mode = crate::model::TerminalMode::Pty;
        process.current_run = None;
        let pane = SelectedPane::Logs(&retained);
        let mut interaction = ProjectInteraction {
            logs: vec![ProcessLogs::default()],
            ..Default::default()
        };
        interaction.console.focus_console(None);

        assert_eq!(
            interaction.route_input(
                key(KeyCode::Char('c')),
                1,
                &pane,
                &process,
                &retained,
                Rect::new(0, 0, 80, 20),
            ),
            InputResult::Changed
        );
        assert_ne!(
            interaction.console.view().warning,
            Some(ConsoleWarning::LogsCommandOnly),
            "copying visible PTY Logs must not be routed as child input"
        );
    }

    #[test]
    fn logs_frame_reports_the_current_follow_state() {
        let output = crate::output::OutputViews::new(1);
        output.for_process(0).unwrap().append_at(
            1,
            crate::runtime::OutputStream::Stdout,
            0,
            (0..30)
                .map(|line| format!("line-{line}\n"))
                .collect::<String>()
                .into_bytes(),
        );
        let retained = output.for_process(0).unwrap().snapshot();
        let pane = SelectedPane::Logs(&retained);
        let mut interaction = ProjectInteraction {
            logs: vec![ProcessLogs::default()],
            ..Default::default()
        };

        interaction.logs[0].scroll_page(20, -1);
        assert!(!interaction.frame(&pane, &retained, 20).view.following);

        interaction.logs[0].scroll_page(20, 1);
        assert!(interaction.frame(&pane, &retained, 20).view.following);
    }
}
