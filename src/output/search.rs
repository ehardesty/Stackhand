//! Incremental newest-first search. A UI turn examines at most 512 KiB of
//! uncached segments; cached results never keep retired text alive.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use super::segments::Segment;
use super::snapshot::search_chunk;
use super::{LogSearch, RetainedOutput, SEARCH_MATCH_LIMIT};

const SEARCH_WORK_BYTES: usize = 512 * 1024;

#[derive(Clone, Debug, Default)]
pub(crate) struct SearchCache {
    query: String,
    entries: HashMap<usize, Cached>,
}

#[derive(Clone, Debug)]
struct Cached {
    segment: Weak<Segment>,
    result: LogSearch,
}

impl SearchCache {
    pub(crate) fn search(&mut self, output: &RetainedOutput, query: &str) -> LogSearch {
        if self.query != query {
            self.query = query.to_owned();
            self.entries.clear();
        }
        if query.is_empty() {
            return LogSearch::default();
        }
        self.entries
            .retain(|_, entry| entry.segment.strong_count() != 0);
        let mut result = LogSearch::default();
        let mut work = 0;
        for segment in output.chunks.segments.iter().rev() {
            let id = Arc::as_ptr(segment) as usize;
            let cached = self.entries.get(&id).filter(|entry| {
                entry
                    .segment
                    .upgrade()
                    .is_some_and(|old| Arc::ptr_eq(&old, segment))
            });
            if cached.is_none() {
                if work >= SEARCH_WORK_BYTES {
                    result.pending = true;
                    break;
                }
                let mut found = LogSearch::default();
                for record in segment.records.iter().rev() {
                    work += super::retained_size(record);
                    search_chunk(record, query, &mut found);
                    if found.limited {
                        break;
                    }
                }
                self.entries.insert(
                    id,
                    Cached {
                        segment: Arc::downgrade(segment),
                        result: found,
                    },
                );
            }
            let cached = &self.entries[&id].result;
            let remaining = SEARCH_MATCH_LIMIT - result.matches.len();
            result.matches.extend(cached.matches.iter().take(remaining));
            if cached.limited || cached.matches.len() > remaining {
                result.limited = true;
                break;
            }
        }
        let mut keep = std::collections::HashSet::new();
        let mut count = 0;
        for segment in output.chunks.segments.iter().rev() {
            let id = Arc::as_ptr(segment) as usize;
            if count > SEARCH_MATCH_LIMIT {
                break;
            }
            if let Some(entry) = self.entries.get(&id) {
                count += entry.result.matches.len();
                keep.insert(id);
            }
        }
        self.entries.retain(|id, _| keep.contains(id));
        result
    }
}
