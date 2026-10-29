use ratatui::buffer::Buffer;
use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState, StatefulWidget};

use crate::output::{RetainedChunk, RetainedOutput};
use crate::runtime::OutputStream;

use super::*;

fn output(range: std::ops::Range<usize>, generation: u64) -> RetainedOutput {
    RetainedOutput {
        chunks: range
            .map(|line| RetainedChunk::Data {
                run_id: 1,
                stream: OutputStream::Stdout,
                text: format!("line-{line:05}\n"),
                sequence: line as u64,
                observed_at_ms: line as u64,
                continued: false,
            })
            .collect(),
        generation,
        ..RetainedOutput::default()
    }
}

fn sources(lines: &[PipeLine]) -> Vec<Option<(u64, usize)>> {
    lines.iter().map(|line| line.source).collect()
}

#[test]
fn paused_window_stays_anchored_when_lines_append() {
    let mut scroll = PipeScroll::default();
    let first = output(0..25, 1);
    scroll.scroll_lines(5, -1);
    let before = sources(scroll.window(&first, 20));

    let later = output(0..30, 2);

    assert_eq!(sources(scroll.window(&later, 20)), before);
    assert!(!scroll.following());
}

#[test]
fn an_empty_search_target_returns_to_the_live_tail() {
    let mut scroll = PipeScroll::default();
    scroll.show_source((1, 0));

    assert!(scroll.window(&RetainedOutput::default(), 20).is_empty());
    assert!(scroll.following());
}

#[test]
fn search_pause_keeps_the_same_visible_lines_as_output_arrives() {
    let mut scroll = PipeScroll::default();
    let first = output(0..25, 1);
    let before = sources(scroll.window(&first, 20));
    scroll.pause();

    let later = output(0..30, 2);

    assert_eq!(sources(scroll.window(&later, 20)), before);
    assert!(!scroll.following());
}

#[test]
fn logs_mouse_selection_is_visible_and_returns_selected_text() {
    let mut scroll = PipeScroll::default();
    let retained = output(0..25, 1);
    scroll.window(&retained, 20);

    assert!(scroll.begin_selection(0, 0));
    assert!(scroll.update_selection(1, 5));

    let visible = scroll.window(&retained, 20);
    assert!(visible[0].selection.is_some());
    assert!(visible[1].selection.is_some());
    assert!(scroll.selected_text(&retained).unwrap().contains('\n'));
    assert!(!scroll.following());
}

#[test]
fn scrolling_preserves_a_logs_selection() {
    let retained = output(0..40, 1);
    let mut scroll = PipeScroll::default();
    scroll.window(&retained, 20);
    scroll.begin_selection(5, 0);
    scroll.update_selection(10, 5);
    scroll.finish_selection(10, 5);
    let selected = scroll.selected_text(&retained);

    scroll.scroll_lines(3, -1);
    scroll.window(&retained, 20);

    assert_eq!(scroll.selected_text(&retained), selected);
    assert!(
        scroll
            .window(&retained, 20)
            .iter()
            .any(|line| line.selection.is_some())
    );
}

#[test]
fn expired_page_copy_preserves_the_selected_range_in_both_directions() {
    for notice in [false, true] {
        for reverse in [false, true] {
            let mut retained = output(0..10, 1);
            retained.truncated = notice;
            let mut scroll = PipeScroll::default();
            scroll.head();
            scroll.window(&retained, 5);
            if reverse {
                scroll.begin_selection(3, 30);
                // Drag endpoints include their character; stop on the prefix's final space.
                scroll.finish_selection(2, 17);
            } else {
                scroll.begin_selection(2, 18);
                scroll.finish_selection(3, 30);
            }
            let before = scroll.selected_text(&retained).unwrap();
            assert_eq!(
                before,
                if notice {
                    "line-00001\n00:00:00.002 out: line-00002"
                } else {
                    "line-00002\n00:00:00.003 out: line-00003"
                }
            );

            let mut expired = output(10..20, 2);
            expired.truncated = true;
            scroll.window(&expired, 5);
            assert!(scroll.expired());
            assert_eq!(
                scroll.selected_text(&expired).as_deref(),
                Some(before.as_str()),
                "notice={notice}, reverse={reverse}"
            );
        }
    }
}

#[test]
fn wheel_scrolling_extends_an_active_logs_selection() {
    let retained = output(0..40, 1);
    let mut scroll = PipeScroll::default();
    scroll.window(&retained, 10);
    scroll.begin_selection(5, 0);
    scroll.update_selection(6, 5);
    let before = scroll.selected_text(&retained).unwrap();

    scroll.scroll_lines(5, -1);
    scroll.window(&retained, 10);

    let after = scroll.selected_text(&retained).unwrap();
    assert!(after.len() > before.len());
    assert!(after.contains("line-00031"));
    assert!(!after.contains("line-00030"));
}

#[test]
fn selected_text_includes_lines_outside_the_visible_window() {
    let retained = output(0..40, 1);
    let mut scroll = PipeScroll::default();
    scroll.window(&retained, 10);
    scroll.begin_selection(8, 0);
    scroll.update_selection(9, 5);
    scroll.scroll_lines(10, -1);
    scroll.window(&retained, 10);
    scroll.finish_selection(9, 5);

    let selected = scroll.selected_text(&retained).unwrap();
    assert!(selected.contains("line-00029"));
    assert!(!selected.contains("line-00028"));
    assert!(selected.contains("line-00037"));
}

#[test]
fn scrollbar_reports_the_retained_viewport_and_hides_when_logs_fit() {
    let retained = output(0..40, 1);
    let mut scroll = PipeScroll::default();

    scroll.window(&retained, 10);

    assert_eq!(
        scroll.scrollbar(),
        Some(ConsoleScrollbar {
            position: 30,
            content_length: 31,
            viewport_length: 10,
        })
    );
    let short = output(0..10, 2);
    scroll.follow();
    scroll.window(&short, 10);
    assert_eq!(scroll.scrollbar(), None);
}

#[test]
fn scrollbar_hit_area_matches_ratatui_thumb_geometry() {
    for track_rows in 1..=8 {
        let area = Rect::new(0, 0, 1, track_rows as u16);
        for content_length in 1..=10 {
            for viewport_length in 1..=5 {
                for position in 0..content_length {
                    let scrollbar = ConsoleScrollbar {
                        position,
                        content_length,
                        viewport_length,
                    };
                    let mut buffer = Buffer::empty(area);
                    let mut state = ScrollbarState::new(content_length)
                        .position(position)
                        .viewport_content_length(viewport_length);
                    StatefulWidget::render(
                        Scrollbar::new(ScrollbarOrientation::VerticalRight)
                            .begin_symbol(None)
                            .end_symbol(None),
                        area,
                        &mut buffer,
                        &mut state,
                    );
                    let thumb_rows = (0..track_rows)
                        .filter(|row| buffer[(0, *row as u16)].symbol() == "█")
                        .collect::<Vec<_>>();
                    let rendered = (thumb_rows.first().copied().unwrap_or(0), thumb_rows.len());

                    assert_eq!(scrollbar_thumb(scrollbar, track_rows), rendered);
                }
            }
        }
    }
}

#[test]
fn scrollbar_thumb_drag_maps_to_a_stable_logs_position() {
    let retained = output(0..40, 1);
    let mut scroll = PipeScroll::default();
    let area = Rect::new(5, 5, 20, 10);
    scroll.window(&retained, 10);
    let mouse = |kind, column, row| MouseEvent {
        kind,
        column,
        row,
        modifiers: crossterm::event::KeyModifiers::NONE,
    };

    assert!(scroll.handle_scrollbar_mouse(
        mouse(MouseEventKind::Down(MouseButton::Left), 24, 13),
        area,
        &retained,
    ));
    assert!(scroll.scrollbar_gesture_active());
    assert!(scroll.handle_scrollbar_mouse(
        mouse(MouseEventKind::Drag(MouseButton::Left), 30, 8),
        area,
        &retained,
    ));
    assert_eq!(sources(scroll.window(&retained, 10))[0], Some((8, 0)));
    assert_eq!(
        scroll.scrollbar(),
        Some(ConsoleScrollbar {
            position: 8,
            content_length: 31,
            viewport_length: 10,
        })
    );
    assert!(scroll.handle_scrollbar_mouse(
        mouse(MouseEventKind::Up(MouseButton::Left), 30, 2),
        area,
        &retained,
    ));
    assert!(!scroll.scrollbar_gesture_active());

    assert!(scroll.handle_scrollbar_mouse(
        mouse(MouseEventKind::Down(MouseButton::Left), 24, 14),
        area,
        &retained,
    ));
    assert_eq!(sources(scroll.window(&retained, 10))[0], Some((30, 0)));
    assert!(scroll.handle_scrollbar_mouse(
        mouse(MouseEventKind::Up(MouseButton::Left), 0, 0),
        area,
        &retained,
    ));
    assert!(!scroll.scrollbar_gesture_active());
}

#[test]
fn scrollbar_navigation_preserves_a_finished_logs_selection() {
    let retained = output(0..40, 1);
    let mut scroll = PipeScroll::default();
    let area = Rect::new(0, 0, 20, 10);
    scroll.window(&retained, 10);
    scroll.begin_selection(5, 0);
    scroll.update_selection(6, 5);
    scroll.finish_selection(6, 5);
    let selected = scroll.selected_text(&retained);
    let mouse = |kind, row| MouseEvent {
        kind,
        column: 19,
        row,
        modifiers: crossterm::event::KeyModifiers::NONE,
    };

    scroll.handle_scrollbar_mouse(
        mouse(MouseEventKind::Down(MouseButton::Left), 0),
        area,
        &retained,
    );
    scroll.handle_scrollbar_mouse(
        mouse(MouseEventKind::Up(MouseButton::Left), 0),
        area,
        &retained,
    );

    assert_eq!(scroll.selected_text(&retained), selected);
}

#[test]
fn selection_columns_keep_wide_and_combining_characters_whole() {
    assert_eq!(byte_at_column("界a", 0, false), 0);
    assert_eq!(byte_at_column("界a", 1, true), "界".len());
    assert_eq!(byte_at_column("e\u{301}x", 0, true), "e\u{301}".len());
}

#[test]
fn reaching_the_tail_resumes_following() {
    let retained = output(0..30, 1);
    let mut scroll = PipeScroll::default();
    scroll.scroll_lines(10, -1);
    scroll.window(&retained, 20);
    assert!(!scroll.following());

    scroll.scroll_lines(10, 1);

    assert_eq!(sources(scroll.window(&retained, 20))[0], Some((10, 0)));
    assert!(scroll.following());
}

#[test]
fn evicted_anchor_freezes_the_visible_page_until_explicit_navigation() {
    let mut scroll = PipeScroll::default();
    let first = output(0..25, 1);
    scroll.scroll_lines(5, -1);
    let before = scroll.window(&first, 20).to_vec();

    let mut later = output(5..30, 2);
    later.truncated = true;
    let visible = scroll.window(&later, 20);

    assert_eq!(visible, before);
    assert!(scroll.expired());
    assert!(!scroll.following());
    let even_later = output(100..150, 3);
    assert_eq!(scroll.window(&even_later, 20), before);
    scroll.follow();
    assert_eq!(
        scroll.window(&even_later, 20).last().unwrap().source,
        Some((149, 0))
    );
    assert!(!scroll.expired());
}

#[test]
fn retained_history_is_not_hidden_by_a_second_navigation_limit() {
    let retained = output(0..5_000, 1);
    let mut scroll = PipeScroll::default();

    scroll.scroll_lines(10_000, -1);

    assert_eq!(sources(scroll.window(&retained, 20))[0], Some((0, 0)));
}

#[test]
fn extra_scroll_up_at_the_retained_head_has_no_scroll_debt() {
    let retained = output(0..25, 1);
    let mut scroll = PipeScroll::default();
    scroll.scroll_lines(6, -1);
    assert_eq!(sources(scroll.window(&retained, 20))[0], Some((0, 0)));

    scroll.scroll_lines(1, 1);

    assert_eq!(sources(scroll.window(&retained, 20))[0], Some((1, 0)));
}

#[test]
fn follow_returns_to_the_live_tail() {
    let retained = output(0..100, 1);
    let mut scroll = PipeScroll::default();
    scroll.scroll_page(20, -1);
    assert_eq!(sources(scroll.window(&retained, 20))[0], Some((61, 0)));

    scroll.follow();

    assert_eq!(sources(scroll.window(&retained, 20))[0], Some((80, 0)));
    assert!(scroll.following());
}
