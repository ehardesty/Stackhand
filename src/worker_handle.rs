//! At-most-once ownership and bounded joining for one worker thread.

use std::sync::Mutex;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const JOIN_POLL_INTERVAL: Duration = Duration::from_millis(2);

pub(crate) struct WorkerHandle {
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl WorkerHandle {
    pub(crate) fn new(thread: JoinHandle<()>) -> Self {
        Self {
            thread: Mutex::new(Some(thread)),
        }
    }

    pub(crate) fn join(&self) -> thread::Result<()> {
        let Some(thread) = self.take() else {
            return Ok(());
        };
        thread.join()
    }

    pub(crate) fn join_until(&self, deadline: Instant) -> thread::Result<bool> {
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
            thread::sleep(JOIN_POLL_INTERVAL);
        }
    }

    pub(crate) fn abandon_nonblocking(&self) -> bool {
        let Some(thread) = self.take() else {
            return true;
        };
        if thread.is_finished() {
            thread.join().is_ok()
        } else {
            false
        }
    }

    fn take(&self) -> Option<JoinHandle<()>> {
        self.thread
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}
