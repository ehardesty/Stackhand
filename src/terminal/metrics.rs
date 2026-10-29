//! Parsed output progress and native memory measurements are separate facts.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TerminalMetrics {
    pub parsed_bytes: usize,
    pub parsed_chunks: usize,
    pub scrollback_limit_bytes: usize,
    pub memory: Option<TerminalMemory>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TerminalMemory {
    pub page_bytes: usize,
    pub resident_bytes: usize,
    pub compressed_bytes: usize,
    pub compression_supported: bool,
}

impl From<libghostty_vt::terminal::MemoryUsage> for TerminalMemory {
    fn from(value: libghostty_vt::terminal::MemoryUsage) -> Self {
        Self {
            page_bytes: value
                .primary_virtual_bytes
                .saturating_add(value.alternate_virtual_bytes) as usize,
            resident_bytes: value
                .primary_resident_bytes
                .saturating_add(value.alternate_resident_bytes)
                as usize,
            compressed_bytes: value
                .primary_compressed_bytes
                .saturating_add(value.alternate_compressed_bytes)
                as usize,
            compression_supported: value.compression_supported,
        }
    }
}
