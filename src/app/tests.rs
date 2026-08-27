use super::*;

#[test]
fn shutdown_result_keeps_failures_and_remaining_pids() {
    let result = ProjectShutdownSnapshot {
        complete: true,
        timed_out: true,
        failures: vec![crate::supervisor::ProcessShutdownFailure {
            process: "api".to_string(),
            detail: "Project shutdown deadline expired".to_string(),
            remaining_pids: vec![41, 42],
        }],
    };

    let error = report_shutdown_result(&result).expect_err("cleanup failure is reported");
    assert_eq!(
        error.to_string(),
        "Project shutdown did not finish cleanly: api: Project shutdown deadline expired (remaining PIDs: [41, 42])"
    );
}

#[test]
fn metric_and_age_labels_use_compact_units() {
    assert_eq!(format_cpu(3.24), "3.2%");
    assert_eq!(format_cpu(12.4), "12%");
    assert_eq!(format_rss(768), "768K");
    assert_eq!(format_rss(180 * 1024), "180M");
    assert_eq!(format_rss(1024 * 1024), "1.0G");
    assert_eq!(format_age(59_000), "59s");
    assert_eq!(format_age(61_000), "1m1s");
}

#[test]
fn plain_q_quits_only_from_process_list_focus() {
    let plain_q = KeyEvent::new(KeyCode::Char('q'), crossterm::event::KeyModifiers::NONE);
    let ctrl_q = KeyEvent::new(KeyCode::Char('q'), crossterm::event::KeyModifiers::CONTROL);
    let list = crate::tui::ConsoleViewState::default();
    let console = crate::tui::ConsoleViewState {
        mode: crate::tui::ConsoleViewMode::Console,
        ..list
    };

    assert!(is_quit(plain_q, list));
    assert!(!is_quit(plain_q, console));
    assert!(is_quit(ctrl_q, list));
    assert!(is_quit(ctrl_q, console));
}

#[test]
fn only_a_mouse_press_changes_keyboard_focus() {
    assert!(mouse_changes_focus(MouseEventKind::Down(MouseButton::Left)));
    assert!(!mouse_changes_focus(MouseEventKind::ScrollUp));
    assert!(!mouse_changes_focus(MouseEventKind::Drag(
        MouseButton::Left
    )));
    assert!(!mouse_changes_focus(MouseEventKind::Up(MouseButton::Left)));
    assert!(mouse_starts_console_focus(
        MouseEventKind::Down(MouseButton::Left),
        crate::tui::ConsoleViewMode::ProcessList
    ));
    assert!(!mouse_starts_console_focus(
        MouseEventKind::Down(MouseButton::Left),
        crate::tui::ConsoleViewMode::Copy
    ));
}

#[test]
fn process_row_hit_testing_excludes_borders_and_empty_rows() {
    let list = ratatui::layout::Rect::new(0, 0, 40, 6);

    assert_eq!(process_row_at(list, 0, 3), None);
    assert_eq!(process_row_at(list, 1, 3), Some(0));
    assert_eq!(process_row_at(list, 3, 3), Some(2));
    assert_eq!(process_row_at(list, 4, 3), None);
    assert_eq!(process_row_at(list, 5, 3), None);
}

#[test]
fn rapid_resize_uses_only_the_last_valid_geometry() {
    let started = Instant::now();
    let mut pending = PendingResize::default();
    pending.update(TerminalGeometry::new(120, 40).unwrap(), started);
    pending.update(
        TerminalGeometry::new(1, 1).unwrap(),
        started + Duration::from_millis(2),
    );
    pending.update(
        TerminalGeometry::new(73, 19).unwrap(),
        started + Duration::from_millis(4),
    );

    assert_eq!(
        pending.take_ready(started + Duration::from_millis(19)),
        None
    );
    assert_eq!(
        pending.take_ready(started + Duration::from_millis(20)),
        TerminalGeometry::new(73, 19)
    );
    assert_eq!(
        pending.take_ready(started + Duration::from_millis(21)),
        None
    );
}
