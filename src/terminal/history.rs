use std::collections::VecDeque;

pub const OUTPUT_HISTORY_BYTES: usize = 16 * 1_024 * 1_024;
pub const OUTPUT_HISTORY_CHUNKS: usize = 4_096;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OutputHistoryMetrics {
    pub bytes: usize,
    pub chunks: usize,
    pub evicted_bytes: usize,
}

pub struct OutputHistoryLedger {
    chunk_lengths: VecDeque<usize>,
    bytes: usize,
    evicted_bytes: usize,
}

impl OutputHistoryLedger {
    pub fn new() -> Self {
        Self {
            chunk_lengths: VecDeque::new(),
            bytes: 0,
            evicted_bytes: 0,
        }
    }

    pub fn push(&mut self, chunk_bytes: usize) -> usize {
        if chunk_bytes == 0 {
            return 0;
        }
        let mut evicted = 0;
        if chunk_bytes > OUTPUT_HISTORY_BYTES {
            evicted = self.bytes + chunk_bytes - OUTPUT_HISTORY_BYTES;
            self.chunk_lengths.clear();
            self.chunk_lengths.push_back(OUTPUT_HISTORY_BYTES);
            self.bytes = OUTPUT_HISTORY_BYTES;
        } else {
            while self.bytes + chunk_bytes > OUTPUT_HISTORY_BYTES
                || self.chunk_lengths.len() >= OUTPUT_HISTORY_CHUNKS
            {
                let oldest = self
                    .chunk_lengths
                    .pop_front()
                    .expect("history ledger is not empty");
                self.bytes -= oldest;
                evicted += oldest;
            }
            self.chunk_lengths.push_back(chunk_bytes);
            self.bytes += chunk_bytes;
        }
        self.evicted_bytes += evicted;
        evicted
    }

    pub fn metrics(&self) -> OutputHistoryMetrics {
        OutputHistoryMetrics {
            bytes: self.bytes,
            chunks: self.chunk_lengths.len(),
            evicted_bytes: self.evicted_bytes,
        }
    }
}

impl Default for OutputHistoryLedger {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_accounting_evicts_at_the_chunk_limit() {
        let mut ledger = OutputHistoryLedger::new();
        for _ in 0..=OUTPUT_HISTORY_CHUNKS {
            ledger.push(1);
        }
        assert_eq!(ledger.metrics().chunks, OUTPUT_HISTORY_CHUNKS);
        assert_eq!(ledger.metrics().bytes, OUTPUT_HISTORY_CHUNKS);
        assert_eq!(ledger.metrics().evicted_bytes, 1);
    }

    #[test]
    fn oversized_chunk_is_accounted_at_the_byte_limit() {
        let mut ledger = OutputHistoryLedger::new();
        ledger.push(OUTPUT_HISTORY_BYTES + 1);
        assert_eq!(ledger.metrics().bytes, OUTPUT_HISTORY_BYTES);
        assert_eq!(ledger.metrics().chunks, 1);
        assert_eq!(ledger.metrics().evicted_bytes, 1);
    }
}
