use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::mpsc::{self, Sender as CompletionSender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::byte_budget::ByteBudget;
use crate::terminal::PasteCompletion;

pub const WRITER_QUEUE_SLOTS: usize = 1_024;
pub const WRITER_EVENT_SLOTS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PtyWriterEvent {
    Backpressure {
        attempted_bytes: usize,
        pending_bytes: usize,
        limit_bytes: usize,
    },
    Failed(String),
}

#[derive(Default)]
struct WriterStatus {
    events: Mutex<WriterEvents>,
}

#[derive(Default)]
struct WriterEvents {
    queue: VecDeque<PtyWriterEvent>,
    latched_failure: Option<PtyWriterEvent>,
}

impl WriterStatus {
    fn record(&self, event: PtyWriterEvent) {
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if events.queue.len() < WRITER_EVENT_SLOTS {
            events.queue.push_back(event);
        } else if matches!(event, PtyWriterEvent::Failed(_)) && events.latched_failure.is_none() {
            // A full diagnostic queue must not hide a terminal writer failure.
            events.latched_failure = Some(event);
        }
    }

    fn take(&self) -> Option<PtyWriterEvent> {
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        events
            .queue
            .pop_front()
            .or_else(|| events.latched_failure.take())
    }
}

pub struct PtyWriterOwner {
    status: Arc<WriterStatus>,
    worker: crate::worker_handle::WorkerHandle,
}

impl PtyWriterOwner {
    pub fn cancel(&self) {}

    pub fn poll_event(&self) -> Option<PtyWriterEvent> {
        self.status.take()
    }

    pub fn join(&self) -> io::Result<()> {
        self.worker
            .join()
            .map_err(|_| io::Error::other("PTY writer thread panicked"))
    }

    /// Join the writer only while the supplied deadline remains. A blocked
    /// operating-system write is detached after the deadline and reported by
    /// the caller instead of extending Run shutdown indefinitely.
    pub fn join_until(&self, deadline: Instant) -> io::Result<bool> {
        self.worker
            .join_until(deadline)
            .map_err(|_| io::Error::other("PTY writer thread panicked"))
    }

    /// Detach a writer that is still blocked. Returns whether it joined.
    pub fn abandon_nonblocking(&self) -> bool {
        self.worker.abandon_nonblocking()
    }
}

impl Drop for PtyWriterOwner {
    fn drop(&mut self) {
        let _ = self.join();
    }
}

pub(crate) struct BoundedPtyWriter {
    sender: SyncSender<WriterItem>,
    budget: ByteBudget,
    limit_bytes: usize,
    status: Arc<WriterStatus>,
}

struct WriterItem {
    data: Vec<u8>,
    completion: Option<CompletionSender<PasteCompletion>>,
}

impl BoundedPtyWriter {
    /// Admit one complete encoded input or effect item.
    ///
    /// `Ok` is a durable acknowledgement. The writer owner will retry partial
    /// operating-system writes until this full item is delivered or it emits
    /// a terminal failure. `WouldBlock` means that no byte was admitted.
    pub(crate) fn try_enqueue(&self, data: &[u8]) -> io::Result<()> {
        self.try_enqueue_with_completion(data, None)
    }

    pub(crate) fn try_enqueue_with_completion(
        &self,
        data: &[u8],
        completion: Option<&CompletionSender<PasteCompletion>>,
    ) -> io::Result<()> {
        if data.is_empty() {
            if let Some(completion) = completion {
                let _ = completion.send(PasteCompletion::Delivered);
            }
            return Ok(());
        }

        let reservation = self.budget.reserve(data.len()).map_err(|error| {
            self.status.record(PtyWriterEvent::Backpressure {
                attempted_bytes: data.len(),
                pending_bytes: error.pending_bytes,
                limit_bytes: self.limit_bytes,
            });
            io::Error::new(io::ErrorKind::WouldBlock, "PTY input queue is full")
        })?;
        let pending_before = reservation.pending_before();

        let item = WriterItem {
            data: data.to_vec(),
            completion: completion.cloned(),
        };
        match self.sender.try_send(item) {
            Ok(()) => {
                reservation.commit();
                Ok(())
            }
            Err(TrySendError::Full(_)) => {
                drop(reservation);
                self.status.record(PtyWriterEvent::Backpressure {
                    attempted_bytes: data.len(),
                    pending_bytes: pending_before,
                    limit_bytes: self.limit_bytes,
                });
                Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "PTY input queue has no free message slot",
                ))
            }
            Err(TrySendError::Disconnected(item)) => {
                drop(reservation);
                if let Some(completion) = item.completion {
                    let _ = completion.send(PasteCompletion::Failed(
                        "PTY writer is not available".to_string(),
                    ));
                }
                self.status.record(PtyWriterEvent::Failed(
                    "PTY writer is not available".to_string(),
                ));
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "PTY writer is not available",
                ))
            }
        }
    }
}

impl Write for BoundedPtyWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.try_enqueue(data)?;
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn spawn_bounded_pty_writer(
    writer: Box<dyn Write + Send>,
    limit_bytes: usize,
) -> io::Result<(BoundedPtyWriter, PtyWriterOwner)> {
    if limit_bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "PTY input queue limit must be non-zero",
        ));
    }

    let (sender, receiver) = mpsc::sync_channel::<WriterItem>(WRITER_QUEUE_SLOTS);
    let budget = ByteBudget::new(limit_bytes);
    let status = Arc::new(WriterStatus::default());
    let thread_budget = budget.clone();
    let thread_status = Arc::clone(&status);
    let thread_limit = limit_bytes;
    let thread = thread::Builder::new()
        .name("pty-writer".to_string())
        .spawn(move || {
            let mut writer = writer;
            while let Ok(item) = receiver.recv() {
                let result = write_with_retry(
                    &mut writer,
                    &item.data,
                    &thread_status,
                    &thread_budget,
                    thread_limit,
                );
                thread_budget.release(item.data.len());
                if let Err(error) = result {
                    if let Some(completion) = item.completion {
                        let _ = completion.send(PasteCompletion::Failed(error.to_string()));
                    }
                    thread_status.record(PtyWriterEvent::Failed(error.to_string()));
                    break;
                }
                if let Some(completion) = item.completion {
                    let _ = completion.send(PasteCompletion::Delivered);
                }
            }
        })?;

    let queued_writer = BoundedPtyWriter {
        sender,
        budget,
        limit_bytes,
        status: Arc::clone(&status),
    };
    let owner = PtyWriterOwner {
        status,
        worker: crate::worker_handle::WorkerHandle::new(thread),
    };
    Ok((queued_writer, owner))
}

fn write_with_retry(
    writer: &mut Box<dyn Write + Send>,
    data: &[u8],
    status: &WriterStatus,
    budget: &ByteBudget,
    limit_bytes: usize,
) -> io::Result<()> {
    let mut offset = 0;
    let mut reported_backpressure = false;
    while offset < data.len() {
        match writer.write(&data[offset..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "PTY writer made no progress",
                ));
            }
            Ok(written) => offset += written,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if !reported_backpressure {
                    status.record(PtyWriterEvent::Backpressure {
                        attempted_bytes: data.len(),
                        pending_bytes: budget.pending_bytes(),
                        limit_bytes,
                    });
                    reported_backpressure = true;
                }
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => return Err(error),
        }
    }

    loop {
        match writer.flush() {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if !reported_backpressure {
                    status.record(PtyWriterEvent::Backpressure {
                        attempted_bytes: data.len(),
                        pending_bytes: budget.pending_bytes(),
                        limit_bytes,
                    });
                    reported_backpressure = true;
                }
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests;
