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
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::geometry::TerminalGeometry;
use crate::runtime::PtyProcess;
use crate::runtime::ladder::{self, RUN_EXIT_POLL_INTERVAL};
use crate::runtime::metrics::{MetricsSampler, RunMetrics};
use crate::runtime::outcome::{
    ResizeRejected, RunExitDisposition, RunOutcome, ShutdownLadder, StageResult,
};
use crate::runtime::pipe::PipeRun;
use crate::runtime::process_tree::{SemanticSignal, SignalError, UnixProcessTree};
use crate::terminal::{InputRejection as SessionInputRejection, TerminalSession};

use super::start::RunStartRequest;
use super::terminal_handle::TerminalHandle;

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

/// Identifies an operating-system process. This is deliberately distinct
/// from [`ProcessId`], which identifies a configured Stackhand Process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OsPid(u32);

impl OsPid {
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

    pub fn get(self) -> u64 {
        self.0
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
        root_pid: Option<OsPid>,
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
    /// Bounded backpressure: output bytes were dropped because the
    /// caller's queue was full. Metadata, not a failure.
    OutputDropped {
        bytes: usize,
    },
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
pub enum InputRejected {
    /// The Run is stopping or has already stopped.
    Stopping,
    /// The bounded terminal command queue cannot admit this complete item.
    Backpressure {
        attempted_bytes: usize,
        pending_bytes: usize,
        limit_bytes: usize,
    },
}

impl From<SessionInputRejection> for InputRejected {
    fn from(rejection: SessionInputRejection) -> Self {
        match rejection {
            SessionInputRejection::Stopping => Self::Stopping,
            SessionInputRejection::Backpressure {
                attempted_bytes,
                pending_bytes,
                limit_bytes,
            } => Self::Backpressure {
                attempted_bytes,
                pending_bytes,
                limit_bytes,
            },
        }
    }
}

/// Starts Runs. This is the external seam; callers keep no other handle to a
/// Run's internals.
#[derive(Debug, Default)]
pub struct RunRuntime;

impl RunRuntime {
    pub fn start(&self, request: RunStartRequest) -> Result<OwnedRun> {
        let output_observer = request.output_observer.clone();
        let inner = match request.mode {
            RunMode::Pty { initial_geometry } => {
                let spawned = PtyProcess::spawn(request.command, initial_geometry)?;
                let wake = request.on_output_wake.unwrap_or_else(|| Box::new(|| {}));
                let session = TerminalSession::spawn_with_observer(
                    spawned.io,
                    initial_geometry,
                    wake,
                    output_observer,
                )?;
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
                    output_observer,
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
            signals_stopped: AtomicBool::new(false),
            signal_failure: Mutex::new(None),
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
    /// Fail-closed latch for signal ownership errors. Once set, no later
    /// semantic action can target the same numeric Process Group.
    signals_stopped: AtomicBool,
    signal_failure: Mutex<Option<String>>,
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

fn tree_is_settled(tree: Option<&UnixProcessTree>) -> bool {
    let Some(tree) = tree else {
        return false;
    };
    UnixProcessTree::root_exit_pending(tree.root_pid())
        && tree
            .remaining_members_excluding_root()
            .map(|members| members.is_empty())
            .unwrap_or(false)
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
    pub fn root_pid(&self) -> Option<OsPid> {
        match self.inner.as_ref()? {
            RunInner::Pty { process, .. } => process.process_id().map(OsPid::new),
            RunInner::Pipe(pipe) => pipe.root_pid().map(OsPid::new),
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
        Some(TerminalHandle::new(session, self.stopping.as_ref()))
    }

    /// Whether this Run still admits user input and resize requests.
    pub fn accepts_input(&self) -> bool {
        !self.stopping.load(Ordering::Acquire)
    }

    fn ladder_trace(&self) -> ladder::LadderTrace {
        self.ladder_trace_with(self.ladder)
    }

    fn ladder_trace_with(&self, ladder: crate::runtime::ShutdownLadder) -> ladder::LadderTrace {
        if self.signals_stopped.load(Ordering::Acquire) {
            let detail = self
                .signal_failure
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
                .unwrap_or_else(|| "signal escalation was stopped fail-closed".to_string());
            ladder::LadderTrace::signal_failure(detail)
        } else {
            match self.tree.as_ref() {
                Some(tree) => ladder::run(tree, ladder),
                None => ladder::LadderTrace::without_identity(),
            }
        }
    }

    /// Request a PTY geometry change. Pipe mode has no terminal to resize;
    /// both rejections are non-fatal and leave the Run healthy.
    pub fn resize(&self, geometry: TerminalGeometry) -> Result<(), ResizeRejected> {
        if !self.accepts_input() {
            return Err(ResizeRejected::Stopping);
        }
        match self.inner.as_ref() {
            Some(RunInner::Pty { session, .. }) => match session.resize(geometry) {
                Ok(()) => Ok(()),
                Err(SessionInputRejection::Stopping) => Err(ResizeRejected::Stopping),
                Err(SessionInputRejection::Backpressure {
                    attempted_bytes,
                    pending_bytes,
                    limit_bytes,
                }) => Err(ResizeRejected::Backpressure {
                    attempted_bytes,
                    pending_bytes,
                    limit_bytes,
                }),
            },
            _ => Err(ResizeRejected::Unsupported),
        }
    }

    /// Wait naturally, clean descendants, then reap the root.
    pub fn wait(&mut self) -> Result<RunOutcome> {
        self.wait_with_ladder(self.ladder)
    }

    /// Wait naturally while clamping cleanup to the Project deadline.
    pub(crate) fn wait_with_timeout(&mut self, remaining: Duration) -> Result<RunOutcome> {
        self.wait_with_ladder(self.ladder.clamped_to(remaining))
    }

    fn wait_with_ladder(&mut self, ladder: ShutdownLadder) -> Result<RunOutcome> {
        if let Some(outcome) = &self.outcome {
            return Ok(outcome.clone());
        }
        // Natural completion also closes admission and stops the sampler.
        self.stopping.store(true, Ordering::Release);
        if self.inner.is_none() {
            return Err(anyhow::anyhow!("Run {} already completed", self.run_id.0));
        }

        let deadline = Instant::now() + RUN_EXIT_WAIT_TIMEOUT;
        loop {
            let exited = self
                .tree
                .as_ref()
                .is_some_and(|tree| UnixProcessTree::root_exit_pending(tree.root_pid()));
            if exited {
                break;
            }
            if Instant::now() >= deadline {
                return Err(anyhow::anyhow!(
                    "root process did not exit within its wait deadline"
                ));
            }
            std::thread::sleep(RUN_EXIT_POLL_INTERVAL);
        }

        let trace = self.ladder_trace_with(ladder);
        let disposition = RunExitDisposition::UnexpectedExit;
        self.finalize_after_ladder(trace, disposition, false)
    }

    fn finalize_after_ladder(
        &mut self,
        mut trace: ladder::LadderTrace,
        mut disposition: RunExitDisposition,
        intentional_stop: bool,
    ) -> Result<RunOutcome> {
        if trace.signals_stopped {
            self.signals_stopped.store(true, Ordering::Release);
            if let Some(detail) = trace
                .stages
                .iter()
                .find(|stage| !stage.ok)
                .and_then(|stage| stage.detail.clone())
            {
                *self
                    .signal_failure
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(detail);
            }
        }
        let final_deadline = Instant::now() + self.ladder.final_deadline;
        let settled_before_finalize = tree_is_settled(self.tree.as_ref());
        let mut io_failures = Vec::new();
        let mut terminal_failure = None;
        let mut worker_join_failures = Vec::new();
        let mut exit_code = None;
        let mut dropped_output_bytes = 0u64;

        if let Some(inner) = self.inner.take() {
            match inner {
                RunInner::Pipe(mut pipe) => {
                    if settled_before_finalize {
                        let finalized = pipe.finalize_bounded(final_deadline);
                        exit_code = finalized.exit_code;
                        dropped_output_bytes = finalized.dropped_bytes;
                        io_failures.extend(finalized.io_failures);
                        worker_join_failures.extend(finalized.worker_failures);
                        if finalized.root_reaped {
                            trace.stages.push(StageResult::ok("reap"));
                        } else {
                            trace.stages.push(StageResult::failed(
                                "reap",
                                "root process was not reaped before finalization ended".to_string(),
                            ));
                        }
                        if finalized.readers_joined {
                            trace.stages.push(StageResult::ok("drain"));
                        } else {
                            trace.stages.push(StageResult::failed(
                                "drain",
                                "pipe output EOF was not confirmed before finalization ended"
                                    .to_string(),
                            ));
                        }
                    } else {
                        let notes = pipe.abandon_without_reap();
                        worker_join_failures.extend(notes);
                        trace.stages.push(StageResult::failed(
                            "reap",
                            "Process Tree was not settled before finalization; root was not waited"
                                .to_string(),
                        ));
                        trace.stages.push(StageResult::failed(
                            "drain",
                            "output EOF was not confirmed before finalization".to_string(),
                        ));
                        io_failures.extend(pipe.io_failures());
                        dropped_output_bytes = pipe.dropped_bytes();
                    }
                }
                RunInner::Pty {
                    mut process,
                    session,
                } => {
                    if settled_before_finalize {
                        if let Err(error) = session.shutdown_until(final_deadline) {
                            terminal_failure = Some(error.to_string());
                            trace
                                .stages
                                .push(StageResult::failed("drain", error.to_string()));
                        } else {
                            trace.stages.push(StageResult::ok("drain"));
                        }
                        self.retired_pty = Some(session);
                        match process.reap_bounded(final_deadline) {
                            Ok(code) => {
                                exit_code = code;
                                trace.stages.push(StageResult::ok("reap"));
                            }
                            Err(error) => {
                                worker_join_failures.push(error.to_string());
                                trace
                                    .stages
                                    .push(StageResult::failed("reap", error.to_string()));
                                let _ = process.abandon_nonblocking();
                            }
                        }
                    } else {
                        let (owner_joined, writer_joined) = session.abandon_nonblocking();
                        if !owner_joined {
                            worker_join_failures.push(
                                "terminal owner was detached at the final deadline".to_string(),
                            );
                        }
                        if !writer_joined {
                            worker_join_failures
                                .push("PTY writer was detached at the final deadline".to_string());
                        }
                        self.retired_pty = Some(session);
                        worker_join_failures.extend(process.abandon_without_reap());
                        trace.stages.push(StageResult::failed(
                            "reap",
                            "Process Tree members remained; child wait was not attempted"
                                .to_string(),
                        ));
                        trace.stages.push(StageResult::failed(
                            "drain",
                            "terminal EOF was not confirmed before finalization".to_string(),
                        ));
                    }
                }
            }
        }

        if let Some(tree) = self.tree.as_ref() {
            trace.record_remaining(tree);
        }

        let (final_metrics, sampler_joined) = match self.metrics.take() {
            Some(sampler) => sampler.stop_and_join(),
            None => (None, true),
        };
        if !sampler_joined {
            worker_join_failures.push("metrics sampler did not join".to_string());
        }

        if !intentional_stop {
            disposition = if exit_code == Some(0) {
                RunExitDisposition::NaturalCompletion
            } else {
                RunExitDisposition::UnexpectedExit
            };
        }
        let cleanup_confirmed = tree_is_settled(self.tree.as_ref())
            && !trace.signals_stopped
            && trace.remaining_pids.is_empty()
            && io_failures.is_empty()
            && terminal_failure.is_none()
            && worker_join_failures.is_empty()
            && trace.stages.iter().all(|stage| stage.ok)
            && sampler_joined;
        let outcome = RunOutcome {
            run_id: self.run_id,
            disposition,
            intentional_stop,
            exit_code,
            stage_results: trace.stages,
            cleanup_confirmed,
            remaining_pids: trace.remaining_pids,
            io_failures,
            terminal_failure,
            final_metrics,
            worker_join_failures,
            dropped_output_bytes,
        };
        self.emit(RunEventKind::Exited { code: exit_code });
        self.emit(match cleanup_confirmed {
            true => RunEventKind::ShutdownComplete,
            false => RunEventKind::Failed("Run cleanup did not fully confirm".to_string()),
        });
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
        if self.signals_stopped.load(Ordering::Acquire) {
            let detail = self
                .signal_failure
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
                .unwrap_or_else(|| "signal escalation was stopped fail-closed".to_string());
            return Err(anyhow::anyhow!(detail));
        }
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
            Err(error) => {
                let detail = error.detail();
                self.signals_stopped.store(true, Ordering::Release);
                *self
                    .signal_failure
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(detail.clone());
                Err(anyhow::anyhow!(detail))
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn mark_signal_failure_for_test(&self, detail: &str) {
        self.signals_stopped.store(true, Ordering::Release);
        *self
            .signal_failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(detail.to_string());
    }

    /// Run the semantic signal ladder, confirm containment, reap the root,
    /// drain final output, finalize the terminal, and join all Run workers.
    /// Repeated calls observe the first structured outcome.
    pub fn shutdown(&mut self) -> Result<RunOutcome> {
        self.shutdown_with_ladder(self.ladder)
    }

    /// Shut down this Run with every ladder wait clamped to one remaining
    /// Project deadline. Repeated calls still observe the first outcome.
    pub fn shutdown_with_timeout(&mut self, remaining: Duration) -> Result<RunOutcome> {
        self.shutdown_with_ladder(self.ladder.clamped_to(remaining))
    }

    fn shutdown_with_ladder(
        &mut self,
        ladder: crate::runtime::ShutdownLadder,
    ) -> Result<RunOutcome> {
        if let Some(outcome) = &self.outcome {
            return Ok(outcome.clone());
        }
        // One shutdown request records intentional stop before cleanup.
        self.stopping.store(true, Ordering::Release);

        let trace = self.ladder_trace_with(ladder);
        self.finalize_after_ladder(trace, RunExitDisposition::IntentionalStop, true)
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
    /// - SIGKILL is applied to the whole owned Process Tree on a short,
    ///   bounded retry loop unless a signal fails closed;
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
        let mut diagnostics = Vec::new();
        let mut containment_confirmed = false;
        let signals_stopped = self.signals_stopped.load(Ordering::Acquire);
        if let Some(tree) = self.tree.as_ref() {
            let deadline = Instant::now() + Duration::from_millis(300);
            let mut next_signal = Instant::now();
            if signals_stopped {
                let detail = self
                    .signal_failure
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone()
                    .unwrap_or_else(|| "signal escalation was stopped fail-closed".to_string());
                diagnostics.push(format!(
                    "Drop skipped Process Tree signaling after a prior failure: {detail}"
                ));
            }
            while Instant::now() < deadline {
                if tree_is_settled(Some(tree)) {
                    break;
                }
                if !signals_stopped && Instant::now() >= next_signal {
                    match tree.signal(SemanticSignal::Kill) {
                        Ok(()) | Err(SignalError::NotFound) => {}
                        Err(error) => {
                            diagnostics.push(format!(
                                "Drop cleanup stopped signaling the owned Process Tree: {}",
                                error.detail()
                            ));
                            break;
                        }
                    }
                    next_signal = Instant::now() + Duration::from_millis(25);
                }
                std::thread::sleep(RUN_EXIT_POLL_INTERVAL);
            }
            containment_confirmed = tree_is_settled(Some(tree));
            if !containment_confirmed {
                diagnostics.push(
                    "Drop cleanup could not confirm that the owned Process Tree stopped"
                        .to_string(),
                );
                if let Ok(members) = tree.remaining_members() {
                    diagnostics.push(format!(
                        "Drop cleanup left unconfirmed Process Tree members: {members:?}"
                    ));
                }
            }
        }
        if let Some(inner) = self.inner.take() {
            match inner {
                RunInner::Pipe(mut pipe) => {
                    if containment_confirmed {
                        diagnostics.extend(pipe.abandon_nonblocking());
                    } else {
                        diagnostics.extend(pipe.abandon_without_reap());
                    }
                }
                RunInner::Pty {
                    mut process,
                    session,
                } => {
                    let (owner_joined, writer_joined) = session.abandon_nonblocking();
                    if !owner_joined {
                        diagnostics.push("Drop detached the terminal owner thread".to_string());
                    }
                    if !writer_joined {
                        diagnostics.push("Drop detached the PTY writer thread".to_string());
                    }
                    self.retired_pty = Some(session);
                    if containment_confirmed {
                        diagnostics.extend(process.abandon_nonblocking());
                    } else {
                        diagnostics.extend(process.abandon_without_reap());
                    }
                }
            }
        }
        if diagnostics.is_empty() {
            diagnostics.push(
                "OwnedRun was dropped before wait or shutdown; only bounded best-effort cleanup was performed"
                    .to_string(),
            );
        }
        for detail in diagnostics {
            self.emit(RunEventKind::Failed(format!("Run dropped: {detail}")));
        }
    }
}
