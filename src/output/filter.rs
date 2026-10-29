//! VS Code-style Logs filter: comma-separated terms, `!term` excludes, and
//! a leading `\` makes the rest of a term literal. Include terms are OR'd,
//! any exclusion wins, and matching is a case-insensitive literal substring
//! of one retained line. Run markers and the history-limit notice always stay
//! visible.
//!
//! Like search, a UI turn examines at most 512 KiB of unscanned segments,
//! newest first. Unscanned segments contribute no lines until a later turn
//! reaches them, so the newest output is filtered first.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use super::segments::Segment;
use super::{LogLines, RetainedChunk, RetainedOutput, retained_size};
use crate::tui::PipeLine;

const FILTER_WORK_BYTES: usize = 512 * 1024;

/// One parsed filter. Terms are lower-cased once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LineFilter {
    include: Vec<String>,
    exclude: Vec<String>,
}

impl LineFilter {
    /// Parse filter text. Text without terms is no filter.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let mut filter = Self {
            include: Vec::new(),
            exclude: Vec::new(),
        };
        for term in text.split(',').map(str::trim) {
            let (list, term) = if let Some(literal) = term.strip_prefix('\\') {
                (&mut filter.include, literal)
            } else if let Some(excluded) = term.strip_prefix('!') {
                (&mut filter.exclude, excluded.trim())
            } else {
                (&mut filter.include, term)
            };
            if !term.is_empty() {
                list.push(term.to_lowercase());
            }
        }
        (!filter.include.is_empty() || !filter.exclude.is_empty()).then_some(filter)
    }

    pub(crate) fn matches(&self, line: &str) -> bool {
        let line = line.to_lowercase();
        !self.exclude.iter().any(|term| line.contains(term.as_str()))
            && (self.include.is_empty()
                || self.include.iter().any(|term| line.contains(term.as_str())))
    }
}

/// Per-segment filter results for one filter, keyed like the search cache.
/// A segment that is replaced or appended in place loses its weak pointer,
/// so a stale result is never reused.
#[derive(Clone, Debug, Default)]
pub(crate) struct FilterCache {
    filter: Option<LineFilter>,
    entries: HashMap<usize, Cached>,
}

#[derive(Clone, Debug)]
struct Cached {
    segment: Weak<Segment>,
    shown: ShownLines,
}

/// Shown line offsets inside one segment, and how many are output lines
/// rather than always-shown Run markers.
#[derive(Clone, Debug)]
struct ShownLines {
    offsets: Arc<[u32]>,
    matched: usize,
}

impl FilterCache {
    /// Project retained output through `filter`, scanning a bounded amount of
    /// new text.
    pub(crate) fn project(
        &mut self,
        output: &RetainedOutput,
        filter: &LineFilter,
    ) -> FilteredLines {
        if self.filter.as_ref() != Some(filter) {
            self.filter = Some(filter.clone());
            self.entries.clear();
        }
        let mut work = 0;
        let mut matched = 0;
        let mut pending = false;
        let mut segments = Vec::new();
        let starts: Vec<_> = output.chunks.segment_line_starts().collect();
        for (start, segment) in starts.into_iter().rev() {
            let id = Arc::as_ptr(segment) as usize;
            let cached = self.entries.get(&id).filter(|entry| {
                entry
                    .segment
                    .upgrade()
                    .is_some_and(|old| Arc::ptr_eq(&old, segment))
            });
            let shown = match cached {
                Some(entry) => entry.shown.clone(),
                None if work >= FILTER_WORK_BYTES => {
                    pending = true;
                    continue;
                }
                None => {
                    work += segment.records.iter().map(retained_size).sum::<usize>();
                    let shown = shown_lines(&segment.records, filter);
                    self.entries.insert(
                        id,
                        Cached {
                            segment: Arc::downgrade(segment),
                            shown: shown.clone(),
                        },
                    );
                    shown
                }
            };
            segments.push(ShownSegment {
                start,
                before: 0,
                lines: shown.offsets,
            });
            matched += shown.matched;
        }
        self.entries
            .retain(|_, entry| entry.segment.strong_count() != 0);
        segments.reverse();
        let mut shown = 0;
        for segment in &mut segments {
            segment.before = shown;
            shown += segment.lines.len();
        }
        FilteredLines {
            segments,
            shown,
            matched,
            pending,
        }
    }
}

fn shown_lines(records: &[RetainedChunk], filter: &LineFilter) -> ShownLines {
    let mut offsets = Vec::new();
    let mut matched = 0;
    let mut offset = 0u32;
    for record in records {
        match record {
            RetainedChunk::Marker { .. } => {
                offsets.push(offset);
                offset += 1;
            }
            RetainedChunk::Data { text, .. } => {
                for line in text.split_terminator('\n') {
                    if filter.matches(line) {
                        offsets.push(offset);
                        matched += 1;
                    }
                    offset += 1;
                }
            }
        }
    }
    ShownLines {
        offsets: offsets.into(),
        matched,
    }
}

/// The retained lines that pass one filter, as offsets into the unfiltered
/// display-line space. Building it never copies text.
#[derive(Clone, Debug, Default)]
pub(crate) struct FilteredLines {
    segments: Vec<ShownSegment>,
    shown: usize,
    matched: usize,
    pending: bool,
}

#[derive(Clone, Debug)]
struct ShownSegment {
    /// First retained line of the segment, excluding the history notice.
    start: usize,
    /// Shown lines in earlier segments.
    before: usize,
    /// Shown line offsets inside the segment.
    lines: Arc<[u32]>,
}

impl FilteredLines {
    /// Output lines that pass the filter. Run markers are not counted.
    pub(crate) fn matched(&self) -> usize {
        self.matched
    }

    /// Older segments are still waiting to be filtered.
    pub(crate) fn pending(&self) -> bool {
        self.pending
    }

    fn line_at(&self, shown: usize) -> Option<usize> {
        let index = self
            .segments
            .partition_point(|segment| segment.before + segment.lines.len() <= shown);
        let segment = self.segments.get(index)?;
        let offset = segment.lines.get(shown - segment.before)?;
        Some(segment.start + *offset as usize)
    }

    fn shown_at(&self, line: usize) -> Option<usize> {
        let index = self
            .segments
            .partition_point(|segment| segment.start <= line)
            .checked_sub(1)?;
        let segment = &self.segments[index];
        let offset = u32::try_from(line - segment.start).ok()?;
        let position = segment.lines.binary_search(&offset).ok()?;
        Some(segment.before + position)
    }
}

/// Retained output seen through an optional filter. Without one, every
/// retained line is shown. Positions count only shown lines; sources and
/// formatted text come from the retained output.
#[derive(Clone, Copy)]
pub(crate) struct FilteredOutput<'a> {
    pub(crate) output: &'a RetainedOutput,
    pub(crate) filtered: Option<&'a FilteredLines>,
}

impl FilteredOutput<'_> {
    fn notice(&self) -> usize {
        usize::from(self.output.truncated)
    }

    /// Map a shown position to an unfiltered display position.
    fn unfiltered(&self, position: usize) -> Option<usize> {
        let Some(filtered) = self.filtered else {
            return Some(position);
        };
        let notice = self.notice();
        if position < notice {
            return Some(position);
        }
        Some(filtered.line_at(position - notice)? + notice)
    }
}

impl LogLines for FilteredOutput<'_> {
    fn total_lines(&self) -> usize {
        match self.filtered {
            Some(filtered) => filtered.shown + self.notice(),
            None => self.output.total_lines(),
        }
    }

    fn truncated(&self) -> bool {
        self.output.truncated
    }

    fn display_range(&self, start: usize, end: usize) -> Vec<PipeLine> {
        (start..end)
            .filter_map(|position| self.output.display_line(self.unfiltered(position)?))
            .collect()
    }

    fn display_position(&self, source: Option<(u64, usize)>) -> Option<(usize, usize)> {
        let Some(filtered) = self.filtered else {
            return self.output.display_position(source);
        };
        let total = self.total_lines();
        let Some(source) = source else {
            return Some((0, total));
        };
        let (line, _) = self.output.display_position(Some(source))?;
        let notice = self.notice();
        Some((filtered.shown_at(line - notice)? + notice, total))
    }

    fn display_source_at(&self, position: usize) -> Option<(u64, usize)> {
        self.output.display_source_at(self.unfiltered(position)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::OutputViews;
    use crate::runtime::OutputStream;

    #[test]
    fn filter_terms_include_exclude_and_escape_like_vs_code() {
        let filter = LineFilter::parse(" Error, warn ,!retry").unwrap();
        assert!(filter.matches("ERROR request failed"));
        assert!(filter.matches("a warning"));
        assert!(!filter.matches("error: retry 3"), "an exclusion wins");
        assert!(!filter.matches("info"));

        let exclude_only = LineFilter::parse("!health").unwrap();
        assert!(exclude_only.matches("GET /users"));
        assert!(!exclude_only.matches("GET /HEALTH"));

        let literal = LineFilter::parse(r"\!important").unwrap();
        assert!(literal.matches("this is !important"));
        assert!(!literal.matches("important"));

        assert_eq!(LineFilter::parse(" , ,"), None);
        assert_eq!(LineFilter::parse("!"), None);
    }

    #[test]
    fn filtered_positions_round_trip_through_shown_lines_only() {
        let process = OutputViews::new(1).for_process(0).unwrap();
        process.append_at(
            1,
            OutputStream::Stdout,
            0,
            b"keep 1\ndrop\nkeep 2\ndrop\nkeep 3\n",
        );
        let output = process.snapshot();
        let filtered = FilterCache::default().project(&output, &LineFilter::parse("keep").unwrap());
        let view = FilteredOutput {
            output: &output,
            filtered: Some(&filtered),
        };

        let lines = view.display_lines(10);
        let texts: Vec<_> = lines
            .iter()
            .filter(|line| !line.marker)
            .map(|line| line.text.rsplit(": ").next().unwrap())
            .collect();
        assert_eq!(texts, ["keep 1", "keep 2", "keep 3"]);
        assert_eq!(view.total_lines(), lines.len());
        for (position, line) in lines.iter().enumerate() {
            assert_eq!(view.display_source_at(position), line.source);
            assert_eq!(
                view.display_position(line.source),
                Some((position, lines.len()))
            );
        }
        let dropped = output
            .display_lines(10)
            .into_iter()
            .find(|line| line.text.ends_with("drop"))
            .unwrap();
        assert_eq!(view.display_position(dropped.source), None);
    }

    #[test]
    fn hidden_lines_do_not_use_up_the_search_match_limit() {
        let process = OutputViews::new(1).for_process(0).unwrap();
        process.append(1, OutputStream::Stdout, b"needle shown\n");
        let hidden = "needle hidden\n".repeat(crate::output::SEARCH_MATCH_LIMIT + 10);
        process.append(1, OutputStream::Stdout, hidden.as_bytes());
        let output = process.snapshot();
        let filter = LineFilter::parse("shown").unwrap();

        let found =
            crate::output::SearchCache::default().search_shown(&output, "needle", Some(&filter));
        assert_eq!(found.matches.len(), 1);
        assert!(!found.limited);
    }

    #[test]
    fn filtering_large_history_is_bounded_and_finishes_newest_first() {
        let process = OutputViews::new(1).for_process(0).unwrap();
        process.append(1, OutputStream::Stdout, b"needle old\n");
        let line = format!("{}\n", "ordinary ".repeat(100));
        for _ in 0..3_000 {
            process.append(1, OutputStream::Stdout, line.as_bytes());
        }
        process.append(1, OutputStream::Stdout, b"needle new\n");
        let output = process.snapshot();
        let filter = LineFilter::parse("needle").unwrap();
        let mut cache = FilterCache::default();

        let first = cache.project(&output, &filter);
        assert!(first.pending());
        assert_eq!(first.matched(), 1, "the newest match is shown first");
        let mut last = first;
        for _ in 0..30 {
            last = cache.project(&output, &filter);
            if !last.pending() {
                break;
            }
        }
        assert!(!last.pending());
        assert_eq!(last.matched(), 2);
    }
}
