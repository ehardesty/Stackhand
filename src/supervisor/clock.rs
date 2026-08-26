//! The time seam. Production uses the system clock; Supervisor tests use a
//! fake clock so ordering never depends on wall-clock sleeps.

use std::time::Instant;

pub(crate) trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

pub(crate) struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}
