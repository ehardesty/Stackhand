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

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn leader() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)
}

#[test]
fn process_list_owns_the_keyboard_at_startup() {
    let interaction = ConsoleInteraction::default();

    assert_eq!(interaction.view().mode, ConsoleViewMode::ProcessList);
    assert_eq!(
        process_command(KeyCode::Char('j')),
        Some(ProcessCommand::MoveSelection(SelectionMove::Down))
    );
    assert_eq!(
        process_command(KeyCode::Char('s')),
        Some(ProcessCommand::Lifecycle(LifecycleCommand::Start))
    );
    assert_eq!(
        process_command(KeyCode::Char('x')),
        Some(ProcessCommand::Lifecycle(LifecycleCommand::Stop))
    );
    assert_eq!(
        process_command(KeyCode::Char('r')),
        Some(ProcessCommand::Lifecycle(LifecycleCommand::Restart))
    );
}

#[test]
fn process_navigation_works_before_ctrl_a_and_never_reaches_the_child() {
    let (session, peer) = session();
    let stopped = std::sync::atomic::AtomicBool::new(false);
    let handle = crate::runtime::handle_for_test(&session, &stopped);
    let mut interaction = ConsoleInteraction::default();

    for (code, expected) in [
        (KeyCode::Down, SelectionMove::Down),
        (KeyCode::Char('j'), SelectionMove::Down),
        (KeyCode::Up, SelectionMove::Up),
        (KeyCode::Char('k'), SelectionMove::Up),
    ] {
        assert!(interaction.handle_key(key(code), &handle, 20));
        assert_eq!(interaction.take_selection_moves(), vec![expected]);
        assert_eq!(interaction.view().mode, ConsoleViewMode::ProcessList);
    }

    assert!(interaction.handle_key(leader(), &handle, 20));
    assert_eq!(interaction.view().mode, ConsoleViewMode::Console);
    let leader_release = KeyEvent {
        kind: KeyEventKind::Release,
        ..leader()
    };
    assert!(
        interaction.handle_key(leader_release, &handle, 20),
        "the leader release stays owned after console focus begins"
    );
    assert_eq!(interaction.view().mode, ConsoleViewMode::Console);
    assert!(!interaction.handle_key(key(KeyCode::Char('j')), &handle, 20));
    assert_eq!(interaction.take_selection_moves(), Vec::new());

    assert!(interaction.handle_key(leader(), &handle, 20));
    assert_eq!(interaction.view().mode, ConsoleViewMode::ProcessList);

    drop(peer);
    session.shutdown().unwrap();
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

    interaction.selection_requests = vec![SelectionMove::Down];
    assert!(interaction.apply_selection_moves(&mut selected, 2));
    assert_eq!(selected, 1, "selection clamps at the Project end");
}

#[test]
fn list_scrolling_keeps_list_focus_and_f_returns_to_live_tail() {
    let (session, peer) = session();
    let stopped = std::sync::atomic::AtomicBool::new(false);
    let handle = crate::runtime::handle_for_test(&session, &stopped);
    let mut interaction = ConsoleInteraction::default();

    assert!(interaction.handle_key(key(KeyCode::PageUp), &handle, 20));
    assert_eq!(interaction.view().mode, ConsoleViewMode::ProcessList);
    assert!(!interaction.view().following);

    assert!(interaction.handle_key(key(KeyCode::Char('f')), &handle, 20));
    assert_eq!(interaction.view().mode, ConsoleViewMode::ProcessList);
    assert!(interaction.view().following);

    drop(peer);
    session.shutdown().unwrap();
}

#[test]
fn disabled_input_is_rejected_only_with_console_focus() {
    let plain = key(KeyCode::Char('x'));
    let repeat = KeyEvent {
        kind: KeyEventKind::Repeat,
        ..plain
    };
    let list = ConsoleViewState::default();
    let console = ConsoleViewState {
        mode: ConsoleViewMode::Console,
        ..list
    };

    assert!(!child_input_rejected(list, false, &plain));
    assert!(child_input_rejected(console, false, &plain));
    assert!(child_input_rejected(console, false, &repeat));
    assert!(!child_input_rejected(console, false, &leader()));
    assert!(!child_input_rejected(console, true, &plain));
}

#[test]
fn read_only_pane_has_immediate_list_commands_and_explicit_console_focus() {
    let mut interaction = ConsoleInteraction::default();
    interaction.set_pane(ConsolePaneKind::Pipe);
    let mut scroll: Option<PipeScroll> = None;

    assert!(interaction.handle_key_read_only(key(KeyCode::Char('j')), &mut scroll, 20));
    assert_eq!(
        interaction.take_selection_moves(),
        vec![SelectionMove::Down]
    );

    assert!(interaction.handle_key_read_only(key(KeyCode::PageUp), &mut scroll, 20));
    assert_eq!(scroll.unwrap().offset(), 19);
    assert_eq!(interaction.view().mode, ConsoleViewMode::ProcessList);
    assert!(!interaction.view().following);

    assert!(interaction.handle_key_read_only(key(KeyCode::Char('f')), &mut scroll, 20));
    assert_eq!(scroll.unwrap().offset(), 0);
    assert!(interaction.view().following);

    assert!(interaction.handle_key_read_only(leader(), &mut scroll, 20));
    assert_eq!(interaction.view().mode, ConsoleViewMode::Console);
    assert!(interaction.handle_key_read_only(key(KeyCode::Char('x')), &mut scroll, 20));
    assert_eq!(
        interaction.view().warning,
        Some(ConsoleWarning::PipeReadOnly)
    );

    assert!(interaction.handle_key_read_only(leader(), &mut scroll, 20));
    assert_eq!(interaction.view().mode, ConsoleViewMode::ProcessList);
    assert!(interaction.handle_key_read_only(key(KeyCode::Char('x')), &mut scroll, 20));
    assert_eq!(
        interaction.take_lifecycle_commands(),
        vec![LifecycleCommand::Stop]
    );
}

#[test]
fn copy_mode_supports_vim_navigation_and_copy_aliases() {
    let (session, peer) = session();
    let stopped = std::sync::atomic::AtomicBool::new(false);
    let handle = crate::runtime::handle_for_test(&session, &stopped);
    let mut interaction = ConsoleInteraction::default();

    assert!(interaction.handle_key(key(KeyCode::Char('v')), &handle, 20));
    assert_eq!(interaction.view().mode, ConsoleViewMode::Copy);
    for code in [
        KeyCode::Char('h'),
        KeyCode::Char('j'),
        KeyCode::Char('k'),
        KeyCode::Char('l'),
        KeyCode::Left,
        KeyCode::Down,
        KeyCode::Up,
        KeyCode::Right,
        KeyCode::Char('v'),
        KeyCode::Char('c'),
        KeyCode::Char('y'),
    ] {
        assert!(interaction.handle_key(key(code), &handle, 20), "{code:?}");
    }
    assert!(interaction.handle_key(key(KeyCode::Esc), &handle, 20));
    assert_eq!(interaction.view().mode, ConsoleViewMode::ProcessList);

    drop(peer);
    session.shutdown().unwrap();
}

#[test]
fn console_click_focuses_and_drag_enters_copy_mode() {
    let (session, peer) = session();
    let stopped = std::sync::atomic::AtomicBool::new(false);
    let handle = crate::runtime::handle_for_test(&session, &stopped);
    let mut interaction = ConsoleInteraction::default();
    interaction.focus_console(Some(&handle));
    let area = Rect::new(5, 5, 20, 4);

    let mouse = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 8,
        row: 6,
        modifiers: KeyModifiers::NONE,
    };
    assert!(interaction.handle_mouse(mouse, area, false, &handle));
    assert_eq!(interaction.view().mode, ConsoleViewMode::Console);
    assert!(interaction.mouse_gesture_active());

    let drag = MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: 12,
        ..mouse
    };
    assert!(interaction.handle_mouse(drag, area, false, &handle));
    assert_eq!(interaction.view().mode, ConsoleViewMode::Copy);

    let release = MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        ..mouse
    };
    assert!(interaction.handle_mouse(release, area, false, &handle));
    assert!(!interaction.mouse_gesture_active());

    interaction.focus_console(Some(&handle));
    assert!(interaction.handle_mouse(mouse, area, false, &handle));
    assert_eq!(interaction.view().mode, ConsoleViewMode::Console);
    assert!(interaction.handle_mouse(release, area, false, &handle));
    assert!(interaction.handle_mouse(mouse, area, false, &handle));
    assert_eq!(
        interaction.view().mode,
        ConsoleViewMode::Copy,
        "a repeated click exposes the terminal-owned word selection"
    );
    assert!(interaction.handle_mouse(release, area, false, &handle));
    assert!(interaction.handle_mouse(mouse, area, false, &handle));
    assert_eq!(
        interaction.view().mode,
        ConsoleViewMode::Copy,
        "a third click keeps Copy mode for the logical-line selection"
    );

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
