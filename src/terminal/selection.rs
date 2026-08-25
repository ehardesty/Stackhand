use std::time::Duration;

use libghostty_vt::fmt::Format;
use libghostty_vt::selection::FormatOptions;
use libghostty_vt::selection::gesture::{
    Autoscroll, AutoscrollTickEvent, DragEvent, Geometry, Gesture, PressEvent, ReleaseEvent,
};
use libghostty_vt::terminal::{Point, PointCoordinate, Terminal};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectionPoint {
    pub col: u16,
    pub surface_row: i32,
}

pub struct SelectionController {
    gesture: Gesture<'static>,
    press: PressEvent<'static>,
    drag: DragEvent<'static>,
    release: ReleaseEvent<'static>,
    tick: AutoscrollTickEvent<'static>,
    last_drag: Option<SelectionPoint>,
}

impl SelectionController {
    pub fn new() -> Result<Self, libghostty_vt::Error> {
        Ok(Self {
            gesture: Gesture::new()?,
            press: PressEvent::new()?,
            drag: DragEvent::new()?,
            release: ReleaseEvent::new()?,
            tick: AutoscrollTickEvent::new()?,
            last_drag: None,
        })
    }

    pub fn press(
        &mut self,
        terminal: &Terminal<'_, '_>,
        point: SelectionPoint,
        time: Duration,
    ) -> Result<(), libghostty_vt::Error> {
        let reference = terminal.grid_ref(viewport_point(terminal, point))?;
        self.press
            .set_position(
                f64::from(point.col) + 0.5,
                f64::from(point.surface_row) + 0.5,
            )?
            .set_time(time)?
            .set_repeat_distance(1.0)?
            .set_repeat_interval(Duration::from_millis(500))?;
        let selection = self.press.apply(&mut self.gesture, terminal, reference)?;
        terminal.set_selection(selection.as_ref())?;
        Ok(())
    }

    pub fn drag(
        &mut self,
        terminal: &mut Terminal<'_, '_>,
        point: SelectionPoint,
    ) -> Result<(), libghostty_vt::Error> {
        let reference = terminal.grid_ref(viewport_point(terminal, point))?;
        self.drag.set_position(
            f64::from(point.col) + 0.5,
            f64::from(point.surface_row) + 0.5,
        )?;
        let selection =
            self.drag
                .apply(&mut self.gesture, terminal, reference, geometry(terminal))?;
        terminal.set_selection(selection.as_ref())?;
        self.last_drag = Some(point);
        self.tick_autoscroll(terminal)?;
        Ok(())
    }

    pub fn tick_autoscroll(
        &mut self,
        terminal: &mut Terminal<'_, '_>,
    ) -> Result<bool, libghostty_vt::Error> {
        let direction = self.gesture.autoscroll(terminal)?;
        let Some(point) = self.last_drag else {
            return Ok(false);
        };
        if direction == Autoscroll::None {
            return Ok(false);
        }
        let viewport = PointCoordinate {
            x: point.col.min(terminal.cols()?.saturating_sub(1)),
            y: clamped_row(terminal, point.surface_row),
        };
        self.tick.set_position(
            f64::from(point.col) + 0.5,
            f64::from(point.surface_row) + 0.5,
        )?;
        let selection =
            self.tick
                .apply(&mut self.gesture, terminal, viewport, geometry(terminal))?;
        terminal.set_selection(selection.as_ref())?;
        Ok(true)
    }

    pub fn release(
        &mut self,
        terminal: &Terminal<'_, '_>,
        point: SelectionPoint,
    ) -> Result<(), libghostty_vt::Error> {
        let reference = terminal.grid_ref(viewport_point(terminal, point)).ok();
        self.release.apply(&mut self.gesture, terminal, reference)?;
        self.last_drag = None;
        Ok(())
    }

    pub fn select_all(&mut self, terminal: &Terminal<'_, '_>) -> Result<(), libghostty_vt::Error> {
        let selection = terminal.select_all()?;
        terminal.set_selection(selection.as_ref())?;
        Ok(())
    }

    pub fn clear(&mut self, terminal: &Terminal<'_, '_>) -> Result<(), libghostty_vt::Error> {
        self.gesture.reset(terminal);
        self.last_drag = None;
        terminal.set_selection(None)?;
        Ok(())
    }

    pub fn text(
        &self,
        terminal: &Terminal<'_, '_>,
    ) -> Result<Option<String>, libghostty_vt::Error> {
        let options = FormatOptions::new()
            .with_emit_format(Format::Plain)
            .with_unwrap(true)
            .with_trim(true);
        Ok(terminal
            .format_selection_alloc(None, options)?
            .map(|bytes| String::from_utf8_lossy(bytes.as_ref()).into_owned()))
    }
}

fn geometry(terminal: &Terminal<'_, '_>) -> Geometry {
    Geometry {
        columns: u32::from(terminal.cols().unwrap_or(1).max(1)),
        cell_width: 1,
        padding_left: 0,
        screen_height: u32::from(terminal.rows().unwrap_or(1).max(1)),
    }
}

fn viewport_point(terminal: &Terminal<'_, '_>, point: SelectionPoint) -> Point {
    Point::Viewport(PointCoordinate {
        x: point
            .col
            .min(terminal.cols().unwrap_or(1).saturating_sub(1)),
        y: clamped_row(terminal, point.surface_row),
    })
}

fn clamped_row(terminal: &Terminal<'_, '_>, row: i32) -> u32 {
    let last = i32::from(terminal.rows().unwrap_or(1).saturating_sub(1));
    u32::try_from(row.clamp(0, last)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
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
}
