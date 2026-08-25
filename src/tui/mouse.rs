use std::time::Duration;

use crossterm::event::{KeyModifiers, MouseButton as HostMouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use super::ConsoleViewMode;
use crate::terminal::{MouseButton, MouseKind, MouseModifiers, SelectionPoint, TerminalMouseEvent};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MouseRoute {
    pub event: TerminalMouseEvent,
    pub stackhand_owns: bool,
    pub changes_history_view: bool,
    pub stackhand_gesture_active: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GestureOwner {
    Stackhand,
    Child,
}

#[derive(Default)]
pub struct MouseRouter {
    gesture_owner: Option<GestureOwner>,
}

impl MouseRouter {
    pub fn route(
        &mut self,
        mouse: MouseEvent,
        area: Rect,
        mode: ConsoleViewMode,
        child_tracking: bool,
        time: Duration,
    ) -> Option<MouseRoute> {
        if area.width == 0 || area.height == 0 {
            return None;
        }

        let captured_gesture =
            matches!(mouse.kind, MouseEventKind::Drag(_) | MouseEventKind::Up(_));
        let inside = mouse.column >= area.x
            && mouse.column < area.right()
            && mouse.row >= area.y
            && mouse.row < area.bottom();
        if !inside && !captured_gesture {
            return None;
        }

        let current_owner = if mode != ConsoleViewMode::ChildInput
            || mouse.modifiers.contains(KeyModifiers::SHIFT)
            || !child_tracking
        {
            GestureOwner::Stackhand
        } else {
            GestureOwner::Child
        };
        let (owner, gesture_active_after_event) = match mouse.kind {
            MouseEventKind::Down(_) => {
                self.gesture_owner = Some(current_owner);
                (current_owner, true)
            }
            MouseEventKind::Drag(_) => (
                self.gesture_owner.unwrap_or(current_owner),
                self.gesture_owner.is_some(),
            ),
            MouseEventKind::Up(_) => {
                let owner = self.gesture_owner.take().unwrap_or(current_owner);
                (owner, false)
            }
            _ => (current_owner, false),
        };
        let stackhand_owns = owner == GestureOwner::Stackhand;
        Some(MouseRoute {
            event: TerminalMouseEvent {
                kind: mouse_kind(mouse.kind),
                point: SelectionPoint {
                    col: mouse.column.saturating_sub(area.x).min(area.width - 1),
                    surface_row: i32::from(mouse.row) - i32::from(area.y),
                },
                modifiers: MouseModifiers {
                    shift: mouse.modifiers.contains(KeyModifiers::SHIFT),
                    control: mouse.modifiers.contains(KeyModifiers::CONTROL),
                    alt: mouse.modifiers.contains(KeyModifiers::ALT),
                },
                application_owned: stackhand_owns,
                time,
            },
            stackhand_owns,
            changes_history_view: stackhand_owns
                && matches!(
                    mouse.kind,
                    MouseEventKind::Down(HostMouseButton::Left)
                        | MouseEventKind::Drag(HostMouseButton::Left)
                        | MouseEventKind::Up(HostMouseButton::Left)
                        | MouseEventKind::ScrollUp
                        | MouseEventKind::ScrollDown
                ),
            stackhand_gesture_active: stackhand_owns && gesture_active_after_event,
        })
    }
}

fn mouse_kind(kind: MouseEventKind) -> MouseKind {
    match kind {
        MouseEventKind::Down(button) => MouseKind::Press(mouse_button(button)),
        MouseEventKind::Up(button) => MouseKind::Release(mouse_button(button)),
        MouseEventKind::Drag(button) => MouseKind::Drag(mouse_button(button)),
        MouseEventKind::Moved => MouseKind::Motion,
        MouseEventKind::ScrollUp => MouseKind::WheelUp,
        MouseEventKind::ScrollDown => MouseKind::WheelDown,
        MouseEventKind::ScrollLeft => MouseKind::WheelLeft,
        MouseEventKind::ScrollRight => MouseKind::WheelRight,
    }
}

fn mouse_button(button: HostMouseButton) -> MouseButton {
    match button {
        HostMouseButton::Left => MouseButton::Left,
        HostMouseButton::Middle => MouseButton::Middle,
        HostMouseButton::Right => MouseButton::Right,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: MouseEventKind, modifiers: KeyModifiers) -> MouseEvent {
        MouseEvent {
            kind,
            column: 12,
            row: 7,
            modifiers,
        }
    }

    fn route(
        router: &mut MouseRouter,
        event: MouseEvent,
        mode: ConsoleViewMode,
        child_tracking: bool,
    ) -> MouseRoute {
        router
            .route(
                event,
                Rect::new(10, 5, 20, 4),
                mode,
                child_tracking,
                Duration::ZERO,
            )
            .unwrap()
    }

    #[test]
    fn shift_override_keeps_a_tracked_child_from_owning_the_mouse() {
        let route = MouseRouter::default()
            .route(
                event(
                    MouseEventKind::Down(HostMouseButton::Left),
                    KeyModifiers::SHIFT,
                ),
                Rect::new(10, 5, 20, 4),
                ConsoleViewMode::ChildInput,
                true,
                Duration::ZERO,
            )
            .unwrap();

        assert!(route.event.application_owned);
        assert!(route.stackhand_owns);
        assert!(route.changes_history_view);
        assert_eq!(
            route.event.point,
            SelectionPoint {
                col: 2,
                surface_row: 2
            }
        );
    }

    #[test]
    fn tracked_child_owns_unmodified_motion() {
        let route = MouseRouter::default()
            .route(
                event(MouseEventKind::Moved, KeyModifiers::NONE),
                Rect::new(10, 5, 20, 4),
                ConsoleViewMode::ChildInput,
                true,
                Duration::ZERO,
            )
            .unwrap();

        assert!(!route.event.application_owned);
        assert!(!route.stackhand_owns);
        assert!(!route.changes_history_view);
    }

    #[test]
    fn app_modes_keep_mouse_ownership() {
        for mode in [
            ConsoleViewMode::AppCommand,
            ConsoleViewMode::Scroll,
            ConsoleViewMode::Selection,
        ] {
            let route = MouseRouter::default()
                .route(
                    event(MouseEventKind::ScrollUp, KeyModifiers::NONE),
                    Rect::new(10, 5, 20, 4),
                    mode,
                    true,
                    Duration::ZERO,
                )
                .unwrap();
            assert!(route.stackhand_owns, "mode {mode:?} lost mouse ownership");
        }
    }

    #[test]
    fn release_outside_the_pane_still_reaches_the_active_owner() {
        let release = MouseEvent {
            kind: MouseEventKind::Up(HostMouseButton::Left),
            column: 40,
            row: 4,
            modifiers: KeyModifiers::SHIFT,
        };

        let route = MouseRouter::default()
            .route(
                release,
                Rect::new(10, 5, 20, 4),
                ConsoleViewMode::ChildInput,
                true,
                Duration::ZERO,
            )
            .unwrap();

        assert_eq!(
            route.event.point,
            SelectionPoint {
                col: 19,
                surface_row: -1
            }
        );
        assert!(route.stackhand_owns);
    }

    #[test]
    fn adding_shift_during_a_child_gesture_does_not_change_ui_ownership() {
        let mut router = MouseRouter::default();
        let press = route(
            &mut router,
            event(
                MouseEventKind::Down(HostMouseButton::Left),
                KeyModifiers::NONE,
            ),
            ConsoleViewMode::ChildInput,
            true,
        );
        let drag = route(
            &mut router,
            event(
                MouseEventKind::Drag(HostMouseButton::Left),
                KeyModifiers::SHIFT,
            ),
            ConsoleViewMode::ChildInput,
            true,
        );
        let release = route(
            &mut router,
            event(
                MouseEventKind::Up(HostMouseButton::Left),
                KeyModifiers::SHIFT,
            ),
            ConsoleViewMode::ChildInput,
            true,
        );

        assert!(!press.stackhand_owns);
        assert!(!drag.stackhand_owns);
        assert!(!drag.changes_history_view);
        assert!(!release.stackhand_owns);
        assert!(!release.changes_history_view);
        assert!(!release.stackhand_gesture_active);
    }

    #[test]
    fn releasing_shift_during_a_stackhand_gesture_keeps_ui_ownership() {
        let mut router = MouseRouter::default();
        let press = route(
            &mut router,
            event(
                MouseEventKind::Down(HostMouseButton::Left),
                KeyModifiers::SHIFT,
            ),
            ConsoleViewMode::ChildInput,
            true,
        );
        let drag = route(
            &mut router,
            event(
                MouseEventKind::Drag(HostMouseButton::Left),
                KeyModifiers::NONE,
            ),
            ConsoleViewMode::ChildInput,
            true,
        );
        let release = route(
            &mut router,
            event(
                MouseEventKind::Up(HostMouseButton::Left),
                KeyModifiers::NONE,
            ),
            ConsoleViewMode::ChildInput,
            true,
        );

        assert!(press.stackhand_gesture_active);
        assert!(drag.stackhand_owns);
        assert!(drag.changes_history_view);
        assert!(drag.stackhand_gesture_active);
        assert!(release.stackhand_owns);
        assert!(release.changes_history_view);
        assert!(!release.stackhand_gesture_active);
    }
}
