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

use anyhow::Result;

use crate::geometry::TerminalGeometry;
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
pub enum RunMode {
    /// Interactive transport with terminal semantics.
    Pty { initial_geometry: TerminalGeometry },
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
        let RunMode::Pty { initial_geometry } = request.mode;
        let spawned = PtyProcess::spawn(request.command, initial_geometry)?;
        let wake = request.on_output_wake.unwrap_or_else(|| Box::new(|| {}));
        let session = TerminalSession::spawn(spawned.io, initial_geometry, wake)?;
        let run = OwnedRun {
            process_id: request.process_id,
            run_id: request.run_id,
            process: Some(spawned.process),
            session: Some(session),
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
    process: Option<PtyProcess>,
    session: Option<TerminalSession>,
    events: mpsc::Sender<RunEvent>,
    cleanup_error: Option<anyhow::Error>,
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
        self.process
            .as_ref()
            .and_then(|process| process.process_id())
            .map(ProcessId::new)
    }

    /// A non-owning terminal view of this Run. The handle exposes the
    /// existing terminal actions but has no terminal shutdown action and
    /// cannot detach the TerminalSession from its Run.
    pub fn terminal(&self) -> TerminalHandle<'_> {
        let session = self.session.as_ref().expect("run already cleaned up");
        TerminalHandle { session }
    }

    /// Complete Run cleanup: stop the root process, then finalize the
    /// TerminalSession and join all terminal tasks. Natural root exit and an
    /// explicit stop both end here. Repeated calls observe the first
    /// cleanup instead of repeating it.
    pub fn shutdown(&mut self) -> Result<()> {
        if !self.is_cleaned_up() {
            let process_result = match self.process.as_mut() {
                Some(process) => process.shutdown(),
                None => Ok(()),
            };
            let session = self.session.take();
            let session_result = session.map_or(Ok(()), |session| session.shutdown());
            self.process = None;
            self.cleanup_error = process_result.err().or(session_result.err());
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
        self.process.is_none() && self.session.is_none()
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

        let handle = run.terminal();
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
