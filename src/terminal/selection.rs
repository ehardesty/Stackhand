use std::time::Duration;

use libghostty_vt::fmt::Format;
use libghostty_vt::screen::TrackedGridRef;
use libghostty_vt::selection::gesture::{
    Autoscroll, AutoscrollTickEvent, DragEvent, Geometry, Gesture, PressEvent, ReleaseEvent,
};
use libghostty_vt::selection::{Adjustment, FormatOptions, Selection};
use libghostty_vt::terminal::{Point, PointCoordinate, PointSpace, ScrollViewport, Terminal};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectionPoint {
    pub col: u16,
    pub surface_row: i32,
}

/// A cell-granular direction for keyboard copy navigation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum SelectionDirection {
    Left,
    Right,
    Up,
    Down,
}

impl From<SelectionDirection> for Adjustment {
    fn from(direction: SelectionDirection) -> Self {
        match direction {
            SelectionDirection::Left => Self::Left,
            SelectionDirection::Right => Self::Right,
            SelectionDirection::Up => Self::Up,
            SelectionDirection::Down => Self::Down,
        }
    }
}

pub struct SelectionController {
    gesture: Gesture<'static>,
    press: PressEvent<'static>,
    drag: DragEvent<'static>,
    release: ReleaseEvent<'static>,
    tick: AutoscrollTickEvent<'static>,
    last_drag: Option<SelectionPoint>,
    keyboard_active: bool,
    endpoint_active: bool,
    rectangle: bool,
    anchor: Option<TrackedGridRef>,
    cursor: Option<TrackedGridRef>,
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
            keyboard_active: false,
            endpoint_active: false,
            rectangle: false,
            anchor: None,
            cursor: None,
        })
    }

    pub fn enter_keyboard_navigation(
        &mut self,
        terminal: &mut Terminal<'_, '_>,
    ) -> Result<(), libghostty_vt::Error> {
        self.keyboard_active = true;
        if self.cursor.as_ref().is_some_and(|cursor| {
            tracked_viewport_point(terminal, cursor)
                .ok()
                .flatten()
                .is_some()
        }) {
            return Ok(());
        }

        let cursor = terminal
            .track_grid_ref(Point::Active(PointCoordinate {
                x: terminal.cursor_x()?.min(terminal.cols()?.saturating_sub(1)),
                y: u32::from(terminal.cursor_y()?.min(terminal.rows()?.saturating_sub(1))),
            }))
            .ok()
            .filter(|cursor| {
                tracked_viewport_point(terminal, cursor)
                    .ok()
                    .flatten()
                    .is_some()
            })
            .map(Ok)
            .unwrap_or_else(|| track_viewport_point(terminal, bottom_left(terminal)))?;
        self.cursor = Some(cursor);
        Ok(())
    }

    pub fn toggle_keyboard_endpoint(
        &mut self,
        terminal: &mut Terminal<'_, '_>,
    ) -> Result<(), libghostty_vt::Error> {
        self.enter_keyboard_navigation(terminal)?;
        if self.endpoint_active {
            self.endpoint_active = false;
            self.anchor = None;
            return Ok(());
        }

        let point = self
            .keyboard_cursor(terminal)?
            .unwrap_or_else(|| bottom_left(terminal));
        let anchor = track_viewport_point(terminal, point)?;
        let cursor = track_viewport_point(terminal, point)?;
        let start = anchor
            .snapshot(terminal)?
            .ok_or(libghostty_vt::Error::InvalidValue)?;
        let end = cursor
            .snapshot(terminal)?
            .ok_or(libghostty_vt::Error::InvalidValue)?;
        let selection = Selection::new(start, end, false);
        terminal.set_selection(Some(&selection))?;
        self.anchor = Some(anchor);
        self.cursor = Some(cursor);
        self.endpoint_active = true;
        self.rectangle = false;
        Ok(())
    }

    pub fn move_keyboard_cursor(
        &mut self,
        terminal: &mut Terminal<'_, '_>,
        direction: SelectionDirection,
    ) -> Result<(), libghostty_vt::Error> {
        self.enter_keyboard_navigation(terminal)?;
        if self.endpoint_active {
            return self.extend_keyboard_selection(terminal, direction);
        }

        let current = self
            .keyboard_cursor(terminal)?
            .unwrap_or_else(|| bottom_left(terminal));
        let next = moved_in_viewport(terminal, current, direction);
        if let Some(cursor) = self.cursor.as_mut() {
            let next = viewport_point(terminal, next);
            cursor.set(terminal, next)?;
        } else {
            self.cursor = Some(track_viewport_point(terminal, next)?);
        }
        Ok(())
    }

    pub fn keyboard_cursor(
        &self,
        terminal: &Terminal<'_, '_>,
    ) -> Result<Option<SelectionPoint>, libghostty_vt::Error> {
        if !self.keyboard_active {
            return Ok(None);
        }
        let Some(cursor) = self.cursor.as_ref() else {
            return Ok(None);
        };
        tracked_viewport_point(terminal, cursor)
    }

    fn extend_keyboard_selection(
        &mut self,
        terminal: &mut Terminal<'_, '_>,
        direction: SelectionDirection,
    ) -> Result<(), libghostty_vt::Error> {
        let Some(anchor) = self.anchor.as_ref() else {
            self.endpoint_active = false;
            return self.toggle_keyboard_endpoint(terminal);
        };
        let Some(cursor) = self.cursor.as_ref() else {
            self.endpoint_active = false;
            return self.toggle_keyboard_endpoint(terminal);
        };
        let cursor_point = tracked_viewport_point(terminal, cursor)?;
        let scroll = match (direction, cursor_point) {
            (SelectionDirection::Up, Some(SelectionPoint { surface_row: 0, .. })) => -1,
            (SelectionDirection::Down, Some(SelectionPoint { surface_row, .. }))
                if surface_row == i32::from(terminal.rows()?.saturating_sub(1)) =>
            {
                1
            }
            _ => 0,
        };
        if scroll != 0 {
            terminal.scroll_viewport(ScrollViewport::Delta(scroll));
            if tracked_viewport_point(terminal, cursor)? == cursor_point {
                return Ok(());
            }
        }
        let start = anchor
            .snapshot(terminal)?
            .ok_or(libghostty_vt::Error::InvalidValue)?;
        let end = cursor
            .snapshot(terminal)?
            .ok_or(libghostty_vt::Error::InvalidValue)?;
        let mut selection = Selection::new(start, end, self.rectangle);
        selection.adjust(terminal, direction.into())?;
        let end_point = terminal
            .point_from_grid_ref(&selection.end(), PointSpace::Screen)?
            .ok_or(libghostty_vt::Error::InvalidValue)?;
        let next_cursor = terminal.track_grid_ref(Point::Screen(end_point))?;
        terminal.set_selection(Some(&selection))?;
        self.cursor = Some(next_cursor);
        Ok(())
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
        self.keyboard_active = false;
        self.last_drag = None;
        if let Some(selection) = selection.as_ref() {
            self.install_selection(terminal, selection)?;
        } else {
            terminal.set_selection(None)?;
            self.anchor = match self.gesture.anchor(terminal)? {
                Some(anchor) => Some(track_grid_ref(terminal, anchor)?),
                None => None,
            };
            self.cursor = Some(track_viewport_point(terminal, point)?);
            self.endpoint_active = false;
            self.rectangle = false;
        }
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
        if let Some(selection) = selection.as_ref() {
            self.install_selection(terminal, selection)?;
        } else {
            terminal.set_selection(None)?;
            self.cursor = Some(track_viewport_point(terminal, point)?);
            self.endpoint_active = false;
        }
        self.keyboard_active = false;
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
        if let Some(selection) = selection.as_ref() {
            self.install_selection(terminal, selection)?;
        } else {
            terminal.set_selection(None)?;
            self.endpoint_active = false;
        }
        Ok(true)
    }

    pub fn release(
        &mut self,
        terminal: &Terminal<'_, '_>,
        point: SelectionPoint,
    ) -> Result<(), libghostty_vt::Error> {
        let reference = terminal.grid_ref(viewport_point(terminal, point)).ok();
        self.release.apply(&mut self.gesture, terminal, reference)?;
        if !self.endpoint_active {
            self.cursor = Some(track_viewport_point(terminal, point)?);
        }
        self.last_drag = None;
        Ok(())
    }

    pub fn select_all(&mut self, terminal: &Terminal<'_, '_>) -> Result<(), libghostty_vt::Error> {
        let selection = terminal.select_all()?;
        if let Some(selection) = selection.as_ref() {
            self.install_selection(terminal, selection)?;
        } else {
            terminal.set_selection(None)?;
            self.anchor = None;
            self.cursor = None;
            self.endpoint_active = false;
        }
        Ok(())
    }

    pub fn clear(&mut self, terminal: &Terminal<'_, '_>) -> Result<(), libghostty_vt::Error> {
        self.gesture.reset(terminal);
        self.last_drag = None;
        self.keyboard_active = false;
        self.endpoint_active = false;
        self.rectangle = false;
        self.anchor = None;
        self.cursor = None;
        terminal.set_selection(None)?;
        Ok(())
    }

    fn install_selection(
        &mut self,
        terminal: &Terminal<'_, '_>,
        selection: &Selection<'_>,
    ) -> Result<(), libghostty_vt::Error> {
        let anchor = track_grid_ref(terminal, selection.start())?;
        let cursor = track_grid_ref(terminal, selection.end())?;
        terminal.set_selection(Some(selection))?;
        self.anchor = Some(anchor);
        self.cursor = Some(cursor);
        self.endpoint_active = true;
        self.rectangle = selection.is_rectangle();
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

fn tracked_viewport_point(
    terminal: &Terminal<'_, '_>,
    tracked: &TrackedGridRef,
) -> Result<Option<SelectionPoint>, libghostty_vt::Error> {
    let Some(point) = tracked.point(PointSpace::Viewport)? else {
        return Ok(None);
    };
    if point.x >= terminal.cols()? || point.y >= u32::from(terminal.rows()?) {
        return Ok(None);
    }
    Ok(Some(SelectionPoint {
        col: point.x,
        surface_row: i32::try_from(point.y).unwrap_or(i32::MAX),
    }))
}

fn track_grid_ref(
    terminal: &Terminal<'_, '_>,
    grid_ref: libghostty_vt::screen::GridRef<'_>,
) -> Result<TrackedGridRef, libghostty_vt::Error> {
    let point = terminal
        .point_from_grid_ref(&grid_ref, PointSpace::Screen)?
        .ok_or(libghostty_vt::Error::InvalidValue)?;
    terminal.track_grid_ref(Point::Screen(point))
}

fn track_viewport_point(
    terminal: &Terminal<'_, '_>,
    point: SelectionPoint,
) -> Result<TrackedGridRef, libghostty_vt::Error> {
    terminal.track_grid_ref(viewport_point(terminal, point))
}

fn bottom_left(terminal: &Terminal<'_, '_>) -> SelectionPoint {
    SelectionPoint {
        col: 0,
        surface_row: i32::from(terminal.rows().unwrap_or(1).saturating_sub(1)),
    }
}

fn moved_in_viewport(
    terminal: &Terminal<'_, '_>,
    point: SelectionPoint,
    direction: SelectionDirection,
) -> SelectionPoint {
    let last_col = terminal.cols().unwrap_or(1).saturating_sub(1);
    let last_row = i32::from(terminal.rows().unwrap_or(1).saturating_sub(1));
    match direction {
        SelectionDirection::Left => SelectionPoint {
            col: point.col.saturating_sub(1),
            ..point
        },
        SelectionDirection::Right => SelectionPoint {
            col: point.col.saturating_add(1).min(last_col),
            ..point
        },
        SelectionDirection::Up => SelectionPoint {
            surface_row: point.surface_row.saturating_sub(1).max(0),
            ..point
        },
        SelectionDirection::Down => SelectionPoint {
            surface_row: point.surface_row.saturating_add(1).min(last_row),
            ..point
        },
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
#[path = "selection/tests.rs"]
mod tests;
