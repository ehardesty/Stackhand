//! Live literal matching for one Run's output stream.
//!
//! The matcher is attached at the first output ingress for each transport. It
//! removes terminal controls for matching, keeps only parser and pattern state,
//! and reports each configured match once. Raw output still follows its normal
//! terminal or retained-output path unchanged.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::start::RunOutputObserver;

/// One health-check log child that the live matcher reports when its
/// literal appears.
#[derive(Clone, Debug)]
pub(crate) struct LogPattern {
    pub(crate) key: u64,
    pub(crate) contains: String,
    /// Scheduled liveness attempts carry their attempt identity. Latched
    /// readiness observations use `None`.
    pub(crate) attempt_id: Option<u64>,
}

/// Why a live matcher could not be constructed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LogMatcherError {
    EmptyPattern(u64),
}

/// One shared observer for all log checks attached to one Run.
///
/// The output owners call [`RunOutputObserver::observe`] with raw chunks. The
/// matcher serializes parser state because pipe stdout and stderr can arrive
/// on separate reader threads. It never retains the output stream.
pub(crate) struct LiveLogMatcher {
    state: Mutex<MatcherState>,
    cancelled: Arc<AtomicBool>,
    on_match: Box<dyn Fn(u64, Option<u64>) + Send + Sync + 'static>,
}

struct MatcherState {
    parser: ControlParser,
    patterns: Vec<PatternState>,
}

struct PatternState {
    key: u64,
    matcher: LiteralMatcher,
    latched: bool,
    attempt_id: Option<u64>,
}

impl LiveLogMatcher {
    #[cfg(test)]
    pub(crate) fn new(
        patterns: Vec<LogPattern>,
        cancelled: Arc<AtomicBool>,
        on_match: impl Fn(u64) + Send + Sync + 'static,
    ) -> Result<Arc<Self>, LogMatcherError> {
        Self::new_with_attempts(patterns, cancelled, move |key, _| on_match(key))
    }

    pub(crate) fn new_with_attempts(
        patterns: Vec<LogPattern>,
        cancelled: Arc<AtomicBool>,
        on_match: impl Fn(u64, Option<u64>) + Send + Sync + 'static,
    ) -> Result<Arc<Self>, LogMatcherError> {
        let patterns = patterns
            .into_iter()
            .map(pattern_state)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Arc::new(Self {
            state: Mutex::new(MatcherState {
                parser: ControlParser::default(),
                patterns,
            }),
            cancelled,
            on_match: Box::new(on_match),
        }))
    }

    /// Replace one pattern and clear only that pattern's rolling match state.
    /// A later liveness attempt therefore cannot be satisfied by output that
    /// matched an older attempt.
    pub(crate) fn replace(&self, pattern: LogPattern) -> Result<(), LogMatcherError> {
        let replacement = pattern_state(pattern)?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = state
            .patterns
            .iter_mut()
            .find(|existing| existing.key == replacement.key)
        {
            *existing = replacement;
        } else {
            state.patterns.push(replacement);
        }
        Ok(())
    }
}

impl RunOutputObserver for LiveLogMatcher {
    fn observe(&self, data: &[u8]) {
        if data.is_empty() || self.cancelled.load(Ordering::Acquire) {
            return;
        }

        let mut matched = Vec::new();
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.patterns.iter().all(|pattern| pattern.latched) {
                return;
            }
            for &byte in data {
                if self.cancelled.load(Ordering::Acquire) {
                    break;
                }
                let Some(byte) = state.parser.consume(byte) else {
                    continue;
                };
                for pattern in &mut state.patterns {
                    if !pattern.latched && pattern.matcher.push(byte) {
                        pattern.latched = true;
                        matched.push((pattern.key, pattern.attempt_id));
                    }
                }
            }
        }

        for (key, attempt_id) in matched {
            if self.cancelled.load(Ordering::Acquire) {
                return;
            }
            (self.on_match)(key, attempt_id);
        }
    }
}

fn pattern_state(pattern: LogPattern) -> Result<PatternState, LogMatcherError> {
    if pattern.contains.is_empty() {
        return Err(LogMatcherError::EmptyPattern(pattern.key));
    }
    Ok(PatternState {
        key: pattern.key,
        matcher: LiteralMatcher::new(normalize_literal(pattern.contains.as_bytes())),
        latched: false,
        attempt_id: pattern.attempt_id,
    })
}

/// Normalize the literal in the same way as live carriage-return input.
/// `\r\n` and a lone `\r` both become one logical newline.
fn normalize_literal(input: &[u8]) -> Vec<u8> {
    let mut normalized = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] == b'\r' {
            normalized.push(b'\n');
            index += 1;
            if input.get(index) == Some(&b'\n') {
                index += 1;
            }
        } else {
            normalized.push(input[index]);
            index += 1;
        }
    }
    normalized
}

/// A literal matcher with constant state after construction.
struct LiteralMatcher {
    pattern: Vec<u8>,
    prefix: Vec<usize>,
    matched: usize,
}

impl LiteralMatcher {
    fn new(pattern: Vec<u8>) -> Self {
        let mut prefix = vec![0; pattern.len()];
        for index in 1..pattern.len() {
            let mut prefix_len = prefix[index - 1];
            while prefix_len > 0 && pattern[index] != pattern[prefix_len] {
                prefix_len = prefix[prefix_len - 1];
            }
            if pattern[index] == pattern[prefix_len] {
                prefix_len += 1;
            }
            prefix[index] = prefix_len;
        }
        Self {
            pattern,
            prefix,
            matched: 0,
        }
    }

    /// Feed one normalized byte. Return true on the first complete match.
    fn push(&mut self, byte: u8) -> bool {
        while self.matched > 0 && self.pattern[self.matched] != byte {
            self.matched = self.prefix[self.matched - 1];
        }
        if self.pattern[self.matched] == byte {
            self.matched += 1;
        }
        if self.matched == self.pattern.len() {
            self.matched = self.prefix[self.matched - 1];
            true
        } else {
            false
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ParserState {
    #[default]
    Ground,
    Escape,
    EscapeIntermediate,
    Csi,
    StringSequence,
    StringEscape,
}

/// A finite-state terminal-control filter. It stores no control payload.
#[derive(Default)]
struct ControlParser {
    state: ParserState,
    skip_lf_after_cr: bool,
    utf8_continuations: u8,
}

impl ControlParser {
    /// Consume one raw byte and return one visible matching byte when present.
    fn consume(&mut self, byte: u8) -> Option<u8> {
        if self.state == ParserState::Ground && self.utf8_continuations > 0 {
            if (0x80..=0xbf).contains(&byte) {
                self.utf8_continuations -= 1;
                return Some(byte);
            }
            self.utf8_continuations = 0;
        }

        if self.state == ParserState::Ground && self.skip_lf_after_cr {
            self.skip_lf_after_cr = false;
            if byte == b'\n' {
                return None;
            }
        }

        match self.state {
            ParserState::Ground => self.consume_ground(byte),
            ParserState::Escape => {
                self.state = match byte {
                    b'[' => ParserState::Csi,
                    b']' | b'P' | b'X' | b'^' | b'_' => ParserState::StringSequence,
                    0x20..=0x2f => ParserState::EscapeIntermediate,
                    _ => ParserState::Ground,
                };
                None
            }
            ParserState::EscapeIntermediate => {
                if byte == 0x1b {
                    self.state = ParserState::Escape;
                } else if (0x30..=0x7e).contains(&byte) {
                    self.state = ParserState::Ground;
                }
                None
            }
            ParserState::Csi => {
                if byte == 0x1b {
                    self.state = ParserState::Escape;
                } else if byte == 0x9c || (0x40..=0x7e).contains(&byte) {
                    self.state = ParserState::Ground;
                }
                None
            }
            ParserState::StringSequence => {
                if byte == 0x07 {
                    self.state = ParserState::Ground;
                } else if byte == 0x1b {
                    self.state = ParserState::StringEscape;
                } else if byte == 0x9c {
                    self.state = ParserState::Ground;
                }
                None
            }
            ParserState::StringEscape => {
                self.state = if byte == b'\\' || byte == 0x9c {
                    ParserState::Ground
                } else if byte == 0x1b {
                    ParserState::StringEscape
                } else {
                    ParserState::StringSequence
                };
                None
            }
        }
    }

    fn consume_ground(&mut self, byte: u8) -> Option<u8> {
        match byte {
            0x1b => {
                self.state = ParserState::Escape;
                None
            }
            0x9b => {
                self.state = ParserState::Csi;
                None
            }
            0x90 | 0x98 | 0x9d | 0x9e | 0x9f => {
                self.state = ParserState::StringSequence;
                None
            }
            0x9c => None,
            b'\r' => {
                self.skip_lf_after_cr = true;
                Some(b'\n')
            }
            b'\n' | b'\t' => Some(byte),
            0x20..=0x7e => Some(byte),
            0xc2..=0xf4 => {
                self.utf8_continuations = if byte <= 0xdf {
                    1
                } else if byte <= 0xef {
                    2
                } else {
                    3
                };
                Some(byte)
            }
            byte if byte >= 0x80 => Some(byte),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn matcher(patterns: &[(u64, &str)]) -> (Arc<LiveLogMatcher>, Arc<Mutex<Vec<u64>>>) {
        let matched = Arc::new(Mutex::new(Vec::new()));
        let received = Arc::clone(&matched);
        let observer = LiveLogMatcher::new(
            patterns
                .iter()
                .map(|(key, contains)| LogPattern {
                    key: *key,
                    contains: (*contains).to_string(),
                    attempt_id: None,
                })
                .collect(),
            Arc::new(AtomicBool::new(false)),
            move |key| received.lock().unwrap().push(key),
        )
        .expect("valid matcher");
        (observer, matched)
    }

    #[test]
    fn matches_across_chunks_and_latches_each_pattern() {
        let (matcher, matched) = matcher(&[(7, "ready")]);
        matcher.observe(b"rea");
        matcher.observe(b"dy");
        matcher.observe(b"ready");

        assert_eq!(*matched.lock().unwrap(), vec![7]);
    }

    #[test]
    fn strips_control_sequences_even_when_the_sequence_spans_chunks() {
        let (matcher, matched) = matcher(&[(7, "READY")]);
        matcher.observe(b"\x1b[3");
        matcher.observe(b"1mREADY\x1b[0m");

        assert_eq!(*matched.lock().unwrap(), vec![7]);
    }

    #[test]
    fn normalizes_carriage_return_updates_and_crlf() {
        let (first_matcher, first_matched) = matcher(&[(7, "progress\nready")]);
        first_matcher.observe(b"progress\r\nready");
        assert_eq!(*first_matched.lock().unwrap(), vec![7]);

        let (second_matcher, second_matched) = matcher(&[(8, "ready")]);
        second_matcher.observe(b"old\rready");
        assert_eq!(*second_matched.lock().unwrap(), vec![8]);
    }

    #[test]
    fn long_unterminated_output_does_not_prevent_a_later_match() {
        let (matcher, matched) = matcher(&[(7, "ready")]);
        matcher.observe(&vec![b'x'; 2 * 1024 * 1024]);
        matcher.observe(b"ready");
        assert_eq!(*matched.lock().unwrap(), vec![7]);
    }

    #[test]
    fn cancellation_ignores_late_output() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let matched = Arc::new(Mutex::new(Vec::new()));
        let received = Arc::clone(&matched);
        let matcher = LiveLogMatcher::new(
            vec![LogPattern {
                key: 7,
                contains: "ready".into(),
                attempt_id: None,
            }],
            Arc::clone(&cancelled),
            move |key| received.lock().unwrap().push(key),
        )
        .expect("valid matcher");
        cancelled.store(true, Ordering::Release);
        matcher.observe(b"ready");
        assert!(matched.lock().unwrap().is_empty());
    }
}
