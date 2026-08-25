use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender as CompletionSender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use std::time::Instant;

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
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl PtyWriterOwner {
    pub fn poll_event(&self) -> Option<PtyWriterEvent> {
        self.status.take()
    }

    pub fn join(&self) -> io::Result<()> {
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
            .map_err(|_| io::Error::other("PTY writer thread panicked"))
    }

    /// Join the writer only while the supplied deadline remains. A blocked
    /// operating-system write is detached after the deadline and reported by
    /// the caller instead of extending Run shutdown indefinitely.
    pub fn join_until(&self, deadline: Instant) -> io::Result<bool> {
        loop {
            let finished = self
                .thread
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_ref()
                .is_none_or(JoinHandle::is_finished);
            if finished {
                self.join()?;
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(self.abandon_nonblocking());
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// Detach a writer that is still blocked. Returns whether it joined.
    pub fn abandon_nonblocking(&self) -> bool {
        let Some(thread) = self
            .thread
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        else {
            return true;
        };
        if thread.is_finished() {
            thread.join().is_ok()
        } else {
            false
        }
    }
}

impl Drop for PtyWriterOwner {
    fn drop(&mut self) {
        let _ = self.join();
    }
}

pub(crate) struct BoundedPtyWriter {
    sender: SyncSender<WriterItem>,
    pending_bytes: Arc<AtomicUsize>,
    limit_bytes: usize,
    status: Arc<WriterStatus>,
}

struct WriterItem {
    data: Vec<u8>,
    completion: Option<CompletionSender<Result<(), String>>>,
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
        completion: Option<&CompletionSender<Result<(), String>>>,
    ) -> io::Result<()> {
        if data.is_empty() {
            if let Some(completion) = completion {
                let _ = completion.send(Ok(()));
            }
            return Ok(());
        }

        let pending = reserve_bytes(&self.pending_bytes, data.len(), self.limit_bytes).map_err(
            |pending_bytes| {
                self.status.record(PtyWriterEvent::Backpressure {
                    attempted_bytes: data.len(),
                    pending_bytes,
                    limit_bytes: self.limit_bytes,
                });
                io::Error::new(io::ErrorKind::WouldBlock, "PTY input queue is full")
            },
        )?;

        let item = WriterItem {
            data: data.to_vec(),
            completion: completion.cloned(),
        };
        match self.sender.try_send(item) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.pending_bytes.fetch_sub(data.len(), Ordering::AcqRel);
                self.status.record(PtyWriterEvent::Backpressure {
                    attempted_bytes: data.len(),
                    pending_bytes: pending,
                    limit_bytes: self.limit_bytes,
                });
                Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "PTY input queue has no free message slot",
                ))
            }
            Err(TrySendError::Disconnected(item)) => {
                self.pending_bytes.fetch_sub(data.len(), Ordering::AcqRel);
                if let Some(completion) = item.completion {
                    let _ = completion.send(Err("PTY writer is not available".to_string()));
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
    let pending_bytes = Arc::new(AtomicUsize::new(0));
    let status = Arc::new(WriterStatus::default());
    let thread_pending = Arc::clone(&pending_bytes);
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
                    &thread_pending,
                    thread_limit,
                );
                thread_pending.fetch_sub(item.data.len(), Ordering::AcqRel);
                if let Err(error) = result {
                    if let Some(completion) = item.completion {
                        let _ = completion.send(Err(error.to_string()));
                    }
                    thread_status.record(PtyWriterEvent::Failed(error.to_string()));
                    break;
                }
                if let Some(completion) = item.completion {
                    let _ = completion.send(Ok(()));
                }
            }
        })?;

    let queued_writer = BoundedPtyWriter {
        sender,
        pending_bytes,
        limit_bytes,
        status: Arc::clone(&status),
    };
    let owner = PtyWriterOwner {
        status,
        thread: Mutex::new(Some(thread)),
    };
    Ok((queued_writer, owner))
}

fn write_with_retry(
    writer: &mut Box<dyn Write + Send>,
    data: &[u8],
    status: &WriterStatus,
    pending_bytes: &AtomicUsize,
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
                        pending_bytes: pending_bytes.load(Ordering::Acquire),
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
                        pending_bytes: pending_bytes.load(Ordering::Acquire),
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

fn reserve_bytes(pending: &AtomicUsize, amount: usize, limit: usize) -> Result<usize, usize> {
    let mut current = pending.load(Ordering::Acquire);
    loop {
        let Some(next) = current.checked_add(amount).filter(|next| *next <= limit) else {
            return Err(current);
        };
        match pending.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Ok(current),
            Err(actual) => current = actual,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct CapturedBytes(Arc<Mutex<Vec<u8>>>);

    impl Write for CapturedBytes {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn writer_preserves_order() {
        let capture = CapturedBytes::default();
        let read_capture = capture.clone();
        let (mut writer, owner) = spawn_bounded_pty_writer(Box::new(capture), 64).unwrap();

        writer.write_all(b"user-").unwrap();
        writer.write_all(b"query-").unwrap();
        writer.write_all(b"focus").unwrap();
        drop(writer);
        owner.join().unwrap();

        assert_eq!(
            read_capture
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_slice(),
            b"user-query-focus"
        );
    }

    #[test]
    fn writer_retries_partial_writes_until_the_full_paste_is_delivered() {
        #[derive(Clone)]
        struct PartialWriter {
            bytes: Arc<Mutex<Vec<u8>>>,
            maximum: usize,
        }

        impl Write for PartialWriter {
            fn write(&mut self, data: &[u8]) -> io::Result<usize> {
                let count = self.maximum.min(data.len());
                self.bytes
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .extend_from_slice(&data[..count]);
                Ok(count)
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let bytes = Arc::new(Mutex::new(Vec::new()));
        let read_bytes = Arc::clone(&bytes);
        let writer = PartialWriter { bytes, maximum: 2 };
        let (mut queued, owner) = spawn_bounded_pty_writer(Box::new(writer), 64).unwrap();

        queued.write_all(b"normal-paste").unwrap();
        queued.write_all(b"-bracketed-paste").unwrap();
        drop(queued);
        owner.join().unwrap();

        assert_eq!(
            read_bytes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_slice(),
            b"normal-paste-bracketed-paste"
        );
    }

    #[test]
    fn writer_retries_temporary_os_backpressure_without_losing_paste_bytes() {
        #[derive(Clone)]
        struct TemporarilyBlockedWriter {
            bytes: Arc<Mutex<Vec<u8>>>,
            blocked: Arc<AtomicUsize>,
        }

        impl Write for TemporarilyBlockedWriter {
            fn write(&mut self, data: &[u8]) -> io::Result<usize> {
                if self.blocked.load(Ordering::Acquire) > 0 {
                    self.blocked.fetch_sub(1, Ordering::AcqRel);
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "fixture backpressure",
                    ));
                }
                self.bytes
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .extend_from_slice(data);
                Ok(data.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let bytes = Arc::new(Mutex::new(Vec::new()));
        let read_bytes = Arc::clone(&bytes);
        let writer = TemporarilyBlockedWriter {
            bytes,
            blocked: Arc::new(AtomicUsize::new(1)),
        };
        let (mut queued, owner) = spawn_bounded_pty_writer(Box::new(writer), 64).unwrap();

        queued.write_all(b"retry-this-paste").unwrap();
        drop(queued);
        owner.join().unwrap();
        assert_eq!(
            owner.poll_event(),
            Some(PtyWriterEvent::Backpressure {
                attempted_bytes: 16,
                pending_bytes: 16,
                limit_bytes: 64,
            })
        );
        assert_eq!(
            read_bytes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_slice(),
            b"retry-this-paste"
        );
    }

    #[test]
    fn oversized_write_reports_backpressure_without_partial_data() {
        let capture = CapturedBytes::default();
        let read_capture = capture.clone();
        let (mut writer, owner) = spawn_bounded_pty_writer(Box::new(capture), 4).unwrap();

        let error = writer.write_all(b"12345").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(
            owner.poll_event(),
            Some(PtyWriterEvent::Backpressure {
                attempted_bytes: 5,
                pending_bytes: 0,
                limit_bytes: 4,
            })
        );
        drop(writer);
        owner.join().unwrap();

        assert!(
            read_capture
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty()
        );
    }

    #[test]
    fn writer_reports_backpressure_events_in_order() {
        let (mut writer, owner) =
            spawn_bounded_pty_writer(Box::new(CapturedBytes::default()), 4).unwrap();

        assert_eq!(
            writer.write(b"12345").unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            writer.write(b"67890").unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );

        assert_eq!(
            owner.poll_event(),
            Some(PtyWriterEvent::Backpressure {
                attempted_bytes: 5,
                pending_bytes: 0,
                limit_bytes: 4,
            })
        );
        assert_eq!(
            owner.poll_event(),
            Some(PtyWriterEvent::Backpressure {
                attempted_bytes: 5,
                pending_bytes: 0,
                limit_bytes: 4,
            })
        );
        assert_eq!(owner.poll_event(), None);

        drop(writer);
        owner.join().unwrap();
    }

    #[test]
    fn writer_status_preserves_event_order() {
        let status = WriterStatus::default();
        status.record(PtyWriterEvent::Backpressure {
            attempted_bytes: 5,
            pending_bytes: 0,
            limit_bytes: 4,
        });
        status.record(PtyWriterEvent::Failed("write failed".to_string()));
        status.record(PtyWriterEvent::Backpressure {
            attempted_bytes: 6,
            pending_bytes: 0,
            limit_bytes: 4,
        });

        assert_eq!(
            status.take(),
            Some(PtyWriterEvent::Backpressure {
                attempted_bytes: 5,
                pending_bytes: 0,
                limit_bytes: 4,
            })
        );
        assert_eq!(
            status.take(),
            Some(PtyWriterEvent::Failed("write failed".to_string()))
        );
        assert_eq!(
            status.take(),
            Some(PtyWriterEvent::Backpressure {
                attempted_bytes: 6,
                pending_bytes: 0,
                limit_bytes: 4,
            })
        );
        assert_eq!(status.take(), None);
    }

    #[test]
    fn writer_status_bounds_diagnostics_and_preserves_failure() {
        let status = WriterStatus::default();
        for attempted_bytes in 0..WRITER_EVENT_SLOTS + 1 {
            status.record(PtyWriterEvent::Backpressure {
                attempted_bytes,
                pending_bytes: 0,
                limit_bytes: 4,
            });
        }
        status.record(PtyWriterEvent::Failed("write failed".to_string()));

        let events = status.events.lock().unwrap();
        assert_eq!(events.queue.len(), WRITER_EVENT_SLOTS);
        assert_eq!(
            events.queue.front(),
            Some(&PtyWriterEvent::Backpressure {
                attempted_bytes: 0,
                pending_bytes: 0,
                limit_bytes: 4,
            })
        );
        assert_eq!(
            events.queue.back(),
            Some(&PtyWriterEvent::Backpressure {
                attempted_bytes: WRITER_EVENT_SLOTS - 1,
                pending_bytes: 0,
                limit_bytes: 4,
            })
        );
        assert_eq!(
            events.latched_failure,
            Some(PtyWriterEvent::Failed("write failed".to_string()))
        );
        drop(events);

        for attempted_bytes in 0..WRITER_EVENT_SLOTS {
            assert_eq!(
                status.take(),
                Some(PtyWriterEvent::Backpressure {
                    attempted_bytes,
                    pending_bytes: 0,
                    limit_bytes: 4,
                })
            );
        }
        assert_eq!(
            status.take(),
            Some(PtyWriterEvent::Failed("write failed".to_string()))
        );
        assert_eq!(status.take(), None);
    }
}
