//! One owning Run value per Process execution attempt.
//!
//! `RunRuntime::start` is the highest test and caller seam. It starts one Run
//! and returns an [`OwnedRun`] that owns the root process, the process I/O,
//! the output drain, and the optional [`TerminalSession`] lifetime. Callers
//! use semantic operations through [`OwnedRun`] and its non-owning
//! [`TerminalHandle`]. They never coordinate separate process and terminal
//! shutdown, and they never receive raw child, pipe, PTY, reader, writer, or
//! sampler handles.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::geometry::TerminalGeometry;
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

/// Why a PTY-only operation could not run in pipe mode. Non-fatal: the Run
/// stays healthy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // Part of the semantic Run interface consumed by callers.
pub struct ResizeUnsupported;

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
    /// Optional wake called when terminal output arrives. This is the
    /// redraw-notification path for interactive hosts; it never carries
    /// output bytes.
    pub on_output_wake: Option<Box<dyn Fn() + Send + 'static>>,
}

/// A low-volume Run lifecycle event. Every event carries the `RunId`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunEvent {
    pub run_id: RunId,
    pub kind: RunEventKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
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
}

impl RunEvent {
    fn new(run_id: RunId, kind: RunEventKind) -> Self {
        Self { run_id, kind }
    }
}

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
        let run = OwnedRun {
            process_id: request.process_id,
            run_id: request.run_id,
            inner: Some(inner),
            tree,
            events: request.events,
            cleanup_error: None,
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
    events: mpsc::Sender<RunEvent>,
    cleanup_error: Option<anyhow::Error>,
}

enum RunInner {
    Pty {
        process: PtyProcess,
        session: TerminalSession,
    },
    Pipe(PipeRun),
}

const RUN_EXIT_POLL_INTERVAL: Duration = Duration::from_millis(5);
/// Bounded wait between terminate and kill during explicit cleanup. The full
/// configured shutdown ladder is ticket #16.
const TERMINATE_GRACE: Duration = Duration::from_secs(1);
/// Bounded wait after kill before containment results are reported.
const KILL_GRACE: Duration = Duration::from_millis(500);
const RUN_EXIT_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

/// The completion record for a naturally completed Run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunExit {
    /// The root process exit code when the platform reports one.
    pub code: Option<i32>,
}

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
        match self.inner.as_ref()? {
            RunInner::Pty { session, .. } => Some(TerminalHandle { session }),
            RunInner::Pipe(_) => None,
        }
    }

    /// Request a PTY geometry change. Pipe mode has no terminal to resize;
    /// the request is rejected without affecting Run health.
    pub fn resize(&self, geometry: TerminalGeometry) -> Result<(), ResizeUnsupported> {
        match self.inner.as_ref() {
            Some(RunInner::Pty { session, .. }) => {
                session.resize(geometry);
                Ok(())
            }
            _ => Err(ResizeUnsupported),
        }
    }

    /// Wait for natural completion: reap the root process and drain all
    /// output to EOF before returning. The exit event carries the `RunId`.
    pub fn wait(&mut self) -> Result<RunExit> {
        let Some(inner) = self.inner.as_mut() else {
            return Err(anyhow::anyhow!("Run {} already completed", self.run_id.0));
        };
        let exit = match inner {
            RunInner::Pipe(pipe) => {
                let code = pipe.reap_and_join()?;
                RunExit { code }
            }
            RunInner::Pty { process, session } => {
                let code = wait_for_pty_exit(process)?;
                // Finalize terminal tasks after root exit.
                session.shutdown()?;
                RunExit { code }
            }
        };
        self.emit(RunEventKind::Exited { code: exit.code });
        Ok(exit)
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

    /// Complete Run cleanup: stop the root process, then finalize the
    /// TerminalSession (PTY mode) or join both stream readers (pipe mode).
    /// Natural root exit followed by cleanup uses the same path. Repeated
    /// calls observe the first cleanup instead of repeating it.
    pub fn shutdown(&mut self) -> Result<()> {
        if !self.is_cleaned_up() {
            let inner = self.inner.take();
            let result = match inner {
                Some(RunInner::Pty {
                    mut process,
                    session,
                }) => {
                    let mut failure_notes = Vec::new();
                    if let Some(tree) = self.tree.as_ref() {
                        failure_notes = self.escalate_group_shutdown(tree);
                    }
                    if failure_notes.is_empty() {
                        // Escalation is complete, so reaping is safe now:
                        // no further Process Group signal follows a reap.
                        process.shutdown().and_then(|()| session.shutdown())
                    } else {
                        if let Err(error) = session.shutdown() {
                            failure_notes.push(error.to_string());
                        }
                        failure_notes.extend(process.abandon_nonblocking());
                        Err(anyhow::anyhow!(
                            "Run cleanup failed: {}",
                            failure_notes.join("; ")
                        ))
                    }
                }
                Some(RunInner::Pipe(mut pipe)) => {
                    let mut failure_notes = Vec::new();
                    if let Some(tree) = self.tree.as_ref() {
                        failure_notes = self.escalate_group_shutdown(tree);
                    }
                    if failure_notes.is_empty() {
                        pipe.reap_and_join().map(|_code| ())
                    } else {
                        failure_notes.extend(pipe.abandon_nonblocking());
                        Err(anyhow::anyhow!(
                            "Run cleanup failed: {}",
                            failure_notes.join("; ")
                        ))
                    }
                }
                None => Ok(()),
            };
            self.cleanup_error = result.err();
            self.emit(match &self.cleanup_error {
                None => RunEventKind::ShutdownComplete,
                Some(error) => RunEventKind::Failed(error.to_string()),
            });
        }
        match &self.cleanup_error {
            None => Ok(()),
            Some(error) => Err(anyhow::anyhow!(
                "Run {} did not clean up completely: {error}",
                self.run_id.0
            )),
        }
    }

    fn is_cleaned_up(&self) -> bool {
        self.inner.is_none()
    }

    /// Escalate terminate → kill against the owned Process Tree while its
    /// identity is intact. The unreaped root keeps the group in existence
    /// and its PID reserved, so both group signals and direct root signals
    /// are safe during this function. No signal of any kind follows a reap.
    ///
    /// Target selection:
    /// - Live members other than the root receive group signals.
    /// - A root that has not exited yet receives direct signals (safe while
    ///   unreaped).
    /// - An exited root with no other members needs no signal at all: this
    ///   avoids the Darwin EPERM quirk for zombie-only setsid groups.
    ///
    /// "Settled" means the root has exited AND no other member remains.
    /// An empty member list alone is not settled: the root may still be
    /// starting its children.
    ///
    /// Fail-closed rules: an Ownership or Failed signal error stops
    /// escalation immediately; members that remain after kill are reported,
    /// never hidden; and the caller must not reap until this returns.
    fn escalate_group_shutdown(&self, tree: &UnixProcessTree) -> Vec<String> {
        let mut notes = Vec::new();
        for stage in [SemanticSignal::Terminate, SemanticSignal::Kill] {
            if Self::tree_settled(tree) {
                return notes;
            }
            let target_group = !Self::tree_is_empty(tree);
            let result = if target_group {
                tree.signal(stage)
            } else {
                tree.signal_root_unreaped(stage)
            };
            match result {
                Ok(()) | Err(SignalError::NotFound) => {}
                Err(error) => {
                    notes.push(format!("escalation stopped: {}", error.detail()));
                    return notes;
                }
            }
            let budget = match stage {
                SemanticSignal::Terminate => TERMINATE_GRACE,
                _ => KILL_GRACE,
            };
            let deadline = Instant::now() + budget;
            while Instant::now() < deadline && !Self::tree_settled(tree) {
                std::thread::sleep(RUN_EXIT_POLL_INTERVAL);
            }
        }
        if let Ok(members) = tree.remaining_members_excluding_root()
            && !members.is_empty()
        {
            notes.push(format!(
                "Process Tree members remained after kill: {:?}",
                members
            ));
        }
        notes
    }

    fn tree_is_empty(tree: &UnixProcessTree) -> bool {
        tree.remaining_members_excluding_root()
            .map(|members| members.is_empty())
            .unwrap_or(false)
    }

    /// Whether nothing is left except possibly the root itself, which may be
    /// alive or an unreaped zombie. Group signals are skipped in this state.
    fn only_unreaped_root_remains(tree: &UnixProcessTree) -> bool {
        Self::tree_is_empty(tree) || UnixProcessTree::root_exit_pending(tree.root_pid())
    }

    /// Whether the Run's Process Tree work is done: the root has exited and
    /// no other member remains. Both halves are required — an empty member
    /// list alone cannot distinguish "all clear" from "children not yet
    /// spawned by a live root".
    fn tree_settled(tree: &UnixProcessTree) -> bool {
        UnixProcessTree::root_exit_pending(tree.root_pid()) && Self::tree_is_empty(tree)
    }

    fn emit(&self, kind: RunEventKind) {
        let _ = self.events.send(RunEvent::new(self.run_id, kind));
    }
}

/// Non-owning terminal actions for one Run.
///
/// The handle cannot shut down, replace, or detach the TerminalSession from
/// its Run. Terminal semantics stay inside `TerminalSession`; Process Tree
/// containment and shutdown policy stay inside the Run owner.
pub struct TerminalHandle<'a> {
    session: &'a TerminalSession,
}

/// Internal constructor for crate tests that already own a session.
#[cfg(test)]
pub(crate) fn handle_for_test<'a>(session: &'a TerminalSession) -> TerminalHandle<'a> {
    TerminalHandle { session }
}

impl TerminalHandle<'_> {
    pub fn send_key(&self, event: crossterm::event::KeyEvent) {
        self.session.send_key(event);
    }

    pub fn send_focus(&self, gained: bool) {
        self.session.send_focus(gained);
    }

    pub fn send_mouse(&self, event: TerminalMouseEvent) {
        self.session.send_mouse(event);
    }

    pub fn send_raw(&self, data: Vec<u8>) {
        self.session.send_raw(data);
    }

    /// See [`TerminalSession::send_paste`].
    pub fn send_paste(&self, data: &str) -> Result<PasteRequest, PasteRejection> {
        self.session.send_paste(data)
    }

    pub fn resize(&self, geometry: TerminalGeometry) {
        self.session.resize(geometry);
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
