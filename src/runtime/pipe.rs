//! Pipe-mode Run I/O: one root process with continuously drained stdout and
//! stderr.
//!
//! This is a private adapter behind the Run owner. Pipe transport differs
//! from PTY transport in read, write, resize, and stream behavior, so it
//! lives beside the PTY adapter rather than being merged with it. Callers
//! never see these types; they use [`crate::runtime::OwnedRun`].

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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
/// The caller-provided high-volume output queue is bounded. A full queue is
/// reported and the reader continues draining the operating-system pipe so a
/// noisy Run cannot block its own lifecycle or another Run.
pub const OUTPUT_QUEUE_SLOTS: usize = 65_536;
/// Maximum process-output bytes retained by one caller-provided queue.
pub const OUTPUT_QUEUE_BYTES: usize = 16 * 1_024 * 1_024;
const IO_FAILURE_SLOTS: usize = 64;

const READ_BUFFER_BYTES: usize = 64 * 1_024;

/// A bounded high-volume output sender. Byte reservations are released by
/// [`RunOutputReceiver`] when a caller consumes a chunk, so many small writes
/// cannot exhaust the queue's message slots while large writes still respect
/// the same memory bound.
#[derive(Clone)]
pub struct RunOutputSender {
    sender: SyncSender<RunOutput>,
    pending_bytes: Arc<AtomicUsize>,
}

#[allow(dead_code)]
pub struct RunOutputReceiver {
    receiver: Receiver<RunOutput>,
    pending_bytes: Arc<AtomicUsize>,
}

pub enum OutputSendError {
    Full(RunOutput),
    Disconnected,
}

pub fn output_channel() -> (RunOutputSender, RunOutputReceiver) {
    let (sender, receiver) = std::sync::mpsc::sync_channel(OUTPUT_QUEUE_SLOTS);
    let pending_bytes = Arc::new(AtomicUsize::new(0));
    (
        RunOutputSender {
            sender,
            pending_bytes: Arc::clone(&pending_bytes),
        },
        RunOutputReceiver {
            receiver,
            pending_bytes,
        },
    )
}

impl RunOutputSender {
    pub(crate) fn try_send(&self, chunk: RunOutput) -> Result<(), OutputSendError> {
        let amount = chunk.data.len();
        let mut current = self.pending_bytes.load(Ordering::Acquire);
        loop {
            let Some(next) = current.checked_add(amount) else {
                return Err(OutputSendError::Full(chunk));
            };
            if next > OUTPUT_QUEUE_BYTES {
                return Err(OutputSendError::Full(chunk));
            }
            match self.pending_bytes.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
        match self.sender.try_send(chunk) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(chunk)) => {
                self.pending_bytes.fetch_sub(amount, Ordering::AcqRel);
                Err(OutputSendError::Full(chunk))
            }
            Err(TrySendError::Disconnected(_chunk)) => {
                self.pending_bytes.fetch_sub(amount, Ordering::AcqRel);
                Err(OutputSendError::Disconnected)
            }
        }
    }
}

#[allow(dead_code)]
impl RunOutputReceiver {
    pub fn try_recv(&self) -> Result<RunOutput, TryRecvError> {
        match self.receiver.try_recv() {
            Ok(chunk) => {
                self.pending_bytes
                    .fetch_sub(chunk.data.len(), Ordering::AcqRel);
                Ok(chunk)
            }
            Err(error) => Err(error),
        }
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<RunOutput, RecvTimeoutError> {
        match self.receiver.recv_timeout(timeout) {
            Ok(chunk) => {
                self.pending_bytes
                    .fetch_sub(chunk.data.len(), Ordering::AcqRel);
                Ok(chunk)
            }
            Err(error) => Err(error),
        }
    }
}

type OutputSink = RunOutputSender;

fn record_io_failure(failures: &Mutex<Vec<String>>, detail: String) -> bool {
    let mut failures = failures
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let first = failures.is_empty();
    if failures.len() < IO_FAILURE_SLOTS {
        failures.push(detail);
    } else if failures.len() == IO_FAILURE_SLOTS {
        failures.push("additional pipe I/O diagnostics were suppressed".to_string());
    }
    first
}

/// The owned pipe-mode process and its reader tasks.
pub(crate) struct PipeRun {
    child: Option<Child>,
    readers: Vec<JoinHandle<()>>,
    io_failures: Arc<Mutex<Vec<String>>>,
    /// Bytes dropped because the caller's bounded output queue was full.
    /// Dropped bytes are bounded backpressure, not hard I/O failures, so
    /// they never block a cleanup confirmation.
    dropped_bytes: Arc<AtomicUsize>,
}

pub(crate) struct PipeFinalize {
    pub exit_code: Option<i32>,
    pub root_reaped: bool,
    pub readers_joined: bool,
    pub io_failures: Vec<String>,
    pub worker_failures: Vec<String>,
    pub dropped_bytes: u64,
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
        if let Some(dir) = command.current_dir() {
            cmd.current_dir(dir);
        }
        for (key, value) in command.envs() {
            cmd.env(key, value);
        }
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
        let dropped_bytes = Arc::new(AtomicUsize::new(0));
        let mut readers = Vec::new();

        if let Some(stdout) = stdout {
            readers.push(spawn_stream_reader(
                stdout,
                run_id,
                OutputStream::Stdout,
                &output,
                &events,
                Arc::clone(&io_failures),
                Arc::clone(&dropped_bytes),
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
                Arc::clone(&dropped_bytes),
            ));
        }

        Ok(Self {
            child: Some(child),
            readers,
            io_failures,
            dropped_bytes,
        })
    }

    /// Root PID of the owned pipe Run when it is not yet reaped.
    pub(crate) fn root_pid(&self) -> Option<u32> {
        self.child.as_ref().map(|child| child.id())
    }

    /// Reap and join only while the caller's deadline remains. A reader can
    /// remain blocked when a descendant holds an inherited pipe open; after
    /// the deadline its JoinHandle is detached and the failure is retained.
    pub(crate) fn finalize_bounded(&mut self, deadline: Instant) -> PipeFinalize {
        let mut exit_code = None;
        let mut root_reaped = self.child.is_none();
        let mut worker_failures = Vec::new();

        while let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    exit_code = status.code();
                    let _ = self.child.take();
                    root_reaped = true;
                    break;
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Ok(None) => {
                    worker_failures
                        .push("root process was not reaped before the final deadline".to_string());
                    break;
                }
                Err(error) => {
                    worker_failures.push(format!("root process state was not observable: {error}"));
                    break;
                }
            }
        }

        let mut readers_joined = true;
        for handle in self.readers.drain(..) {
            while !handle.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(2));
            }
            if handle.is_finished() {
                if handle.join().is_err() {
                    worker_failures.push("pipe reader task panicked".to_string());
                }
            } else {
                readers_joined = false;
                worker_failures
                    .push("pipe reader did not reach EOF before the final deadline".to_string());
                // Dropping the handle deliberately detaches a reader whose
                // inherited pipe endpoint remains owned by a survivor.
            }
        }

        PipeFinalize {
            exit_code,
            root_reaped,
            readers_joined,
            io_failures: self.io_failures(),
            worker_failures,
            dropped_bytes: self.dropped_bytes(),
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

    /// Detach without polling or reaping the root. Use this when Process
    /// Tree containment or output EOF is still unconfirmed; later PID reuse
    /// must not follow an incomplete cleanup decision.
    pub(crate) fn abandon_without_reap(&mut self) -> Vec<String> {
        let mut notes = Vec::new();
        if self.child.take().is_some() {
            notes.push(
                "root process detached without reaping because containment was unconfirmed"
                    .to_string(),
            );
        }
        for handle in self.readers.drain(..) {
            if handle.is_finished() {
                let _ = handle.join();
            } else {
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

    /// Bytes the readers dropped because the bounded output queue was full.
    pub(crate) fn dropped_bytes(&self) -> u64 {
        self.dropped_bytes.load(Ordering::Relaxed) as u64
    }
}

fn spawn_stream_reader(
    mut stream: impl Read + Send + 'static,
    run_id: RunId,
    stream_kind: OutputStream,
    output: &OutputSink,
    events: &EventSink,
    io_failures: Arc<Mutex<Vec<String>>>,
    dropped_bytes: Arc<AtomicUsize>,
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
            // One first-drop report per stream: bounded backpressure is
            // expected under a noisy producer, so only the first drop goes
            // through the low-volume event path.
            let mut drop_reported = false;
            let mut emit = |data: Vec<u8>| -> bool {
                let chunk = RunOutput {
                    run_id,
                    stream: stream_kind,
                    data,
                };
                match output.try_send(chunk) {
                    Ok(()) => true,
                    Err(OutputSendError::Full(chunk)) => {
                        dropped_bytes.fetch_add(
                            chunk.data.len(),
                            Ordering::Relaxed,
                        );
                        if !drop_reported {
                            drop_reported = true;
                            let detail = format!(
                                "{stream_kind:?} output sink is full; bytes dropped under the bounded output queue"
                            );
                            let _ = events.send(RunEvent {
                                run_id,
                                kind: RunEventKind::IoFailed(detail),
                            });
                        }
                        true
                    }
                    Err(OutputSendError::Disconnected) => {
                        let detail = format!(
                            "{stream_kind:?} output sink was closed; remaining bytes were not retained"
                        );
                        if record_io_failure(&io_failures, detail.clone()) {
                            let _ = events.send(RunEvent {
                                run_id,
                                kind: RunEventKind::IoFailed(detail),
                            });
                        }
                        false
                    }
                }
            };
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        if !emit(buffer[..count].to_vec()) {
                            return;
                        }
                    }
                    Err(error) => {
                        let detail = format!("{stream_kind:?} read failed: {error}");
                        let should_emit = record_io_failure(&io_failures, detail.clone());
                        // I/O failure reaches callers only through the
                        // low-volume path; it carries no output bytes.
                        if should_emit {
                            let _ = events.send(RunEvent {
                                run_id,
                                kind: RunEventKind::IoFailed(detail),
                            });
                        }
                        break;
                    }
                }
            }
        })
        .expect("pipe reader thread spawns with valid configuration")
}
