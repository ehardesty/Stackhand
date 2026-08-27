use anyhow::Result;
use crossterm::event::{Event, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

const INPUT_BATCH_LIMIT: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WheelSurface {
    ProcessList,
    Console,
}

/// Drain one ready burst of Stackhand-owned wheel input before the next
/// application render. The first different event stays ordered for the next
/// loop. Child-owned wheel events remain one-by-one terminal protocol input.
pub(super) fn collect_input_batch(
    first: Event,
    list_area: Rect,
    console_area: Rect,
    mut stackhand_owns_console_wheel: impl FnMut(&MouseEvent) -> bool,
    mut next_ready: impl FnMut() -> Result<Option<Event>>,
) -> Result<(Vec<Event>, Option<Event>)> {
    let Some(surface) = wheel_surface(
        &first,
        list_area,
        console_area,
        &mut stackhand_owns_console_wheel,
    ) else {
        return Ok((vec![first], None));
    };
    let mut events = Vec::with_capacity(INPUT_BATCH_LIMIT);
    events.push(first);
    while events.len() < INPUT_BATCH_LIMIT {
        let Some(next) = next_ready()? else {
            break;
        };
        if wheel_surface(
            &next,
            list_area,
            console_area,
            &mut stackhand_owns_console_wheel,
        ) == Some(surface)
        {
            events.push(next);
        } else {
            return Ok((events, Some(next)));
        }
    }
    Ok((events, None))
}

fn wheel_surface(
    event: &Event,
    list_area: Rect,
    console_area: Rect,
    stackhand_owns_console_wheel: &mut impl FnMut(&MouseEvent) -> bool,
) -> Option<WheelSurface> {
    let Event::Mouse(mouse) = event else {
        return None;
    };
    if !matches!(
        mouse.kind,
        MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight
    ) {
        return None;
    }
    if rect_contains(list_area, mouse.column, mouse.row) {
        Some(WheelSurface::ProcessList)
    } else if rect_contains(console_area, mouse.column, mouse.row)
        && stackhand_owns_console_wheel(mouse)
    {
        Some(WheelSurface::Console)
    } else {
        None
    }
}

fn rect_contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;

    fn wheel() -> Event {
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 40,
            row: 15,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn areas() -> (Rect, Rect) {
        (Rect::new(0, 0, 80, 8), Rect::new(0, 8, 80, 15))
    }

    #[test]
    fn stackhand_wheel_bursts_form_one_bounded_input_batch() {
        let (list, console) = areas();
        let quit = Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
        let mut queued = VecDeque::from(
            std::iter::repeat_n(wheel(), 199)
                .chain(std::iter::once(quit.clone()))
                .collect::<Vec<_>>(),
        );

        let (batch, deferred) =
            collect_input_batch(wheel(), list, console, |_| true, || Ok(queued.pop_front()))
                .unwrap();

        assert_eq!(batch.len(), 200);
        assert_eq!(deferred, Some(quit));
    }

    #[test]
    fn child_owned_wheels_are_not_batched_into_the_terminal_queue() {
        let (list, console) = areas();
        let wheel = wheel();

        let (batch, deferred) = collect_input_batch(
            wheel.clone(),
            list,
            console,
            |_| false,
            || panic!("a child-owned wheel must remain one ordered input event"),
        )
        .unwrap();

        assert_eq!(batch, vec![wheel]);
        assert_eq!(deferred, None);
    }

    #[test]
    fn tracking_change_stops_the_batch_after_one_stale_event() {
        let (list, console) = areas();
        let next_wheel = wheel();
        let mut queued = VecDeque::from([next_wheel.clone()]);
        let mut checks = 0;

        let (batch, deferred) = collect_input_batch(
            wheel(),
            list,
            console,
            |_| {
                checks += 1;
                checks == 1
            },
            || Ok(queued.pop_front()),
        )
        .unwrap();

        assert_eq!(batch.len(), 1);
        assert_eq!(deferred, Some(next_wheel));
    }

    #[test]
    fn a_batch_never_drains_more_than_the_scheduling_bound() {
        let (list, console) = areas();
        let mut reads = 0;

        let (batch, deferred) = collect_input_batch(
            wheel(),
            list,
            console,
            |_| true,
            || {
                reads += 1;
                Ok(Some(wheel()))
            },
        )
        .unwrap();

        assert_eq!(batch.len(), INPUT_BATCH_LIMIT);
        assert_eq!(reads, INPUT_BATCH_LIMIT - 1);
        assert_eq!(deferred, None);
    }
}
