use std::collections::VecDeque;

pub const OUTPUT_HISTORY_BYTES: usize = 16 * 1_024 * 1_024;
pub const OUTPUT_HISTORY_CHUNKS: usize = 4_096;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OutputHistoryMetrics {
    pub bytes: usize,
    pub chunks: usize,
    pub evicted_bytes: usize,
}

pub struct BoundedOutputHistory {
    chunks: VecDeque<Vec<u8>>,
    bytes: usize,
    evicted_bytes: usize,
}

impl BoundedOutputHistory {
    pub fn new() -> Self {
        Self {
            chunks: VecDeque::new(),
            bytes: 0,
            evicted_bytes: 0,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> usize {
        if chunk.is_empty() {
            return 0;
        }
        let mut evicted = 0;
        if chunk.len() > OUTPUT_HISTORY_BYTES {
            let retained = &chunk[chunk.len() - OUTPUT_HISTORY_BYTES..];
            evicted = self.bytes + chunk.len() - retained.len();
            self.chunks.clear();
            self.chunks.push_back(retained.to_vec());
            self.bytes = retained.len();
        } else {
            while self.bytes + chunk.len() > OUTPUT_HISTORY_BYTES
                || self.chunks.len() >= OUTPUT_HISTORY_CHUNKS
            {
                let oldest = self.chunks.pop_front().expect("history is not empty");
                self.bytes -= oldest.len();
                evicted += oldest.len();
            }
            self.chunks.push_back(chunk.to_vec());
            self.bytes += chunk.len();
        }
        self.evicted_bytes += evicted;
        evicted
    }

    pub fn metrics(&self) -> OutputHistoryMetrics {
        OutputHistoryMetrics {
            bytes: self.bytes,
            chunks: self.chunks.len(),
            evicted_bytes: self.evicted_bytes,
        }
    }
}

impl Default for BoundedOutputHistory {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actual_chunks_evict_at_the_chunk_limit() {
        let mut history = BoundedOutputHistory::new();
        for _ in 0..=OUTPUT_HISTORY_CHUNKS {
            history.push(b"x");
        }
        assert_eq!(history.metrics().chunks, OUTPUT_HISTORY_CHUNKS);
        assert_eq!(history.metrics().bytes, OUTPUT_HISTORY_CHUNKS);
        assert_eq!(history.metrics().evicted_bytes, 1);
    }

    #[test]
    fn oversized_chunk_keeps_only_its_bounded_tail() {
        let mut history = BoundedOutputHistory::new();
        history.push(&vec![1; OUTPUT_HISTORY_BYTES + 1]);
        assert_eq!(history.metrics().bytes, OUTPUT_HISTORY_BYTES);
        assert_eq!(history.metrics().chunks, 1);
        assert_eq!(history.metrics().evicted_bytes, 1);
    }
}
