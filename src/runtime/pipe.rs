//! Pipe-mode Run I/O: one root process with continuously drained stdout and
//! stderr.
//!
//! This is a private adapter behind the Run owner. Pipe transport differs
//! from PTY transport in read, write, resize, and stream behavior, so it
//! lives beside the PTY adapter rather than being merged with it. Callers
//! never see these types; they use [`crate::runtime::OwnedRun`].

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{Context, Result};

use crate::runtime::{RunEvent, RunEventKind, RunId};

/// Identifies which stream produced an output chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// One high-volume output chunk. Output bytes travel only through this path,
/// never through the low-volume Run event sink.
///
/// Part of the public Run ownership seam; its fields are read by callers and
/// Milestone 0B tests outside this adapter.
#[derive(Debug)]
#[allow(dead_code)]
pub struct RunOutput {
    pub run_id: RunId,
    pub stream: OutputStream,
    pub data: Vec<u8>,
}

type EventSink = Sender<RunEvent>;
type OutputSink = Sender<RunOutput>;

const READ_BUFFER_BYTES: usize = 64 * 1_024;

/// The owned pipe-mode process and its reader tasks.
pub(crate) struct PipeRun {
    child: Option<Child>,
    readers: Vec<JoinHandle<()>>,
    io_failures: Arc<Mutex<Vec<String>>>,
}

impl PipeRun {
    /// Spawn one direct or shell command with piped stdout and stderr. Two
    /// reader tasks drain both streams from spawn to EOF. The root process
    /// becomes a process-group leader, matching PTY-mode containment.
    pub(crate) fn spawn(
        command: &crate::runtime::SpawnCommand,
        run_id: RunId,
        events: EventSink,
        output: OutputSink,
    ) -> Result<Self> {
        let mut cmd = Command::new(command.program());
        cmd.args(command.args())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("could not start {}", command.program().to_string_lossy()))?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let io_failures = Arc::new(Mutex::new(Vec::new()));
        let mut readers = Vec::new();

        if let Some(stdout) = stdout {
            readers.push(spawn_stream_reader(
                stdout,
                run_id,
                OutputStream::Stdout,
                &output,
                &events,
                Arc::clone(&io_failures),
            ));
        }
        if let Some(stderr) = stderr {
            readers.push(spawn_stream_reader(
                stderr,
                run_id,
                OutputStream::Stderr,
                &output,
                &events,
                Arc::clone(&io_failures),
            ));
        }

        Ok(Self {
            child: Some(child),
            readers,
            io_failures,
        })
    }

    /// Root PID of the owned pipe Run when it is not yet reaped.
    pub(crate) fn root_pid(&self) -> Option<u32> {
        self.child.as_ref().map(|child| child.id())
    }

    /// Reap the root process (blocking), then join both stream readers at
    /// EOF. Call this only after Process Tree escalation is finished: once
    /// the root is reaped its PID can be reused, so no further group signal
    /// may follow. Signal escalation stays with the Process Tree adapter in
    /// the Run owner; this method only reaps and reports I/O results.
    pub(crate) fn reap_and_join(&mut self) -> Result<Option<i32>> {
        let mut code = None;
        if let Some(mut child) = self.child.take() {
            code = child.wait()?.code();
        }
        self.join_readers();
        let failures = self.io_failures();
        if failures.is_empty() {
            Ok(code)
        } else {
            Err(anyhow::anyhow!("Run I/O failed: {}", failures.join("; ")))
        }
    }

    /// Non-blocking cleanup for failed shutdown paths. Never waits: it
    /// reaps the root only if it has already exited, detaches unfinished
    /// reader tasks, and returns diagnostics about what could not be
    /// confirmed.
    pub(crate) fn abandon_nonblocking(&mut self) -> Vec<String> {
        let mut notes = Vec::new();
        if let Some(mut child) = self.child.take() {
            match child.try_wait() {
                Ok(Some(_)) => {}
                Ok(None) => notes.push("root process left unreaped".to_string()),
                Err(error) => {
                    notes.push(format!("root process state unobservable: {error}"));
                }
            }
        }
        for handle in self.readers.drain(..) {
            if handle.is_finished() {
                let _ = handle.join();
            } else {
                // The reader stays alive but detached; a descendant holding
                // the pipes keeps it from EOF. This is reported, not hidden.
                notes.push("an output reader task did not reach EOF".to_string());
            }
        }
        let failures = self.io_failures();
        if !failures.is_empty() {
            notes.push(format!("I/O failures: {}", failures.join("; ")));
        }
        notes
    }

    pub(crate) fn io_failures(&self) -> Vec<String> {
        self.io_failures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(crate) fn join_readers(&mut self) {
        for handle in self.readers.drain(..) {
            let _ = handle.join();
        }
    }
}

fn spawn_stream_reader(
    mut stream: impl Read + Send + 'static,
    run_id: RunId,
    stream_kind: OutputStream,
    output: &OutputSink,
    events: &EventSink,
    io_failures: Arc<Mutex<Vec<String>>>,
) -> JoinHandle<()> {
    let output = output.clone();
    let events = events.clone();
    let name = match stream_kind {
        OutputStream::Stdout => "pipe-stdout",
        OutputStream::Stderr => "pipe-stderr",
    };
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            let mut buffer = vec![0u8; READ_BUFFER_BYTES];
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        let chunk = RunOutput {
                            run_id,
                            stream: stream_kind,
                            data: buffer[..count].to_vec(),
                        };
                        if output.send(chunk).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let detail = format!("{stream_kind:?} read failed: {error}");
                        io_failures
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push(detail.clone());
                        // I/O failure reaches callers only through the
                        // low-volume path; it carries no output bytes.
                        let _ = events.send(RunEvent {
                            run_id,
                            kind: RunEventKind::IoFailed(detail),
                        });
                        break;
                    }
                }
            }
        })
        .expect("pipe reader thread spawns with valid configuration")
}
