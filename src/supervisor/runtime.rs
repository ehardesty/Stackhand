//! The production runtime adapter. It wraps the existing Run interface —
//! `RunRuntime`, `OwnedRun`, and their bounded shutdown — and never exposes
//! Process Tree, pipe, PTY, sampler, or terminal ownership to the
//! Supervisor.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(test)]
use crate::geometry::TerminalGeometry;
use crate::output::{OutputViews, ProcessOutput};
use crate::runtime::{
    LiveLogMatcher, LogPattern, OsPid, ProcessId, RunEvent, RunEventKind, RunId as RuntimeRunId,
    RunMode, RunOutputObserver, RunOutputReceiver, RunRuntime, RunStartRequest, SpawnCommand,
    root_exit_pending,
};
use crate::supervisor::FailureKind;
use crate::supervisor::seam::{
    AttemptId, FinishedRun, LogMatcherIntent, RunSeam, SeamEvent, SeamSender, StartIntent, WorkId,
};

use super::consoles::Consoles;
#[cfg(test)]
use super::run_record::{AdapterTestHooks, TestPause};
use super::run_record::{FinishCause, RunKey, RunRecord, RunRegistry};

/// How often a retained-output drain polls for the Run's output channel
/// closing; output chunks themselves arrive without waiting for this.
const OUTPUT_DRAIN_POLL: Duration = Duration::from_millis(50);
const METRICS_INTERVAL: Duration = Duration::from_secs(2);

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

    fn begin_shutdown(&self, deadline: Instant) {
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
        let log_matcher = build_output_observer(&intent, record.cancellation_flag(), events);
        record.set_log_matcher(log_matcher.clone());
        let output_observer = build_run_output_observer(
            intent.pty,
            intent.run_id,
            self.outputs
                .for_process_id(intent.process_id)
                .expect("the registry covers every configured Process"),
            log_matcher,
        );
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
            // Mark the Run before starting its readers. This gives every
            // observed byte, including immediate PTY output, a preceding Run
            // boundary in Logs view.
            outputs
                .for_process_id(intent.process_id)
                .expect("the registry covers every configured Process")
                .mark_run(intent.run_id.get());
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
        deadline: Option<Instant>,
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
            Some(record) => record.request_stop(deadline),
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

/// PTY output has one combined stream. This observer projects it into the
/// same bounded Logs owner as pipe output without routing bytes through the
/// Supervisor. A fan-out is real here: readiness matching and Logs retention
/// are independent consumers at the existing Run output seam.
struct PtyLogsObserver {
    output: Arc<ProcessOutput>,
    run_id: RuntimeRunId,
}

impl RunOutputObserver for PtyLogsObserver {
    fn observe(&self, data: &[u8]) {
        self.output.append(
            self.run_id.get(),
            crate::runtime::OutputStream::Combined,
            data.to_vec(),
        );
    }
}

struct OutputObserverFanout {
    observers: Vec<Arc<dyn RunOutputObserver>>,
}

impl RunOutputObserver for OutputObserverFanout {
    fn observe(&self, data: &[u8]) {
        for observer in &self.observers {
            observer.observe(data);
        }
    }
}

fn build_run_output_observer(
    pty: bool,
    run_id: RuntimeRunId,
    output: Arc<ProcessOutput>,
    matcher: Option<Arc<LiveLogMatcher>>,
) -> Option<Arc<dyn RunOutputObserver>> {
    let mut observers: Vec<Arc<dyn RunOutputObserver>> = matcher
        .into_iter()
        .map(|observer| observer as Arc<dyn RunOutputObserver>)
        .collect();
    if pty {
        observers.push(Arc::new(PtyLogsObserver { output, run_id }));
    }
    match observers.len() {
        0 => None,
        1 => observers.pop(),
        _ => Some(Arc::new(OutputObserverFanout { observers })),
    }
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
                FinishCause::Natural => match record.project_deadline() {
                    Some(deadline) => run.wait_until(deadline),
                    None => run.wait(),
                },
                FinishCause::Stop(request) => match request.deadline {
                    Some(deadline) => run.shutdown_until(deadline),
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
            best_effort: metrics.best_effort,
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
    for key in &intent.env_remove {
        command = command.without_env(key.clone());
    }
    for (key, value) in &intent.env {
        command = command.with_env(key.clone(), value.clone());
    }
    command
}

#[cfg(test)]
#[path = "tests/runtime_adapter.rs"]
mod tests;
