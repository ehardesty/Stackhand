/// Scroll position of one pipe output pane. `offset` counts lines up from
/// the live tail; zero means following. One view per Process: scrolling or
/// re-following one pane never changes another Process's view.
use crate::tui::PipeLine;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PipeScroll {
    offset: usize,
}
/// The pipe scroll offset stops growing this far above the tail. Scrolling
/// stays bounded work even after the user pages for a long time.
const PIPE_MAX_SCROLL_LINES: usize = 4096;

impl PipeScroll {
    /// Lines the visible window sits above the live tail.
    pub fn offset(self) -> usize {
        self.offset
    }

    /// True while the pane follows the live tail.
    pub fn following(self) -> bool {
        self.offset == 0
    }

    /// Page the view; a negative direction moves toward the head.
    pub fn scroll_page(&mut self, page_rows: u16, direction: isize) {
        let page = isize::try_from(page_rows.saturating_sub(1).max(1))
            .expect("u16 page size always fits in isize");
        if direction < 0 {
            self.offset = (self.offset.saturating_add(page as usize)).min(PIPE_MAX_SCROLL_LINES);
        } else {
            self.offset = self.offset.saturating_sub(page as usize);
        }
    }

    /// Move by a small line count. A negative direction moves toward the
    /// retained head. Mouse-wheel input uses this instead of page jumps.
    pub fn scroll_lines(&mut self, lines: usize, direction: isize) {
        if direction < 0 {
            self.offset = self.offset.saturating_add(lines).min(PIPE_MAX_SCROLL_LINES);
        } else {
            self.offset = self.offset.saturating_sub(lines);
        }
    }

    /// Return the pane to the live tail.
    pub fn follow(&mut self) {
        self.offset = 0;
    }

    /// The visible window of the newest retained lines for the pane's
    /// height. A view scrolled past the head shows the head, never an
    /// empty pane.
    pub fn window<'a>(&self, lines: &'a [PipeLine], pane_rows: usize) -> &'a [PipeLine] {
        let len = lines.len();
        if len == 0 {
            return &[];
        }
        let h = pane_rows.min(len);
        let bottom = if self.offset >= len {
            h - 1
        } else {
            len - 1 - self.offset
        };
        let start = bottom.saturating_sub(h - 1);
        &lines[start..=bottom]
    }
}

#[cfg(test)]
mod tests {
    use crate::pipe_scroll::PipeScroll;
    use crate::tui::PipeLine;

    #[test]
    fn pipe_scroll_window_counts_lines_up_from_the_live_tail() {
        let lines: Vec<PipeLine> = (0..100)
            .map(|n| PipeLine {
                text: format!("line-{n:03}"),
                marker: n % 50 == 0,
            })
            .collect();
        let mut scroll = PipeScroll::default();

        // Following: the newest pane height lines.
        assert_eq!(scroll.window(&lines, 20), &lines[80..100]);

        // One page up: the window ends 19 lines before the tail.
        scroll.scroll_page(20, -1);
        assert_eq!(scroll.window(&lines, 20), &lines[61..=80]);

        // Scrolling past the head shows the head, never an empty pane.
        for _ in 0..10 {
            scroll.scroll_page(20, -1);
        }
        assert_eq!(scroll.window(&lines, 20), &lines[0..20]);
        scroll.scroll_page(20, 1);
        assert_eq!(scroll.window(&lines, 20), &lines[0..20]);

        // Follow returns to the tail.
        scroll.follow();
        assert_eq!(scroll.window(&lines, 20), &lines[80..100]);
    }
}
