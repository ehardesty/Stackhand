//! One bounded per-Process output module.
//!
//! Every Process has exactly one module for the whole session; it spans
//! Runs but owns no Process Tree shutdown. Pipe output arrives as owned
//! chunks that keep their stream identity; one marker chunk separates the
//! output of each Run attempt. PTY output stays inside the fresh terminal
//! session that each Run owns, so this module retains no PTY bytes, only
//! markers and truncation state. Output bytes travel through this module
//! only — never through the Supervisor control queue.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::runtime::OutputStream;

/// The maximum retained output bytes for one Process.
pub const RETAINED_BYTES: usize = 1024 * 1024;
/// The maximum retained output chunks for one Process.
pub const RETAINED_CHUNKS: usize = 4096;

/// One retained output unit.
#[derive(Clone, Debug, PartialEq)]
pub enum RetainedChunk {
    /// The synthetic divider recorded when one Run attempt starts.
    Marker { run_id: u64, label: String },
    /// Pipe output with its stream identity preserved.
    Data {
        run_id: u64,
        stream: OutputStream,
        text: String,
    },
}

/// One owned view of a Process's retained output. Views and renderers may
/// hold these freely; they cannot mutate the module.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RetainedOutput {
    pub chunks: Vec<RetainedChunk>,
    /// The most recent marked Run, when any.
    pub latest_run: Option<u64>,
    /// Whether older output was dropped at the retention bounds.
    pub truncated: bool,
    /// How many chunks were dropped so far.
    pub dropped_chunks: u64,
    /// How many retained bytes were dropped so far.
    pub dropped_bytes: u64,
    /// Bumped on every retained mutation; a cheap change signal for views.
    pub generation: u64,
}

/// The retained output of one Process across its Runs.
pub struct ProcessOutput {
    inner: Mutex<Inner>,
}

struct Inner {
    chunks: VecDeque<RetainedChunk>,
    bytes: usize,
    dropped_chunks: u64,
    dropped_bytes: u64,
    /// The newest marked Run. A drain tail from an older Run that lands
    /// after this marker would corrupt marker order, so it is dropped.
    latest_run: Option<u64>,
    /// Bumped on every append or marker.
    generation: u64,
}

impl ProcessOutput {
    fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                chunks: VecDeque::new(),
                bytes: 0,
                dropped_chunks: 0,
                dropped_bytes: 0,
                latest_run: None,
                generation: 0,
            }),
        }
    }

    /// Retain one pipe-mode chunk. The chunk's Run identity is checked
    /// against the newest marked Run; a stale tail from an ended attempt
    /// never reorders behind a newer marker. Crates outside the data path
    /// must read through [`Self::snapshot`] only.
    pub(crate) fn append(&self, run_id: u64, stream: OutputStream, data: Vec<u8>) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if inner.latest_run.is_some_and(|latest| run_id < latest) {
            return;
        }
        let text = String::from_utf8_lossy(&data).into_owned();
        let size = text.len();
        inner.chunks.push_back(RetainedChunk::Data {
            run_id,
            stream,
            text,
        });
        inner.bytes += size;
        inner.enforce_bounds();
        inner.generation += 1;
    }

    /// Record that one Run attempt started. The marker is visible in
    /// snapshots and separates the attempts' output.
    pub(crate) fn mark_run(&self, run_id: u64) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if inner.latest_run.is_some_and(|latest| run_id <= latest) {
            return;
        }
        let label = format!("── Run {run_id} started ──");
        let size = label.len();
        inner.chunks.push_back(RetainedChunk::Marker {
            run_id,
            label: label.clone(),
        });
        inner.bytes += size;
        inner.latest_run = Some(run_id);
        inner.enforce_bounds();
        inner.generation += 1;
    }

    /// One owned snapshot of the retained output.
    pub fn snapshot(&self) -> RetainedOutput {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        RetainedOutput {
            chunks: inner.chunks.iter().cloned().collect(),
            latest_run: inner.latest_run,
            truncated: inner.dropped_chunks > 0,
            dropped_chunks: inner.dropped_chunks,
            dropped_bytes: inner.dropped_bytes,
            generation: inner.generation,
        }
    }
}

impl Inner {
    /// Drop the oldest retained units until both bounds hold. Dropped
    /// amounts are counted so truncation stays observable.
    fn enforce_bounds(&mut self) {
        while self.bytes > RETAINED_BYTES || self.chunks.len() > RETAINED_CHUNKS {
            let Some(chunk) = self.chunks.pop_front() else {
                break;
            };
            let (chunk_bytes, chunk_count) = retained_size(&chunk);
            self.bytes = self.bytes.saturating_sub(chunk_bytes);
            self.dropped_chunks += chunk_count as u64;
            self.dropped_bytes += chunk_bytes as u64;
        }
    }
}

/// The retention cost of one unit: markers count their label, data counts
/// its text. Chunk counts are always one.
fn retained_size(chunk: &RetainedChunk) -> (usize, usize) {
    match chunk {
        RetainedChunk::Marker { label, .. } => (label.len(), 1),
        RetainedChunk::Data { text, .. } => (text.len(), 1),
    }
}

/// The registry of one module per Process. Built with the Supervisor so
/// every Process has its retained output in exactly one place.
#[derive(Clone)]
pub struct OutputViews {
    inner: Arc<Vec<Arc<ProcessOutput>>>,
}

impl OutputViews {
    pub fn new(process_count: usize) -> Self {
        Self {
            inner: Arc::new(
                (0..process_count)
                    .map(|_| Arc::new(ProcessOutput::new()))
                    .collect(),
            ),
        }
    }

    /// The module for one Process, by its stable session position.
    pub fn for_process(&self, process_id: u32) -> Option<Arc<ProcessOutput>> {
        self.inner.get(process_id as usize).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_keeps_stream_identity_and_run_marker_order() {
        let output = ProcessOutput::new();
        output.mark_run(1);
        output.append(1, OutputStream::Stdout, b"first".to_vec());
        output.append(1, OutputStream::Stderr, b"second".to_vec());
        output.mark_run(2);
        output.append(2, OutputStream::Stdout, b"third".to_vec());

        let snapshot = output.snapshot();
        assert_eq!(snapshot.latest_run, Some(2));
        let rendered: Vec<String> = snapshot
            .chunks
            .iter()
            .map(|chunk| match chunk {
                RetainedChunk::Marker { label, .. } => format!("MARK {label}"),
                RetainedChunk::Data { stream, text, .. } => {
                    let stream = match stream {
                        OutputStream::Stdout => "out",
                        OutputStream::Stderr => "err",
                    };
                    format!("{stream}: {text}")
                }
            })
            .collect();
        assert_eq!(
            rendered,
            vec![
                "MARK ── Run 1 started ──",
                "out: first",
                "err: second",
                "MARK ── Run 2 started ──",
                "out: third",
            ]
        );
        assert!(!snapshot.truncated);
    }

    #[test]
    fn retention_bounds_drop_the_oldest_first_and_count_the_loss() {
        let output = ProcessOutput::new();
        // Two chunks that together just exceed the byte bound: the oldest
        // unit goes and the loss is counted.
        output.append(1, OutputStream::Stdout, vec![b'a'; RETAINED_BYTES - 100]);
        output.append(1, OutputStream::Stdout, vec![b'b'; 101]);

        let snapshot = output.snapshot();
        assert!(snapshot.truncated);
        assert_eq!(snapshot.dropped_bytes, (RETAINED_BYTES - 100) as u64);
        assert_eq!(snapshot.chunks.len(), 1);

        // The chunk bound applies even for tiny chunks.
        let tiny = ProcessOutput::new();
        for _ in 0..=RETAINED_CHUNKS {
            tiny.append(1, OutputStream::Stdout, b"a".to_vec());
        }
        let tiny_snapshot = tiny.snapshot();
        assert!(tiny_snapshot.truncated);
        assert_eq!(tiny_snapshot.chunks.len(), RETAINED_CHUNKS);
        assert_eq!(tiny_snapshot.dropped_chunks, 1);
    }

    #[test]
    fn a_stale_tail_from_ended_runs_never_reorders_behind_a_newer_marker() {
        let output = ProcessOutput::new();
        output.mark_run(1);
        output.append(1, OutputStream::Stdout, b"old".to_vec());
        output.mark_run(2);
        // The finished drain of Run 1 lands after Run 2's marker.
        output.append(1, OutputStream::Stdout, b"stale tail".to_vec());

        let snapshot = output.snapshot();
        assert_eq!(
            snapshot.chunks,
            vec![
                RetainedChunk::Marker {
                    run_id: 1,
                    label: "── Run 1 started ──".to_string()
                },
                RetainedChunk::Data {
                    run_id: 1,
                    stream: OutputStream::Stdout,
                    text: "old".to_string()
                },
                RetainedChunk::Marker {
                    run_id: 2,
                    label: "── Run 2 started ──".to_string()
                },
            ]
        );
    }

    #[test]
    fn duplicate_and_out_of_order_markers_are_ignored() {
        let output = ProcessOutput::new();
        output.mark_run(2);
        output.mark_run(1);
        output.mark_run(2);

        let snapshot = output.snapshot();
        assert_eq!(
            snapshot.chunks,
            vec![RetainedChunk::Marker {
                run_id: 2,
                label: "── Run 2 started ──".to_string()
            }]
        );
        assert_eq!(snapshot.latest_run, Some(2));
    }
}
