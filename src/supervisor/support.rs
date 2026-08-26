//! Private test-only adapters: a scripted fake runtime and a fake clock.
//! They implement the same seams as the production adapters and never
//! become part of the external Supervisor interface.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::runtime::{ProcessId, RunId};
use crate::supervisor::clock::Clock;
use crate::supervisor::seam::{
    ProbeIntent, ProbeSeam, RunSeam, SeamEvent, SeamSender, StartIntent,
};

/// One observable runtime action the Supervisor requested.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Intent {
    Start {
        process_id: ProcessId,
        run_id: RunId,
    },
    Stop {
        process_id: ProcessId,
        run_id: RunId,
    },
}

/// A scripted runtime. It records every intent so tests assert emitted
/// runtime actions, and it can be told how Runs behave without any real
/// process work.
#[derive(Default)]
pub(crate) struct FakeRuntime {
    intents: Mutex<Vec<Intent>>,
    /// When set, every start reports a spawn failure instead of succeeding.
    pub(crate) fail_spawn: AtomicBool,
    /// When set, stop reports unconfirmed cleanup.
    pub(crate) fail_cleanup: AtomicBool,
}

impl FakeRuntime {
    pub(crate) fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) fn set_fail_spawn(&self, value: bool) {
        self.fail_spawn.store(value, Ordering::Release);
    }

    pub(crate) fn intents(&self) -> Vec<Intent> {
        self.intents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn record(&self, intent: Intent) {
        self.intents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(intent);
    }
}

impl RunSeam for FakeRuntime {
    fn start(&self, intent: StartIntent, events: &SeamSender) {
        self.record(Intent::Start {
            process_id: intent.process_id,
            run_id: intent.run_id,
        });
        if self.fail_spawn.load(Ordering::Acquire) {
            events.send(SeamEvent::Failed {
                process_id: intent.process_id,
                run_id: intent.run_id,
                detail: "scripted spawn failure".to_string(),
            });
        }
    }

    fn stop(&self, process_id: ProcessId, run_id: RunId, events: &SeamSender) {
        self.record(Intent::Stop { process_id, run_id });
        // A real adapter observes the root exit during cleanup; the fake
        // reports the same event order with a scripted code.
        events.send(SeamEvent::Exited {
            process_id,
            run_id,
            code: Some(0),
        });
        events.send(SeamEvent::ShutdownComplete {
            process_id,
            run_id,
            confirmed: !self.fail_cleanup.load(Ordering::Acquire),
            detail: self
                .fail_cleanup
                .load(Ordering::Acquire)
                .then(|| "scripted cleanup failure".to_string()),
        });
    }
}

/// A controllable clock. Tests advance it explicitly; nothing in a test
/// depends on wall-clock sleeps. Readiness interval scheduling consumes it.
#[derive(Clone)]
pub(crate) struct FakeClock {
    now: Arc<Mutex<Instant>>,
}

impl FakeClock {
    pub(crate) fn new() -> Self {
        Self {
            now: Arc::new(Mutex::new(Instant::now())),
        }
    }

    pub(crate) fn advance(&self, by: Duration) {
        let mut now = self
            .now
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *now += by;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        *self
            .now
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Let tests keep a shared handle to the scripted runtime while the core
/// owns its boxed seam.
impl RunSeam for Arc<FakeRuntime> {
    fn start(&self, intent: StartIntent, events: &SeamSender) {
        (**self).start(intent, events);
    }

    fn stop(&self, process_id: ProcessId, run_id: RunId, events: &SeamSender) {
        (**self).stop(process_id, run_id, events);
    }
}

/// A scripted probe runner. It records every dispatched attempt so tests
/// assert readiness scheduling; results arrive as scripted `Readiness`
/// events through [`Harness`](super::tests)'s event path.
#[derive(Default)]
pub(crate) struct FakeProbes {
    attempts: Mutex<Vec<(ProcessId, RunId)>>,
}

impl FakeProbes {
    pub(crate) fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The (process, run) identities of every dispatched attempt in order.
    pub(crate) fn attempts(&self) -> Vec<(ProcessId, RunId)> {
        self.attempts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl ProbeSeam for FakeProbes {
    fn probe(&self, intent: ProbeIntent, _events: &SeamSender) {
        self.attempts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((intent.process_id, intent.run_id));
    }
}

impl ProbeSeam for Arc<FakeProbes> {
    fn probe(&self, intent: ProbeIntent, events: &SeamSender) {
        (**self).probe(intent, events);
    }
}
