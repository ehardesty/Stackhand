//! Shared, bounded segments. Snapshots share text; only the segment being
//! written can need a copy. Retired segments remain charged until readers drop
//! them, so a paused reader cannot hide memory from Project retention policy.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{RetainedChunk, retained_size, sequence};

const SEGMENT_BYTES: usize = 64 * 1024;
const SEGMENT_RECORDS: usize = 256;

#[derive(Debug)]
pub(super) struct Segment {
    pub(super) records: Vec<RetainedChunk>,
    charged: usize,
    allocation: Arc<AtomicUsize>,
}

impl Segment {
    fn new(records: Vec<RetainedChunk>, allocation: Arc<AtomicUsize>) -> Self {
        let mut value = Self {
            records,
            charged: 0,
            allocation,
        };
        value.recharge();
        value
    }

    fn recharge(&mut self) {
        let bytes = std::mem::size_of::<Self>()
            + 2 * std::mem::size_of::<usize>()
            + self.records.capacity() * std::mem::size_of::<RetainedChunk>()
            + self.records.iter().map(text_capacity).sum::<usize>();
        if bytes >= self.charged {
            self.allocation
                .fetch_add(bytes - self.charged, Ordering::Relaxed);
        } else {
            self.allocation
                .fetch_sub(self.charged - bytes, Ordering::Relaxed);
        }
        self.charged = bytes;
    }
}

impl Clone for Segment {
    fn clone(&self) -> Self {
        Self::new(self.records.clone(), Arc::clone(&self.allocation))
    }
}

impl Drop for Segment {
    fn drop(&mut self) {
        self.allocation.fetch_sub(self.charged, Ordering::Relaxed);
    }
}

fn text_capacity(chunk: &RetainedChunk) -> usize {
    match chunk {
        RetainedChunk::Data { text, .. } => text.capacity(),
        RetainedChunk::Marker { label, .. } => label.capacity(),
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Index {
    lines: usize,
}

/// Cheap immutable Logs collection with indexed source and row lookup.
#[derive(Clone, Default)]
pub struct RetainedChunks {
    pub(super) segments: VecDeque<Arc<Segment>>,
    index: Vec<Index>,
    len: usize,
    bytes: usize,
    allocation: Arc<AtomicUsize>,
}

impl std::fmt::Debug for RetainedChunks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetainedChunks")
            .field("segments", &self.segments.len())
            .field("records", &self.len)
            .field("bytes", &self.bytes)
            .finish()
    }
}

impl PartialEq for RetainedChunks {
    fn eq(&self, other: &Self) -> bool {
        self.iter().eq(other.iter())
    }
}

impl RetainedChunks {
    pub(super) fn new(allocation: Arc<AtomicUsize>) -> Self {
        Self {
            allocation,
            ..Self::default()
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.bytes
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &RetainedChunk> {
        self.segments
            .iter()
            .flat_map(|segment| segment.records.iter())
    }

    pub(super) fn first(&self) -> Option<&RetainedChunk> {
        self.segments.front()?.records.first()
    }

    pub(super) fn push(&mut self, chunk: RetainedChunk) {
        let seq = sequence(&chunk);
        let mut index = self.segments.partition_point(|segment| {
            sequence(segment.records.last().expect("nonempty segment")) < seq
        });
        if index == self.segments.len() {
            if self.segments.back().is_some_and(|segment| {
                segment.charged < SEGMENT_BYTES && segment.records.len() < SEGMENT_RECORDS
            }) {
                index -= 1;
            } else {
                self.segments.push_back(Arc::new(Segment::new(
                    Vec::new(),
                    Arc::clone(&self.allocation),
                )));
            }
        }
        self.bytes += retained_size(&chunk);
        self.len += 1;
        let segment = Arc::make_mut(&mut self.segments[index]);
        let position = segment
            .records
            .partition_point(|record| sequence(record) < seq);
        segment.records.insert(position, chunk);
        segment.recharge();
        if segment.records.len() > SEGMENT_RECORDS
            || (segment.charged > SEGMENT_BYTES && segment.records.len() > 1)
        {
            let records = segment.records.split_off(segment.records.len() / 2);
            segment.records.shrink_to_fit();
            segment.recharge();
            self.segments.insert(
                index + 1,
                Arc::new(Segment::new(records, Arc::clone(&self.allocation))),
            );
        }
        self.reindex_from(index);
    }

    /// Evict a complete segment without copying a reader's pinned text.
    pub(super) fn pop_oldest(&mut self) -> Option<(usize, usize)> {
        let segment = self.segments.pop_front()?;
        let count = segment.records.len();
        let bytes = segment.records.iter().map(retained_size).sum::<usize>();
        self.bytes -= bytes;
        self.len -= count;
        self.reindex_from(0);
        Some((bytes, count))
    }

    fn reindex_from(&mut self, from: usize) {
        self.index.truncate(from);
        let mut total = self.index.last().copied().unwrap_or_default();
        for segment in self.segments.iter().skip(from) {
            total.lines += segment.records.iter().map(line_count).sum::<usize>();
            self.index.push(total);
        }
    }

    pub(super) fn total_lines(&self) -> usize {
        self.index.last().map_or(0, |index| index.lines)
    }

    pub(super) fn position(&self, source: (u64, usize)) -> Option<usize> {
        let index = self.segments.partition_point(|segment| {
            sequence(segment.records.last().expect("nonempty segment")) < source.0
        });
        let segment = self.segments.get(index)?;
        let record = segment
            .records
            .binary_search_by_key(&source.0, sequence)
            .ok()?;
        if source.1 >= line_count(&segment.records[record]) {
            return None;
        }
        Some(
            index.checked_sub(1).map_or(0, |i| self.index[i].lines)
                + segment.records[..record]
                    .iter()
                    .map(line_count)
                    .sum::<usize>()
                + source.1,
        )
    }

    pub(super) fn at_line(&self, line: usize) -> Option<(&RetainedChunk, usize)> {
        let index = self.index.partition_point(|index| index.lines <= line);
        let mut offset = line - index.checked_sub(1).map_or(0, |i| self.index[i].lines);
        for record in &self.segments.get(index)?.records {
            let count = line_count(record);
            if offset < count {
                return Some((record, offset));
            }
            offset -= count;
        }
        None
    }
}

pub(super) fn line_count(chunk: &RetainedChunk) -> usize {
    match chunk {
        RetainedChunk::Marker { .. } => 1,
        RetainedChunk::Data { text, .. } => text.split_terminator('\n').count(),
    }
}

impl FromIterator<RetainedChunk> for RetainedChunks {
    fn from_iter<T: IntoIterator<Item = RetainedChunk>>(iter: T) -> Self {
        let mut chunks = Self::default();
        for chunk in iter {
            chunks.push(chunk);
        }
        chunks
    }
}
