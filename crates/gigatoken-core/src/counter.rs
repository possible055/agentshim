use std::{cmp::Reverse, collections::BinaryHeap, mem::size_of, sync::Arc};

use rustc_hash::{FxBuildHasher, FxHashMap};

use crate::{
    model::{LoadError, Model},
    pretokenizer::O200kPretokens,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CountUpTo {
    Exact(usize),
    Exceeded,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CounterLimits {
    pub short_cache_slots: usize,
    pub long_cache_entries: usize,
    pub long_cache_bytes: usize,
    pub retained_scratch_tokens: usize,
    pub cancellation_stride: usize,
}

impl Default for CounterLimits {
    fn default() -> Self {
        Self {
            short_cache_slots: 4_096,
            long_cache_entries: 256,
            long_cache_bytes: 64 * 1024,
            retained_scratch_tokens: 64 * 1024,
            cancellation_stride: 256,
        }
    }
}

impl CounterLimits {
    pub(crate) fn validate(self) -> Result<(), LoadError> {
        if self.short_cache_slots == 0 || !self.short_cache_slots.is_power_of_two() {
            return Err(LoadError::InvalidShortCacheSlots);
        }
        if self.long_cache_entries == 0
            || self.long_cache_bytes == 0
            || self.retained_scratch_tokens == 0
            || self.cancellation_stride == 0
        {
            return Err(LoadError::InvalidCounterLimits);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CounterMetrics {
    pub short_hits: u64,
    pub long_hits: u64,
    pub misses: u64,
    pub uncached: u64,
    pub resets: u64,
    pub early_exits: u64,
    pub cancellations: u64,
    pub resident_bytes: usize,
}

#[derive(Clone, Copy, Default)]
struct ShortEntry {
    key: u128,
    count: u32,
}

#[derive(Clone, Copy)]
struct Node {
    token: u32,
    previous: usize,
    next: usize,
    alive: bool,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Candidate {
    rank: u32,
    left: usize,
    right: usize,
}

/// Count-only worker with independent bounded caches and retained scratch.
pub struct O200kCounter {
    model: Arc<Model>,
    limits: CounterLimits,
    short_cache: Box<[ShortEntry]>,
    long_cache: FxHashMap<Box<[u8]>, u32>,
    long_cache_bytes: usize,
    nodes: Vec<Node>,
    candidates: BinaryHeap<Reverse<Candidate>>,
    metrics: CounterMetrics,
}

impl O200kCounter {
    pub(crate) fn new(model: Arc<Model>, limits: CounterLimits) -> Self {
        Self {
            model,
            limits,
            short_cache: vec![ShortEntry::default(); limits.short_cache_slots].into_boxed_slice(),
            long_cache: FxHashMap::with_capacity_and_hasher(
                limits.long_cache_entries,
                FxBuildHasher,
            ),
            long_cache_bytes: 0,
            nodes: Vec::new(),
            candidates: BinaryHeap::new(),
            metrics: CounterMetrics::default(),
        }
    }

    pub fn count_ordinary_up_to(
        &mut self,
        text: &str,
        limit: usize,
        mut cancelled: impl FnMut() -> bool,
    ) -> CountUpTo {
        let mut total = 0_usize;
        let mut cancellation_work = 0_usize;
        for pretoken in O200kPretokens::new(text) {
            if cancellation_work == 0 && cancelled() {
                self.metrics.cancellations = self.metrics.cancellations.saturating_add(1);
                return CountUpTo::Cancelled;
            }
            cancellation_work += 1;
            if cancellation_work == self.limits.cancellation_stride {
                cancellation_work = 0;
            }
            let minimum = pretoken.len().div_ceil(self.model.max_token_bytes);
            if total.checked_add(minimum).is_none_or(|count| count > limit) {
                self.metrics.early_exits = self.metrics.early_exits.saturating_add(1);
                return CountUpTo::Exceeded;
            }
            let count = if let Some(count) = self.cached_count(pretoken) {
                count
            } else {
                self.metrics.misses = self.metrics.misses.saturating_add(1);
                let Some(count) = self.count_pretoken(pretoken, &mut cancelled) else {
                    self.metrics.cancellations = self.metrics.cancellations.saturating_add(1);
                    self.release_oversized_scratch();
                    return CountUpTo::Cancelled;
                };
                self.insert_cache(pretoken, count);
                count
            };
            let Some(next) = total.checked_add(count) else {
                self.metrics.early_exits = self.metrics.early_exits.saturating_add(1);
                return CountUpTo::Exceeded;
            };
            if next > limit {
                self.metrics.early_exits = self.metrics.early_exits.saturating_add(1);
                return CountUpTo::Exceeded;
            }
            total = next;
        }
        CountUpTo::Exact(total)
    }

    #[must_use]
    pub fn metrics(&self) -> CounterMetrics {
        let mut metrics = self.metrics;
        metrics.resident_bytes = self.resident_bytes();
        metrics
    }

    fn resident_bytes(&self) -> usize {
        self.short_cache
            .len()
            .saturating_mul(size_of::<ShortEntry>())
            + self
                .long_cache
                .capacity()
                .saturating_mul(size_of::<(Box<[u8]>, u32)>())
            + self.long_cache_bytes
            + self.nodes.capacity().saturating_mul(size_of::<Node>())
            + self
                .candidates
                .capacity()
                .saturating_mul(size_of::<Candidate>())
    }

    fn cached_count(&mut self, pretoken: &[u8]) -> Option<usize> {
        if let Some(key) = pack_short(pretoken) {
            let index = short_index(key, self.short_cache.len());
            let entry = self.short_cache[index];
            if entry.key == key {
                self.metrics.short_hits = self.metrics.short_hits.saturating_add(1);
                return Some(entry.count as usize);
            }
            return None;
        }
        let count = self
            .long_cache
            .get(pretoken)
            .copied()
            .map(|count| count as usize);
        if count.is_some() {
            self.metrics.long_hits = self.metrics.long_hits.saturating_add(1);
        }
        count
    }

    fn insert_cache(&mut self, pretoken: &[u8], count: usize) {
        let Ok(count) = u32::try_from(count) else {
            self.metrics.uncached = self.metrics.uncached.saturating_add(1);
            return;
        };
        if let Some(key) = pack_short(pretoken) {
            let index = short_index(key, self.short_cache.len());
            self.short_cache[index] = ShortEntry { key, count };
            return;
        }
        if pretoken.len() > self.limits.long_cache_bytes {
            self.metrics.uncached = self.metrics.uncached.saturating_add(1);
            return;
        }
        if self.long_cache.len() == self.limits.long_cache_entries
            || self
                .long_cache_bytes
                .checked_add(pretoken.len())
                .is_none_or(|bytes| bytes > self.limits.long_cache_bytes)
        {
            self.long_cache.clear();
            self.long_cache_bytes = 0;
            self.metrics.resets = self.metrics.resets.saturating_add(1);
        }
        self.long_cache_bytes += pretoken.len();
        self.long_cache.insert(pretoken.into(), count);
    }

    fn count_pretoken(
        &mut self,
        pretoken: &[u8],
        cancelled: &mut impl FnMut() -> bool,
    ) -> Option<usize> {
        let byte_count = pretoken.len();
        self.nodes.clear();
        self.candidates.clear();
        self.nodes
            .reserve(byte_count.saturating_sub(self.nodes.capacity()));
        for (index, byte) in pretoken.iter().enumerate() {
            self.nodes.push(Node {
                token: self.model.byte_ranks[usize::from(*byte)],
                previous: index.checked_sub(1).unwrap_or(usize::MAX),
                next: if index + 1 < byte_count {
                    index + 1
                } else {
                    usize::MAX
                },
                alive: true,
            });
        }
        for left in 0..byte_count.saturating_sub(1) {
            self.push_candidate(left);
        }

        let mut count = byte_count;
        let mut work = 0_usize;
        while let Some(Reverse(candidate)) = self.candidates.pop() {
            if work % self.limits.cancellation_stride == 0 && cancelled() {
                return None;
            }
            work = work.saturating_add(1);
            if !self.candidate_is_current(candidate) {
                continue;
            }
            let previous = self.nodes[candidate.left].previous;
            let next = self.nodes[candidate.right].next;
            self.nodes[candidate.left].token = candidate.rank;
            self.nodes[candidate.left].next = next;
            self.nodes[candidate.right].alive = false;
            if next != usize::MAX {
                self.nodes[next].previous = candidate.left;
            }
            count -= 1;
            if previous != usize::MAX {
                self.push_candidate(previous);
            }
            self.push_candidate(candidate.left);
        }
        self.release_oversized_scratch();
        Some(count)
    }

    fn push_candidate(&mut self, left: usize) {
        if !self.nodes[left].alive {
            return;
        }
        let right = self.nodes[left].next;
        if right == usize::MAX || !self.nodes[right].alive {
            return;
        }
        if let Some(&rank) = self
            .model
            .merges
            .get(&(self.nodes[left].token, self.nodes[right].token))
        {
            self.candidates
                .push(Reverse(Candidate { rank, left, right }));
        }
    }

    fn candidate_is_current(&self, candidate: Candidate) -> bool {
        self.nodes[candidate.left].alive
            && self.nodes[candidate.right].alive
            && self.nodes[candidate.left].next == candidate.right
            && self.model.merges.get(&(
                self.nodes[candidate.left].token,
                self.nodes[candidate.right].token,
            )) == Some(&candidate.rank)
    }

    fn release_oversized_scratch(&mut self) {
        if self.nodes.capacity() > self.limits.retained_scratch_tokens {
            self.nodes = Vec::new();
            self.metrics.uncached = self.metrics.uncached.saturating_add(1);
        }
        let candidate_limit = self.limits.retained_scratch_tokens.saturating_mul(3);
        if self.candidates.capacity() > candidate_limit {
            self.candidates = BinaryHeap::new();
            self.metrics.uncached = self.metrics.uncached.saturating_add(1);
        }
    }
}

fn pack_short(bytes: &[u8]) -> Option<u128> {
    if bytes.is_empty() || bytes.len() > 15 {
        return None;
    }
    let mut packed = [0_u8; 16];
    packed[..bytes.len()].copy_from_slice(bytes);
    packed[15] = u8::try_from(bytes.len()).expect("short pretoken length fits u8");
    Some(u128::from_le_bytes(packed))
}

fn short_index(key: u128, slots: usize) -> usize {
    let bytes = key.to_le_bytes();
    let low = u64::from_le_bytes(bytes[..8].try_into().expect("slice has eight bytes"));
    let high = u64::from_le_bytes(bytes[8..].try_into().expect("slice has eight bytes"));
    let mut hash = (low ^ high.rotate_right(25)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    hash ^= hash >> 32;
    let mask = u64::try_from(slots - 1).expect("cache slot mask fits u64");
    usize::try_from(hash & mask).expect("masked cache index fits usize")
}
