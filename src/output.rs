//! Bounded Project Logs history.
//!
//! One deep module owns normalized Logs output for every Process. Callers append
//! observed bytes and take immutable snapshots. The implementation hides lossy
//! UTF-8 conversion, terminal-control filtering, long-line splitting, per-Process
//! limits, the hard Project limit, oldest-first eviction, timestamps, stream
//! labels, and literal search.
//! Output bytes do not enter the Supervisor control queue.

use std::sync::atomic::{AtomicUsize, Ordering};

mod search;
mod segments;
mod snapshot;
use crate::output_settings::OutputSettings;
pub(crate) use search::SearchCache;
pub use segments::RetainedChunks;
pub use snapshot::{LogMatch, LogSearch, RetainedOutput};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::runtime::{OutputStream, ProcessId};
use crate::terminal_control::ControlParser;

/// Maximum retained Logs bytes for one Process.
pub const RETAINED_BYTES: usize = 32 * 1024 * 1024;
/// Maximum retained Logs units for one Process.
pub const RETAINED_CHUNKS: usize = RETAINED_BYTES / std::mem::size_of::<RetainedChunk>();
/// Maximum bytes accepted for one displayed part of a logical line.
pub const LOGICAL_LINE_BYTES: usize = 16 * 1024;
/// Search never builds an unbounded result list.
pub const SEARCH_MATCH_LIMIT: usize = 1_000;
const LONG_LINE_MARKER: &str = " … [line continued at 16 KiB]";

/// One retained Logs unit. Data retains its Run, observation order, observation
/// time, and stream identity. PTY output uses [`OutputStream::Combined`].
#[derive(Clone, Debug, PartialEq)]
pub enum RetainedChunk {
    Marker {
        run_id: u64,
        label: String,
        sequence: u64,
        observed_at_ms: u64,
    },
    Data {
        run_id: u64,
        stream: OutputStream,
        text: String,
        sequence: u64,
        observed_at_ms: u64,
        continued: bool,
    },
}

/// A handle to one Process inside the Project Logs owner.
pub struct ProcessOutput {
    process: usize,
    project: Arc<Mutex<ProjectHistory>>,
}

struct ProcessHistory {
    chunks: RetainedChunks,
    pending: [Option<PendingLine>; 3],
    parsers: [Option<(u64, ControlParser)>; 3],
    bytes: usize,
    dropped_chunks: u64,
    dropped_bytes: u64,
    latest_run: Option<u64>,
    generation: u64,
}

struct PendingLine {
    run_id: u64,
    stream: OutputStream,
    bytes: Vec<u8>,
    sequence: u64,
    observed_at_ms: u64,
}

struct ProjectHistory {
    processes: Vec<ProcessHistory>,
    bytes: usize,
    next_sequence: u64,
    limits: OutputSettings,
    allocation: Arc<AtomicUsize>,
}

impl ProcessOutput {
    pub(crate) fn append(&self, run_id: u64, stream: OutputStream, data: &[u8]) {
        self.append_at(run_id, stream, observation_millis(), data);
    }

    pub(crate) fn append_at(
        &self,
        run_id: u64,
        stream: OutputStream,
        observed_at_ms: u64,
        data: &[u8],
    ) {
        let mut project = lock(&self.project);
        if project.processes[self.process]
            .latest_run
            .is_some_and(|latest| run_id < latest)
        {
            return;
        }
        let stream_index = stream_index(stream);
        if project.processes[self.process].pending[stream_index]
            .as_ref()
            .is_some_and(|pending| pending.run_id != run_id)
        {
            project.flush_pending(self.process, stream_index, false);
        }
        let parser = &mut project.processes[self.process].parsers[stream_index];
        if parser.as_ref().is_none_or(|(id, _)| *id != run_id) {
            *parser = Some((run_id, ControlParser::default()));
        }
        for &byte in data {
            let Some(byte) = project.processes[self.process].parsers[stream_index]
                .as_mut()
                .expect("the stream parser exists")
                .1
                .consume(byte)
            else {
                continue;
            };
            if project.processes[self.process].pending[stream_index].is_none() {
                let sequence = project.next_sequence;
                project.next_sequence = project.next_sequence.wrapping_add(1);
                project.processes[self.process].pending[stream_index] = Some(PendingLine {
                    run_id,
                    stream,
                    bytes: Vec::with_capacity(LOGICAL_LINE_BYTES.min(4096)),
                    sequence,
                    observed_at_ms,
                });
            }
            project.processes[self.process].pending[stream_index]
                .as_mut()
                .expect("the pending line exists")
                .bytes
                .push(byte);
            project.processes[self.process].bytes += 1;
            project.bytes += 1;
            let newline = byte == b'\n';
            let full = project.processes[self.process].pending[stream_index]
                .as_ref()
                .is_some_and(|pending| pending.bytes.len() == LOGICAL_LINE_BYTES);
            if newline || full {
                project.flush_pending(self.process, stream_index, full && !newline);
            }
        }
        project.processes[self.process].generation =
            project.processes[self.process].generation.wrapping_add(1);
        project.enforce(self.process);
    }

    pub(crate) fn mark_run(&self, run_id: u64) {
        self.mark_run_at(run_id, observation_millis());
    }

    pub(crate) fn mark_run_at(&self, run_id: u64, observed_at_ms: u64) {
        let mut project = lock(&self.project);
        if project.processes[self.process]
            .latest_run
            .is_some_and(|latest| run_id <= latest)
        {
            return;
        }
        for stream_index in 0..3 {
            project.flush_pending(self.process, stream_index, false);
        }
        project.processes[self.process].parsers = std::array::from_fn(|_| None);
        let sequence = project.next_sequence;
        project.next_sequence = project.next_sequence.wrapping_add(1);
        project.processes[self.process].latest_run = Some(run_id);
        project.push(
            self.process,
            RetainedChunk::Marker {
                run_id,
                label: format!("── Run {run_id} started ──"),
                sequence,
                observed_at_ms,
            },
        );
        project.processes[self.process].generation =
            project.processes[self.process].generation.wrapping_add(1);
        project.enforce(self.process);
    }

    pub fn snapshot(&self) -> RetainedOutput {
        self.snapshot_if_changed(None)
            .expect("an unconditional Logs snapshot is always returned")
    }

    /// Return a new immutable snapshot only when this Process changed. Idle
    /// view operations can reuse their prior snapshot instead of cloning the
    /// full retained history for every input batch.
    pub fn snapshot_if_changed(&self, known_generation: Option<u64>) -> Option<RetainedOutput> {
        let mut project = lock(&self.project);
        let process = &project.processes[self.process];
        if known_generation == Some(process.generation) {
            return None;
        }
        let snapshot_chunks = |history: &ProcessHistory| {
            let mut chunks = history.chunks.clone();
            for line in history.pending.iter().flatten() {
                chunks.push(pending_chunk(line));
            }
            chunks
        };
        let mut chunks = snapshot_chunks(process);
        // Lossy UTF-8 replacement can expand pending raw bytes. Enforce the
        // normalized snapshot's cap too, recording any expiry in its owner.
        while chunks.retained_bytes() > project.limits.per_process_history_bytes
            && project.evict_from(self.process)
        {
            chunks = snapshot_chunks(&project.processes[self.process]);
        }
        // Snapshot tail copies are charged too. Free owner-held history before
        // publishing; readers of retired segments remain charged until drop.
        // A single snapshot never owns more than one Process's configured cap.

        let latest_run = project.processes[self.process].latest_run;
        project.enforce(self.process);
        if project.allocation.load(Ordering::Relaxed) > project.limits.global_history_bytes {
            drop(chunks);
            project.enforce(self.process);
            chunks = project.processes[self.process].chunks.clone();
        }
        let process = &project.processes[self.process];
        let snapshot = RetainedOutput {
            chunks,
            latest_run,
            truncated: process.dropped_chunks > 0,
            dropped_chunks: process.dropped_chunks,
            dropped_bytes: process.dropped_bytes,
            generation: process.generation,
        };
        Some(snapshot)
    }
}

impl ProjectHistory {
    fn flush_pending(&mut self, process: usize, stream: usize, continued: bool) {
        let Some(pending) = self.processes[process].pending[stream].take() else {
            return;
        };
        let raw_bytes = pending.bytes.len();
        self.processes[process].bytes = self.processes[process].bytes.saturating_sub(raw_bytes);
        self.bytes = self.bytes.saturating_sub(raw_bytes);
        let mut text = String::from_utf8_lossy(&pending.bytes).into_owned();
        if continued {
            text.push_str(LONG_LINE_MARKER);
            text.push('\n');
        }
        self.push(
            process,
            RetainedChunk::Data {
                run_id: pending.run_id,
                stream: pending.stream,
                text,
                sequence: pending.sequence,
                observed_at_ms: pending.observed_at_ms,
                continued,
            },
        );
    }

    fn push(&mut self, process: usize, chunk: RetainedChunk) {
        let bytes = retained_size(&chunk);
        self.processes[process].chunks.push(chunk);
        self.processes[process].bytes += bytes;
        self.bytes += bytes;
        self.enforce(process);
    }

    fn enforce(&mut self, changed: usize) {
        while self.processes[changed].bytes > self.limits.per_process_history_bytes {
            if !self.evict_from(changed) {
                break;
            }
        }
        while self.bytes > self.limits.global_history_bytes
            || self.allocation.load(Ordering::Relaxed) > self.limits.global_history_bytes
        {
            // Protect quieter Processes by taking history from the largest
            // current consumer. Within that Process, remove oldest first.
            let candidate = self
                .processes
                .iter()
                .enumerate()
                .filter(|(_, history)| history.bytes > 0)
                .max_by_key(|(_, history)| {
                    (history.bytes, std::cmp::Reverse(oldest_sequence(history)))
                })
                .map(|(index, _)| index);
            let Some(index) = candidate else {
                break;
            };
            if !self.evict_from(index) {
                break;
            }
        }
    }

    fn evict_from(&mut self, process: usize) -> bool {
        let history = &mut self.processes[process];
        let oldest = oldest_sequence(history);
        let front_is_oldest = history
            .chunks
            .first()
            .is_some_and(|chunk| Some(sequence(chunk)) == oldest);
        let (bytes, count) = if front_is_oldest {
            history.chunks.pop_oldest().expect("oldest segment exists")
        } else {
            let Some(stream) = history.pending.iter().position(|pending| {
                pending
                    .as_ref()
                    .is_some_and(|line| Some(line.sequence) == oldest)
            }) else {
                return false;
            };
            (
                history.pending[stream]
                    .take()
                    .expect("oldest pending line exists")
                    .bytes
                    .len(),
                1,
            )
        };
        history.bytes = history.bytes.saturating_sub(bytes);
        history.dropped_chunks += count as u64;
        history.dropped_bytes += bytes as u64;
        history.generation = history.generation.wrapping_add(1);
        self.bytes = self.bytes.saturating_sub(bytes);
        true
    }
}

fn pending_chunk(pending: &PendingLine) -> RetainedChunk {
    RetainedChunk::Data {
        run_id: pending.run_id,
        stream: pending.stream,
        text: String::from_utf8_lossy(&pending.bytes).into_owned(),
        sequence: pending.sequence,
        observed_at_ms: pending.observed_at_ms,
        continued: false,
    }
}

fn stream_index(stream: OutputStream) -> usize {
    match stream {
        OutputStream::Stdout => 0,
        OutputStream::Stderr => 1,
        OutputStream::Combined => 2,
    }
}

fn oldest_sequence(process: &ProcessHistory) -> Option<u64> {
    process
        .chunks
        .first()
        .map(sequence)
        .into_iter()
        .chain(process.pending.iter().flatten().map(|line| line.sequence))
        .min()
}

fn observation_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn sequence(chunk: &RetainedChunk) -> u64 {
    match chunk {
        RetainedChunk::Marker { sequence, .. } | RetainedChunk::Data { sequence, .. } => *sequence,
    }
}

fn retained_size(chunk: &RetainedChunk) -> usize {
    match chunk {
        RetainedChunk::Marker { label, .. } => label.len() + std::mem::size_of::<RetainedChunk>(),
        RetainedChunk::Data { text, .. } => text.len() + std::mem::size_of::<RetainedChunk>(),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Registry of one Logs handle per Process and one hard Project budget.
#[derive(Clone)]
pub struct OutputViews {
    handles: Arc<Vec<Arc<ProcessOutput>>>,
}

impl OutputViews {
    pub fn new(process_count: usize) -> Self {
        Self::with_settings(process_count, OutputSettings::default())
    }

    pub fn with_settings(process_count: usize, limits: OutputSettings) -> Self {
        let allocation = Arc::new(AtomicUsize::new(0));
        let project = Arc::new(Mutex::new(ProjectHistory {
            processes: (0..process_count)
                .map(|_| ProcessHistory {
                    chunks: RetainedChunks::new(Arc::clone(&allocation)),
                    pending: std::array::from_fn(|_| None),
                    parsers: std::array::from_fn(|_| None),
                    bytes: 0,
                    dropped_chunks: 0,
                    dropped_bytes: 0,
                    latest_run: None,
                    generation: 0,
                })
                .collect(),
            bytes: 0,
            next_sequence: 0,
            limits,
            allocation,
        }));
        Self {
            handles: Arc::new(
                (0..process_count)
                    .map(|process| {
                        Arc::new(ProcessOutput {
                            process,
                            project: Arc::clone(&project),
                        })
                    })
                    .collect(),
            ),
        }
    }

    pub fn for_process_id(&self, process_id: ProcessId) -> Option<Arc<ProcessOutput>> {
        self.handles.get(process_id.get() as usize).cloned()
    }

    pub fn for_process(&self, process_id: u32) -> Option<Arc<ProcessOutput>> {
        self.for_process_id(ProcessId::new(process_id))
    }
}

#[cfg(test)]
mod tests;
