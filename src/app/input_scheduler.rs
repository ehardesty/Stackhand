//! Bounded application input scheduling.
//!
//! One reader owns host input. The application receives semantic batches
//! through a small interface instead of replaying an unbounded host queue.
//! Repeated wheels share one queue entry, stale movement is replaced, and old
//! lossy mouse input yields to current input. Ordered keys, clicks, drags,
//! paste, focus, and resize events are never discarded.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use crate::host::{HostAction, HostReader, SessionStop};
use anyhow::{Context, Result, anyhow};
use crossterm::event::{Event, MouseEventKind};

use crate::worker_handle::WorkerHandle;

const INPUT_QUEUE_LIMIT: usize = 256;
const READER_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug)]
pub(super) struct InputBatch {
    actions: Vec<(HostAction, usize)>,
}

impl InputBatch {
    pub(super) fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }
}

impl IntoIterator for InputBatch {
    type Item = (HostAction, usize);
    type IntoIter = std::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.actions.into_iter()
    }
}

/// Owns the host-input reader and bounded semantic queue.
pub(super) struct InputScheduler {
    shared: Arc<Shared>,
    reader: Option<WorkerHandle>,
}

struct Shared {
    state: Mutex<QueueState>,
    ready: Condvar,
    space: Condvar,
    shutdown: AtomicBool,
    discarding: AtomicBool,
    stop: SessionStop,
}

#[derive(Default)]
struct QueueState {
    actions: VecDeque<(HostAction, usize)>,
    error: Option<String>,
    keyboard_supported: bool,
    device_attributes: bool,
}

impl InputScheduler {
    pub(super) fn start(source: HostReader, stop: SessionStop) -> Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(QueueState::default()),
            ready: Condvar::new(),
            space: Condvar::new(),
            shutdown: AtomicBool::new(false),
            discarding: AtomicBool::new(false),
            stop,
        });
        let reader_shared = Arc::clone(&shared);
        let reader = thread::Builder::new()
            .name("stackhand-input".to_string())
            .spawn(move || read_host(source, reader_shared))
            .context("could not start the terminal input reader")?;
        Ok(Self {
            shared,
            reader: Some(WorkerHandle::new(reader)),
        })
    }

    pub(super) fn keyboard_support(&self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut state = lock(&self.shared.state);
        while !state.keyboard_supported && !state.device_attributes {
            if self.shared.stop.requested() || Instant::now() >= deadline {
                return false;
            }
            state = wait_timeout(&self.shared.ready, state, READER_POLL_INTERVAL);
        }
        state.keyboard_supported
    }

    pub(super) fn stop(&self) {
        self.shared.shutdown.store(true, Ordering::Release);
        self.shared.space.notify_all();
        self.shared.ready.notify_all();
    }

    pub(super) fn discard(&self) {
        self.shared.discarding.store(true, Ordering::Release);
        lock(&self.shared.state).actions.clear();
        self.shared.space.notify_all();
        self.shared.ready.notify_all();
    }

    /// Wait until useful input is ready or the caller's current scheduling
    /// interval ends. Every returned action is in host-observation order.
    pub(super) fn receive(&self, timeout: Duration) -> Result<InputBatch> {
        let deadline = Instant::now() + timeout;
        let mut state = lock(&self.shared.state);
        loop {
            if let Some(error) = self.shared.stop.error() {
                return Err(anyhow!(error));
            }
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
        self.stop();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join_until(Instant::now() + Duration::from_millis(100));
        }
    }
}

fn read_host(mut source: HostReader, shared: Arc<Shared>) {
    while !shared.shutdown.load(Ordering::Acquire) {
        if shared.discarding.load(Ordering::Acquire) || shared.stop.requested() {
            if !source.discard_batch(READER_POLL_INTERVAL).unwrap_or(false) {
                return;
            }
            continue;
        }
        match source.next_batch(READER_POLL_INTERVAL) {
            Ok(batch) => {
                {
                    let mut state = lock(&shared.state);
                    state.keyboard_supported = source.keyboard_supported;
                    state.device_attributes = source.device_attributes;
                }
                shared.ready.notify_all();
                for action in batch {
                    if !push(&shared, action, || source.check()) {
                        break;
                    }
                }
            }
            Err(error) => {
                shared.stop.fail(crate::host::HostFault::InputFailed);
                lock(&shared.state).error = Some(format!("terminal input failed: {error}"));
                shared.ready.notify_all();
            }
        }
    }
}

fn push(
    shared: &Shared,
    event: HostAction,
    mut check: impl FnMut() -> std::io::Result<()>,
) -> bool {
    let mut state = lock(&shared.state);
    loop {
        if shared.shutdown.load(Ordering::Acquire)
            || shared.discarding.load(Ordering::Acquire)
            || shared.stop.requested()
        {
            return false;
        }
        if let Err(error) = check() {
            state.error = Some(error.to_string());
            shared.ready.notify_all();
            return false;
        }
        if merge_tail(&mut state.actions, &event) {
            shared.ready.notify_one();
            return true;
        }
        if state.actions.len() < INPUT_QUEUE_LIMIT {
            break;
        }
        if let Some(index) = state
            .actions
            .iter()
            .position(|(queued, _)| is_lossy_mouse(queued))
        {
            state.actions.remove(index);
            break;
        }
        if is_lossy_mouse(&event) {
            return true;
        }
        state = wait_timeout(&shared.space, state, READER_POLL_INTERVAL);
    }
    state.actions.push_back((event, 1));
    drop(state);
    shared.ready.notify_one();
    true
}

fn merge_tail(actions: &mut VecDeque<(HostAction, usize)>, event: &HostAction) -> bool {
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

fn is_lossy_mouse(event: &HostAction) -> bool {
    is_wheel(event) || is_motion(event)
}

fn is_wheel(event: &HostAction) -> bool {
    matches!(
        event,
        HostAction::Input(Event::Mouse(mouse))
            if matches!(
                mouse.kind,
                MouseEventKind::ScrollUp
                    | MouseEventKind::ScrollDown
                    | MouseEventKind::ScrollLeft
                    | MouseEventKind::ScrollRight
            )
    )
}

fn is_motion(event: &HostAction) -> bool {
    matches!(event, HostAction::Input(Event::Mouse(mouse)) if mouse.kind == MouseEventKind::Moved)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
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
mod tests;
