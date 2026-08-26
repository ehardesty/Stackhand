//! The production runtime adapter. It wraps the existing Run interface —
//! `RunRuntime`, `OwnedRun`, and their bounded shutdown — and never exposes
//! Process Tree, pipe, PTY, sampler, or terminal ownership to the
//! Supervisor.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use crate::geometry::TerminalGeometry;
use crate::runtime::{
    OsPid, OwnedRun, ProcessId, RunEvent, RunEventKind, RunId as RuntimeRunId, RunMode, RunRuntime,
    RunStartRequest, SpawnCommand, TerminalHandle, root_exit_pending,
};
use crate::supervisor::seam::{RunSeam, SeamEvent, SeamSender, StartIntent};
use crate::terminal::{OwnedTerminalSnapshot, TerminalEvent};

/// Identifies one active Run inside the adapter.
type RunKey = (u32, u64);

/// How often a Run's owner loop polls its root child for natural exit.
const NATURAL_EXIT_POLL: Duration = Duration::from_millis(50);

/// One map slot for a Run between the start request and the finished
/// spawn. The reservation exists synchronously before the spawn worker
/// starts so a stop request can cancel a Run that has not appeared yet.
#[allow(clippy::large_enum_variant)] // The spawning slot exists only for the short spawn window.
enum RunSlot {
    Spawning { cancelled: Arc<AtomicBool> },
    Active(OwnedRun),
}

const METRICS_INTERVAL: Duration = Duration::from_secs(2);

/// Starts Runs through the real Run interface and performs bounded shutdown
/// on worker threads. Each active Run has one owner loop that observes a
/// root exiting on its own — a One-shot completing or a Service dying — and
/// reports `Exited` plus `ShutdownComplete` without user action. All results
/// return as typed [`SeamEvent`]s.
#[derive(Default)]
pub(crate) struct RealRunSeam {
    runs: Arc<Mutex<HashMap<RunKey, RunSlot>>>,
    /// Run keys whose bounded cleanup a worker already claimed. A stop for a
    /// tombstoned key is a harmless no-op: the in-flight natural completion
    /// already reports this Run's ShutdownComplete.
    completed: Arc<Mutex<std::collections::HashSet<RunKey>>>,
}

impl RealRunSeam {
    pub(crate) fn consoles(&self) -> Consoles {
        Consoles {
            runs: Arc::clone(&self.runs),
        }
    }
}

impl RunSeam for RealRunSeam {
    fn start(&self, intent: StartIntent, events: &SeamSender) {
        let events = events.clone();
        let runs = Arc::clone(&self.runs);
        let completed = Arc::clone(&self.completed);
        // Reserve the Run identity synchronously so no stop can fall into
        // the window before the spawn worker registers the OwnedRun.
        let cancelled = Arc::new(AtomicBool::new(false));
        self.runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                (intent.process_id.get(), intent.run_id.get()),
                RunSlot::Spawning {
                    cancelled: Arc::clone(&cancelled),
                },
            );
        // Spawn work stays off the Supervisor control task.
        thread::spawn(move || {
            let key = (intent.process_id.get(), intent.run_id.get());
            let (event_tx, event_rx) = mpsc::channel::<RunEvent>();
            let (output_tx, _output_rx) = crate::runtime::output_channel();
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
            };
            match RunRuntime.start(request) {
                Ok(mut run) => {
                    if cancelled.load(Ordering::Acquire) {
                        // A stop arrived while the spawn was starting;
                        // finish it immediately instead of leaving an
                        // orphaned process behind.
                        report_shutdown(
                            (intent.process_id, intent.run_id),
                            run.shutdown(),
                            &events,
                        );
                        return;
                    }
                    let root_pid = run.root_pid();
                    runs.lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .insert(key, RunSlot::Active(run));
                    // This worker becomes the Run's single owner: it
                    // forwards Run events and observes natural root exit in
                    // one serialized loop, so the Supervisor always sees
                    // `Exited` before the completion report.
                    own_run(key, root_pid, runs, completed, events, event_rx);
                }
                Err(error) => {
                    runs.lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&key);
                    events.send(SeamEvent::Failed {
                        process_id: intent.process_id,
                        run_id: intent.run_id,
                        detail: format!("spawn failed: {error}"),
                    });
                }
            }
        });
    }

    fn stop(&self, process_id: ProcessId, run_id: RuntimeRunId, events: &SeamSender) {
        let key = (process_id.get(), run_id.get());
        let completed = Arc::clone(&self.completed);
        let slot = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&key);
        match slot {
            Some(RunSlot::Active(run)) => {
                completed
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(key);
                let events = events.clone();
                thread::spawn(move || {
                    let mut run = run;
                    // The complete bounded shutdown ladder runs here, never
                    // on the control task.
                    report_shutdown((process_id, run_id), run.shutdown(), &events);
                });
            }
            Some(RunSlot::Spawning { cancelled }) => {
                // The spawn worker owns this Run's completion; ask it to
                // shut down as soon as the child appears.
                cancelled.store(true, Ordering::Release);
            }
            // A tombstoned Run already has an in-flight natural completion
            // that will report this Run's ShutdownComplete; stay silent so a
            // racing stop never fabricates a false failure.
            None if self
                .completed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(&key) => {}
            None => events.send(SeamEvent::Failed {
                process_id,
                run_id,
                detail: "stop requested for a Run that is not active".to_string(),
            }),
        }
    }
}

/// Own one active Run until it ends: forward low-volume Run events into the
/// Supervisor seam and observe natural root exit in one serialized loop, so
/// `Exited` always reaches the Supervisor before the Run's completion
/// report. When a stop worker claims the Run first, this loop keeps only the
/// forwarding duty until the Run's event channel closes.
fn own_run(
    key: RunKey,
    root_pid: Option<OsPid>,
    runs: Arc<Mutex<HashMap<RunKey, RunSlot>>>,
    completed: Arc<Mutex<std::collections::HashSet<RunKey>>>,
    events: SeamSender,
    event_rx: mpsc::Receiver<RunEvent>,
) {
    loop {
        // Everything the Run already emitted goes out first; the Spawned
        // event is waiting here before this loop ever runs.
        match event_rx.try_recv() {
            Ok(event) => forward_event(key, event, &events),
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => return,
        }
        if completed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(&key)
        {
            // A stop worker owns this Run's cleanup; keep forwarding until
            // the channel closes.
            while let Ok(event) = event_rx.recv() {
                forward_event(key, event, &events);
            }
            return;
        }
        let Some(root) = root_pid else {
            // Without a root identity there is nothing to observe; forward
            // until the Run ends.
            while let Ok(event) = event_rx.recv() {
                forward_event(key, event, &events);
            }
            return;
        };
        if !root_exit_pending(root) {
            thread::sleep(NATURAL_EXIT_POLL);
            continue;
        }
        // Natural root exit observed. Claiming the map entry makes a
        // concurrent stop a harmless no-op against the completed set.
        let claimed = runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&key);
        let Some(RunSlot::Active(mut run)) = claimed else {
            // Still spawning or already claimed by a stop; that owner
            // reports this Run's completion.
            continue;
        };
        completed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key);
        // shutdown() emits the Run's own Exited event synchronously; every
        // emission is already buffered when it returns, so a non-blocking
        // sweep forwards them before the completion report. (The channel
        // itself only closes when `run` drops below.)
        let outcome = run.shutdown();
        while let Ok(event) = event_rx.try_recv() {
            forward_event(key, event, &events);
        }
        report_shutdown(key_to_ids(key), outcome, &events);
        return;
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
        RunEventKind::Exited { code } => SeamEvent::Exited {
            process_id,
            run_id,
            code,
        },
        // Completion is reported by the Run's owning worker so it can carry
        // structured cleanup results.
        RunEventKind::ShutdownComplete => return,
        RunEventKind::Failed(detail) | RunEventKind::IoFailed(detail) => SeamEvent::Failed {
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

fn key_to_ids(key: RunKey) -> (ProcessId, RuntimeRunId) {
    (ProcessId::new(key.0), RuntimeRunId::new(key.1))
}

fn report_shutdown(
    (process_id, run_id): (ProcessId, RuntimeRunId),
    outcome: anyhow::Result<crate::runtime::RunOutcome>,
    events: &SeamSender,
) {
    let (confirmed, detail) = match outcome {
        Ok(outcome) if outcome.cleanup_confirmed => (true, None),
        Ok(outcome) => (
            false,
            Some(format!(
                "cleanup did not confirm; remaining PIDs: {:?}",
                outcome.remaining_pids
            )),
        ),
        Err(error) => (false, Some(error.to_string())),
    };
    events.send(SeamEvent::ShutdownComplete {
        process_id,
        run_id,
        confirmed,
        detail,
    });
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

/// Data-plane access to the terminal session each PTY Run owns. Output
/// bytes and terminal interaction stay outside the Supervisor control
/// queue; this registry only locates the current Run of a Process.
pub struct Consoles {
    runs: Arc<Mutex<HashMap<RunKey, RunSlot>>>,
}

impl Consoles {
    /// The live console view for one Process's current Run, when one is
    /// active.
    pub fn view(&self, process_id: u32, run_id: u64) -> Option<ConsoleView> {
        let guard = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        matches!(guard.get(&(process_id, run_id)), Some(RunSlot::Active(_))).then(|| ConsoleView {
            runs: Arc::clone(&self.runs),
            key: (process_id, run_id),
        })
    }
}

/// A shared handle to one active PTY Run's terminal. Every operation locks
/// briefly and never blocks on process work.
pub struct ConsoleView {
    runs: Arc<Mutex<HashMap<RunKey, RunSlot>>>,
    key: RunKey,
}

impl ConsoleView {
    pub(crate) fn with<R>(&self, f: impl FnOnce(&TerminalHandle<'_>) -> R) -> Option<R> {
        let guard = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match guard.get(&self.key) {
            Some(RunSlot::Active(run)) => run.terminal().map(|handle| f(&handle)),
            _ => None,
        }
    }

    pub fn snapshot(&self) -> Option<OwnedTerminalSnapshot> {
        self.with(|handle| handle.snapshot())
    }

    pub fn is_dirty(&self) -> bool {
        self.with(|handle| handle.is_dirty()).unwrap_or(false)
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
