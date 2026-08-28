use std::time::{Duration, Instant};

use crate::geometry::TerminalGeometry;

use super::{EVENT_POLL_INTERVAL, RESIZE_SETTLE_INTERVAL};

#[derive(Default)]
pub(super) struct PendingResize {
    latest: Option<(TerminalGeometry, Instant)>,
}

impl PendingResize {
    pub(super) fn update(&mut self, geometry: TerminalGeometry, now: Instant) {
        self.latest = Some((geometry, now + RESIZE_SETTLE_INTERVAL));
    }

    pub(super) fn take_ready(&mut self, now: Instant) -> Option<TerminalGeometry> {
        let (geometry, ready_at) = self.latest?;
        if now < ready_at {
            return None;
        }
        self.latest = None;
        Some(geometry)
    }

    pub(super) fn poll_interval(&self, now: Instant) -> Duration {
        self.latest
            .map(|(_, ready_at)| ready_at.saturating_duration_since(now))
            .unwrap_or(EVENT_POLL_INTERVAL)
            .min(EVENT_POLL_INTERVAL)
    }
}
