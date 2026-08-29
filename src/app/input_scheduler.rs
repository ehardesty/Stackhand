//! Bounded application input scheduling.
//!
//! One reader owns Crossterm input. The application receives semantic batches
//! through a small interface instead of replaying an unbounded host queue.
//! Repeated wheels share one queue entry, stale movement is replaced, and old
//! lossy mouse input yields to current input. Ordered keys, clicks, drags,
//! paste, focus, and resize events are never discarded.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use crossterm::event::{self, Event, MouseEventKind};

const INPUT_QUEUE_LIMIT: usize = 256;
const READER_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug)]
pub(super) struct InputBatch {
    actions: Vec<(Event, usize)>,
}

impl InputBatch {
    pub(super) fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }
}

impl IntoIterator for InputBatch {
    type Item = (Event, usize);
    type IntoIter = std::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.actions.into_iter()
    }
}

/// Owns the host-input reader and bounded semantic queue. Dropping the
/// scheduler stops and joins its reader before terminal cleanup.
pub(super) struct InputScheduler {
    shared: Arc<Shared>,
    reader: Option<JoinHandle<()>>,
}

struct Shared {
    state: Mutex<QueueState>,
    ready: Condvar,
    space: Condvar,
    shutdown: AtomicBool,
}

#[derive(Default)]
struct QueueState {
    actions: VecDeque<(Event, usize)>,
    error: Option<String>,
}

impl InputScheduler {
    pub(super) fn start() -> Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(QueueState::default()),
            ready: Condvar::new(),
            space: Condvar::new(),
            shutdown: AtomicBool::new(false),
        });
        let reader_shared = Arc::clone(&shared);
        let reader = thread::Builder::new()
            .name("stackhand-input".to_string())
            .spawn(move || read_crossterm(reader_shared))
            .context("could not start the terminal input reader")?;
        Ok(Self {
            shared,
            reader: Some(reader),
        })
    }

    /// Wait until useful input is ready or the caller's current scheduling
    /// interval ends. Every returned action is in host-observation order.
    pub(super) fn receive(&self, timeout: Duration) -> Result<InputBatch> {
        let deadline = Instant::now() + timeout;
        let mut state = lock(&self.shared.state);
        loop {
            if let Some(error) = state.error.take() {
                return Err(anyhow!(error));
            }
            if !state.actions.is_empty() {
                let actions = state.actions.drain(..).collect();
                self.shared.space.notify_all();
                return Ok(InputBatch { actions });
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(InputBatch {
                    actions: Vec::new(),
                });
            }
            state = wait_timeout(&self.shared.ready, state, remaining);
        }
    }
}

impl Drop for InputScheduler {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        self.shared.space.notify_all();
        self.shared.ready.notify_all();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn read_crossterm(shared: Arc<Shared>) {
    while !shared.shutdown.load(Ordering::Acquire) {
        let next = match event::poll(READER_POLL_INTERVAL) {
            Ok(true) => event::read().map(Some),
            Ok(false) => Ok(None),
            Err(error) => Err(error),
        };
        match next {
            Ok(Some(input)) => push(&shared, input),
            Ok(None) => {}
            Err(error) => {
                let mut state = lock(&shared.state);
                state.error = Some(format!("terminal input failed: {error}"));
                drop(state);
                shared.ready.notify_one();
                return;
            }
        }
    }
}

fn push(shared: &Shared, event: Event) {
    let mut state = lock(&shared.state);
    if merge_tail(&mut state.actions, &event) {
        drop(state);
        shared.ready.notify_one();
        return;
    }

    while state.actions.len() == INPUT_QUEUE_LIMIT {
        if let Some(index) = state
            .actions
            .iter()
            .position(|(queued, _)| is_lossy_mouse(queued))
        {
            state.actions.remove(index);
            break;
        }
        if is_lossy_mouse(&event) || shared.shutdown.load(Ordering::Acquire) {
            return;
        }
        state = wait(&shared.space, state);
    }

    state.actions.push_back((event, 1));
    drop(state);
    shared.ready.notify_one();
}

fn merge_tail(actions: &mut VecDeque<(Event, usize)>, event: &Event) -> bool {
    let Some((tail, repeats)) = actions.back_mut() else {
        return false;
    };
    if is_wheel(tail) && tail == event {
        *repeats = repeats.saturating_add(1);
        return true;
    }
    if is_motion(tail) && is_motion(event) {
        *tail = event.clone();
        *repeats = 1;
        return true;
    }
    false
}

fn is_lossy_mouse(event: &Event) -> bool {
    is_wheel(event) || is_motion(event)
}

fn is_wheel(event: &Event) -> bool {
    matches!(
        event,
        Event::Mouse(mouse)
            if matches!(
                mouse.kind,
                MouseEventKind::ScrollUp
                    | MouseEventKind::ScrollDown
                    | MouseEventKind::ScrollLeft
                    | MouseEventKind::ScrollRight
            )
    )
}

fn is_motion(event: &Event) -> bool {
    matches!(event, Event::Mouse(mouse) if mouse.kind == MouseEventKind::Moved)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn wait<'a, T>(condvar: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    condvar
        .wait(guard)
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn wait_timeout<'a, T>(
    condvar: &Condvar,
    guard: MutexGuard<'a, T>,
    timeout: Duration,
) -> MutexGuard<'a, T> {
    condvar
        .wait_timeout(guard, timeout)
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .0
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};

    use super::*;

    fn shared() -> Shared {
        Shared {
            state: Mutex::new(QueueState::default()),
            ready: Condvar::new(),
            space: Condvar::new(),
            shutdown: AtomicBool::new(false),
        }
    }

    fn mouse(kind: MouseEventKind, column: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row: 10,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn quit() -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL))
    }

    #[test]
    fn repeated_wheels_use_one_bounded_queue_entry() {
        let shared = shared();
        let wheel = mouse(MouseEventKind::ScrollUp, 20);
        for _ in 0..10_000 {
            push(&shared, wheel.clone());
        }
        let state = lock(&shared.state);
        assert_eq!(state.actions, VecDeque::from([(wheel, 10_000)]));
    }

    #[test]
    fn movement_keeps_only_the_latest_position() {
        let shared = shared();
        push(&shared, mouse(MouseEventKind::Moved, 20));
        let latest = mouse(MouseEventKind::Moved, 40);
        push(&shared, latest.clone());
        let state = lock(&shared.state);
        assert_eq!(state.actions, VecDeque::from([(latest, 1)]));
    }

    #[test]
    fn current_input_replaces_stale_wheels_when_the_queue_is_full() {
        let shared = shared();
        for index in 0..INPUT_QUEUE_LIMIT {
            let direction = if index % 2 == 0 {
                MouseEventKind::ScrollUp
            } else {
                MouseEventKind::ScrollDown
            };
            push(&shared, mouse(direction, index as u16));
        }

        push(&shared, quit());

        let state = lock(&shared.state);
        assert_eq!(state.actions.len(), INPUT_QUEUE_LIMIT);
        assert_eq!(state.actions.back(), Some(&(quit(), 1)));
        assert_eq!(
            state.actions.front().unwrap().0,
            mouse(MouseEventKind::ScrollDown, 1)
        );
    }

    #[test]
    fn opposite_wheels_keep_order_until_overload_requires_shedding() {
        let shared = shared();
        let up = mouse(MouseEventKind::ScrollUp, 20);
        let down = mouse(MouseEventKind::ScrollDown, 20);
        push(&shared, up.clone());
        push(&shared, down.clone());
        push(&shared, up.clone());

        let state = lock(&shared.state);
        assert_eq!(
            state.actions,
            VecDeque::from([(up.clone(), 1), (down, 1), (up, 1)])
        );
    }

    #[test]
    fn receive_wakes_for_new_input() {
        let shared = Arc::new(shared());
        let scheduler = InputScheduler {
            shared: Arc::clone(&shared),
            reader: None,
        };
        let producer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            push(&shared, quit());
        });

        let batch = scheduler.receive(Duration::from_secs(1)).unwrap();

        assert_eq!(batch.actions, vec![(quit(), 1)]);
        producer.join().unwrap();
    }

    #[test]
    fn receive_returns_after_its_timeout() {
        let scheduler = InputScheduler {
            shared: Arc::new(shared()),
            reader: None,
        };
        let started = Instant::now();

        let batch = scheduler.receive(Duration::from_millis(10)).unwrap();

        assert!(batch.is_empty());
        assert!(started.elapsed() >= Duration::from_millis(5));
    }

    #[test]
    fn receive_reports_reader_errors() {
        let shared = Arc::new(shared());
        lock(&shared.state).error = Some("terminal input failed: test".to_string());
        let scheduler = InputScheduler {
            shared,
            reader: None,
        };

        let error = scheduler.receive(Duration::from_secs(1)).unwrap_err();

        assert_eq!(error.to_string(), "terminal input failed: test");
    }
}
