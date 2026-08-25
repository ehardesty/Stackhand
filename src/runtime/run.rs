//! One owning Run value per Process execution attempt.
//!
//! `RunRuntime::start` is the highest test and caller seam. It starts one Run
//! and returns an [`OwnedRun`] that owns the root process, the process I/O,
//! the output drain, and the optional [`TerminalSession`] lifetime. Callers
//! use semantic operations through [`OwnedRun`] and its non-owning
//! [`TerminalHandle`]. They never coordinate separate process and terminal
//! shutdown, and they never receive raw child, pipe, PTY, reader, writer, or
//! sampler handles.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::geometry::TerminalGeometry;
use crate::runtime::ladder::{self, RUN_EXIT_POLL_INTERVAL};
use crate::runtime::metrics::{MetricsSampler, RunMetrics};
use crate::runtime::outcome::{
    ResizeRejected, RunExitDisposition, RunOutcome, ShutdownLadder, StageResult,
};
use crate::runtime::pipe::{PipeRun, RunOutput};
use crate::runtime::process_tree::{SemanticSignal, SignalError, UnixProcessTree};
use crate::runtime::{PtyProcess, SpawnCommand};
use crate::terminal::{
    CopyRequest, OutputHistoryMetrics, OwnedTerminalSnapshot, PasteRejection, PasteRequest,
    TerminalEvent, TerminalMouseEvent, TerminalSession,
};

/// Identifies one supervised Process across Runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProcessId(u32);

impl ProcessId {
    pub fn new(value: u32) -> Self {
        Self(value)
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

/// Identifies one execution attempt of a Process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RunId(u64);

impl RunId {
    pub fn new(value: u64) -> Self {
        Self(value)
    }
}

/// The transport mode requested for one Run.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // Pipe mode is exercised through tests today and by the Supervisor next.
pub enum RunMode {
    /// Interactive transport with terminal semantics.
    Pty { initial_geometry: TerminalGeometry },
    /// Non-interactive transport with separate stdout and stderr drains.
    Pipe,
}

/// Everything needed to start one Run.
pub struct RunStartRequest {
    pub process_id: ProcessId,
    pub run_id: RunId,
    pub command: SpawnCommand,
    pub mode: RunMode,
    /// Low-volume sink for Run state events. High-volume output never enters
    /// this path.
    pub events: mpsc::Sender<RunEvent>,
    /// High-volume sink for pipe-mode process output. PTY-mode Runs deliver
    /// output into their TerminalSession instead.
    pub output: mpsc::Sender<RunOutput>,
    /// The configured semantic shutdown ladder timeouts for this Run.
    pub ladder: ShutdownLadder,
    /// Aggregate Process Tree sampling interval. `None` disables sampling.
    pub metrics_interval: Option<Duration>,
    /// Optional wake called when terminal output arrives. This is the
    /// redraw-notification path for interactive hosts; it never carries
    /// output bytes.
    pub on_output_wake: Option<Box<dyn Fn() + Send + 'static>>,
}

/// A low-volume Run lifecycle event. Every event carries the `RunId`.
#[derive(Clone, Debug, PartialEq)]
pub struct RunEvent {
    pub run_id: RunId,
    pub kind: RunEventKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RunEventKind {
    /// The root process spawned. Carries the root PID when the platform
    /// reports one.
    Spawned {
        root_pid: Option<ProcessId>,
    },
    /// All Run resources completed cleanup and joined.
    ShutdownComplete,
    Failed(String),
    /// The root process exited on its own or was reaped during cleanup.
    Exited {
        code: Option<i32>,
    },
    /// One owned I/O task failed. Carries no output bytes.
    IoFailed(String),
    /// One bounded aggregate Process Tree sample. At most one is emitted
    /// per configured interval.
    Metrics(RunMetrics),
}

impl RunEvent {
    fn new(run_id: RunId, kind: RunEventKind) -> Self {
        Self { run_id, kind }
    }
}

/// Why a fire-and-forget input item was not delivered: the Run is shutting
/// down and no longer admits input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputRejected;

/// Starts Runs. This is the external seam; callers keep no other handle to a
/// Run's internals.
#[derive(Debug, Default)]
pub struct RunRuntime;

impl RunRuntime {
    pub fn start(&self, request: RunStartRequest) -> Result<OwnedRun> {
        let inner = match request.mode {
            RunMode::Pty { initial_geometry } => {
                let spawned = PtyProcess::spawn(request.command, initial_geometry)?;
                let wake = request.on_output_wake.unwrap_or_else(|| Box::new(|| {}));
                let session = TerminalSession::spawn(spawned.io, initial_geometry, wake)?;
                let tree = spawned.process.process_id().map(UnixProcessTree::from_root);
                (
                    tree,
                    RunInner::Pty {
                        process: spawned.process,
                        session,
                    },
                )
            }
            RunMode::Pipe => {
                let pipe = PipeRun::spawn(
                    &request.command,
                    request.run_id,
                    request.events.clone(),
                    request.output.clone(),
                )?;
                let tree = pipe.root_pid().map(UnixProcessTree::from_root);
                (tree, RunInner::Pipe(pipe))
            }
        };
        let (tree, inner) = inner;
        let root_pid = match &inner {
            RunInner::Pty { process, .. } => process.process_id(),
            RunInner::Pipe(pipe) => pipe.root_pid(),
        };
        let stopping = Arc::new(AtomicBool::new(false));
        let metrics = root_pid.and_then(|root_pid| {
            request.metrics_interval.map(|interval| {
                MetricsSampler::spawn(
                    root_pid,
                    request.process_id,
                    request.run_id,
                    interval,
                    Arc::clone(&stopping),
                    request.events.clone(),
                )
            })
        });
        let run = OwnedRun {
            process_id: request.process_id,
            run_id: request.run_id,
            inner: Some(inner),
            tree,
            ladder: request.ladder,
            stopping,
            metrics,
            retired_pty: None,
            events: request.events,
            outcome: None,
        };
        run.emit(RunEventKind::Spawned {
            root_pid: run.root_pid(),
        });
        Ok(run)
    }
}

/// One owned Run. It owns the root process, the process I/O, the output
/// drain, and the optional TerminalSession lifetime.
pub struct OwnedRun {
    // Part of the Run identity surface; consumed by callers even though this
    // module does not read it internally yet.
    #[allow(dead_code)]
    process_id: ProcessId,
    run_id: RunId,
    inner: Option<RunInner>,
    // The owned Process Tree identity. Present when the platform reports a
    // root PID at spawn time.
    #[allow(dead_code)]
    tree: Option<UnixProcessTree>,
    ladder: ShutdownLadder,
    /// Set before any cleanup work starts. Input and resize admission gates
    /// read this flag. Shared with the metrics sampler so it stops with the
    /// Run.
    stopping: Arc<AtomicBool>,
    /// Aggregate Process Tree sampler, present when sampling is enabled.
    metrics: Option<MetricsSampler>,
    /// A finalized PTY session kept only so input/resize admission gates
    /// stay observable after completion.
    retired_pty: Option<TerminalSession>,
    events: mpsc::Sender<RunEvent>,
    outcome: Option<RunOutcome>,
}

enum RunInner {
    Pty {
        process: PtyProcess,
        session: TerminalSession,
    },
    Pipe(PipeRun),
}

/// Upper bound for observing natural root exit through `wait()`.
const RUN_EXIT_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

fn wait_for_pty_exit(process: &mut PtyProcess) -> Result<Option<i32>> {
    let deadline = Instant::now() + RUN_EXIT_WAIT_TIMEOUT;
    while Instant::now() < deadline {
        if let Some(code) = process.try_wait()? {
            return Ok(Some(code));
        }
        std::thread::sleep(RUN_EXIT_POLL_INTERVAL);
    }
    Err(anyhow::anyhow!(
        "root process did not exit within its wait deadline"
    ))
}

impl OwnedRun {
    // Identity accessors stay available on the public seam for callers.
    #[allow(dead_code)]
    pub fn process_id(&self) -> ProcessId {
        self.process_id
    }

    #[allow(dead_code)]
    pub fn run_id(&self) -> RunId {
        self.run_id
    }

    /// The root operating-system PID when the platform reports one. The
    /// root doubles as the owned process-group identity on Unix.
    pub fn root_pid(&self) -> Option<ProcessId> {
        match self.inner.as_ref()? {
            RunInner::Pty { process, .. } => process.process_id().map(ProcessId::new),
            RunInner::Pipe(pipe) => pipe.root_pid().map(ProcessId::new),
        }
    }

    /// A non-owning terminal view of this Run. Present only for PTY-mode
    /// Runs. The handle exposes the existing terminal actions but has no
    /// terminal shutdown action and cannot detach the TerminalSession from
    /// its Run.
    pub fn terminal(&self) -> Option<TerminalHandle<'_>> {
        let session = match self.inner.as_ref() {
            Some(RunInner::Pty { session, .. }) => session,
            Some(RunInner::Pipe(_)) | None => self.retired_pty.as_ref()?,
        };
        Some(TerminalHandle {
            session,
            stopping: self.stopping.as_ref(),
        })
    }

    /// Whether this Run still admits user input and resize requests.
    pub fn accepts_input(&self) -> bool {
        !self.stopping.load(Ordering::Acquire)
    }

    /// Request a PTY geometry change. Pipe mode has no terminal to resize;
    /// both rejections are non-fatal and leave the Run healthy.
    pub fn resize(&self, geometry: TerminalGeometry) -> Result<(), ResizeRejected> {
        if !self.accepts_input() {
            return Err(ResizeRejected::Stopping);
        }
        match self.inner.as_ref() {
            Some(RunInner::Pty { session, .. }) => {
                session.resize(geometry);
                Ok(())
            }
            _ => Err(ResizeRejected::Unsupported),
        }
    }

    /// Wait for natural completion: observe root exit, reap the root
    /// process, drain all output to EOF, finalize the optional
    /// TerminalSession, and join every Run task. No stop request is
    /// recorded, so the disposition is natural or unexpected.
    pub fn wait(&mut self) -> Result<RunOutcome> {
        if let Some(outcome) = &self.outcome {
            return Ok(outcome.clone());
        }
        // Natural completion still closes admission and stops the sampler:
        // the Run is over either way.
        self.stopping.store(true, Ordering::Release);
        let mut stages = Vec::new();
        let mut io_failures = Vec::new();
        let mut terminal_failure = None;

        let exit_code = match self.inner.as_mut() {
            None => return Err(anyhow::anyhow!("Run {} already completed", self.run_id.0)),
            Some(RunInner::Pipe(pipe)) => {
                let code = match pipe.reap_and_join() {
                    Ok(code) => code,
                    Err(error) => {
                        io_failures.push(error.to_string());
                        None
                    }
                };
                stages.push(StageResult::ok("reap"));
                stages.push(StageResult::ok("drain"));
                code
            }
            Some(RunInner::Pty { process, .. }) => {
                let code = wait_for_pty_exit(process)?;
                // Finalize terminal tasks after root exit; readers drain
                // final PTY bytes inside the session owner before it stops.
                if let Some(RunInner::Pty {
                    process: mut owned_process,
                    session,
                }) = self.inner.take()
                {
                    if let Err(error) = session.shutdown() {
                        terminal_failure = Some(error.to_string());
                    }
                    self.retired_pty = Some(session);
                    match owned_process.shutdown() {
                        Ok(()) => stages.push(StageResult::ok("reap")),
                        Err(error) => stages.push(StageResult::failed("reap", error.to_string())),
                    }
                }
                stages.push(StageResult::ok("drain"));
                code
            }
        };

        // The sampler stops and joins with the Run; a stopped sampler can
        // never emit a later sample for this Run.
        let mut worker_join_failures = Vec::new();
        let (final_metrics, sampler_joined) = match self.metrics.take() {
            Some(sampler) => sampler.stop_and_join(),
            None => (None, true),
        };
        if !sampler_joined {
            worker_join_failures.push("metrics sampler did not join".to_string());
        }

        let disposition = if exit_code == Some(0) {
            RunExitDisposition::NaturalCompletion
        } else {
            RunExitDisposition::UnexpectedExit
        };
        let outcome = RunOutcome {
            run_id: self.run_id,
            disposition,
            intentional_stop: false,
            exit_code,
            stage_results: stages,
            cleanup_confirmed: io_failures.is_empty()
                && terminal_failure.is_none()
                && worker_join_failures.is_empty(),
            remaining_pids: Vec::new(),
            io_failures,
            terminal_failure,
            final_metrics,
            worker_join_failures,
        };
        self.emit(RunEventKind::Exited { code: exit_code });
        self.outcome = Some(outcome.clone());
        Ok(outcome)
    }

    /// Deliver a semantic interrupt to the owned Process Tree. A
    /// process-already-gone race is harmless and returns success.
    pub fn interrupt(&self) -> Result<()> {
        self.signal_tree(SemanticSignal::Interrupt)
    }

    /// Deliver a semantic terminate to the owned Process Tree.
    pub fn terminate(&self) -> Result<()> {
        self.signal_tree(SemanticSignal::Terminate)
    }

    /// Deliver a semantic kill to the owned Process Tree.
    pub fn kill(&self) -> Result<()> {
        self.signal_tree(SemanticSignal::Kill)
    }

    fn signal_tree(&self, semantic: SemanticSignal) -> Result<()> {
        let Some(tree) = self.tree.as_ref() else {
            return Err(anyhow::anyhow!(
                "Run {} has no observable Process Tree identity",
                self.run_id.0
            ));
        };
        // A NotFound race means the tree is already gone; that is success.
        // An Ownership failure fails closed: the caller must not escalate.
        match tree.signal(semantic) {
            Ok(()) | Err(SignalError::NotFound) => Ok(()),
            Err(error) => Err(anyhow::anyhow!(error.detail())),
        }
    }

    /// Own the complete semantic shutdown ladder and produce one structured
    /// outcome:
    ///
    /// record intentional stop → interrupt → wait `graceful_timeout` →
    /// terminate remaining members → wait `terminate_timeout` → kill
    /// remaining members → wait up to `final_deadline` → confirm containment
    /// → reap the root → drain final output → finalize the optional
    /// TerminalSession → join all Run tasks.
    ///
    /// Repeated calls observe the first shutdown instead of repeating it.
    pub fn shutdown(&mut self) -> Result<RunOutcome> {
        if let Some(outcome) = &self.outcome {
            return Ok(outcome.clone());
        }
        // One shutdown request records intentional stop before cleanup.
        self.stopping.store(true, Ordering::Release);

        let ladder = self.ladder;
        let mut trace = match self.tree.as_ref() {
            Some(tree) => ladder::run(tree, ladder),
            None => ladder::LadderTrace::without_identity(),
        };

        // Finalization always follows the ladder, even when a signal stage
        // failed: reaping, draining, terminal finalization, and task joins
        // are never skipped by an earlier failure.
        let mut io_failures = Vec::new();
        let mut terminal_failure = None;
        let mut worker_join_failures = Vec::new();
        let mut exit_code = None;

        if let Some(inner) = self.inner.take() {
            match inner {
                RunInner::Pipe(mut pipe) => {
                    if trace.signals_stopped {
                        worker_join_failures.extend(pipe.abandon_nonblocking());
                        trace.stages.push(StageResult::failed(
                            "reap",
                            "signal escalation failed; root state unconfirmed".to_string(),
                        ));
                    } else {
                        match pipe.reap_and_join() {
                            Ok(code) => {
                                exit_code = code;
                                trace.stages.push(StageResult::ok("reap"));
                                trace.stages.push(StageResult::ok("drain"));
                            }
                            Err(error) => {
                                trace
                                    .stages
                                    .push(StageResult::failed("drain", error.to_string()));
                                io_failures.push(error.to_string());
                                worker_join_failures
                                    .push("pipe readers did not reach EOF".to_string());
                            }
                        }
                    }
                }
                RunInner::Pty {
                    mut process,
                    session,
                } => {
                    // TerminalSession finalization happens only after
                    // Process Tree shutdown made further child output
                    // impossible.
                    if let Err(error) = session.shutdown() {
                        terminal_failure = Some(error.to_string());
                    }
                    self.retired_pty = Some(session);
                    trace.stages.push(StageResult::ok("drain"));
                    // The root exited during escalation or earlier; observe
                    // the code while the child handle is still owned.
                    exit_code = process.try_wait().ok().flatten();
                    if trace.signals_stopped {
                        worker_join_failures.extend(process.abandon_nonblocking());
                    } else {
                        match process.shutdown() {
                            Ok(()) => trace.stages.push(StageResult::ok("reap")),
                            Err(error) => trace
                                .stages
                                .push(StageResult::failed("reap", error.to_string())),
                        }
                    }
                }
            }
        }

        let cleanup_confirmed = !trace.signals_stopped
            && trace.remaining_pids.is_empty()
            && io_failures.is_empty()
            && terminal_failure.is_none()
            && worker_join_failures.is_empty()
            && trace.stages.iter().all(|stage| stage.ok);

        // The sampler stops and joins with the Run; a stopped sampler can
        // never emit a later sample for this Run.
        let (final_metrics, sampler_joined) = match self.metrics.take() {
            Some(sampler) => sampler.stop_and_join(),
            None => (None, true),
        };
        if !sampler_joined {
            worker_join_failures.push("metrics sampler did not join".to_string());
        }
        let cleanup_confirmed = cleanup_confirmed && sampler_joined;
        let outcome = RunOutcome {
            run_id: self.run_id,
            disposition: RunExitDisposition::IntentionalStop,
            intentional_stop: true,
            exit_code,
            stage_results: trace.stages,
            cleanup_confirmed,
            remaining_pids: trace.remaining_pids,
            io_failures,
            terminal_failure,
            final_metrics,
            worker_join_failures,
        };
        // Exit is reported through the low-volume path regardless of how
        // completion was requested (#14).
        self.emit(RunEventKind::Exited { code: exit_code });
        self.emit(match cleanup_confirmed {
            true => RunEventKind::ShutdownComplete,
            false => RunEventKind::Failed("Run cleanup did not fully confirm".to_string()),
        });
        self.outcome = Some(outcome.clone());
        Ok(outcome)
    }

    fn emit(&self, kind: RunEventKind) {
        let _ = self.events.send(RunEvent::new(self.run_id, kind));
    }
}

impl Drop for OwnedRun {
    /// Best-effort abort for an `OwnedRun` dropped without `shutdown()` or
    /// `wait()`. Guaranteed:
    ///
    /// - input admission closes and the metrics sampler stops first;
    /// - SIGKILL is applied to the whole owned Process Tree (best effort,
    ///   single shot);
    /// - the root is reaped when its exit is observable within a short
    ///   bounded window; otherwise an unreaped zombie may remain until this
    ///   process exits;
    /// - terminal worker threads join only within TerminalSession's own
    ///   internal bound.
    ///
    /// NOT guaranteed: final output drains (pipes are abandoned without
    /// waiting for EOF, so output emitted near abort may be lost), pipe
    /// reader threads that descendants keep alive stay detached, and no
    /// `RunOutcome` is produced. Callers that need drained output or a
    /// structured result must call `shutdown()`/`wait()` instead.
    fn drop(&mut self) {
        if self.outcome.is_some() {
            return;
        }
        self.stopping.store(true, Ordering::Release);
        let _ = self.metrics.take().map(|sampler| sampler.stop_and_join());
        if let Some(tree) = self.tree.as_ref() {
            let _ = tree.signal(SemanticSignal::Kill);
            let deadline = Instant::now() + Duration::from_millis(300);
            while Instant::now() < deadline {
                if UnixProcessTree::root_exit_pending(tree.root_pid()) {
                    break;
                }
                std::thread::sleep(RUN_EXIT_POLL_INTERVAL);
            }
        }
        if let Some(inner) = self.inner.take() {
            match inner {
                RunInner::Pipe(mut pipe) => {
                    let _ = pipe.abandon_nonblocking();
                }
                RunInner::Pty {
                    mut process,
                    session,
                } => {
                    let _ = session.shutdown();
                    self.retired_pty = Some(session);
                    let _ = process.abandon_nonblocking();
                }
            }
        }
    }
}

/// Non-owning terminal actions for one Run.
///
/// The handle cannot shut down, replace, or detach the TerminalSession from
/// its Run. Terminal semantics stay inside `TerminalSession`; Process Tree
/// containment and shutdown policy stay inside the Run owner.
pub struct TerminalHandle<'a> {
    session: &'a TerminalSession,
    /// Shared admission gate: once shutdown starts, input is rejected.
    stopping: &'a AtomicBool,
}

/// Internal constructor for crate tests that already own a session.
#[cfg(test)]
pub(crate) fn handle_for_test<'s>(
    session: &'s TerminalSession,
    stopping: &'s AtomicBool,
) -> TerminalHandle<'s> {
    TerminalHandle { session, stopping }
}

impl TerminalHandle<'_> {
    fn admit_input(&self) -> Result<(), InputRejected> {
        if self.stopping.load(Ordering::Acquire) {
            Err(InputRejected)
        } else {
            Ok(())
        }
    }

    /// Send one key event. Rejected once shutdown has started.
    pub fn send_key(&self, event: crossterm::event::KeyEvent) -> Result<(), InputRejected> {
        self.admit_input()?;
        self.session.send_key(event);
        Ok(())
    }

    /// Send a focus change. Rejected once shutdown has started.
    pub fn send_focus(&self, gained: bool) -> Result<(), InputRejected> {
        self.admit_input()?;
        self.session.send_focus(gained);
        Ok(())
    }

    /// Send a mouse event. Rejected once shutdown has started.
    pub fn send_mouse(&self, event: TerminalMouseEvent) -> Result<(), InputRejected> {
        self.admit_input()?;
        self.session.send_mouse(event);
        Ok(())
    }

    /// Send raw bytes. Rejected once shutdown has started.
    pub fn send_raw(&self, data: Vec<u8>) -> Result<(), InputRejected> {
        self.admit_input()?;
        self.session.send_raw(data);
        Ok(())
    }

    /// Admit one whole paste. Rejected with `PasteRejection::Stopping`
    /// once shutdown has started. See [`TerminalSession::send_paste`].
    pub fn send_paste(&self, data: &str) -> Result<PasteRequest, PasteRejection> {
        self.admit_input().map_err(|_| PasteRejection::Stopping)?;
        self.session.send_paste(data)
    }

    /// Resize the terminal. Rejected once shutdown has started.
    pub fn resize(&self, geometry: TerminalGeometry) -> Result<(), ResizeRejected> {
        if self.admit_input().is_err() {
            return Err(ResizeRejected::Stopping);
        }
        self.session.resize(geometry);
        Ok(())
    }

    pub fn scroll_lines(&self, delta: isize) {
        self.session.scroll_lines(delta);
    }

    pub fn follow_live(&self) {
        self.session.follow_live();
    }

    pub fn select_all(&self) {
        self.session.select_all();
    }

    pub fn clear_selection(&self) {
        self.session.clear_selection();
    }

    pub fn request_copy(&self) -> CopyRequest {
        self.session.request_copy()
    }

    pub fn snapshot(&self) -> OwnedTerminalSnapshot {
        self.session.snapshot()
    }

    pub fn is_dirty(&self) -> bool {
        self.session.is_dirty()
    }

    pub fn output_history_metrics(&self) -> OutputHistoryMetrics {
        self.session.output_history_metrics()
    }

    pub fn poll_event(&self) -> Option<TerminalEvent> {
        self.session.poll_event()
    }
}
