use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

const WRITER_QUEUE_SLOTS: usize = 1_024;
const WRITER_EVENT_SLOTS: usize = 64;

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
}

struct QueuedPtyWriter {
    sender: SyncSender<Vec<u8>>,
    pending_bytes: Arc<AtomicUsize>,
    limit_bytes: usize,
    status: Arc<WriterStatus>,
}

impl Write for QueuedPtyWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
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

        match self.sender.try_send(data.to_vec()) {
            Ok(()) => Ok(data.len()),
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
            Err(TrySendError::Disconnected(_)) => {
                self.pending_bytes.fetch_sub(data.len(), Ordering::AcqRel);
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

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn spawn_bounded_pty_writer(
    writer: Box<dyn Write + Send>,
    limit_bytes: usize,
) -> io::Result<(Box<dyn Write + Send>, PtyWriterOwner)> {
    if limit_bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "PTY input queue limit must be non-zero",
        ));
    }

    let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(WRITER_QUEUE_SLOTS);
    let pending_bytes = Arc::new(AtomicUsize::new(0));
    let status = Arc::new(WriterStatus::default());
    let thread_pending = Arc::clone(&pending_bytes);
    let thread_status = Arc::clone(&status);
    let thread = thread::Builder::new()
        .name("pty-writer".to_string())
        .spawn(move || {
            let mut writer = writer;
            while let Ok(data) = receiver.recv() {
                let result = writer.write_all(&data).and_then(|()| writer.flush());
                thread_pending.fetch_sub(data.len(), Ordering::AcqRel);
                if let Err(error) = result {
                    thread_status.record(PtyWriterEvent::Failed(error.to_string()));
                    break;
                }
            }
        })?;

    let queued_writer = QueuedPtyWriter {
        sender,
        pending_bytes,
        limit_bytes,
        status: Arc::clone(&status),
    };
    let owner = PtyWriterOwner {
        status,
        thread: Mutex::new(Some(thread)),
    };
    Ok((Box::new(queued_writer), owner))
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
