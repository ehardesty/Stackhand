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
                RunInner::Pty {
                    process: spawned.process,
                    session,
                }
            }
            RunMode::Pipe => RunInner::Pipe(PipeRun::spawn(
                &request.command,
                request.run_id,
                request.events.clone(),
                request.output.clone(),
            )?),
        };
        let run = OwnedRun {
            process_id: request.process_id,
            run_id: request.run_id,
            inner: Some(inner),
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

    /// The root operating-system PID when the platform reports one.
    pub fn root_pid(&self) -> Option<ProcessId> {
        match self.inner.as_ref()? {
            RunInner::Pty { process, .. } => process.process_id().map(ProcessId::new),
            RunInner::Pipe(_) => None,
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
                let status = pipe.wait()?;
                RunExit {
                    code: status.code(),
                }
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
                }) => process.shutdown().and_then(|()| session.shutdown()),
                Some(RunInner::Pipe(mut pipe)) => pipe.stop_and_join(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::{Duration, Instant};

    const WAIT: Duration = Duration::from_secs(5);

    fn start_marker_run() -> (OwnedRun, mpsc::Receiver<RunEvent>) {
        let (events, receiver) = mpsc::channel();
        let command = SpawnCommand::new("/bin/sh")
            .arg("-c")
            .arg("printf 'run-ready\\n'; IFS= read -r line; printf 'run-done\\n'");
        let run = RunRuntime
            .start(RunStartRequest {
                process_id: ProcessId::new(7),
                run_id: RunId::new(42),
                command,
                mode: RunMode::Pty {
                    initial_geometry: TerminalGeometry::DEFAULT,
                },
                events,
                output: mpsc::channel().0,
                on_output_wake: None,
            })
            .expect("fixture run started");
        (run, receiver)
    }

    fn wait_for_output(handle: &TerminalHandle<'_>, marker: &str) -> bool {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            if handle.snapshot().text().contains(marker) {
                return true;
            }
            thread::sleep(Duration::from_millis(2));
        }
        false
    }

    #[test]
    fn run_reports_identity_and_joins_on_explicit_cleanup() {
        let mut run = RunRuntime
            .start(RunStartRequest {
                process_id: ProcessId::new(3),
                run_id: RunId::new(9),
                command: SpawnCommand::new("/bin/sh").arg("-c").arg("sleep 30"),
                mode: RunMode::Pty {
                    initial_geometry: TerminalGeometry::DEFAULT,
                },
                events: mpsc::channel().0,
                output: mpsc::channel().0,
                on_output_wake: None,
            })
            .unwrap();

        assert_eq!(run.process_id(), ProcessId::new(3));
        assert_eq!(run.run_id(), RunId::new(9));
        assert!(run.root_pid().is_some());

        run.shutdown().expect("run cleaned up");
        // Repeated shutdown observes the first cleanup instead of repeating.
        run.shutdown().expect("repeated cleanup stayed successful");
    }

    #[test]
    fn natural_exit_and_explicit_cleanup_both_join_terminal_tasks() {
        let (mut run, receiver) = start_marker_run();
        assert_eq!(run.run_id(), RunId::new(42));

        let handle = run.terminal().expect("PTY fixture");
        assert!(
            wait_for_output(&handle, "run-ready"),
            "fixture output did not appear"
        );
        handle.send_raw(vec![0x04]);

        let spawned = receiver.recv_timeout(WAIT).expect("spawn event arrived");
        assert_eq!(spawned.run_id, RunId::new(42));
        assert!(matches!(
            spawned.kind,
            RunEventKind::Spawned { root_pid: Some(_) }
        ));

        assert!(
            wait_for_output(&handle, "run-done"),
            "root process did not reach natural exit"
        );
        // Cleanup after a natural root exit still finalizes the TerminalSession
        // and joins every terminal task.
        run.shutdown().expect("run joined after natural exit");

        let final_event = receiver.recv_timeout(WAIT).expect("final event arrived");
        assert_eq!(final_event.run_id, RunId::new(42));
        assert_eq!(final_event.kind, RunEventKind::ShutdownComplete);
    }

    #[test]
    fn every_low_volume_event_carries_the_requested_run_id() {
        let (_run, receiver) = start_marker_run();
        while let Ok(event) = receiver.try_recv() {
            assert_eq!(event.run_id, RunId::new(42));
        }
    }
}

#[cfg(test)]
mod pipe_tests {
    use super::*;
    use crate::runtime::pipe::OutputStream;
    use std::sync::mpsc::{Receiver, TryRecvError};
    use std::thread;

    const WAIT: Duration = Duration::from_secs(10);

    fn start_pipe(command: SpawnCommand) -> (OwnedRun, Receiver<RunEvent>, Receiver<RunOutput>) {
        let (events, event_receiver) = mpsc::channel();
        let (output, output_receiver) = mpsc::channel();
        let run = RunRuntime
            .start(RunStartRequest {
                process_id: ProcessId::new(11),
                run_id: RunId::new(77),
                command,
                mode: RunMode::Pipe,
                events,
                output,
                on_output_wake: None,
            })
            .expect("pipe run started");
        (run, event_receiver, output_receiver)
    }

    fn drain_output(receiver: &Receiver<RunOutput>) -> Vec<RunOutput> {
        let mut chunks = Vec::new();
        while let Ok(chunk) = receiver.try_recv() {
            chunks.push(chunk);
        }
        chunks
    }

    fn text(chunks: &[RunOutput], stream: OutputStream) -> String {
        String::from_utf8_lossy(
            &chunks
                .iter()
                .filter(|chunk| chunk.stream == stream)
                .flat_map(|chunk| chunk.data.clone())
                .collect::<Vec<u8>>(),
        )
        .into_owned()
    }

    #[test]
    fn pipe_mode_starts_a_direct_command_and_reaps_natural_exit() {
        let (mut run, events, output) =
            start_pipe(SpawnCommand::new("/bin/echo").arg("hello-direct"));

        let exit = run.wait().expect("direct command completed");
        assert_eq!(exit.code, Some(0));

        let chunks = drain_output(&output);
        assert_eq!(text(&chunks, OutputStream::Stdout), "hello-direct\n");
        assert!(chunks.iter().all(|chunk| chunk.run_id == RunId::new(77)));

        let spawned = events.recv_timeout(WAIT).expect("spawn event");
        assert_eq!(spawned.run_id, RunId::new(77));
        assert!(matches!(spawned.kind, RunEventKind::Spawned { .. }));
        let exited = events.recv_timeout(WAIT).expect("exit event");
        assert_eq!(exited.kind, RunEventKind::Exited { code: Some(0) });

        // The root process was reaped by wait(); cleanup stays successful.
        run.shutdown().expect("run joined");
    }

    #[test]
    fn pipe_mode_preserves_stream_identity_for_shell_commands() {
        let (mut run, _events, output) = start_pipe(
            SpawnCommand::new("/bin/sh")
                .arg("-c")
                .arg("printf to-out; printf to-err >&2"),
        );

        run.wait().expect("shell command completed");
        let chunks = drain_output(&output);
        assert!(text(&chunks, OutputStream::Stdout).contains("to-out"));
        assert!(text(&chunks, OutputStream::Stderr).contains("to-err"));
        assert!(!text(&chunks, OutputStream::Stdout).contains("to-err"));
        run.shutdown().expect("run joined");
    }

    #[test]
    fn pipe_mode_rejects_resize_without_harming_the_run() {
        let (mut run, _events, _output) =
            start_pipe(SpawnCommand::new("/bin/sh").arg("-c").arg("sleep 30"));

        let rejection = run.resize(TerminalGeometry::DEFAULT);
        assert_eq!(rejection, Err(ResizeUnsupported));

        // The Run is still healthy and controllable after the rejection.
        run.shutdown().expect("run stopped after rejected resize");
    }

    #[test]
    fn high_output_pipe_completes_and_keeps_bytes_out_of_the_event_sink() {
        let lines = 20_000u32;
        let (mut run, events, output) = start_pipe(
            SpawnCommand::new("/bin/sh")
                .arg("-c")
                .arg(format!(
                    "i=0; while [ \"$i\" -lt {lines} ]; do printf 'line-%06d\\n' \"$i\"; i=$((i+1)); done"
                )),
        );

        let exit = run.wait().expect("high-output run completed");
        assert_eq!(exit.code, Some(0));
        thread::sleep(Duration::from_millis(50));
        let chunks = drain_output(&output);
        let body = text(&chunks, OutputStream::Stdout);
        let expected_last = format!("line-{:06}", lines - 1);
        assert!(
            body.contains(&expected_last),
            "final line missing from drained output"
        );
        assert_eq!(
            body.lines().count(),
            usize::try_from(lines).unwrap(),
            "every output line must reach the high-volume path"
        );

        // Only lifecycle events may exist on the low-volume sink.
        loop {
            match events.try_recv() {
                Ok(event) => assert!(matches!(
                    event.kind,
                    RunEventKind::Spawned { .. } | RunEventKind::Exited { .. }
                )),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        run.shutdown().expect("run joined");
    }
}
