use std::io;
use std::os::unix::net::UnixStream;

use super::*;
use crate::geometry::TerminalGeometry;
use crate::runtime::PtyIo;
use crate::terminal::TerminalSession;

fn session() -> (TerminalSession, UnixStream) {
    let (reader, peer) = UnixStream::pair().unwrap();
    let session = TerminalSession::spawn(
        PtyIo {
            reader: Box::new(reader),
            writer: Box::new(io::sink()),
            resizer: Box::new(|_, _| Ok(())),
        },
        TerminalGeometry::DEFAULT,
        || {},
    )
    .unwrap();
    (session, peer)
}

#[test]
fn command_modes_share_key_meanings_without_sharing_pane_effects() {
    for mode in [ConsoleViewMode::AppCommand, ConsoleViewMode::Scroll] {
        assert_eq!(
            console_command(mode, KeyCode::PageUp),
            Some(ConsoleCommand::ScrollPage(-1))
        );
        assert_eq!(
            console_command(mode, KeyCode::PageDown),
            Some(ConsoleCommand::ScrollPage(1))
        );
        assert_eq!(
            console_command(mode, KeyCode::Char('f')),
            Some(ConsoleCommand::Follow)
        );
        assert_eq!(
            console_command(mode, KeyCode::Char('x')),
            Some(ConsoleCommand::Lifecycle(LifecycleCommand::Stop))
        );
        assert_eq!(
            console_command(mode, KeyCode::Char('v')),
            Some(ConsoleCommand::EnterSelection)
        );
        assert_eq!(
            console_command(mode, KeyCode::Esc),
            Some(ConsoleCommand::Escape)
        );
    }
    assert_eq!(
        console_command(ConsoleViewMode::AppCommand, KeyCode::Char('j')),
        Some(ConsoleCommand::MoveSelection(SelectionMove::Down))
    );
    assert_eq!(
        console_command(ConsoleViewMode::Scroll, KeyCode::Char('j')),
        None
    );
}

#[test]
fn applying_selection_moves_clamps_and_preserves_process_scroll() {
    let mut interaction = ConsoleInteraction::default();
    interaction.warn(ConsoleWarning::PipeReadOnly);
    interaction.selection_requests = vec![SelectionMove::Up, SelectionMove::Down];
    let mut selected = 0;
    let mut scroll = PipeScroll::default();
    scroll.scroll_page(20, -1);

    assert!(interaction.apply_selection_moves(&mut selected, 2));
    assert_eq!(selected, 1);
    assert_eq!(scroll.offset(), 19);
    assert_eq!(interaction.view().warning, None);
    assert_eq!(interaction.take_selection_moves(), Vec::new());

    interaction.selection_requests = vec![SelectionMove::Down];
    assert!(interaction.apply_selection_moves(&mut selected, 2));
    assert_eq!(selected, 1, "selection clamps at the Project end");
}

#[test]
fn scroll_navigation_stops_following_and_f_returns_to_live_tail() {
    let (session, peer) = session();
    let stopped = std::sync::atomic::AtomicBool::new(false);
    let handle = crate::runtime::handle_for_test(&session, &stopped);
    let mut interaction = ConsoleInteraction::default();

    assert!(interaction.handle_key(
        KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
        &handle,
        20,
    ));
    assert!(interaction.handle_key(
        KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
        &handle,
        20,
    ));
    assert_eq!(interaction.view().mode, ConsoleViewMode::Scroll);
    assert!(!interaction.view().following);

    assert!(interaction.handle_key(
        KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE),
        &handle,
        20,
    ));
    assert_eq!(interaction.view().mode, ConsoleViewMode::ChildInput);
    assert!(interaction.view().following);

    drop(peer);
    session.shutdown().unwrap();
}

#[test]
fn clipboard_failure_is_a_visible_warning_not_a_terminal_failure() {
    let warning = copy_warning(Ok(Some("selected".to_string())), |_| {
        Err(anyhow::anyhow!("clipboard unavailable"))
    });

    assert_eq!(warning, Some(ConsoleWarning::ClipboardFailed));
}

#[test]
fn empty_selection_does_not_call_the_clipboard() {
    let warning = copy_warning(Ok(None), |_| panic!("clipboard must not be called"));

    assert_eq!(warning, Some(ConsoleWarning::NothingSelected));
}

#[test]
fn app_command_j_k_and_arrows_queue_selection_moves_without_touching_the_child() {
    let (session, peer) = session();
    let stopped = std::sync::atomic::AtomicBool::new(false);
    let handle = crate::runtime::handle_for_test(&session, &stopped);
    let mut interaction = ConsoleInteraction::default();

    assert!(interaction.handle_key(
        KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
        &handle,
        20,
    ));
    assert_eq!(interaction.take_selection_moves(), Vec::new());

    for (key, expected) in [
        (KeyCode::Down, SelectionMove::Down),
        (KeyCode::Char('j'), SelectionMove::Down),
        (KeyCode::Up, SelectionMove::Up),
        (KeyCode::Char('k'), SelectionMove::Up),
    ] {
        assert!(interaction.handle_key(KeyEvent::new(key, KeyModifiers::NONE), &handle, 20));
        assert_eq!(interaction.take_selection_moves(), vec![expected]);
        assert_eq!(interaction.view().mode, ConsoleViewMode::AppCommand);
    }
    assert_eq!(interaction.take_selection_moves(), Vec::new());

    drop(peer);
    session.shutdown().unwrap();
}

#[test]
fn child_input_gate_rejects_only_disabled_terminal_child_input() {
    let plain = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
    let repeat = KeyEvent {
        kind: KeyEventKind::Repeat,
        ..plain
    };
    let release = KeyEvent {
        kind: KeyEventKind::Release,
        ..plain
    };
    let leader = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
    let leader_repeat = KeyEvent {
        kind: KeyEventKind::Repeat,
        ..leader
    };
    let default = ConsoleViewState::default();

    // Disabled input in child-input mode rejects keys of every kind.
    assert!(child_input_rejected(default, false, &plain));
    assert!(child_input_rejected(default, false, &repeat));
    assert!(child_input_rejected(default, false, &release));
    // Enabled focused input delivers everything on the terminal path.
    assert!(!child_input_rejected(default, true, &plain));
    assert!(!child_input_rejected(default, true, &repeat));
    // The leader's press is never rejected; it enters the list.
    assert!(!child_input_rejected(default, false, &leader));
    // A leader repeat or release is still child input.
    assert!(child_input_rejected(default, false, &leader_repeat));
    // Command modes are not child input; keys route as commands.
    let commands = ConsoleViewState {
        mode: ConsoleViewMode::AppCommand,
        ..ConsoleViewState::default()
    };
    assert!(!child_input_rejected(commands, false, &plain));
}

#[test]
fn pane_key_seam_rejects_disabled_terminal_input_and_keeps_read_only_keys() {
    let mut interaction = ConsoleInteraction::default();
    let mut scroll: Option<PipeScroll> = None;
    let key = |code: KeyCode| -> KeyEvent { KeyEvent::new(code, KeyModifiers::NONE) };

    // A disabled terminal pane rejects child input visibly and keeps
    // the leader available; no session write happens without one. The
    // pane still owns a live session: a disabled-input PTY is a live
    // PTY whose keys are gated.
    let (session, _peer) = session();
    let stopped = std::sync::atomic::AtomicBool::new(false);
    let handle = crate::runtime::handle_for_test(&session, &stopped);
    assert!(interaction.route_pane_key(
        ConsolePaneKind::Terminal,
        false,
        key(KeyCode::Char('x')),
        Some(&handle),
        &mut scroll,
        20,
    ));
    assert_eq!(
        interaction.view().warning,
        Some(ConsoleWarning::InputDisabled)
    );
    // The read-only pane rejects child input and keeps commands.
    assert!(interaction.route_pane_key(
        ConsolePaneKind::Pipe,
        false,
        key(KeyCode::Char('x')),
        None,
        &mut scroll,
        20,
    ));
    assert_eq!(
        interaction.view().warning,
        Some(ConsoleWarning::PipeReadOnly)
    );
    assert!(interaction.route_pane_key(
        ConsolePaneKind::Pipe,
        false,
        KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
        None,
        &mut scroll,
        20,
    ));
    assert_eq!(interaction.view().mode, ConsoleViewMode::AppCommand);

    // A pane change drops the pane-scoped warning.
    interaction.set_pane(ConsolePaneKind::Terminal);
    assert_eq!(interaction.view().warning, None);
    // The explicit clear does the same from the app selection path.
    interaction.clear_pane_warning();
    assert_eq!(interaction.view().warning, None);
}

#[test]
fn read_only_pane_keys_reject_child_input_and_keep_commands_working() {
    let mut interaction = ConsoleInteraction::default();
    let mut scroll: Option<PipeScroll> = None;

    // Plain child input is rejected visibly and consumed.
    assert!(interaction.handle_key_read_only(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        &mut scroll,
        20,
    ));
    assert_eq!(
        interaction.view().warning,
        Some(ConsoleWarning::PipeReadOnly)
    );
    interaction.clear_pane_warning();

    // The leader enters command mode without a session.
    assert!(interaction.handle_key_read_only(
        KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
        &mut scroll,
        20,
    ));
    assert_eq!(interaction.view().mode, ConsoleViewMode::AppCommand);

    // Selection moves queue without a session.
    assert!(interaction.handle_key_read_only(
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        &mut scroll,
        20,
    ));
    assert_eq!(
        interaction.take_selection_moves(),
        vec![SelectionMove::Down]
    );

    // Pipe scrolling and re-following work without a session.
    assert!(interaction.handle_key_read_only(
        KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
        &mut scroll,
        20,
    ));
    assert_eq!(scroll.unwrap().offset(), 19);
    assert!(!scroll.unwrap().following());
    assert!(interaction.handle_key_read_only(
        KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE),
        &mut scroll,
        20,
    ));
    assert!(scroll.unwrap().following());
    assert_eq!(interaction.view().mode, ConsoleViewMode::ChildInput);

    // Text selection is unavailable in a read-only pane.
    assert!(interaction.handle_key_read_only(
        KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
        &mut scroll,
        20,
    ));
    assert!(interaction.handle_key_read_only(
        KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE),
        &mut scroll,
        20,
    ));
    assert_eq!(
        interaction.view().warning,
        Some(ConsoleWarning::SelectionUnavailable)
    );
    // Lifecycle commands still work from a read-only pane: they are
    // application commands, never child input.
    assert!(interaction.handle_key_read_only(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        &mut scroll,
        20,
    ));
    assert_eq!(
        interaction.take_lifecycle_commands(),
        Vec::from([LifecycleCommand::Stop])
    );
}

#[test]
fn child_input_keys_never_queue_selection_moves() {
    let (session, peer) = session();
    let stopped = std::sync::atomic::AtomicBool::new(false);
    let handle = crate::runtime::handle_for_test(&session, &stopped);
    let mut interaction = ConsoleInteraction::default();

    // In ChildInput mode j/k are child keystrokes; selection stays put.
    assert!(!interaction.handle_key(
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        &handle,
        20,
    ));

    assert_eq!(interaction.take_selection_moves(), Vec::new());

    drop(peer);
    session.shutdown().unwrap();
}
