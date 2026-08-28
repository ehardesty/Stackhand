//! The production runtime adapter. It wraps the existing Run interface —
//! `RunRuntime`, `OwnedRun`, and their bounded shutdown — and never exposes
//! Process Tree, pipe, PTY, sampler, or terminal ownership to the
//! Supervisor.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crate::geometry::TerminalGeometry;
use crate::output::OutputViews;
use crate::runtime::{
    LiveLogMatcher, LogPattern, OsPid, OwnedRun, ProcessId, RunEvent, RunEventKind,
    RunId as RuntimeRunId, RunMode, RunOutputObserver, RunOutputReceiver, RunRuntime,
    RunStartRequest, SpawnCommand, TerminalHandle, root_exit_pending,
};
use crate::supervisor::FailureKind;
use crate::supervisor::seam::{
    AttemptId, FinishedRun, LogMatcherIntent, RunSeam, SeamEvent, SeamSender, StartIntent, WorkId,
};
use crate::terminal::{OwnedTerminalSnapshot, TerminalEvent};

/// Identifies one active Run inside the adapter.
type RunKey = (u32, u64);
type RunRegistry = Arc<Mutex<HashMap<RunKey, Arc<RunRecord>>>>;

/// How often a Run owner polls for natural exit and low-volume events.
const OWNER_POLL: Duration = Duration::from_millis(50);
/// How often a retained-output drain polls for the Run's output channel
/// closing; output chunks themselves arrive without waiting for this.
const OUTPUT_DRAIN_POLL: Duration = Duration::from_millis(50);
const METRICS_INTERVAL: Duration = Duration::from_secs(2);

#[cfg(test)]
#[derive(Clone, Default)]
struct AdapterTestHooks {
    after_spawn: Option<TestPause>,
    after_finished: Option<TestPause>,
}

#[cfg(test)]
#[derive(Clone)]
struct TestPause {
    reached: Arc<std::sync::Barrier>,
    resume: Arc<std::sync::Barrier>,
}

#[cfg(test)]
impl TestPause {
    fn new() -> Self {
        Self {
            reached: Arc::new(std::sync::Barrier::new(2)),
            resume: Arc::new(std::sync::Barrier::new(2)),
        }
    }

    fn pause_worker(&self) {
        self.reached.wait();
        self.resume.wait();
    }

    fn wait_until_reached(&self) {
        self.reached.wait();
    }

    fn resume(&self) {
        self.resume.wait();
    }
}

#[derive(Clone, Copy)]
struct StopRequest {
    remaining: Option<Duration>,
}

#[derive(Clone, Copy)]
enum FinishCause {
    Natural,
    Stop(StopRequest),
}

impl FinishCause {
    fn intentional_stop(self) -> bool {
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

struct RunRecord {
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
    test_hooks: AdapterTestHooks,
}

impl RunRecord {
    fn spawning() -> Self {
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
    fn spawning_with_test_hooks(test_hooks: AdapterTestHooks) -> Self {
        Self {
            test_hooks,
            ..Self::spawning()
        }
    }

    fn set_log_matcher(&self, matcher: Option<Arc<LiveLogMatcher>>) {
        *self
            .log_matcher
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = matcher;
    }

    fn arm_log_matcher(&self, matcher: LogMatcherIntent) {
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
    fn install(&self, run: OwnedRun, on_spawned: impl FnOnce()) {
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

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.wake.notify_all();
    }

    fn request_stop(&self, remaining: Option<Duration>) {
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

    fn set_project_deadline(&self, deadline: Instant) {
        let mut coordination = self
            .coordination
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        coordination.project_deadline = Some(deadline);
        self.wake.notify_one();
    }

    /// Claim the only completion action. A queued stop wins over natural
    /// exit because it was recorded first under the same lock.
    fn take_completion(&self, natural_exit: bool) -> Option<(OwnedRun, FinishCause)> {
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

    fn project_remaining(&self) -> Option<Duration> {
        self.coordination
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .project_deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }

    fn finish(&self, run: OwnedRun, cleanup_confirmed: bool) {
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

    fn wait_for_work(&self) {
        let coordination = self
            .coordination
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = self
            .wake
            .wait_timeout(coordination, OWNER_POLL)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }

    fn is_active(&self) -> bool {
        !self.cancelled.load(Ordering::Acquire)
            && matches!(
                self.coordination
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .state,
                RunState::Active(_)
            )
    }

    fn is_finished(&self) -> bool {
        matches!(
            self.coordination
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .state,
            RunState::Finished
        )
    }

    fn with_terminal<R>(&self, f: impl FnOnce(&TerminalHandle<'_>) -> R) -> Option<R> {
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

/// Starts Runs through the real Run interface. Synchronous reservations and
/// cheap coordinator updates stay on the caller. Spawn, completion, output
/// drain, and worker joins stay on Run-owned worker threads.
pub(crate) struct RealRunSeam {
    runs: RunRegistry,
    outputs: Arc<OutputViews>,
    #[cfg(test)]
    test_hooks: AdapterTestHooks,
}

impl RealRunSeam {
    pub(crate) fn new(outputs: Arc<OutputViews>) -> Self {
        Self {
            runs: Arc::new(Mutex::new(HashMap::new())),
            outputs,
            #[cfg(test)]
            test_hooks: AdapterTestHooks::default(),
        }
    }

    #[cfg(test)]
    fn with_test_hooks(outputs: Arc<OutputViews>, test_hooks: AdapterTestHooks) -> Self {
        Self {
            test_hooks,
            ..Self::new(outputs)
        }
    }

    pub(crate) fn consoles(&self) -> Consoles {
        Consoles {
            runs: Arc::clone(&self.runs),
        }
    }
}

impl RunSeam for RealRunSeam {
    fn cancel(&self, process_id: ProcessId, run_id: RuntimeRunId) {
        let key = (process_id.get(), run_id.get());
        if let Some(record) = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&key)
            .cloned()
        {
            record.cancel();
        }
    }

    fn begin_shutdown(&self, remaining: Duration) {
        let deadline = Instant::now() + remaining;
        let records: Vec<_> = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect();
        for record in records {
            record.set_project_deadline(deadline);
        }
    }

    fn start(&self, intent: StartIntent, events: &SeamSender) {
        let key = (intent.process_id.get(), intent.run_id.get());
        #[cfg(not(test))]
        let record = Arc::new(RunRecord::spawning());
        #[cfg(test)]
        let record = Arc::new(RunRecord::spawning_with_test_hooks(self.test_hooks.clone()));
        let output_observer = build_output_observer(&intent, Arc::clone(&record.cancelled), events);
        record.set_log_matcher(output_observer.clone());
        let mut runs = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Keep at most one published completion per Process. It closes the
        // stop-before-application race, then the next Run reservation
        // replaces it without a separate tombstone registry.
        runs.retain(|existing_key, existing| existing_key.0 != key.0 || !existing.is_finished());
        runs.insert(key, Arc::clone(&record));
        drop(runs);

        let events = events.clone();
        let runs = Arc::clone(&self.runs);
        let outputs = Arc::clone(&self.outputs);
        thread::spawn(move || {
            let (event_tx, event_rx) = mpsc::channel::<RunEvent>();
            let (output_tx, output_rx) = crate::runtime::output_channel();
            let output_observer =
                output_observer.map(|observer| observer as Arc<dyn RunOutputObserver>);
            let request = RunStartRequest {
                process_id: intent.process_id,
                run_id: intent.run_id,
                command: build_command(&intent),
                mode: if intent.pty {
                    RunMode::Pty {
                        initial_geometry: intent.initial_geometry,
                    }
                } else {
                    RunMode::Pipe
                },
                events: event_tx,
                output: output_tx,
                ladder: Default::default(),
                metrics_interval: Some(METRICS_INTERVAL),
                on_output_wake: None,
                output_observer,
            };
            match RunRuntime.start(request) {
                Ok(run) => {
                    #[cfg(test)]
                    if let Some(pause) = &record.test_hooks.after_spawn {
                        pause.pause_worker();
                    }
                    let root_pid = run.root_pid();
                    record.install(run, || {
                        outputs
                            .for_process_id(intent.process_id)
                            .expect("the registry covers every configured Process")
                            .mark_run(intent.run_id.get());
                        drain_retained_output(output_rx, outputs, intent.process_id);
                    });
                    own_run(key, root_pid, record, events, event_rx);
                }
                Err(error) => {
                    remove_record(&runs, key, &record);
                    events.send(SeamEvent::Failed {
                        process_id: intent.process_id,
                        run_id: intent.run_id,
                        kind: FailureKind::Configuration,
                        detail: format!("spawn failed: {error}"),
                    });
                }
            }
        });
    }

    fn arm_log_matcher(
        &self,
        process_id: ProcessId,
        run_id: RuntimeRunId,
        matcher: LogMatcherIntent,
    ) {
        let key = (process_id.get(), run_id.get());
        if let Some(record) = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&key)
            .cloned()
        {
            record.arm_log_matcher(matcher);
        }
    }

    fn stop(
        &self,
        process_id: ProcessId,
        run_id: RuntimeRunId,
        remaining: Option<Duration>,
        events: &SeamSender,
    ) {
        let key = (process_id.get(), run_id.get());
        let record = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&key)
            .cloned();
        match record {
            Some(record) => record.request_stop(remaining),
            None => events.send(SeamEvent::Failed {
                process_id,
                run_id,
                kind: FailureKind::Spawn,
                detail: "stop requested for a Run that is not active".to_string(),
            }),
        }
    }
}

fn build_output_observer(
    intent: &StartIntent,
    cancelled: Arc<AtomicBool>,
    events: &SeamSender,
) -> Option<Arc<LiveLogMatcher>> {
    if intent.log_matchers.is_empty() {
        return None;
    }
    let patterns = intent
        .log_matchers
        .iter()
        .map(|matcher| LogPattern {
            key: matcher.work_id.get(),
            contains: matcher.contains.clone(),
            attempt_id: matcher.attempt_id.map(AttemptId::get),
        })
        .collect();
    let process_id = intent.process_id;
    let run_id = intent.run_id;
    let events = events.clone();
    let matcher = LiveLogMatcher::new_with_attempts(patterns, cancelled, move |key, attempt_id| {
        events.send(SeamEvent::LogMatched {
            process_id,
            run_id,
            work_id: WorkId::new(key),
            attempt_id: attempt_id.map(AttemptId::new),
        });
    })
    .expect("validated log health patterns create a matcher");
    Some(matcher)
}

/// Drain output independently of the cancellation latch. Output is a
/// bounded data-plane stream, not a lifecycle result; continuing the drain
/// preserves final child output while the existing `OwnedRun` cleanup ladder
/// runs. Lifecycle events are gated separately by `RunRecord::cancel`.
fn drain_retained_output(
    output_rx: RunOutputReceiver,
    outputs: Arc<OutputViews>,
    process_id: ProcessId,
) {
    let Some(module) = outputs.for_process_id(process_id) else {
        return;
    };
    thread::spawn(move || {
        loop {
            match output_rx.recv_timeout(OUTPUT_DRAIN_POLL) {
                Ok(chunk) => module.append(chunk.run_id.get(), chunk.stream, chunk.data),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    });
}

/// Own one Run from spawn through confirmed cleanup. The same loop forwards
/// low-volume live events, chooses stop versus natural completion, consumes
/// the public Run completion events, and reports one finished-Run fact.
/// Output remains a separate data plane during cancellation so bytes already
/// produced by the child can still drain before `OwnedRun` cleanup completes;
/// output never changes the Supervisor lifecycle snapshot.
fn own_run(
    key: RunKey,
    root_pid: Option<OsPid>,
    record: Arc<RunRecord>,
    events: SeamSender,
    event_rx: mpsc::Receiver<RunEvent>,
) {
    loop {
        forward_pending_events(key, &event_rx, &events);
        let natural_exit = root_pid.is_some_and(root_exit_pending);
        if let Some((mut run, cause)) = record.take_completion(natural_exit) {
            let result = match cause {
                FinishCause::Natural => match record.project_remaining() {
                    Some(remaining) => run.wait_with_timeout(remaining),
                    None => run.wait(),
                },
                FinishCause::Stop(request) => match request.remaining {
                    Some(remaining) => run.shutdown_with_timeout(remaining),
                    None => run.shutdown(),
                },
            };
            // `OwnedRun` emits Exited before its completion event. Consume
            // that ordered public protocol before publishing the one seam
            // fact derived from its returned outcome.
            forward_pending_events(key, &event_rx, &events);
            let finished = finished_run(key, &result, cause.intentional_stop());
            let cleanup_confirmed = finished.cleanup_confirmed;
            record.finish(run, cleanup_confirmed);
            events.send(SeamEvent::Finished(finished));
            if cleanup_confirmed {
                // Keep this finished record until the Process reserves its
                // next Run. A stop racing event application then sees the
                // completed state and remains a no-op.
                #[cfg(test)]
                if let Some(pause) = &record.test_hooks.after_finished {
                    pause.pause_worker();
                }
                return;
            }
            continue;
        }
        record.wait_for_work();
    }
}

fn remove_record(runs: &RunRegistry, key: RunKey, record: &Arc<RunRecord>) {
    let mut runs = runs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if runs
        .get(&key)
        .is_some_and(|stored| Arc::ptr_eq(stored, record))
    {
        runs.remove(&key);
    }
}

fn forward_pending_events(key: RunKey, event_rx: &mpsc::Receiver<RunEvent>, events: &SeamSender) {
    while let Ok(event) = event_rx.try_recv() {
        forward_event(key, event, events);
    }
}

fn forward_event(key: RunKey, event: RunEvent, events: &SeamSender) {
    let (process_id, run_id) = key_to_ids(key);
    let seam_event = match event.kind {
        RunEventKind::Spawned { root_pid } => SeamEvent::Spawned {
            process_id,
            run_id,
            root_pid,
        },
        // The returned RunOutcome is the one completion authority. These
        // ordered Run events are consumed here and do not create a second
        // Supervisor completion protocol.
        RunEventKind::Exited { .. } | RunEventKind::ShutdownComplete | RunEventKind::Failed(_) => {
            return;
        }
        RunEventKind::OutputDropped { .. } => return,
        RunEventKind::IoFailed(detail) => SeamEvent::OutputFailure {
            process_id,
            run_id,
            detail,
        },
        RunEventKind::Metrics(metrics) => SeamEvent::Metrics {
            process_id,
            run_id,
            cpu_percent: metrics.cpu_percent,
            rss_kib: metrics.rss_kib,
        },
    };
    events.send(seam_event);
}

fn finished_run(
    key: RunKey,
    result: &anyhow::Result<crate::runtime::RunOutcome>,
    intentional_stop_if_error: bool,
) -> FinishedRun {
    let (process_id, run_id) = key_to_ids(key);
    match result {
        Ok(outcome) => FinishedRun {
            process_id,
            run_id,
            exit_code: outcome.exit_code,
            intentional_stop: outcome.intentional_stop,
            cleanup_confirmed: outcome.cleanup_confirmed,
            detail: (!outcome.cleanup_confirmed).then(|| {
                format!(
                    "cleanup did not confirm; remaining PIDs: {:?}",
                    outcome.remaining_pids
                )
            }),
            remaining_pids: outcome.remaining_pids.clone(),
        },
        Err(error) => FinishedRun {
            process_id,
            run_id,
            exit_code: None,
            intentional_stop: intentional_stop_if_error,
            cleanup_confirmed: false,
            detail: Some(error.to_string()),
            remaining_pids: Vec::new(),
        },
    }
}

fn key_to_ids(key: RunKey) -> (ProcessId, RuntimeRunId) {
    (ProcessId::new(key.0), RuntimeRunId::new(key.1))
}

fn build_command(intent: &StartIntent) -> SpawnCommand {
    let mut command = SpawnCommand::new(intent.program.clone());
    for arg in &intent.args {
        command = command.arg(arg.clone());
    }
    command = command.with_current_dir(intent.working_dir.clone());
    for (key, value) in &intent.env {
        command = command.with_env(key.clone(), value.clone());
    }
    command
}

/// Data-plane access to the terminal session each PTY Run owns. Output bytes
/// and terminal interaction stay outside the Supervisor control queue.
pub struct Consoles {
    runs: RunRegistry,
}

impl Consoles {
    /// The live console view for one Process's current Run, when one is
    /// active.
    pub fn view_process(&self, process_id: ProcessId, run_id: u64) -> Option<ConsoleView> {
        let record = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&(process_id.get(), run_id))
            .cloned()?;
        record.is_active().then_some(ConsoleView { record })
    }

    /// The live console view by scalar Process identity.
    /// Kept for caller compatibility; internal callers use [`Self::view_process`].
    pub fn view(&self, process_id: u32, run_id: u64) -> Option<ConsoleView> {
        self.view_process(ProcessId::new(process_id), run_id)
    }
}

/// A shared handle to one active PTY Run's terminal. Every operation locks
/// one Run coordinator briefly and never performs process work.
pub struct ConsoleView {
    record: Arc<RunRecord>,
}

impl ConsoleView {
    pub(crate) fn with<R>(&self, f: impl FnOnce(&TerminalHandle<'_>) -> R) -> Option<R> {
        self.record.with_terminal(f)
    }

    pub fn snapshot(&self) -> Option<OwnedTerminalSnapshot> {
        self.with(|handle| handle.snapshot())
    }

    pub fn is_dirty(&self) -> bool {
        self.with(|handle| handle.is_dirty()).unwrap_or(false)
    }

    pub fn mouse_tracking(&self) -> bool {
        self.with(|handle| handle.mouse_tracking()).unwrap_or(false)
    }

    pub fn poll_event(&self) -> Option<TerminalEvent> {
        self.with(|handle| handle.poll_event())?
    }

    /// Resize to the selected console geometry. Returns false when the Run
    /// rejected the request (stopping or backpressure); non-fatal either way.
    pub fn resize(&self, geometry: TerminalGeometry) -> bool {
        let result = self.with(|handle| handle.resize(geometry));
        !matches!(result, Some(Err(_)))
    }
}

#[cfg(test)]
#[path = "tests/runtime_adapter.rs"]
mod tests;
