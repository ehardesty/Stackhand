use std::time::Duration;

use anyhow::Result;
use libghostty_vt::key;
use libghostty_vt::mouse;
use libghostty_vt::terminal::{ScrollViewport, Terminal};

use super::selection::{SelectionController, SelectionPoint};

const WHEEL_SCROLL_LINES: isize = 3;

pub(super) fn stackhand_wheel_delta(event: TerminalMouseEvent) -> Option<isize> {
    if !event.stackhand_owned {
        return None;
    }
    match event.kind {
        MouseKind::WheelUp => Some(-WHEEL_SCROLL_LINES),
        MouseKind::WheelDown => Some(WHEEL_SCROLL_LINES),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseKind {
    Press(MouseButton),
    Release(MouseButton),
    Drag(MouseButton),
    Motion,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MouseModifiers {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalMouseEvent {
    pub kind: MouseKind,
    pub point: SelectionPoint,
    pub modifiers: MouseModifiers,
    pub stackhand_owned: bool,
    pub time: Duration,
}

pub struct MouseController {
    encoder: mouse::Encoder<'static>,
    pressed: Option<MouseButton>,
    selection_active: bool,
}

impl MouseController {
    pub fn new() -> Result<Self, libghostty_vt::Error> {
        Ok(Self {
            encoder: mouse::Encoder::new()?,
            pressed: None,
            selection_active: false,
        })
    }

    pub fn apply(
        &mut self,
        terminal: &mut Terminal<'_, '_>,
        selection: &mut SelectionController,
        event: TerminalMouseEvent,
    ) -> Result<Vec<u8>> {
        if !event.stackhand_owned {
            self.selection_active = false;
            return self.encode_child(terminal, event).map_err(Into::into);
        }

        self.apply_stackhand(terminal, selection, event)?;
        Ok(Vec::new())
    }

    fn apply_stackhand(
        &mut self,
        terminal: &mut Terminal<'_, '_>,
        selection: &mut SelectionController,
        event: TerminalMouseEvent,
    ) -> Result<(), libghostty_vt::Error> {
        match event.kind {
            MouseKind::Press(button) => {
                if button == MouseButton::Left {
                    selection.press(terminal, event.point, event.time)?;
                    self.selection_active = true;
                }
            }
            MouseKind::Drag(MouseButton::Left) if self.selection_active => {
                selection.drag(terminal, event.point)?;
            }
            MouseKind::Release(button) => {
                if button == MouseButton::Left && self.selection_active {
                    selection.release(terminal, event.point)?;
                }
                self.selection_active = false;
            }
            MouseKind::WheelUp => {
                terminal.scroll_viewport(ScrollViewport::Delta(-WHEEL_SCROLL_LINES));
            }
            MouseKind::WheelDown => {
                terminal.scroll_viewport(ScrollViewport::Delta(WHEEL_SCROLL_LINES));
            }
            _ => {}
        }
        Ok(())
    }

    fn encode_child(
        &mut self,
        terminal: &Terminal<'_, '_>,
        event: TerminalMouseEvent,
    ) -> Result<Vec<u8>, libghostty_vt::Error> {
        match event.kind {
            MouseKind::Press(button) => self.pressed = Some(button),
            MouseKind::Drag(button) => self.pressed = Some(button),
            MouseKind::Release(_) => self.pressed = None,
            _ => {}
        }

        let (action, button) = mouse_action_and_button(event.kind, self.pressed);
        let mut input = mouse::Event::new()?;
        input
            .set_action(action)
            .set_button(button)
            .set_mods(mouse_modifiers(event.modifiers))
            .set_position(mouse::Position {
                x: event.point.col as f32 + 0.5,
                y: event.point.surface_row as f32 + 0.5,
            });

        let cols = u32::from(terminal.cols()?);
        let rows = u32::from(terminal.rows()?);
        self.encoder
            .set_options_from_terminal(terminal)
            .set_size(mouse::EncoderSize {
                screen_width: cols,
                screen_height: rows,
                cell_width: 1,
                cell_height: 1,
                padding_top: 0,
                padding_bottom: 0,
                padding_right: 0,
                padding_left: 0,
            })
            .set_any_button_pressed(self.pressed.is_some())
            .set_track_last_cell(true);

        let mut bytes = Vec::new();
        self.encoder.encode_to_vec(&input, &mut bytes)?;
        Ok(bytes)
    }
}

fn mouse_action_and_button(
    kind: MouseKind,
    pressed: Option<MouseButton>,
) -> (mouse::Action, Option<mouse::Button>) {
    match kind {
        MouseKind::Press(button) => (mouse::Action::Press, Some(mouse_button(button))),
        MouseKind::Release(button) => (mouse::Action::Release, Some(mouse_button(button))),
        MouseKind::Drag(button) => (mouse::Action::Motion, Some(mouse_button(button))),
        MouseKind::Motion => (mouse::Action::Motion, pressed.map(mouse_button)),
        MouseKind::WheelUp => (mouse::Action::Press, Some(mouse::Button::Four)),
        MouseKind::WheelDown => (mouse::Action::Press, Some(mouse::Button::Five)),
        MouseKind::WheelLeft => (mouse::Action::Press, Some(mouse::Button::Six)),
        MouseKind::WheelRight => (mouse::Action::Press, Some(mouse::Button::Seven)),
    }
}

fn mouse_button(button: MouseButton) -> mouse::Button {
    match button {
        MouseButton::Left => mouse::Button::Left,
        MouseButton::Middle => mouse::Button::Middle,
        MouseButton::Right => mouse::Button::Right,
    }
}

fn mouse_modifiers(modifiers: MouseModifiers) -> key::Mods {
    let mut result = key::Mods::empty();
    if modifiers.shift {
        result |= key::Mods::SHIFT;
    }
    if modifiers.control {
        result |= key::Mods::CTRL;
    }
    if modifiers.alt {
        result |= key::Mods::ALT;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use libghostty_vt::terminal::Options;

    fn terminal() -> Terminal<'static, 'static> {
        Terminal::new(Options {
            cols: 20,
            rows: 4,
            max_scrollback: 64 * 1_024,
        })
        .unwrap()
    }

    fn event(kind: MouseKind, col: u16, row: i32, stackhand_owned: bool) -> TerminalMouseEvent {
        TerminalMouseEvent {
            kind,
            point: SelectionPoint {
                col,
                surface_row: row,
            },
            modifiers: MouseModifiers::default(),
            stackhand_owned,
            time: Duration::from_millis(10),
        }
    }

    #[test]
    fn only_stackhand_owned_vertical_wheels_become_viewport_scrolls() {
        assert_eq!(
            stackhand_wheel_delta(event(MouseKind::WheelUp, 0, 0, true)),
            Some(-WHEEL_SCROLL_LINES)
        );
        assert_eq!(
            stackhand_wheel_delta(event(MouseKind::WheelDown, 0, 0, true)),
            Some(WHEEL_SCROLL_LINES)
        );
        assert_eq!(
            stackhand_wheel_delta(event(MouseKind::WheelUp, 0, 0, false)),
            None,
            "child-owned wheel events must keep their terminal protocol bytes"
        );
    }

    #[test]
    fn tracking_off_uses_stackhand_selection_and_sends_no_bytes() {
        let mut terminal = terminal();
        terminal.vt_write(b"alpha beta");
        let mut selection = SelectionController::new().unwrap();
        let mut mouse = MouseController::new().unwrap();

        assert!(
            mouse
                .apply(
                    &mut terminal,
                    &mut selection,
                    event(MouseKind::Press(MouseButton::Left), 0, 0, true),
                )
                .unwrap()
                .is_empty()
        );
        mouse
            .apply(
                &mut terminal,
                &mut selection,
                event(MouseKind::Drag(MouseButton::Left), 5, 0, true),
            )
            .unwrap();
        mouse
            .apply(
                &mut terminal,
                &mut selection,
                event(MouseKind::Release(MouseButton::Left), 5, 0, true),
            )
            .unwrap();

        assert_eq!(selection.text(&terminal).unwrap().as_deref(), Some("alpha"));
    }

    #[test]
    fn tracking_off_wheel_moves_stackhand_history() {
        let mut terminal = terminal();
        for index in 0..20 {
            terminal.vt_write(format!("line-{index}\r\n").as_bytes());
        }
        let mut selection = SelectionController::new().unwrap();
        let mut mouse = MouseController::new().unwrap();
        let before = terminal.scrollbar().unwrap().offset;

        let bytes = mouse
            .apply(
                &mut terminal,
                &mut selection,
                event(MouseKind::WheelUp, 0, 0, true),
            )
            .unwrap();

        assert!(bytes.is_empty());
        assert!(terminal.scrollbar().unwrap().offset < before);
    }

    #[test]
    fn stackhand_ownership_suppresses_child_sgr_bytes() {
        let mut terminal = terminal();
        terminal.vt_write(b"\x1b[?1003h\x1b[?1006hcontent");
        let mut selection = SelectionController::new().unwrap();
        let mut mouse = MouseController::new().unwrap();

        let bytes = mouse
            .apply(
                &mut terminal,
                &mut selection,
                event(MouseKind::Press(MouseButton::Left), 2, 1, true),
            )
            .unwrap();

        assert!(terminal.is_mouse_tracking().unwrap());
        assert!(bytes.is_empty());
    }

    #[test]
    fn stackhand_owned_drag_stays_in_selection_after_tracking_turns_on() {
        let mut terminal = terminal();
        terminal.vt_write(b"alpha beta");
        let mut selection = SelectionController::new().unwrap();
        let mut mouse = MouseController::new().unwrap();
        mouse
            .apply(
                &mut terminal,
                &mut selection,
                event(MouseKind::Press(MouseButton::Left), 0, 0, true),
            )
            .unwrap();
        terminal.vt_write(b"\x1b[?1003h\x1b[?1006h");

        let bytes = mouse
            .apply(
                &mut terminal,
                &mut selection,
                event(MouseKind::Drag(MouseButton::Left), 5, 0, true),
            )
            .unwrap();

        assert!(bytes.is_empty());
    }

    #[test]
    fn child_owned_gesture_refreshes_the_protocol_format_before_release() {
        let mut terminal = terminal();
        terminal.vt_write(b"\x1b[?1003h\x1b[?1006h");
        let mut selection = SelectionController::new().unwrap();
        let mut mouse = MouseController::new().unwrap();
        let press = mouse
            .apply(
                &mut terminal,
                &mut selection,
                event(MouseKind::Press(MouseButton::Left), 2, 1, false),
            )
            .unwrap();
        assert_eq!(press, b"\x1b[<0;3;2M");
        terminal.vt_write(b"\x1b[?1006l\x1b[?1015h");

        let release = mouse
            .apply(
                &mut terminal,
                &mut selection,
                event(MouseKind::Release(MouseButton::Left), 2, 1, false),
            )
            .unwrap();

        assert_eq!(release, b"\x1b[35;3;2M");
    }
}
