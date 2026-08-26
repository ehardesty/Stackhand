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
    OwnedRun, ProcessId, RunEvent, RunEventKind, RunId as RuntimeRunId, RunMode, RunRuntime,
    RunStartRequest, SpawnCommand,
};
use crate::supervisor::seam::{RunSeam, SeamEvent, SeamSender, StartIntent};

/// Identifies one active Run inside the adapter.
type RunKey = (u32, u64);

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
/// on worker threads. All results return as typed [`SeamEvent`]s.
#[derive(Default, Clone)]
pub(crate) struct RealRunSeam {
    runs: Arc<Mutex<HashMap<RunKey, RunSlot>>>,
}

impl RunSeam for RealRunSeam {
    fn start(&self, intent: StartIntent, events: &SeamSender) {
        let events = events.clone();
        let runs = Arc::clone(&self.runs);
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
                        initial_geometry: TerminalGeometry::DEFAULT,
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
                    runs.lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .insert(key, RunSlot::Active(run));
                    forward_events(intent.process_id, intent.run_id, &events, event_rx);
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
        let slot = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&key);
        match slot {
            Some(RunSlot::Active(run)) => {
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
            None => events.send(SeamEvent::Failed {
                process_id,
                run_id,
                detail: "stop requested for a Run that is not active".to_string(),
            }),
        }
    }
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
    command
}

/// Forward low-volume Run events into the Supervisor seam until the Run's
/// event channel closes. High-volume output stays on the Run's own drain
/// path and never enters this loop.
fn forward_events(
    process_id: ProcessId,
    run_id: RuntimeRunId,
    events: &SeamSender,
    event_rx: mpsc::Receiver<RunEvent>,
) {
    while let Ok(event) = event_rx.recv() {
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
            // ShutdownComplete is reported by the stop worker itself so it
            // can carry structured cleanup results.
            RunEventKind::ShutdownComplete => continue,
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
}
