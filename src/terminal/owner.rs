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
use libghostty_vt::key;
use libghostty_vt::render::RenderState;
use libghostty_vt::terminal::{
    ClipboardWrite, ClipboardWriteError, Options as TerminalOptions, Terminal,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::command_gate::CommandReceiver;
use super::commands::{PendingInput, apply_command};
use super::history::{BoundedOutputHistory, OutputHistoryMetrics};
use super::mouse::MouseController;
use super::render;
use super::selection::SelectionController;
use super::session::OwnedCursorState;
use crate::geometry::TerminalGeometry;
use crate::runtime::{BoundedPtyWriter, PtyResizer};

pub const OUTPUT_QUEUE_SLOTS: usize = 64;
pub const OUTPUT_READ_BUFFER_BYTES: usize = 4_096;
pub const OUTPUT_WORK_BUDGET: usize = 32;
const EFFECT_BUFFER_BYTES: usize = 256 * 1_024;
const OWNER_EVENT_SLOTS: usize = 64;
const SELECTION_AUTOSCROLL_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Debug)]
pub enum OwnerEvent {
    Exited,
    Failed(String),
    StateChanged,
    OutputTruncated { evicted_bytes: usize },
}

#[derive(Clone)]
pub struct OwnedRender {
    pub buffer: Buffer,
    pub cursor: Option<OwnedCursorState>,
    pub mouse_tracking: bool,
}

struct SharedOwner {
    render: Mutex<OwnedRender>,
    dirty: AtomicBool,
    alive: AtomicBool,
    shutdown: AtomicBool,
    events: Mutex<VecDeque<OwnerEvent>>,
    history: Mutex<OutputHistoryMetrics>,
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
            OwnerEvent::OutputTruncated { evicted_bytes } => {
                if let Some(OwnerEvent::OutputTruncated {
                    evicted_bytes: pending,
                }) = events
                    .iter_mut()
                    .find(|existing| matches!(existing, OwnerEvent::OutputTruncated { .. }))
                {
                    *pending = pending.saturating_add(*evicted_bytes);
                    return;
                }
                if events.len() == OWNER_EVENT_SLOTS {
                    return;
                }
                events.push_back(OwnerEvent::OutputTruncated {
                    evicted_bytes: *evicted_bytes,
                });
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
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl OwnerHandle {
    pub fn spawn(
        reader: Box<dyn Read + Send>,
        resizer: PtyResizer,
        writer: BoundedPtyWriter,
        commands: CommandReceiver,
        geometry: TerminalGeometry,
        wake: impl Fn() + Send + 'static,
    ) -> Result<Self> {
        let shared = Arc::new(SharedOwner {
            render: Mutex::new(OwnedRender {
                buffer: Buffer::empty(Rect::new(0, 0, geometry.cols(), geometry.rows())),
                cursor: None,
                mouse_tracking: false,
            }),
            dirty: AtomicBool::new(true),
            alive: AtomicBool::new(true),
            shutdown: AtomicBool::new(false),
            events: Mutex::new(VecDeque::new()),
            history: Mutex::new(OutputHistoryMetrics::default()),
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
            thread: Mutex::new(Some(thread)),
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

    pub fn history_metrics(&self) -> OutputHistoryMetrics {
        *self
            .shared
            .history
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn request_shutdown(&self) {
        self.shared.shutdown.store(true, Ordering::Release);
    }

    pub fn join(&self) -> Result<()> {
        let Some(thread) = self
            .thread
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        else {
            return Ok(());
        };
        thread
            .join()
            .map_err(|_| anyhow!("terminal owner thread panicked"))
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

fn run_owner(
    reader: Box<dyn Read + Send>,
    mut resizer: PtyResizer,
    writer: BoundedPtyWriter,
    commands: CommandReceiver,
    geometry: TerminalGeometry,
    shared: &SharedOwner,
    wake: &dyn Fn(),
) -> Result<()> {
    let (output_tx, output_rx) = crossbeam_channel::bounded(OUTPUT_QUEUE_SLOTS);
    let reader = spawn_reader(reader, output_tx)?;
    let mut terminal = Box::new(Terminal::new(TerminalOptions {
        cols: geometry.cols(),
        rows: geometry.rows(),
        max_scrollback: super::SCROLLBACK_TARGET_BYTES,
    })?);
    let effects = Rc::new(RefCell::new(Effects::new()));
    terminal.on_pty_write({
        let effects = Rc::clone(&effects);
        move |_, data| effects.borrow_mut().push(data)
    })?;
    terminal.on_clipboard_write(deny_child_clipboard)?;
    let mut render_state = RenderState::new()?;
    let mut key_encoder = key::Encoder::new()?;
    let mut mouse_controller = MouseController::new()?;
    let mut selection = SelectionController::new()?;
    let mut history = BoundedOutputHistory::new();
    let mut focused = true;
    let mut cols = geometry.cols();
    let mut rows = geometry.rows();
    let mut pending_input: Option<PendingInput> = None;
    let mut pending_effect: Option<Vec<u8>> = None;
    let mut next_selection_tick = Instant::now() + SELECTION_AUTOSCROLL_INTERVAL;

    while !shared.shutdown.load(Ordering::Acquire) {
        let mut did_work = false;

        if let Some(data) = pending_effect.take() {
            match writer.try_enqueue(&data) {
                Ok(()) => did_work = true,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    pending_effect = Some(data);
                }
                Err(error) => return Err(error.into()),
            }
        }
        if pending_effect.is_none() {
            pending_effect = effects.borrow_mut().pop();
        }
        if effects.borrow().overflowed {
            bail!("terminal effect buffer exceeded {EFFECT_BUFFER_BYTES} bytes");
        }

        // Output always gets one bounded turn. A blocked accepted input item
        // stays owned for retry, but it cannot stop PTY draining. Effects can
        // accumulate only up to their separate byte bound.
        for _ in 0..OUTPUT_WORK_BUDGET {
            let data = match output_rx.try_recv() {
                Ok(data) => data,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            };
            let evicted = history.push(&data);
            *shared
                .history
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = history.metrics();
            if evicted > 0 {
                shared.record(OwnerEvent::OutputTruncated {
                    evicted_bytes: evicted,
                });
            }
            terminal.vt_write(&data);
            did_work = true;
            if effects.borrow().overflowed {
                break;
            }
        }

        if pending_effect.is_none() {
            pending_effect = effects.borrow_mut().pop();
        }
        if effects.borrow().overflowed {
            bail!("terminal effect buffer exceeded {EFFECT_BUFFER_BYTES} bytes");
        }

        if pending_effect.is_none() {
            if let Some(pending) = pending_input.take() {
                match writer.try_enqueue_with_completion(&pending.data, pending.completion.as_ref())
                {
                    Ok(()) => {
                        commands.complete(pending.command_bytes);
                        did_work = true;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        pending_input = Some(pending);
                    }
                    Err(error) => return Err(error.into()),
                }
            } else if let Ok(command) = commands.try_recv() {
                let command_bytes = command.estimated_bytes();
                pending_input = apply_command(
                    command,
                    command_bytes,
                    &mut terminal,
                    &mut key_encoder,
                    &mut mouse_controller,
                    &mut resizer,
                    &mut focused,
                    &mut cols,
                    &mut rows,
                    &mut selection,
                )?;
                if pending_input.is_none() {
                    commands.complete(command_bytes);
                }
                did_work = true;
            }
        }

        let now = Instant::now();
        if now >= next_selection_tick {
            if selection.tick_autoscroll(&mut terminal)? {
                did_work = true;
            }
            next_selection_tick = now + SELECTION_AUTOSCROLL_INTERVAL;
        }

        if did_work {
            render(
                &mut terminal,
                &mut render_state,
                focused,
                cols,
                rows,
                shared,
            );
            shared.record(OwnerEvent::StateChanged);
            wake();
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

fn deny_child_clipboard(
    _terminal: &Terminal<'_, '_>,
    _write: ClipboardWrite<'_>,
) -> std::result::Result<(), ClipboardWriteError> {
    Err(ClipboardWriteError::Denied)
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

fn render(
    terminal: &mut Terminal<'static, 'static>,
    render_state: &mut RenderState<'static>,
    focused: bool,
    cols: u16,
    rows: u16,
    shared: &SharedOwner,
) {
    let area = Rect::new(0, 0, cols, rows);
    let mut owned = shared
        .render
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if owned.buffer.area() != &area {
        owned.buffer = Buffer::empty(area);
    }
    owned.buffer.reset();
    owned.cursor =
        render::render(terminal, render_state, &mut owned.buffer, focused, area).unwrap_or(None);
    owned.mouse_tracking = terminal.is_mouse_tracking().unwrap_or(false);
    shared.dirty.store(true, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn effect_collector_stops_at_its_byte_limit_and_reports_overflow() {
        let mut effects = Effects::new();
        effects.push(&vec![1; EFFECT_BUFFER_BYTES]);
        effects.push(&[2]);

        assert_eq!(effects.bytes, EFFECT_BUFFER_BYTES);
        assert_eq!(effects.queue.len(), 1);
        assert!(effects.overflowed);
    }

    #[test]
    fn child_clipboard_reads_are_ignored_and_writes_reach_the_denial_boundary() {
        let writes = Cell::new(0_u8);
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 10,
            rows: 2,
            max_scrollback: 1024,
        })
        .unwrap();
        terminal
            .on_clipboard_write(|_, _| {
                writes.set(writes.get() + 1);
                Err(ClipboardWriteError::Denied)
            })
            .unwrap();

        terminal.vt_write(b"\x1b]52;c;?\x07");
        assert_eq!(writes.get(), 0);
        terminal.vt_write(b"\x1b]52;c;aGk=\x07");
        assert_eq!(writes.get(), 1);
    }
}
