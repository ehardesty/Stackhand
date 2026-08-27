//! Shared synchronization for headless Supervisor fixtures.
//!
//! Production scheduling does not use this module. Each fixture supplies
//! its own bound, polling interval, and phase diagnostics.

use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::supervisor::{ProjectSnapshot, SupervisorHandle};

/// The fixture-owned bounds and diagnostics for one snapshot wait.
pub(crate) struct SnapshotWait<'a> {
    pub(crate) timeout: Duration,
    pub(crate) poll_interval: Duration,
    pub(crate) stopped_message: &'a str,
    pub(crate) timeout_message: &'a str,
}

/// Wait until an immutable Supervisor snapshot satisfies `ready`.
///
/// The wait is always bounded. A stopped Supervisor and an expired bound
/// remain different failures so each fixture can name the failed phase.
pub(crate) fn wait_for_snapshot(
    supervisor: &SupervisorHandle,
    wait: SnapshotWait<'_>,
    ready: impl Fn(&ProjectSnapshot) -> bool,
) -> Result<ProjectSnapshot> {
    let deadline = Instant::now() + wait.timeout;
    loop {
        match supervisor.snapshot() {
            Some(snapshot) if ready(&snapshot) => return Ok(snapshot),
            Some(_) => {}
            None => bail!("{}", wait.stopped_message),
        }
        if Instant::now() >= deadline {
            bail!("{}", wait.timeout_message);
        }
        std::thread::sleep(wait.poll_interval);
    }
}
