//! Stable viewport state for one Process's retained Logs output.
//!
//! Following is the only state that moves when output arrives. Scrolling up
//! anchors the viewport to a retained source line. New output can then arrive
//! without moving that anchor. If retention evicts the anchor, the viewport
//! moves to the retained head instead of guessing from the live tail.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use unicode_width::UnicodeWidthChar;

use crate::output::RetainedOutput;
use crate::tui::{ConsoleScrollbar, PipeLine};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Position {
    #[default]
    Following,
    Head,
    Source((u64, usize)),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ScrollMove {
    lines: usize,
    direction: isize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SelectionPoint {
    source: (u64, usize),
    byte: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LogSelection {
    anchor: SelectionPoint,
    cursor: SelectionPoint,
    cursor_row: usize,
    cursor_column: usize,
    dragging: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScrollbarGesture {
    Track,
    Thumb { grab_row: usize },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PipeScroll {
    position: Position,
    pending: Vec<ScrollMove>,
    target: Option<(u64, usize)>,
    visible: Vec<PipeLine>,
    held: bool,
    selection: Option<LogSelection>,
    scrollbar: Option<ConsoleScrollbar>,
    scrollbar_gesture: Option<ScrollbarGesture>,
}

impl PipeScroll {
    pub fn following(&self) -> bool {
        self.position == Position::Following && self.pending.is_empty() && self.target.is_none()
    }

    /// Queue one page move. Navigation is applied against one immutable Logs
    /// snapshot when the next visible window is requested.
    pub fn scroll_page(&mut self, page_rows: u16, direction: isize) {
        let page = usize::from(page_rows.saturating_sub(1).max(1));
        self.scroll_lines(page, direction);
    }

    /// Queue a line move while preserving the order of opposite directions.
    /// Mouse-wheel input supplies repeated events as one line count.
    pub fn scroll_lines(&mut self, lines: usize, direction: isize) {
        if lines == 0 || direction == 0 {
            return;
        }
        if direction > 0 && self.following() {
            return;
        }
        let direction = direction.signum();
        if let Some(last) = self.pending.last_mut()
            && last.direction == direction
        {
            last.lines = last.lines.saturating_add(lines);
            return;
        }
        self.pending.push(ScrollMove { lines, direction });
    }

    /// Center one retained source line where available. A source that is
    /// evicted before the next frame resolves to the retained head.
    pub fn show_source(&mut self, source: (u64, usize)) {
        self.pending.clear();
        self.target = Some(source);
        self.held = true;
        self.selection = None;
    }

    /// Pin the current visible source. Search and selection use this action so
    /// new output can arrive without moving text that the user is reading.
    pub fn pause(&mut self) {
        self.position = self
            .visible
            .first()
            .and_then(|line| line.source)
            .map_or(Position::Head, Position::Source);
        self.pending.clear();
        self.target = None;
        self.held = true;
    }

    pub fn head(&mut self) {
        self.position = Position::Head;
        self.pending.clear();
        self.target = None;
        self.visible.clear();
        self.held = false;
    }

    pub fn follow(&mut self) {
        self.position = Position::Following;
        self.pending.clear();
        self.target = None;
        self.visible.clear();
        self.held = false;
    }

    pub fn begin_selection(&mut self, row: usize, column: usize) -> bool {
        let Some(point) = self.selection_point(row, column, false) else {
            return false;
        };
        self.selection = Some(LogSelection {
            anchor: point,
            cursor: point,
            cursor_row: row,
            cursor_column: column,
            dragging: true,
        });
        self.apply_selection();
        true
    }

    pub fn update_selection(&mut self, row: usize, column: usize) -> bool {
        let Some(point) = self.selection_point(row, column, true) else {
            return false;
        };
        let Some(selection) = self.selection else {
            return false;
        };
        if selection.anchor == selection.cursor {
            self.pause();
        }
        let selection = self.selection.as_mut().expect("selection exists");
        selection.cursor = point;
        selection.cursor_row = row;
        selection.cursor_column = column;
        self.apply_selection();
        true
    }

    pub fn finish_selection(&mut self, row: usize, column: usize) -> bool {
        if self.selection.is_none() {
            return false;
        }
        self.update_selection(row, column);
        let selection = self.selection.as_mut().expect("selection exists");
        selection.dragging = false;
        if selection.anchor == selection.cursor {
            return self.clear_selection();
        }
        true
    }

    pub fn clear_selection(&mut self) -> bool {
        let changed = self.selection.take().is_some();
        self.apply_selection();
        changed
    }

    pub fn has_selection(&self) -> bool {
        self.selection
            .is_some_and(|selection| selection.anchor != selection.cursor)
    }

    pub fn scrollbar(&self) -> Option<ConsoleScrollbar> {
        self.scrollbar
    }

    pub fn scrollbar_gesture_active(&self) -> bool {
        self.scrollbar_gesture.is_some()
    }

    /// Own pane-scrollbar hit testing and drag mapping. The caller only sends
    /// mouse input and the current immutable Logs snapshot.
    pub fn handle_scrollbar_mouse(
        &mut self,
        mouse: MouseEvent,
        area: Rect,
        output: &RetainedOutput,
    ) -> bool {
        let Some(scrollbar) = self.scrollbar else {
            return false;
        };
        if area.width <= 1 || area.height == 0 {
            return false;
        }
        let track_rows = usize::from(area.height);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left)
                if mouse.column == area.right().saturating_sub(1)
                    && mouse.row >= area.y
                    && mouse.row < area.bottom() =>
            {
                let row = usize::from(mouse.row - area.y);
                let (thumb_start, thumb_len) = scrollbar_thumb(scrollbar, track_rows);
                if row >= thumb_start && row < thumb_start + thumb_len {
                    self.scrollbar_gesture = Some(ScrollbarGesture::Thumb {
                        grab_row: row - thumb_start,
                    });
                } else {
                    let centered = row.saturating_sub(thumb_len / 2);
                    self.set_scrollbar_thumb(output, track_rows, centered);
                    self.scrollbar_gesture = Some(ScrollbarGesture::Track);
                }
                true
            }
            MouseEventKind::Drag(MouseButton::Left) => match self.scrollbar_gesture {
                Some(ScrollbarGesture::Thumb { grab_row }) => {
                    let row = usize::from(
                        mouse.row.clamp(area.y, area.bottom().saturating_sub(1)) - area.y,
                    );
                    self.set_scrollbar_thumb(output, track_rows, row.saturating_sub(grab_row));
                    true
                }
                Some(ScrollbarGesture::Track) => true,
                None => false,
            },
            MouseEventKind::Up(MouseButton::Left) if self.scrollbar_gesture.take().is_some() => {
                true
            }
            _ => false,
        }
    }

    pub fn selected_text(&self, output: &RetainedOutput) -> Option<String> {
        let (start, end) = self.selection_range()?;
        if start == end {
            return None;
        }
        let lines = output.display_window_from(Some(start.source), usize::MAX)?;
        let mut selected = Vec::new();
        for line in lines {
            let source = line.source?;
            if source > end.source {
                break;
            }
            let from = if source == start.source {
                start.byte
            } else {
                0
            };
            let to = if source == end.source {
                end.byte
            } else {
                line.text.len()
            };
            selected
                .push(line.text[from.min(line.text.len())..to.min(line.text.len())].to_string());
            if source == end.source {
                return Some(selected.join("\n"));
            }
        }
        None
    }

    /// Return the visible Logs window. This is the module's main interface:
    /// it owns follow transitions, source anchoring, resize behavior, ordered
    /// navigation, and retention eviction.
    pub fn window<'a>(&'a mut self, output: &RetainedOutput, pane_rows: usize) -> &'a [PipeLine] {
        if pane_rows == 0 {
            self.visible.clear();
            self.scrollbar = None;
            self.scrollbar_gesture = None;
            return &self.visible;
        }
        if !self.selection_is_retained(output) {
            self.selection = None;
        }
        if let Some(source) = self.target.take() {
            self.place_target(output, pane_rows, source);
        } else if !self.pending.is_empty() {
            self.apply_navigation(output, pane_rows);
        } else {
            self.refresh(output, pane_rows);
        }
        self.apply_selection();
        self.update_scrollbar(output, pane_rows);
        &self.visible
    }

    fn update_scrollbar(&mut self, output: &RetainedOutput, pane_rows: usize) {
        let first_source = self.visible.first().and_then(|line| line.source);
        let metrics = output.display_position(first_source);
        self.scrollbar = metrics.and_then(|(position, content_length)| {
            (content_length > pane_rows).then_some(ConsoleScrollbar {
                position,
                content_length: content_length
                    .saturating_sub(self.visible.len())
                    .saturating_add(1),
                viewport_length: self.visible.len(),
            })
        });
        if self.scrollbar.is_none() {
            self.scrollbar_gesture = None;
        }
    }

    fn set_scrollbar_thumb(
        &mut self,
        output: &RetainedOutput,
        track_rows: usize,
        thumb_start: usize,
    ) {
        let Some(scrollbar) = self.scrollbar else {
            return;
        };
        let (_, thumb_len) = scrollbar_thumb(scrollbar, track_rows);
        let max_thumb_start = track_rows.saturating_sub(thumb_len);
        let max_position = scrollbar.content_length.saturating_sub(1);
        let thumb_start = thumb_start.min(max_thumb_start);
        let position = scale(thumb_start, max_position, max_thumb_start);
        self.position = if position == 0 && output.truncated {
            Position::Head
        } else {
            let Some(source) = output.display_source_at(position) else {
                return;
            };
            Position::Source(source)
        };
        self.pending.clear();
        self.target = None;
        self.refresh(output, scrollbar.viewport_length);
        self.apply_selection();
        self.update_scrollbar(output, scrollbar.viewport_length);
    }

    fn refresh(&mut self, output: &RetainedOutput, pane_rows: usize) {
        match self.position {
            Position::Following => self.visible = output.display_lines(pane_rows),
            Position::Head => self.refresh_anchor(output, pane_rows, None),
            Position::Source(source) => self.refresh_anchor(output, pane_rows, Some(source)),
        }
    }

    fn refresh_anchor(
        &mut self,
        output: &RetainedOutput,
        pane_rows: usize,
        anchor: Option<(u64, usize)>,
    ) {
        let limit = pane_rows.saturating_add(1);
        let Some(mut lines) = output.display_window_from(anchor, limit) else {
            self.position = Position::Head;
            self.refresh_anchor(output, pane_rows, None);
            return;
        };
        if lines.len() <= pane_rows && !self.held {
            self.position = Position::Following;
            self.visible = output.display_lines(pane_rows);
            return;
        }
        lines.truncate(pane_rows);
        self.visible = lines;
    }

    fn apply_navigation(&mut self, output: &RetainedOutput, pane_rows: usize) {
        let head = output
            .display_position(None)
            .expect("the retained head always has a display position");
        let total = head.1;
        if total == 0 {
            self.follow();
            return;
        }
        let height = pane_rows.min(total);
        let max_start = total - height;
        let mut start = match self.position {
            Position::Following => max_start,
            Position::Head => head.0,
            Position::Source(source) => output
                .display_position(Some(source))
                .map_or(head.0, |(position, _)| position),
        }
        .min(max_start);
        let mut last_direction = None;
        for movement in self.pending.drain(..) {
            let previous = start;
            if movement.direction < 0 {
                start = start.saturating_sub(movement.lines);
            } else {
                start = start.saturating_add(movement.lines).min(max_start);
            }
            if start != previous {
                last_direction = Some(movement.direction);
            }
        }
        self.place(output, height, start, max_start);
        if last_direction.is_some() {
            self.extend_drag_selection();
        }
    }

    fn place_target(&mut self, output: &RetainedOutput, pane_rows: usize, source: (u64, usize)) {
        let (_, total) = output
            .display_position(None)
            .expect("the retained head always has a display position");
        if total == 0 {
            self.follow();
            return;
        }
        let Some((index, _)) = output.display_position(Some(source)) else {
            self.position = Position::Head;
            self.refresh(output, pane_rows);
            return;
        };
        let height = pane_rows.min(total);
        let max_start = total - height;
        let start = index.saturating_sub(height / 2).min(max_start);
        self.place(output, height, start, max_start);
    }

    fn place(&mut self, output: &RetainedOutput, height: usize, start: usize, max_start: usize) {
        if start == max_start && !self.held {
            self.position = Position::Following;
        } else if start == 0 && output.truncated {
            self.position = Position::Head;
        } else {
            self.position = Position::Source(
                output
                    .display_source_at(start)
                    .expect("only the retained-head notice lacks a source"),
            );
        }
        self.visible = match self.position {
            Position::Following => output.display_lines(height),
            Position::Head => output
                .display_window_from(None, height)
                .expect("the retained head is available"),
            Position::Source(source) => output
                .display_window_from(Some(source), height)
                .expect("the selected source is available"),
        };
    }

    fn selection_point(
        &self,
        row: usize,
        column: usize,
        include_character: bool,
    ) -> Option<SelectionPoint> {
        let row = row.min(self.visible.len().checked_sub(1)?);
        let line = &self.visible[row];
        Some(SelectionPoint {
            source: line.source?,
            byte: byte_at_column(&line.text, column, include_character),
        })
    }

    fn selection_range(&self) -> Option<(SelectionPoint, SelectionPoint)> {
        let selection = self.selection?;
        Some(if selection.anchor <= selection.cursor {
            (selection.anchor, selection.cursor)
        } else {
            (selection.cursor, selection.anchor)
        })
    }

    fn selection_is_retained(&self, output: &RetainedOutput) -> bool {
        let Some(selection) = self.selection else {
            return true;
        };
        [selection.anchor.source, selection.cursor.source]
            .into_iter()
            .all(|source| output.display_window_from(Some(source), 1).is_some())
    }

    fn extend_drag_selection(&mut self) {
        let Some(selection) = self.selection else {
            return;
        };
        if !selection.dragging {
            return;
        }
        let Some(line) = self.visible.get(selection.cursor_row) else {
            return;
        };
        let Some(source) = line.source else {
            return;
        };
        let cursor = SelectionPoint {
            source,
            byte: byte_at_column(&line.text, selection.cursor_column, true),
        };
        self.selection.as_mut().expect("selection exists").cursor = cursor;
    }

    fn apply_selection(&mut self) {
        for line in &mut self.visible {
            line.selection = None;
        }
        let Some((start, end)) = self.selection_range() else {
            return;
        };
        for line in &mut self.visible {
            let Some(source) = line.source else {
                continue;
            };
            if source < start.source || source > end.source {
                continue;
            }
            let from = if source == start.source {
                start.byte
            } else {
                0
            };
            let to = if source == end.source {
                end.byte
            } else {
                line.text.len()
            };
            if from < to {
                line.selection = Some((from.min(line.text.len()), to.min(line.text.len())));
            }
        }
    }
}

pub(crate) fn scrollbar_thumb(scrollbar: ConsoleScrollbar, track_rows: usize) -> (usize, usize) {
    if track_rows == 0 {
        return (0, 0);
    }
    // Ratatui does not expose rendered thumb geometry. Keep pointer hit
    // testing equal to Ratatui 0.30's rounded length and clamped start.
    let max_position = scrollbar.content_length.saturating_sub(1);
    let position = scrollbar.position.min(max_position);
    let total_length = max_position.saturating_add(scrollbar.viewport_length);
    if total_length == 0 {
        return (0, track_rows);
    }
    let thumb_length =
        rounded_scale(scrollbar.viewport_length, track_rows, total_length).clamp(1, track_rows);
    let thumb_start = rounded_scale(position, track_rows, total_length)
        .min(track_rows.saturating_sub(thumb_length));
    (thumb_start, thumb_length)
}

fn rounded_scale(value: usize, target: usize, source: usize) -> usize {
    if source == 0 {
        return 0;
    }
    let numerator = value as u128 * target as u128;
    ((numerator + source as u128 / 2) / source as u128) as usize
}

pub(crate) fn scale(value: usize, target: usize, source: usize) -> usize {
    if source == 0 {
        return 0;
    }
    ((value as u128 * target as u128) / source as u128) as usize
}

fn byte_at_column(text: &str, column: usize, include_character: bool) -> usize {
    let mut width = 0;
    let mut characters = text.char_indices().peekable();
    while let Some((byte, character)) = characters.next() {
        let next = width + UnicodeWidthChar::width(character).unwrap_or(0);
        if column < next {
            if !include_character {
                return byte;
            }
            let mut end = byte + character.len_utf8();
            while let Some((next_byte, next_character)) = characters.peek().copied()
                && UnicodeWidthChar::width(next_character).unwrap_or(0) == 0
            {
                characters.next();
                end = next_byte + next_character.len_utf8();
            }
            return end;
        }
        width = next;
    }
    text.len()
}

#[cfg(test)]
mod tests {
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
    fn evicted_anchor_moves_to_the_new_retained_head() {
        let mut scroll = PipeScroll::default();
        let first = output(0..25, 1);
        scroll.scroll_lines(5, -1);
        scroll.window(&first, 20);

        let mut later = output(5..30, 2);
        later.truncated = true;
        let visible = scroll.window(&later, 20);

        assert!(visible[0].marker);
        assert_eq!(visible[0].source, None);
        assert_eq!(visible[1].source, Some((5, 0)));
        assert!(!scroll.following());
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
}
