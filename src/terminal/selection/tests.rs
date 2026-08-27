use super::*;
use libghostty_vt::terminal::Options;

fn terminal(cols: u16, rows: u16) -> Terminal<'static, 'static> {
    Terminal::new(Options {
        cols,
        rows,
        max_scrollback: 64 * 1_024,
    })
    .unwrap()
}

#[test]
fn keyboard_navigation_starts_at_the_visible_terminal_cursor_and_clamps_movement() {
    let mut terminal = terminal(8, 3);
    terminal.vt_write(b"\x1b[2;4H");
    let mut selection = SelectionController::new().unwrap();

    selection.enter_keyboard_navigation(&mut terminal).unwrap();
    assert_eq!(
        selection.keyboard_cursor(&terminal).unwrap(),
        Some(SelectionPoint {
            col: 3,
            surface_row: 1,
        })
    );

    selection
        .move_keyboard_cursor(&mut terminal, SelectionDirection::Left)
        .unwrap();
    selection
        .move_keyboard_cursor(&mut terminal, SelectionDirection::Up)
        .unwrap();
    assert_eq!(
        selection.keyboard_cursor(&terminal).unwrap(),
        Some(SelectionPoint {
            col: 2,
            surface_row: 0,
        })
    );

    for _ in 0..20 {
        selection
            .move_keyboard_cursor(&mut terminal, SelectionDirection::Left)
            .unwrap();
        selection
            .move_keyboard_cursor(&mut terminal, SelectionDirection::Up)
            .unwrap();
    }
    assert_eq!(
        selection.keyboard_cursor(&terminal).unwrap(),
        Some(SelectionPoint {
            col: 0,
            surface_row: 0,
        })
    );

    for _ in 0..20 {
        selection
            .move_keyboard_cursor(&mut terminal, SelectionDirection::Right)
            .unwrap();
        selection
            .move_keyboard_cursor(&mut terminal, SelectionDirection::Down)
            .unwrap();
    }
    assert_eq!(
        selection.keyboard_cursor(&terminal).unwrap(),
        Some(SelectionPoint {
            col: 7,
            surface_row: 2,
        })
    );
}

#[test]
fn keyboard_navigation_uses_bottom_left_when_the_terminal_cursor_is_off_viewport() {
    use libghostty_vt::terminal::ScrollViewport;

    let mut terminal = terminal(8, 3);
    for index in 0..10 {
        terminal.vt_write(format!("line-{index}\r\n").as_bytes());
    }
    terminal.scroll_viewport(ScrollViewport::Delta(-3));
    let mut selection = SelectionController::new().unwrap();

    selection.enter_keyboard_navigation(&mut terminal).unwrap();

    assert_eq!(
        selection.keyboard_cursor(&terminal).unwrap(),
        Some(SelectionPoint {
            col: 0,
            surface_row: 2,
        })
    );
}

#[test]
fn keyboard_endpoint_extension_uses_ghostty_cell_semantics_and_boundaries() {
    let mut terminal = terminal(8, 3);
    terminal.vt_write(b"abcd\x1b[1;1H");
    let mut selection = SelectionController::new().unwrap();
    selection.enter_keyboard_navigation(&mut terminal).unwrap();
    selection.toggle_keyboard_endpoint(&mut terminal).unwrap();

    assert_eq!(selection.text(&terminal).unwrap().as_deref(), Some("a"));
    selection
        .move_keyboard_cursor(&mut terminal, SelectionDirection::Right)
        .unwrap();
    assert_eq!(selection.text(&terminal).unwrap().as_deref(), Some("ab"));

    for _ in 0..20 {
        selection
            .move_keyboard_cursor(&mut terminal, SelectionDirection::Right)
            .unwrap();
    }
    assert_eq!(selection.text(&terminal).unwrap().as_deref(), Some("abcd"));
    assert_eq!(
        selection.keyboard_cursor(&terminal).unwrap(),
        Some(SelectionPoint {
            col: 3,
            surface_row: 0,
        })
    );
}

#[test]
fn mouse_selection_can_be_extended_with_keyboard_navigation() {
    let mut terminal = terminal(20, 3);
    terminal.vt_write(b"alpha beta");
    let mut selection = SelectionController::new().unwrap();
    selection
        .press(
            &terminal,
            SelectionPoint {
                col: 0,
                surface_row: 0,
            },
            Duration::from_millis(10),
        )
        .unwrap();
    selection
        .drag(
            &mut terminal,
            SelectionPoint {
                col: 5,
                surface_row: 0,
            },
        )
        .unwrap();
    selection
        .release(
            &terminal,
            SelectionPoint {
                col: 5,
                surface_row: 0,
            },
        )
        .unwrap();
    assert_eq!(selection.text(&terminal).unwrap().as_deref(), Some("alpha"));

    selection.enter_keyboard_navigation(&mut terminal).unwrap();
    selection
        .move_keyboard_cursor(&mut terminal, SelectionDirection::Right)
        .unwrap();
    selection
        .move_keyboard_cursor(&mut terminal, SelectionDirection::Right)
        .unwrap();

    assert_eq!(
        selection.text(&terminal).unwrap().as_deref(),
        Some("alpha b")
    );
    assert_eq!(
        selection.keyboard_cursor(&terminal).unwrap(),
        Some(SelectionPoint {
            col: 6,
            surface_row: 0,
        })
    );
}

#[test]
fn clear_selection_resets_keyboard_navigation_state() {
    let mut terminal = terminal(8, 3);
    terminal.vt_write(b"text\x1b[1;1H");
    let mut selection = SelectionController::new().unwrap();
    selection.enter_keyboard_navigation(&mut terminal).unwrap();
    selection.toggle_keyboard_endpoint(&mut terminal).unwrap();

    selection.clear(&terminal).unwrap();

    assert_eq!(selection.keyboard_cursor(&terminal).unwrap(), None);
    assert_eq!(selection.text(&terminal).unwrap(), None);
}

#[test]
fn copy_unwraps_soft_rows_and_preserves_hard_breaks_and_unicode() {
    let mut terminal = terminal(8, 4);
    terminal.vt_write("soft-wrap-value\r\nCafe\u{301} 界\r\nlast".as_bytes());
    let mut selection = SelectionController::new().unwrap();
    selection.select_all(&terminal).unwrap();

    assert_eq!(
        selection.text(&terminal).unwrap().unwrap(),
        "soft-wrap-value\nCafe\u{301} 界\nlast"
    );
}

#[test]
fn copy_is_independent_of_resize_reflow() {
    let mut terminal = terminal(18, 4);
    terminal.vt_write(b"one-logical-line\r\nsecond");
    let mut selection = SelectionController::new().unwrap();
    selection.select_all(&terminal).unwrap();
    terminal.resize(7, 6, 0, 0).unwrap();

    assert_eq!(
        selection.text(&terminal).unwrap().unwrap(),
        "one-logical-line\nsecond"
    );
}

#[test]
fn double_and_triple_click_copy_words_and_lines() {
    let mut terminal = terminal(20, 3);
    terminal.vt_write(b"alpha beta\r\nnext line");
    let mut selection = SelectionController::new().unwrap();
    let beta = SelectionPoint {
        col: 7,
        surface_row: 0,
    };

    selection
        .press(&terminal, beta, Duration::from_millis(100))
        .unwrap();
    selection.release(&terminal, beta).unwrap();
    selection
        .press(&terminal, beta, Duration::from_millis(200))
        .unwrap();
    selection.release(&terminal, beta).unwrap();
    assert_eq!(selection.text(&terminal).unwrap().unwrap(), "beta");

    selection
        .press(&terminal, beta, Duration::from_millis(300))
        .unwrap();
    selection.release(&terminal, beta).unwrap();
    assert_eq!(selection.text(&terminal).unwrap().unwrap(), "alpha beta");
}

#[test]
fn linear_drag_copies_the_selected_cells() {
    let mut terminal = terminal(20, 3);
    terminal.vt_write(b"alpha beta");
    let mut selection = SelectionController::new().unwrap();
    selection
        .press(
            &terminal,
            SelectionPoint {
                col: 0,
                surface_row: 0,
            },
            Duration::from_millis(10),
        )
        .unwrap();
    selection
        .drag(
            &mut terminal,
            SelectionPoint {
                col: 5,
                surface_row: 0,
            },
        )
        .unwrap();

    assert_eq!(selection.text(&terminal).unwrap().unwrap(), "alpha");
}

#[test]
fn tracked_selection_stays_coherent_during_output_and_reflow() {
    let mut terminal = terminal(12, 3);
    terminal.vt_write(b"keep-this\r\n");
    let mut selection = SelectionController::new().unwrap();
    selection.select_all(&terminal).unwrap();
    for index in 0..30 {
        terminal.vt_write(format!("live-{index}\r\n").as_bytes());
    }
    terminal.resize(7, 5, 0, 0).unwrap();

    let copied = selection.text(&terminal).unwrap().unwrap();
    assert!(copied.contains("keep-this"));
    assert!(!copied.contains("live-29"));
}

#[test]
fn drag_outside_viewport_requests_autoscroll_into_history() {
    let mut terminal = terminal(12, 3);
    for index in 0..12 {
        terminal.vt_write(format!("history-{index}\r\n").as_bytes());
    }
    let mut selection = SelectionController::new().unwrap();
    let start = SelectionPoint {
        col: 0,
        surface_row: 1,
    };
    selection
        .press(&terminal, start, Duration::from_millis(10))
        .unwrap();
    selection
        .drag(
            &mut terminal,
            SelectionPoint {
                col: 5,
                surface_row: -1,
            },
        )
        .unwrap();
    for _ in 0..7 {
        assert!(selection.tick_autoscroll(&mut terminal).unwrap());
    }
    selection
        .release(
            &terminal,
            SelectionPoint {
                col: 5,
                surface_row: 0,
            },
        )
        .unwrap();

    let copied = selection.text(&terminal).unwrap().unwrap();
    assert!(
        copied.contains("history-"),
        "selection did not copy history: {copied:?}"
    );
    assert!(
        copied.lines().count() > 1,
        "selection did not enter history: {copied:?}"
    );
}

#[test]
fn each_autoscroll_tick_moves_the_viewport_by_one_row() {
    use libghostty_vt::terminal::PointSpace;

    let mut terminal = terminal(12, 3);
    for index in 0..12 {
        terminal.vt_write(format!("history-{index}\r\n").as_bytes());
    }
    let mut selection = SelectionController::new().unwrap();
    selection
        .press(
            &terminal,
            SelectionPoint {
                col: 0,
                surface_row: 1,
            },
            Duration::from_millis(10),
        )
        .unwrap();
    selection
        .drag(
            &mut terminal,
            SelectionPoint {
                col: 5,
                surface_row: -1,
            },
        )
        .unwrap();
    let tracked = terminal
        .track_grid_ref(Point::Viewport(PointCoordinate { x: 0, y: 0 }))
        .unwrap();
    let before = tracked.point(PointSpace::Viewport).unwrap().unwrap().y;

    assert!(selection.tick_autoscroll(&mut terminal).unwrap());

    let after = tracked.point(PointSpace::Viewport).unwrap().unwrap().y;
    assert_eq!(after, before + 1);
}

#[test]
fn release_stops_active_autoscroll() {
    let mut terminal = terminal(12, 3);
    for index in 0..8 {
        terminal.vt_write(format!("history-{index}\r\n").as_bytes());
    }
    let mut selection = SelectionController::new().unwrap();
    selection
        .press(
            &terminal,
            SelectionPoint {
                col: 0,
                surface_row: 1,
            },
            Duration::from_millis(10),
        )
        .unwrap();
    selection
        .drag(
            &mut terminal,
            SelectionPoint {
                col: 5,
                surface_row: -1,
            },
        )
        .unwrap();
    selection
        .release(
            &terminal,
            SelectionPoint {
                col: 11,
                surface_row: 0,
            },
        )
        .unwrap();

    assert!(!selection.tick_autoscroll(&mut terminal).unwrap());
}
