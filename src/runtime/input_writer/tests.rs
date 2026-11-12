use std::sync::atomic::{AtomicUsize, Ordering};

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
fn paste_completion_waits_for_partial_writes_and_flush() {
    struct PartialWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
        flushing: mpsc::Sender<()>,
        finish: mpsc::Receiver<io::Result<()>>,
    }
    impl Write for PartialWriter {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            let count = 2.min(data.len());
            self.bytes.lock().unwrap().extend_from_slice(&data[..count]);
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.flushing.send(()).unwrap();
            self.finish.recv().unwrap()
        }
    }

    for fail_flush in [false, true] {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let (flushing, started) = mpsc::channel();
        let (finish, finishing) = mpsc::channel();
        let writer = PartialWriter {
            bytes: Arc::clone(&bytes),
            flushing,
            finish: finishing,
        };
        let (queued, owner) = spawn_bounded_pty_writer(Box::new(writer), 64).unwrap();
        let (completion, outcome) = mpsc::channel();
        queued
            .try_enqueue_with_completion(b"normal-paste", Some(&completion))
            .unwrap();
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(outcome.try_recv(), Err(mpsc::TryRecvError::Empty)));
        let result = if fail_flush {
            Err(io::Error::other("flush failed"))
        } else {
            Ok(())
        };
        finish.send(result).unwrap();
        let result = outcome.recv_timeout(Duration::from_secs(2)).unwrap();
        if fail_flush {
            assert_eq!(result, PasteCompletion::Failed("flush failed".to_string()));
        } else {
            assert_eq!(result, PasteCompletion::Delivered);
        }
        assert_eq!(bytes.lock().unwrap().as_slice(), b"normal-paste");
        drop(queued);
        owner.join().unwrap();
    }
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
    assert_eq!(owner.poll_event(), None);
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
#[test]
fn cancellation_fails_in_flight_and_queued_pastes_without_a_writer_fault() {
    struct BlockedWriter {
        released: Arc<std::sync::atomic::AtomicBool>,
        attempted: Option<mpsc::Sender<()>>,
        closed: mpsc::Sender<()>,
    }

    impl Write for BlockedWriter {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            if self.released.load(Ordering::Acquire) {
                Ok(data.len())
            } else {
                if let Some(attempted) = self.attempted.take() {
                    let _ = attempted.send(());
                }
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Drop for BlockedWriter {
        fn drop(&mut self) {
            let _ = self.closed.send(());
        }
    }

    let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (attempted, blocked) = mpsc::channel();
    let (closed, stopped) = mpsc::channel();
    let writer = BlockedWriter {
        released: Arc::clone(&released),
        attempted: Some(attempted),
        closed,
    };
    let (queued, owner) = spawn_bounded_pty_writer(Box::new(writer), 64).unwrap();
    let (completion_a, outcome_a) = mpsc::channel();
    let (completion_b, outcome_b) = mpsc::channel();
    queued
        .try_enqueue_with_completion(b"paste-a", Some(&completion_a))
        .unwrap();
    queued
        .try_enqueue_with_completion(b"paste-b", Some(&completion_b))
        .unwrap();
    drop((completion_a, completion_b));
    let mut request_a = crate::terminal::PasteRequest::new(1, outcome_a);
    let mut request_b = crate::terminal::PasteRequest::new(2, outcome_b);
    let blocked = blocked.recv_timeout(Duration::from_secs(2));

    owner.cancel();
    let joined = owner.join_until(Instant::now() + Duration::from_secs(2));
    released.store(true, Ordering::Release);
    drop(queued);
    let stopped = stopped.recv_timeout(Duration::from_secs(2));

    assert!(
        blocked.is_ok(),
        "the current paste never reached OS backpressure"
    );
    assert!(
        stopped.is_ok(),
        "the fixture writer did not stop after cleanup"
    );
    assert!(
        joined.unwrap(),
        "cancellation did not join the writer in time"
    );
    assert!(matches!(request_a.poll(), Some(PasteCompletion::Failed(_))));
    assert!(matches!(request_b.poll(), Some(PasteCompletion::Failed(_))));
    while let Some(event) = owner.poll_event() {
        assert!(!matches!(event, PtyWriterEvent::Failed(_)));
    }
}
