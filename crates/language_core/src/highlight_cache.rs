use crate::grammar::Grammar;
use crate::highlight_map::{HighlightId, HighlightMap};
use collections::FxHasher;
use lru::LruCache;
use parking_lot::Mutex;
use smallvec::SmallVec;
use std::{
    fmt,
    hash::{Hash, Hasher as _},
    ops::Range,
    sync::Arc,
};

pub const MAX_TEXT_HIGHLIGHT_ENTRY_BYTES: usize = MAX_TEXT_HIGHLIGHT_CACHE_BYTES / 8;

const MAX_TEXT_HIGHLIGHT_CACHE_BYTES: usize = 4 * 1024 * 1024;
const MAX_CHUNK_HIGHLIGHT_CACHE_BYTES: usize = 10 * 1024 * 1024;
const APPROXIMATE_LRU_NODE_BYTES: usize = 4 * size_of::<usize>();

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TextHighlightKey {
    text_hash: u64,
    text_len: usize,
}

impl TextHighlightKey {
    pub fn new<'a>(text_chunks: impl Iterator<Item = &'a str>, text_len: usize) -> Self {
        let mut hasher = FxHasher::default();
        for chunk in text_chunks {
            hasher.write(chunk.as_bytes());
        }
        Self {
            text_hash: hasher.finish(),
            text_len,
        }
    }
}

#[derive(Clone, Default)]
pub struct ResolvedHighlights {
    pub sources: SmallVec<[(Arc<Grammar>, HighlightMap); 2]>,
    pub runs: Arc<[(Range<usize>, HighlightId)]>,
}

impl ResolvedHighlights {
    pub fn runs_in_range(
        self,
        range: Range<usize>,
    ) -> impl Iterator<Item = (Range<usize>, HighlightId)> {
        let runs = self.runs;
        (0..runs.len()).filter_map(move |index| {
            let (run_range, highlight_id) = &runs[index];
            (run_range.start < range.end && run_range.end > range.start).then(|| {
                (
                    run_range.start.max(range.start) - range.start
                        ..run_range.end.min(range.end) - range.start,
                    *highlight_id,
                )
            })
        })
    }

    pub fn is_current(&self) -> bool {
        self.sources
            .iter()
            .all(|(grammar, highlight_map)| grammar.highlight_map_matches(highlight_map))
    }

    pub fn cost_bytes(&self) -> usize {
        self.sources.len() * size_of::<(Arc<Grammar>, HighlightMap)>()
            + self.runs.len() * size_of::<(Range<usize>, HighlightId)>()
    }
}

impl fmt::Debug for ResolvedHighlights {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedHighlights")
            .field("runs", &self.runs)
            .finish_non_exhaustive()
    }
}

pub struct TextHighlightCache(Mutex<CostBudgetedLru<TextHighlightKey, TextHighlightEntry>>);

impl Default for TextHighlightCache {
    fn default() -> Self {
        Self(Mutex::new(CostBudgetedLru::new(
            MAX_TEXT_HIGHLIGHT_CACHE_BYTES,
            MAX_TEXT_HIGHLIGHT_ENTRY_BYTES,
        )))
    }
}

impl TextHighlightCache {
    pub fn get<'a>(
        &self,
        key: &TextHighlightKey,
        text_chunks: impl Iterator<Item = &'a str>,
    ) -> Option<ResolvedHighlights> {
        let mut cache = self.0.lock();
        let entry = cache.get(key)?;
        if !entry.highlights.is_current() {
            return None;
        }
        if !chunks_match_text(&entry.text, text_chunks) {
            return None;
        }
        Some(entry.highlights.clone())
    }

    pub fn insert(
        &self,
        key: TextHighlightKey,
        text: String,
        highlights: ResolvedHighlights,
    ) -> ResolvedHighlights {
        let cost = text.len() + highlights.cost_bytes();
        self.0.lock().insert(
            key,
            TextHighlightEntry {
                text,
                highlights: highlights.clone(),
            },
            cost,
        );
        highlights
    }
}

pub struct ChunkHighlightCache(Mutex<CostBudgetedLru<usize, ResolvedHighlights>>);

impl Default for ChunkHighlightCache {
    fn default() -> Self {
        Self(Mutex::new(CostBudgetedLru::new(
            MAX_CHUNK_HIGHLIGHT_CACHE_BYTES,
            MAX_CHUNK_HIGHLIGHT_CACHE_BYTES,
        )))
    }
}

impl fmt::Debug for ChunkHighlightCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChunkHighlightCache")
            .finish_non_exhaustive()
    }
}

impl ChunkHighlightCache {
    pub fn get(&self, chunk_id: usize) -> Option<ResolvedHighlights> {
        let mut lru = self.0.lock();
        let highlights = lru.get(&chunk_id)?;
        if !highlights.is_current() {
            return None;
        }
        Some(highlights.clone())
    }

    pub fn insert(&self, chunk_id: usize, highlights: ResolvedHighlights) {
        let cost = highlights.cost_bytes();
        self.0.lock().insert(chunk_id, highlights, cost);
    }

    pub fn clear(&mut self) {
        self.0.get_mut().clear();
    }
}

struct CostBudgetedLru<K: Hash + Eq, V> {
    entries: LruCache<K, (V, usize)>,
    total_cost: usize,
    max_total_cost: usize,
    max_entry_cost: usize,
}

impl<K: Hash + Eq, V> CostBudgetedLru<K, V> {
    const ENTRY_OVERHEAD_BYTES: usize =
        size_of::<K>() + size_of::<(V, usize)>() + APPROXIMATE_LRU_NODE_BYTES;

    fn new(max_total_cost: usize, max_entry_cost: usize) -> Self {
        Self {
            entries: LruCache::unbounded(),
            total_cost: 0,
            max_total_cost,
            max_entry_cost,
        }
    }

    fn get(&mut self, key: &K) -> Option<&V> {
        self.entries.get(key).map(|(value, _)| value)
    }

    fn insert(&mut self, key: K, value: V, cost: usize) {
        if cost > self.max_entry_cost {
            return;
        }
        let cost = cost + Self::ENTRY_OVERHEAD_BYTES;
        if let Some((_, old_cost)) = self.entries.put(key, (value, cost)) {
            self.total_cost -= old_cost;
        }
        self.total_cost += cost;
        while self.total_cost > self.max_total_cost {
            let Some((_, (_, evicted_cost))) = self.entries.pop_lru() else {
                break;
            };
            self.total_cost -= evicted_cost;
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.total_cost = 0;
    }
}

struct TextHighlightEntry {
    text: String,
    highlights: ResolvedHighlights,
}

fn chunks_match_text<'a>(text: &str, text_chunks: impl Iterator<Item = &'a str>) -> bool {
    let mut remaining = text;
    for chunk in text_chunks {
        let Some(rest) = remaining.strip_prefix(chunk) else {
            return false;
        };
        remaining = rest;
    }
    remaining.is_empty()
}

