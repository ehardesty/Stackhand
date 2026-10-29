use super::{RetainedChunk, RetainedChunks, SEARCH_MATCH_LIMIT, sequence};
use crate::runtime::OutputStream;

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
    pub pending: bool,
}

/// One owned snapshot of a Process's retained Logs history.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RetainedOutput {
    pub chunks: RetainedChunks,
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
        let total = self.total_lines();
        self.display_range(total.saturating_sub(tail_limit), total)
    }

    pub fn display_window_from(
        &self,
        anchor: Option<(u64, usize)>,
        limit: usize,
    ) -> Option<Vec<crate::tui::PipeLine>> {
        let start = self.display_position(anchor)?.0;
        Some(self.display_range(start, start.saturating_add(limit).min(self.total_lines())))
    }

    fn display_range(&self, start: usize, end: usize) -> Vec<crate::tui::PipeLine> {
        (start..end)
            .filter_map(|position| {
                if self.truncated && position == 0 {
                    return Some(history_limit_line());
                }
                let (chunk, line) = self
                    .chunks
                    .at_line(position - usize::from(self.truncated))?;
                formatted_line(chunk, line)
            })
            .collect()
    }

    pub(crate) fn total_lines(&self) -> usize {
        self.chunks.total_lines() + usize::from(self.truncated)
    }

    pub(crate) fn display_position(&self, source: Option<(u64, usize)>) -> Option<(usize, usize)> {
        let position = match source {
            None => 0,
            Some(source) => self.chunks.position(source)? + usize::from(self.truncated),
        };
        Some((position, self.total_lines()))
    }

    pub(crate) fn display_source_at(&self, position: usize) -> Option<(u64, usize)> {
        let (chunk, line) = self
            .chunks
            .at_line(position.checked_sub(usize::from(self.truncated))?)?;
        Some((sequence(chunk), line))
    }

    pub(crate) fn scope(&self) -> String {
        let first = self.chunks.iter().next();
        let Some(first) = first else {
            return "no retained output".into();
        };
        let (run, observed) = match first {
            RetainedChunk::Marker {
                run_id,
                observed_at_ms,
                ..
            }
            | RetainedChunk::Data {
                run_id,
                observed_at_ms,
                ..
            } => (*run_id, *observed_at_ms),
        };
        let latest = self.latest_run.unwrap_or(run);
        let runs = if run == latest {
            format!("Run {run}")
        } else {
            format!("Runs {run}–{latest}")
        };
        format!(
            "{runs} · since {} UTC{}",
            timestamp(observed),
            if self.truncated {
                " · older output expired"
            } else {
                ""
            }
        )
    }

    /// Find case-sensitive literal substrings in all retained normalized text.
    /// Search uses the immutable snapshot, so ingestion never waits for it.
    pub fn search(&self, query: &str) -> LogSearch {
        if query.is_empty() {
            return LogSearch::default();
        }
        let mut found = LogSearch::default();
        for chunk in self.chunks.iter().rev() {
            search_chunk(chunk, query, &mut found);
            if found.limited {
                break;
            }
        }
        found
    }
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

fn formatted_line(chunk: &RetainedChunk, line: usize) -> Option<crate::tui::PipeLine> {
    use crate::tui::PipeLine;
    match chunk {
        RetainedChunk::Marker {
            label,
            sequence,
            observed_at_ms,
            ..
        } => (line == 0).then(|| PipeLine {
            text: format!("{} {label}", timestamp(*observed_at_ms)),
            marker: true,
            source: Some((*sequence, 0)),
            content_offset: 0,
            highlight: None,
            selection: None,
        }),
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
            let value = text.split_terminator('\n').nth(line)?;
            let prefix = format!("{} {label}: ", timestamp(*observed_at_ms));
            let content_offset = prefix.len();
            Some(PipeLine {
                text: format!("{prefix}{value}"),
                marker: false,
                source: Some((*sequence, line)),
                content_offset,
                highlight: None,
                selection: None,
            })
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

pub(super) fn search_chunk(chunk: &RetainedChunk, query: &str, found: &mut LogSearch) {
    let RetainedChunk::Data { text, sequence, .. } = chunk else {
        return;
    };
    let lines: Vec<_> = text.split_terminator('\n').enumerate().collect();
    for (line, value) in lines.into_iter().rev() {
        let matches: Vec<_> = value.match_indices(query).map(|(start, _)| start).collect();
        for start in matches.into_iter().rev() {
            if found.matches.len() == SEARCH_MATCH_LIMIT {
                found.limited = true;
                return;
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
