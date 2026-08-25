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
    /// reader tasks drain both streams from spawn to EOF.
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

    /// Reap the root process, then join both stream readers at EOF.
    // Consumed by OwnedRun::wait on the natural-completion path.
    #[allow(dead_code)]
    pub(crate) fn wait(&mut self) -> Result<std::process::ExitStatus> {
        let Some(mut child) = self.child.take() else {
            return Err(anyhow::anyhow!("pipe Run already completed"));
        };
        let status = child.wait()?;
        self.join_readers();
        Ok(status)
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

impl PipeRun {
    /// Explicit stop for a still-running pipe Run: terminate, bounded wait,
    /// then kill. Reader tasks always join so no stream stays undrained.
    /// (The complete semantic shutdown ladder is owned by the Run owner from
    /// ticket #16; this keeps pipe-mode cleanup correct and bounded until
    /// then.)
    pub(crate) fn stop_and_join(&mut self) -> anyhow::Result<()> {
        const TERMINATE_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

        #[cfg(unix)]
        fn send_signal(child: &Child, signal: libc::c_int) {
            let pid = child.id() as libc::pid_t;
            // SAFETY: signaling one owned root process; the call itself
            // cannot fail unsafely.
            unsafe {
                libc::kill(pid, signal);
            }
        }

        if let Some(child) = self.child.as_mut() {
            #[cfg(unix)]
            if child.try_wait()?.is_none() {
                send_signal(child, libc::SIGTERM);
                let deadline = std::time::Instant::now() + TERMINATE_GRACE;
                while child.try_wait()?.is_none() && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }
        if let Some(mut child) = self.child.take() {
            #[cfg(unix)]
            if child.try_wait()?.is_none() {
                send_signal(&child, libc::SIGKILL);
            }
            child.wait()?;
        }
        self.join_readers();
        let failures = self.io_failures();
        if failures.is_empty() {
            Ok(())
        } else {
            Err(anyhow::anyhow!("Run I/O failed: {}", failures.join("; ")))
        }
    }
}
