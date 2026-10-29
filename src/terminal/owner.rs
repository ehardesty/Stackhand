use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{self, Read};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use crossbeam_channel::TryRecvError;
use libghostty_vt::terminal::{ClipboardWrite, ClipboardWriteError, CompressionResult, Terminal};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::command_gate::CommandReceiver;
use super::metrics::TerminalMetrics;
use super::session::{OwnedCursorState, OwnedTerminalScrollbar};
use super::state::{PendingInput, TerminalState};
use crate::geometry::TerminalGeometry;
use crate::runtime::{BoundedPtyWriter, PtyResizer, RunOutputObserver};

pub const OUTPUT_QUEUE_SLOTS: usize = 64;
pub const OUTPUT_READ_BUFFER_BYTES: usize = 4_096;
pub const OUTPUT_WORK_BUDGET: usize = 32;
const OUTPUT_SLICE_WORK_BUDGET: usize = 2;
const INPUT_WORK_BUDGET: usize = super::command_gate::COMMAND_QUEUE_SLOTS;
const EFFECT_BUFFER_BYTES: usize = 256 * 1_024;
const OWNER_EVENT_SLOTS: usize = 64;
const SELECTION_AUTOSCROLL_INTERVAL: Duration = Duration::from_millis(16);
const SCROLLBACK_COMPRESSION_IDLE: Duration = Duration::from_millis(250);

#[derive(Debug)]
pub enum OwnerEvent {
    Exited,
    Failed(String),
    StateChanged,
}

#[derive(Clone)]
pub struct OwnedRender {
    pub buffer: Buffer,
    pub cursor: Option<OwnedCursorState>,
    pub mouse_tracking: bool,
    pub scrollbar: OwnedTerminalScrollbar,
}

struct SharedOwner {
    render: Mutex<OwnedRender>,
    dirty: AtomicBool,
    mouse_tracking: AtomicBool,
    alive: AtomicBool,
    shutdown: AtomicBool,
    events: Mutex<VecDeque<OwnerEvent>>,
    metrics: Mutex<TerminalMetrics>,
}

impl SharedOwner {
    fn record(&self, event: OwnerEvent) {
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match &event {
            OwnerEvent::StateChanged
                if events
                    .iter()
                    .any(|existing| matches!(existing, OwnerEvent::StateChanged)) =>
            {
                return;
            }
            _ => {}
        }
        if events.len() == OWNER_EVENT_SLOTS {
            if matches!(event, OwnerEvent::Failed(_)) {
                if let Some(index) = events
                    .iter()
                    .position(|existing| !matches!(existing, OwnerEvent::Failed(_)))
                {
                    events.remove(index);
                } else {
                    return;
                }
            } else {
                return;
            }
        }
        events.push_back(event);
    }
}

pub struct OwnerHandle {
    shared: Arc<SharedOwner>,
    worker: crate::worker_handle::WorkerHandle,
}

impl OwnerHandle {
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        reader: Box<dyn Read + Send>,
        resizer: PtyResizer,
        writer: BoundedPtyWriter,
        commands: CommandReceiver,
        geometry: TerminalGeometry,
        wake: impl Fn() + Send + 'static,
        observer: Option<Arc<dyn RunOutputObserver>>,
        scrollback_bytes: usize,
    ) -> Result<Self> {
        let shared = Arc::new(SharedOwner {
            render: Mutex::new(OwnedRender {
                buffer: Buffer::empty(Rect::new(0, 0, geometry.cols(), geometry.rows())),
                cursor: None,
                mouse_tracking: false,
                scrollbar: OwnedTerminalScrollbar::new(geometry.rows()),
            }),
            dirty: AtomicBool::new(true),
            mouse_tracking: AtomicBool::new(false),
            alive: AtomicBool::new(true),
            shutdown: AtomicBool::new(false),
            events: Mutex::new(VecDeque::new()),
            metrics: Mutex::new(TerminalMetrics {
                scrollback_limit_bytes: scrollback_bytes,
                ..Default::default()
            }),
        });
        let worker_shared = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("terminal-owner".to_string())
            .spawn(move || {
                let result = run_owner(
                    reader,
                    resizer,
                    writer,
                    commands,
                    geometry,
                    &worker_shared,
                    &wake,
                    observer,
                    scrollback_bytes,
                );
                if let Err(error) = result {
                    worker_shared.record(OwnerEvent::Failed(error.to_string()));
                }
                worker_shared.record(OwnerEvent::Exited);
                worker_shared.alive.store(false, Ordering::Release);
                wake();
            })
            .context("could not start the terminal owner")?;
        Ok(Self {
            shared,
            worker: crate::worker_handle::WorkerHandle::new(thread),
        })
    }

    pub fn render(&self) -> OwnedRender {
        self.shared.dirty.store(false, Ordering::Release);
        self.shared
            .render
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn is_dirty(&self) -> bool {
        self.shared.dirty.load(Ordering::Acquire)
    }

    pub fn mouse_tracking(&self) -> bool {
        self.shared.mouse_tracking.load(Ordering::Acquire)
    }

    pub fn scrollbar(&self) -> OwnedTerminalScrollbar {
        self.shared
            .render
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .scrollbar
    }

    pub fn is_alive(&self) -> bool {
        self.shared.alive.load(Ordering::Acquire)
    }

    pub fn poll_event(&self) -> Option<OwnerEvent> {
        self.shared
            .events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop_front()
    }

    pub fn metrics(&self) -> TerminalMetrics {
        *self
            .shared
            .metrics
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn request_shutdown(&self) {
        self.shared.shutdown.store(true, Ordering::Release);
    }

    /// Join the owner thread only while the supplied deadline remains. The
    /// owner can be blocked in its PTY reader when a survivor keeps the
    /// terminal open; after the deadline the thread is detached and the
    /// caller retains a structured worker failure.
    pub fn join_until(&self, deadline: Instant) -> Result<bool> {
        self.worker
            .join_until(deadline)
            .map_err(|_| anyhow!("terminal owner thread panicked"))
    }

    /// Detach an owner thread that is still blocked. Returns whether it
    /// joined cleanly.
    pub fn abandon_nonblocking(&self) -> bool {
        self.worker.abandon_nonblocking()
    }
}

struct Effects {
    queue: VecDeque<Vec<u8>>,
    bytes: usize,
    overflowed: bool,
}

impl Effects {
    fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            bytes: 0,
            overflowed: false,
        }
    }

    fn push(&mut self, data: &[u8]) {
        if self.bytes.saturating_add(data.len()) > EFFECT_BUFFER_BYTES {
            self.overflowed = true;
            return;
        }
        self.bytes += data.len();
        self.queue.push_back(data.to_vec());
    }

    fn pop(&mut self) -> Option<Vec<u8>> {
        let data = self.queue.pop_front()?;
        self.bytes -= data.len();
        Some(data)
    }
}

#[allow(clippy::too_many_arguments)]
fn run_owner(
    reader: Box<dyn Read + Send>,
    resizer: PtyResizer,
    writer: BoundedPtyWriter,
    commands: CommandReceiver,
    geometry: TerminalGeometry,
    shared: &SharedOwner,
    wake: &dyn Fn(),
    observer: Option<Arc<dyn RunOutputObserver>>,
    scrollback_bytes: usize,
) -> Result<()> {
    let (output_tx, output_rx) = crossbeam_channel::bounded(OUTPUT_QUEUE_SLOTS);
    let reader = spawn_reader(reader, output_tx)?;
    let mut terminal = Box::new(crate::terminal::new_terminal(
        geometry.cols(),
        geometry.rows(),
        scrollback_bytes,
    )?);
    let effects = Rc::new(RefCell::new(Effects::new()));
    terminal.on_pty_write({
        let effects = Rc::clone(&effects);
        move |_, data| effects.borrow_mut().push(data)
    })?;
    terminal.on_clipboard_write(deny_child_clipboard)?;
    let mut state = TerminalState::new(terminal, resizer, geometry)?;
    shared
        .metrics
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .scrollback_limit_bytes = state.scrollback_limit_bytes()?;
    let mut next_memory_sample = Instant::now();
    let mut pending_input: Option<PendingInput> = None;
    let mut pending_effect: Option<Vec<u8>> = None;
    let mut next_selection_tick = Instant::now() + SELECTION_AUTOSCROLL_INTERVAL;
    let mut compression_activity = state.compression_activity().ok();
    let mut compression_complete = false;
    let mut compression_due = Instant::now() + SCROLLBACK_COMPRESSION_IDLE;

    while !shared.shutdown.load(Ordering::Acquire) {
        let mut did_work = false;

        did_work |= service_effect(&writer, &effects, &mut pending_effect)?;

        // Output gets one bounded turn. Service the input gate between small
        // output slices so a busy PTY cannot monopolise the owner. A blocked
        // accepted input item stays owned for retry, but it cannot stop PTY
        // draining. Effects can accumulate only up to their separate bound.
        let mut output_slice_work = 0;
        for _ in 0..OUTPUT_WORK_BUDGET {
            let data = match output_rx.try_recv() {
                Ok(data) => data,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            };
            if let Some(observer) = &observer {
                observer.observe(&data);
            }
            state.write_output(&data);
            let mut metrics = shared.metrics.lock().unwrap_or_else(|p| p.into_inner());
            metrics.parsed_bytes = metrics.parsed_bytes.saturating_add(data.len());
            metrics.parsed_chunks = metrics.parsed_chunks.saturating_add(1);
            drop(metrics);
            did_work = true;
            output_slice_work += 1;
            if effects.borrow().overflowed {
                break;
            }

            if output_slice_work == OUTPUT_SLICE_WORK_BUDGET {
                did_work |= service_effect(&writer, &effects, &mut pending_effect)?;
                if pending_effect.is_none() {
                    did_work |= service_input(&mut state, &writer, &commands, &mut pending_input)?;
                }
                output_slice_work = 0;
            }
        }

        did_work |= service_effect(&writer, &effects, &mut pending_effect)?;

        if pending_effect.is_none() {
            // Scroll bursts change terminal state many times but need only one
            // published frame. The queue bound also bounds this work turn.
            did_work |= service_input_batch(&mut state, &writer, &commands, &mut pending_input)?;
        }

        let now = Instant::now();
        if now >= next_memory_sample {
            let memory = state.memory_usage().ok();
            shared
                .metrics
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .memory = memory;
            next_memory_sample = now + Duration::from_secs(2);
        }
        if now >= next_selection_tick {
            if state.tick_selection_autoscroll()? {
                did_work = true;
            }
            next_selection_tick = now + SELECTION_AUTOSCROLL_INTERVAL;
        }

        if did_work {
            // Output, input, selection, and effects always take priority.
            // A changed Ghostty activity token restarts the idle delay.
            let activity = state.compression_activity().ok();
            if activity != compression_activity {
                compression_activity = activity;
                compression_complete = false;
            }
            compression_due = Instant::now() + SCROLLBACK_COMPRESSION_IDLE;
            render(&mut state, shared);
            shared.record(OwnerEvent::StateChanged);
            wake();
        } else if !compression_complete && Instant::now() >= compression_due {
            // One incremental call is the complete maintenance turn. The next
            // loop checks output and input again before any continuation.
            compression_complete =
                !matches!(state.compress_scrollback()?, CompressionResult::Pending);
            compression_due = Instant::now() + Duration::from_millis(1);
        } else {
            thread::sleep(Duration::from_millis(1));
        }
    }

    drop(output_rx);
    reader
        .join()
        .map_err(|_| anyhow!("PTY reader thread panicked"))?;
    Ok(())
}

fn service_effect(
    writer: &BoundedPtyWriter,
    effects: &Rc<RefCell<Effects>>,
    pending_effect: &mut Option<Vec<u8>>,
) -> Result<bool> {
    if pending_effect.is_none() {
        *pending_effect = effects.borrow_mut().pop();
    }
    let serviced = match pending_effect.take() {
        None => false,
        Some(data) => match writer.try_enqueue(&data) {
            Ok(()) => true,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                *pending_effect = Some(data);
                false
            }
            Err(error) => return Err(error.into()),
        },
    };
    if effects.borrow().overflowed {
        bail!("terminal effect buffer exceeded {EFFECT_BUFFER_BYTES} bytes");
    }
    Ok(serviced)
}

fn service_input_batch(
    state: &mut TerminalState,
    writer: &BoundedPtyWriter,
    commands: &CommandReceiver,
    pending_input: &mut Option<PendingInput>,
) -> Result<bool> {
    let mut did_work = false;
    for _ in 0..INPUT_WORK_BUDGET {
        let serviced = service_input(state, writer, commands, pending_input)?;
        did_work |= serviced;
        if !serviced || pending_input.is_some() {
            break;
        }
    }
    Ok(did_work)
}

fn service_input(
    state: &mut TerminalState,
    writer: &BoundedPtyWriter,
    commands: &CommandReceiver,
    pending_input: &mut Option<PendingInput>,
) -> Result<bool> {
    if let Some(pending) = pending_input.take() {
        match writer.try_enqueue_with_completion(&pending.data, pending.completion.as_ref()) {
            Ok(()) => {
                commands.complete(pending.command_bytes);
                return Ok(true);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                *pending_input = Some(pending);
                return Ok(false);
            }
            Err(error) => return Err(error.into()),
        }
    }

    let Ok(command) = commands.try_recv() else {
        return Ok(false);
    };
    let command_bytes = command.estimated_bytes();
    *pending_input = state.apply_command(command, command_bytes)?;
    if pending_input.is_none() {
        commands.complete(command_bytes);
    }
    Ok(true)
}

fn deny_child_clipboard(_terminal: &Terminal<'_, '_>, write: ClipboardWrite<'_>) {
    write.reply(Err(ClipboardWriteError::Denied), false);
}

fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    sender: crossbeam_channel::Sender<Vec<u8>>,
) -> Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("pty-reader".to_string())
        .spawn(move || {
            let mut buffer = vec![0; OUTPUT_READ_BUFFER_BYTES];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) if sender.send(buffer[..count].to_vec()).is_err() => break,
                    Ok(_) => {}
                }
            }
        })
        .context("could not start the PTY reader")
}

fn render(state: &mut TerminalState, shared: &SharedOwner) {
    let area = state.area();
    let mut owned = shared
        .render
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if owned.buffer.area() != &area {
        owned.buffer = Buffer::empty(area);
    }
    owned.buffer.reset();
    if let Ok((cursor, scrollbar)) = state.render(&mut owned.buffer) {
        owned.cursor = cursor;
        owned.scrollbar = scrollbar;
    } else {
        owned.cursor = None;
    }
    owned.mouse_tracking = state.mouse_tracking();
    shared
        .mouse_tracking
        .store(owned.mouse_tracking, Ordering::Release);
    shared.dirty.store(true, Ordering::Release);
}

#[cfg(test)]
mod tests;
