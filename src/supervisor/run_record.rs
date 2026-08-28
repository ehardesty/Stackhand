//! Run ownership coordination for the production runtime adapter.
//!
//! This module owns the adapter-side protocol from spawn reservation through
//! confirmed cleanup. It reports facts to the Supervisor but does not own
//! lifecycle policy.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::runtime::{LiveLogMatcher, LogPattern, OwnedRun, TerminalHandle};
use crate::supervisor::seam::{AttemptId, LogMatcherIntent};

/// Identifies one active Run inside the adapter.
pub(super) type RunKey = (u32, u64);
pub(super) type RunRegistry = Arc<Mutex<HashMap<RunKey, Arc<RunRecord>>>>;

/// How often a Run owner polls for natural exit and low-volume events.
pub(super) const OWNER_POLL: Duration = Duration::from_millis(50);

#[cfg(test)]
#[derive(Clone, Default)]
pub(super) struct AdapterTestHooks {
    pub(super) after_spawn: Option<TestPause>,
    pub(super) after_finished: Option<TestPause>,
}

#[cfg(test)]
#[derive(Clone)]
pub(super) struct TestPause {
    reached: Arc<std::sync::Barrier>,
    resume: Arc<std::sync::Barrier>,
}

#[cfg(test)]
impl TestPause {
    pub(super) fn new() -> Self {
        Self {
            reached: Arc::new(std::sync::Barrier::new(2)),
            resume: Arc::new(std::sync::Barrier::new(2)),
        }
    }

    pub(super) fn pause_worker(&self) {
        self.reached.wait();
        self.resume.wait();
    }

    pub(super) fn wait_until_reached(&self) {
        self.reached.wait();
    }

    pub(super) fn resume(&self) {
        self.resume.wait();
    }
}

#[derive(Clone, Copy)]
pub(super) struct StopRequest {
    pub(super) remaining: Option<Duration>,
}

#[derive(Clone, Copy)]
pub(super) enum FinishCause {
    Natural,
    Stop(StopRequest),
}

impl FinishCause {
    pub(super) fn intentional_stop(self) -> bool {
        matches!(self, Self::Stop(_))
    }
}

/// One serialized ownership protocol for a Run from synchronous reservation
/// through confirmed cleanup. The Run stays in exactly one phase. Stop,
/// natural exit, terminal access, and Project deadline updates all use this
/// record instead of competing map removals or a second completion registry.
enum RunState {
    Spawning,
    StopBeforeSpawn(StopRequest),
    Active(OwnedRun),
    Pending { run: OwnedRun, cause: FinishCause },
    Finishing,
    Unconfirmed(OwnedRun),
    Finished,
}

struct RunCoordination {
    state: RunState,
    project_deadline: Option<Instant>,
}

pub(super) struct RunRecord {
    coordination: Mutex<RunCoordination>,
    wake: Condvar,
    /// Closes terminal access and auxiliary observations as soon as the
    /// Supervisor replaces or stops this Run. Process cleanup and the
    /// bounded output drain remain owned by `OwnedRun`.
    cancelled: Arc<AtomicBool>,
    /// The output observer is retained so the Supervisor can arm fresh
    /// liveness log windows after the Run has spawned.
    log_matcher: Mutex<Option<Arc<LiveLogMatcher>>>,
    #[cfg(test)]
    pub(super) test_hooks: AdapterTestHooks,
}

impl RunRecord {
    pub(super) fn spawning() -> Self {
        Self {
            coordination: Mutex::new(RunCoordination {
                state: RunState::Spawning,
                project_deadline: None,
            }),
            wake: Condvar::new(),
            cancelled: Arc::new(AtomicBool::new(false)),
            log_matcher: Mutex::new(None),
            #[cfg(test)]
            test_hooks: AdapterTestHooks::default(),
        }
    }

    #[cfg(test)]
    pub(super) fn spawning_with_test_hooks(test_hooks: AdapterTestHooks) -> Self {
        Self {
            test_hooks,
            ..Self::spawning()
        }
    }

    pub(super) fn cancellation_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancelled)
    }

    pub(super) fn set_log_matcher(&self, matcher: Option<Arc<LiveLogMatcher>>) {
        *self
            .log_matcher
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = matcher;
    }

    pub(super) fn arm_log_matcher(&self, matcher: LogMatcherIntent) {
        let live_matcher = self
            .log_matcher
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(live_matcher) = live_matcher else {
            return;
        };
        live_matcher
            .replace(LogPattern {
                key: matcher.work_id.get(),
                contains: matcher.contains,
                attempt_id: matcher.attempt_id.map(AttemptId::get),
            })
            .expect("validated log liveness patterns remain valid");
    }

    /// Install a spawned Run. Its output marker and drain are ready before
    /// either active use or a stop queued during spawn can finish the Run.
    pub(super) fn install(&self, run: OwnedRun, on_spawned: impl FnOnce()) {
        let mut coordination = self
            .coordination
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        on_spawned();
        coordination.state = match coordination.state {
            RunState::Spawning => RunState::Active(run),
            RunState::StopBeforeSpawn(request) => RunState::Pending {
                run,
                cause: FinishCause::Stop(request),
            },
            _ => panic!("a Run can be installed only into its spawn reservation"),
        };
        self.wake.notify_one();
    }

    pub(super) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.wake.notify_all();
    }

    pub(super) fn request_stop(&self, remaining: Option<Duration>) {
        let request = StopRequest { remaining };
        let mut coordination = self
            .coordination
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let state = std::mem::replace(&mut coordination.state, RunState::Finishing);
        coordination.state = match state {
            RunState::Spawning => RunState::StopBeforeSpawn(request),
            RunState::StopBeforeSpawn(_) => RunState::StopBeforeSpawn(request),
            RunState::Active(run) | RunState::Unconfirmed(run) => RunState::Pending {
                run,
                cause: FinishCause::Stop(request),
            },
            state @ (RunState::Pending { .. } | RunState::Finishing | RunState::Finished) => state,
        };
        self.wake.notify_one();
    }

    pub(super) fn set_project_deadline(&self, deadline: Instant) {
        let mut coordination = self
            .coordination
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        coordination.project_deadline = Some(deadline);
        self.wake.notify_one();
    }

    /// Claim the only completion action. A queued stop wins over natural
    /// exit because it was recorded first under the same lock.
    pub(super) fn take_completion(&self, natural_exit: bool) -> Option<(OwnedRun, FinishCause)> {
        let mut coordination = self
            .coordination
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let state = std::mem::replace(&mut coordination.state, RunState::Finishing);
        match state {
            RunState::Pending { run, cause } => Some((run, cause)),
            RunState::Active(run) if natural_exit => Some((run, FinishCause::Natural)),
            state => {
                coordination.state = state;
                None
            }
        }
    }

    pub(super) fn project_remaining(&self) -> Option<Duration> {
        self.coordination
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .project_deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }

    pub(super) fn finish(&self, run: OwnedRun, cleanup_confirmed: bool) {
        let mut coordination = self
            .coordination
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        coordination.state = if cleanup_confirmed {
            RunState::Finished
        } else {
            RunState::Unconfirmed(run)
        };
        self.wake.notify_all();
    }

    pub(super) fn wait_for_work(&self) {
        let coordination = self
            .coordination
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = self
            .wake
            .wait_timeout(coordination, OWNER_POLL)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }

    pub(super) fn is_active(&self) -> bool {
        !self.cancelled.load(Ordering::Acquire)
            && matches!(
                self.coordination
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .state,
                RunState::Active(_)
            )
    }

    pub(super) fn is_finished(&self) -> bool {
        matches!(
            self.coordination
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .state,
            RunState::Finished
        )
    }

    pub(super) fn with_terminal<R>(&self, f: impl FnOnce(&TerminalHandle<'_>) -> R) -> Option<R> {
        if self.cancelled.load(Ordering::Acquire) {
            return None;
        }
        let coordination = self
            .coordination
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match &coordination.state {
            RunState::Active(run) => run.terminal().map(|handle| f(&handle)),
            _ => None,
        }
    }
}
