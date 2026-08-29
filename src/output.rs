//! Bounded Project Logs history.
//!
//! One deep module owns normalized Logs output for every Process. Callers append
//! observed bytes and take immutable snapshots. The implementation hides lossy
//! UTF-8 conversion, long-line splitting, per-Process limits, the hard Project
//! limit, oldest-first eviction, timestamps, stream labels, and literal search.
//! Output bytes do not enter the Supervisor control queue.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::runtime::{OutputStream, ProcessId};

/// Maximum retained Logs bytes for one Process.
pub const RETAINED_BYTES: usize = 1024 * 1024;
/// Maximum retained Logs units for one Process.
pub const RETAINED_CHUNKS: usize = 4096;
/// Hard retained Logs byte limit for the complete Project.
pub const PROJECT_RETAINED_BYTES: usize = 8 * RETAINED_BYTES;
/// Hard retained Logs unit limit for the complete Project.
pub const PROJECT_RETAINED_CHUNKS: usize = 16_384;
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

/// One literal match in retained normalized Logs text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogMatch {
    pub sequence: u64,
    pub line: usize,
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogSearch {
    pub matches: Vec<LogMatch>,
    pub limited: bool,
}

/// One owned snapshot of a Process's retained Logs history.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RetainedOutput {
    pub chunks: Vec<RetainedChunk>,
    pub latest_run: Option<u64>,
    pub truncated: bool,
    pub dropped_chunks: u64,
    pub dropped_bytes: u64,
    pub generation: u64,
}

impl RetainedOutput {
    /// Format only the newest required lines. Timestamps use UTC time-of-day
    /// derived from the recorded Unix time, so fixed test times are independent
    /// of the host clock and time zone.
    pub fn display_lines(&self, tail_limit: usize) -> Vec<crate::tui::PipeLine> {
        if tail_limit == 0 {
            return Vec::new();
        }
        let mut lines = Vec::new();
        for chunk in self.chunks.iter().rev() {
            let mut chunk_lines = formatted_lines(chunk);
            while let Some(line) = chunk_lines.pop() {
                lines.push(line);
                if lines.len() == tail_limit {
                    break;
                }
            }
            if lines.len() == tail_limit {
                break;
            }
        }
        if self.truncated && lines.len() < tail_limit {
            lines.push(history_limit_line());
        }
        lines.reverse();
        lines
    }

    /// Format one visible window from the retained head or one stable source
    /// position. `None` selects the retained head. A missing source returns
    /// `None`, so a paused view can detect eviction without tail-relative
    /// offset guesses.
    pub fn display_window_from(
        &self,
        anchor: Option<(u64, usize)>,
        limit: usize,
    ) -> Option<Vec<crate::tui::PipeLine>> {
        if limit == 0 {
            return Some(Vec::new());
        }
        let mut lines = Vec::with_capacity(limit.min(self.chunks.len().saturating_add(1)));
        if anchor.is_none() && self.truncated {
            lines.push(history_limit_line());
            if lines.len() == limit {
                return Some(lines);
            }
        }

        let mut found = anchor.is_none();
        for chunk in &self.chunks {
            if !found && Some(sequence(chunk)) != anchor.map(|position| position.0) {
                continue;
            }
            for line in formatted_lines(chunk) {
                if !found {
                    if line.source != anchor {
                        continue;
                    }
                    found = true;
                }
                lines.push(line);
                if lines.len() == limit {
                    return Some(lines);
                }
            }
        }
        found.then_some(lines)
    }

    /// Return the zero-based rendered line position and total rendered lines
    /// for one retained source. `None` selects the retained head, including
    /// its synthetic history-limit line when older Logs were removed.
    pub(crate) fn display_position(&self, source: Option<(u64, usize)>) -> Option<(usize, usize)> {
        let mut total = usize::from(self.truncated);
        let mut position = source.is_none().then_some(0);
        for chunk in &self.chunks {
            let count = displayed_line_count(chunk);
            if position.is_none()
                && let Some((target_sequence, target_line)) = source
                && target_sequence == sequence(chunk)
                && target_line < count
            {
                position = Some(total + target_line);
            }
            total = total.saturating_add(count);
        }
        position.map(|position| (position, total))
    }

    /// Return the retained source at one zero-based rendered line position.
    /// The synthetic history-limit line has no source.
    pub(crate) fn display_source_at(&self, position: usize) -> Option<(u64, usize)> {
        let mut remaining = position.checked_sub(usize::from(self.truncated))?;
        for chunk in &self.chunks {
            let count = displayed_line_count(chunk);
            if remaining < count {
                return Some((sequence(chunk), remaining));
            }
            remaining = remaining.saturating_sub(count);
        }
        None
    }

    /// Find case-sensitive literal substrings in all retained normalized text.
    /// Search uses the immutable snapshot, so ingestion never waits for it.
    pub fn search(&self, query: &str) -> LogSearch {
        if query.is_empty() {
            return LogSearch::default();
        }
        let mut found = LogSearch::default();
        'chunks: for chunk in &self.chunks {
            let RetainedChunk::Data { text, sequence, .. } = chunk else {
                continue;
            };
            for (line, value) in text.split_terminator('\n').enumerate() {
                for (start, _) in value.match_indices(query) {
                    if found.matches.len() == SEARCH_MATCH_LIMIT {
                        found.limited = true;
                        break 'chunks;
                    }
                    found.matches.push(LogMatch {
                        sequence: *sequence,
                        line,
                        start,
                        end: start + query.len(),
                    });
                }
            }
        }
        found
    }
}

/// A handle to one Process inside the Project Logs owner.
pub struct ProcessOutput {
    process: usize,
    project: Arc<Mutex<ProjectHistory>>,
}

struct ProcessHistory {
    chunks: VecDeque<RetainedChunk>,
    pending: [Option<PendingLine>; 3],
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
    chunks: usize,
    next_sequence: u64,
}

impl ProcessOutput {
    pub(crate) fn append(&self, run_id: u64, stream: OutputStream, data: Vec<u8>) {
        self.append_at(run_id, stream, observation_millis(), data);
    }

    pub(crate) fn append_at(
        &self,
        run_id: u64,
        stream: OutputStream,
        observed_at_ms: u64,
        data: Vec<u8>,
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
        for byte in data {
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
                project.chunks += 1;
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
        let project = lock(&self.project);
        let process = &project.processes[self.process];
        if known_generation == Some(process.generation) {
            return None;
        }
        let mut chunks: Vec<_> = process.chunks.iter().cloned().collect();
        chunks.extend(process.pending.iter().flatten().map(pending_chunk));
        chunks.sort_by_key(sequence);
        Some(RetainedOutput {
            chunks,
            latest_run: process.latest_run,
            truncated: process.dropped_chunks > 0,
            dropped_chunks: process.dropped_chunks,
            dropped_bytes: process.dropped_bytes,
            generation: process.generation,
        })
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
        self.chunks = self.chunks.saturating_sub(1);
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
        self.processes[process].chunks.push_back(chunk);
        self.processes[process].bytes += bytes;
        self.bytes += bytes;
        self.chunks += 1;
    }

    fn enforce(&mut self, changed: usize) {
        while self.processes[changed].bytes > RETAINED_BYTES
            || process_chunk_count(&self.processes[changed]) > RETAINED_CHUNKS
        {
            self.evict_from(changed);
        }
        while self.bytes > PROJECT_RETAINED_BYTES || self.chunks > PROJECT_RETAINED_CHUNKS {
            let Some(oldest) = self
                .processes
                .iter()
                .enumerate()
                .filter_map(|(index, process)| {
                    oldest_sequence(process).map(|sequence| (index, sequence))
                })
                .min_by_key(|(_, sequence)| *sequence)
                .map(|(index, _)| index)
            else {
                break;
            };
            self.evict_from(oldest);
        }
    }

    fn evict_from(&mut self, process: usize) {
        let oldest = oldest_sequence(&self.processes[process]);
        let front_is_oldest = self.processes[process]
            .chunks
            .front()
            .is_some_and(|chunk| Some(sequence(chunk)) == oldest);
        let bytes = if front_is_oldest {
            let chunk = self.processes[process]
                .chunks
                .pop_front()
                .expect("the oldest chunk exists");
            retained_size(&chunk)
        } else {
            let Some(stream) = self.processes[process].pending.iter().position(|pending| {
                pending
                    .as_ref()
                    .is_some_and(|line| Some(line.sequence) == oldest)
            }) else {
                return;
            };
            self.processes[process].pending[stream]
                .take()
                .expect("the oldest pending line exists")
                .bytes
                .len()
        };
        let history = &mut self.processes[process];
        history.bytes = history.bytes.saturating_sub(bytes);
        history.dropped_chunks += 1;
        history.dropped_bytes += bytes as u64;
        history.generation = history.generation.wrapping_add(1);
        self.bytes = self.bytes.saturating_sub(bytes);
        self.chunks = self.chunks.saturating_sub(1);
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

fn process_chunk_count(process: &ProcessHistory) -> usize {
    process.chunks.len() + process.pending.iter().flatten().count()
}

fn oldest_sequence(process: &ProcessHistory) -> Option<u64> {
    process
        .chunks
        .front()
        .map(sequence)
        .into_iter()
        .chain(process.pending.iter().flatten().map(|line| line.sequence))
        .min()
}

fn history_limit_line() -> crate::tui::PipeLine {
    crate::tui::PipeLine {
        text: "[history limit: older Logs output removed]".to_string(),
        marker: true,
        source: None,
        content_offset: 0,
        highlight: None,
        selection: None,
    }
}

fn displayed_line_count(chunk: &RetainedChunk) -> usize {
    match chunk {
        RetainedChunk::Marker { .. } => 1,
        RetainedChunk::Data { text, .. } => text.split_terminator('\n').count(),
    }
}

fn formatted_lines(chunk: &RetainedChunk) -> Vec<crate::tui::PipeLine> {
    use crate::tui::PipeLine;
    match chunk {
        RetainedChunk::Marker {
            label,
            sequence,
            observed_at_ms,
            ..
        } => vec![PipeLine {
            text: format!("{} {label}", timestamp(*observed_at_ms)),
            marker: true,
            source: Some((*sequence, 0)),
            content_offset: 0,
            highlight: None,
            selection: None,
        }],
        RetainedChunk::Data {
            stream,
            text,
            sequence,
            observed_at_ms,
            ..
        } => {
            let label = match stream {
                OutputStream::Stdout => "out",
                OutputStream::Stderr => "err",
                OutputStream::Combined => "pty",
            };
            text.split_terminator('\n')
                .enumerate()
                .map(|(line, value)| {
                    let prefix = format!("{} {label}: ", timestamp(*observed_at_ms));
                    let content_offset = prefix.len();
                    PipeLine {
                        text: format!("{prefix}{value}"),
                        marker: false,
                        source: Some((*sequence, line)),
                        content_offset,
                        highlight: None,
                        selection: None,
                    }
                })
                .collect()
        }
    }
}

fn timestamp(unix_ms: u64) -> String {
    let day_ms = unix_ms % 86_400_000;
    let hours = day_ms / 3_600_000;
    let minutes = day_ms / 60_000 % 60;
    let seconds = day_ms / 1_000 % 60;
    let millis = day_ms % 1_000;
    format!("{hours:02}:{minutes:02}:{seconds:02}.{millis:03}")
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
        RetainedChunk::Marker { label, .. } => label.len(),
        RetainedChunk::Data { text, .. } => text.len(),
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
        let project = Arc::new(Mutex::new(ProjectHistory {
            processes: (0..process_count)
                .map(|_| ProcessHistory {
                    chunks: VecDeque::new(),
                    pending: std::array::from_fn(|_| None),
                    bytes: 0,
                    dropped_chunks: 0,
                    dropped_bytes: 0,
                    latest_run: None,
                    generation: 0,
                })
                .collect(),
            bytes: 0,
            chunks: 0,
            next_sequence: 0,
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
mod tests {
    use super::*;

    fn output() -> Arc<ProcessOutput> {
        OutputViews::new(1).for_process(0).unwrap()
    }

    #[test]
    fn logs_format_fixed_timestamps_streams_runs_and_invalid_utf8() {
        let output = output();
        output.mark_run_at(1, 3_723_004);
        output.append_at(1, OutputStream::Stdout, 3_723_005, b"ok\n".to_vec());
        output.append_at(1, OutputStream::Stderr, 3_723_006, vec![b'x', 0xff, b'\n']);
        let text: Vec<_> = output
            .snapshot()
            .display_lines(10)
            .into_iter()
            .map(|line| line.text)
            .collect();
        assert_eq!(
            text,
            [
                "01:02:03.004 ── Run 1 started ──",
                "01:02:03.005 out: ok",
                "01:02:03.006 err: x�",
            ]
        );
    }

    #[test]
    fn long_lines_are_split_visibly_and_later_output_survives() {
        let output = output();
        let mut data = vec![b'a'; LOGICAL_LINE_BYTES + 1];
        data.extend_from_slice(b"\nafter\n");
        output.append_at(1, OutputStream::Combined, 0, data);
        let lines = output.snapshot().display_lines(10);
        assert!(lines[0].text.contains("[line continued at 16 KiB]"));
        assert!(lines.last().unwrap().text.ends_with("pty: after"));
    }

    #[test]
    fn normalization_spans_reader_chunks_without_breaking_utf8_or_line_bounds() {
        let output = output();
        let euro = "€".as_bytes();
        output.append_at(1, OutputStream::Combined, 0, euro[..1].to_vec());
        output.append_at(1, OutputStream::Combined, 1, euro[1..].to_vec());
        for _ in 0..LOGICAL_LINE_BYTES {
            output.append_at(1, OutputStream::Combined, 2, vec![b'x']);
        }
        output.append_at(1, OutputStream::Combined, 3, b"\nafter\n".to_vec());

        let lines = output.snapshot().display_lines(10);
        assert!(lines.iter().any(|line| line.text.contains('€')));
        assert!(
            lines
                .iter()
                .any(|line| line.text.contains("[line continued at 16 KiB]"))
        );
        assert!(lines.last().unwrap().text.ends_with("pty: after"));
    }

    #[test]
    fn unchanged_logs_do_not_clone_another_snapshot() {
        let output = output();
        let first = output
            .snapshot_if_changed(None)
            .expect("the first snapshot is returned");

        assert!(output.snapshot_if_changed(Some(first.generation)).is_none());

        output.append_at(1, OutputStream::Stdout, 0, b"later\n".to_vec());
        let changed = output
            .snapshot_if_changed(Some(first.generation))
            .expect("new output returns a new snapshot");
        assert_ne!(changed.generation, first.generation);
    }

    #[test]
    fn literal_search_is_case_sensitive_and_bounded_to_retained_text() {
        let output = output();
        output.append_at(
            1,
            OutputStream::Stdout,
            0,
            b"needle Needle needle\n".to_vec(),
        );
        let search = output.snapshot().search("needle");
        assert_eq!(search.matches.len(), 2);
        assert!(!search.limited);
        assert!(output.snapshot().search("NEEDLE").matches.is_empty());
    }

    #[test]
    fn per_process_bound_evicts_oldest_and_shows_the_history_marker() {
        let output = output();
        output.append_at(1, OutputStream::Stdout, 0, vec![b'a'; RETAINED_BYTES]);
        output.append_at(1, OutputStream::Stdout, 1, b"new\n".to_vec());
        let snapshot = output.snapshot();
        assert!(snapshot.truncated);
        assert!(
            snapshot
                .chunks
                .iter()
                .any(|chunk| matches!(chunk, RetainedChunk::Data { text, .. } if text == "new\n"))
        );
        assert!(
            snapshot.display_lines(100)[0]
                .text
                .contains("history limit")
        );
    }

    #[test]
    fn project_bound_evicts_the_oldest_process_even_when_another_is_selected() {
        let views = OutputViews::new(9);
        for process in 0..9 {
            views.for_process(process).unwrap().append_at(
                1,
                OutputStream::Stdout,
                process as u64,
                vec![b'a' + process as u8; RETAINED_BYTES],
            );
        }
        let oldest = views.for_process(0).unwrap().snapshot();
        let newest = views.for_process(8).unwrap().snapshot();
        assert!(oldest.truncated);
        assert!(!newest.chunks.is_empty());
    }
}
