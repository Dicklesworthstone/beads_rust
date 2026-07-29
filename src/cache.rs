//! Pure cache policies for future high-RAM workspace acceleration.
//!
//! The S3-FIFO implementation here is deliberately storage-independent. It is
//! a bounded policy kernel that can be replayed against traces before any
//! storage read path depends on it.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

const DEFAULT_SMALL_QUEUE_PERCENT: u8 = 10;
const DEFAULT_GHOST_CAPACITY: usize = 1024;
const MAX_FREQUENCY: u8 = 3;

/// Configuration for a bounded S3-FIFO cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct S3FifoConfig {
    /// Whether the cache is allowed to store entries.
    pub enabled: bool,
    /// Maximum total entry weight retained in memory.
    pub max_weight: usize,
    /// Target percent of weight reserved for the recent-entry queue.
    pub small_queue_percent: u8,
    /// Maximum number of keys retained in the ghost queue.
    pub ghost_capacity: usize,
}

impl S3FifoConfig {
    /// Build an enabled cache configuration with conservative defaults.
    #[must_use]
    pub const fn new(max_weight: usize) -> Self {
        Self {
            enabled: true,
            max_weight,
            small_queue_percent: DEFAULT_SMALL_QUEUE_PERCENT,
            ghost_capacity: DEFAULT_GHOST_CAPACITY,
        }
    }

    /// Build an explicitly disabled cache configuration.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            max_weight: 0,
            small_queue_percent: DEFAULT_SMALL_QUEUE_PERCENT,
            ghost_capacity: 0,
        }
    }
}

impl Default for S3FifoConfig {
    fn default() -> Self {
        Self::disabled()
    }
}

/// Queue segment containing a cached entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum S3FifoSegment {
    /// Recent entries admitted without ghost evidence.
    Small,
    /// Entries with evidence of reuse.
    Main,
}

/// Result of an admission attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum S3FifoAdmission {
    /// The value was inserted as a new cache entry.
    Stored,
    /// The value replaced an existing cache entry.
    Replaced,
    /// The cache is disabled or has no usable memory budget.
    RejectedDisabled,
    /// The single value is larger than the entire cache budget.
    RejectedOversized,
}

/// S3-FIFO counters for replay decisions and fallback gates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct S3FifoStats {
    /// Successful lookups.
    pub hits: u64,
    /// Failed lookups.
    pub misses: u64,
    /// New entries accepted into the cache.
    pub admissions: u64,
    /// Admission attempts rejected by policy.
    pub admission_rejections: u64,
    /// Entries evicted from the resident cache.
    pub evictions: u64,
    /// Admissions that found the key in the ghost queue.
    pub ghost_hits: u64,
    /// Hits served from the small queue.
    pub small_hits: u64,
    /// Hits served from the main queue.
    pub main_hits: u64,
}

#[derive(Debug, Clone)]
struct CacheEntry<V> {
    value: V,
    weight: usize,
    segment: S3FifoSegment,
    frequency: u8,
}

/// Bounded S3-FIFO cache policy.
#[derive(Debug, Clone)]
pub struct S3FifoCache<K, V> {
    config: S3FifoConfig,
    entries: HashMap<K, CacheEntry<V>>,
    small: VecDeque<K>,
    main: VecDeque<K>,
    ghost: VecDeque<K>,
    current_weight: usize,
    small_weight: usize,
    main_weight: usize,
    stats: S3FifoStats,
}

impl<K, V> S3FifoCache<K, V>
where
    K: Clone + Eq + Hash,
{
    /// Build a cache from an explicit configuration.
    #[must_use]
    pub fn with_config(config: S3FifoConfig) -> Self {
        Self {
            config,
            entries: HashMap::new(),
            small: VecDeque::new(),
            main: VecDeque::new(),
            ghost: VecDeque::new(),
            current_weight: 0,
            small_weight: 0,
            main_weight: 0,
            stats: S3FifoStats::default(),
        }
    }

    /// Build an enabled cache with a maximum retained weight.
    #[must_use]
    pub fn new(max_weight: usize) -> Self {
        Self::with_config(S3FifoConfig::new(max_weight))
    }

    /// Return the active configuration.
    #[must_use]
    pub const fn config(&self) -> S3FifoConfig {
        self.config
    }

    /// Return a copy of the current counters.
    #[must_use]
    pub const fn stats(&self) -> S3FifoStats {
        self.stats
    }

    /// Number of resident entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the resident cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Total retained weight.
    #[must_use]
    pub const fn current_weight(&self) -> usize {
        self.current_weight
    }

    /// Return whether a key is resident.
    #[must_use]
    pub fn contains_key(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    /// Return whether a key is in the ghost queue.
    #[must_use]
    pub fn ghost_contains(&self, key: &K) -> bool {
        self.ghost.iter().any(|candidate| candidate == key)
    }

    /// Return the resident queue segment for a key.
    #[must_use]
    pub fn segment_of(&self, key: &K) -> Option<S3FifoSegment> {
        self.entries.get(key).map(|entry| entry.segment)
    }

    /// Look up a value and update hit/miss counters.
    pub fn get(&mut self, key: &K) -> Option<&V> {
        if !self.config.enabled {
            self.stats.misses += 1;
            return None;
        }

        let Some(segment) = self.entries.get(key).map(|entry| entry.segment) else {
            self.stats.misses += 1;
            return None;
        };

        self.stats.hits += 1;
        match segment {
            S3FifoSegment::Small => self.stats.small_hits += 1,
            S3FifoSegment::Main => self.stats.main_hits += 1,
        }

        let entry = self.entries.get_mut(key)?;
        entry.frequency = entry.frequency.saturating_add(1).min(MAX_FREQUENCY);
        Some(&entry.value)
    }

    /// Admit or replace a value with a caller-provided memory weight.
    #[allow(clippy::needless_pass_by_value)] // `key` clones into `insert_resident`
    // and is also borrowed by `remove`/`entries.get_mut`; restructuring the
    // function to take `&K` is a wider API change than this commit warrants.
    pub fn put(&mut self, key: K, value: V, weight: usize) -> S3FifoAdmission {
        if !self.config.enabled || self.config.max_weight == 0 {
            self.stats.admission_rejections += 1;
            return S3FifoAdmission::RejectedDisabled;
        }

        let weight = weight.max(1);
        if weight > self.config.max_weight {
            self.remove(&key);
            self.stats.admission_rejections += 1;
            return S3FifoAdmission::RejectedOversized;
        }

        let old_entry = self.entries.get(&key).map(|e| (e.segment, e.frequency));
        let replaced = self.remove(&key).is_some();

        let (segment, frequency) = if let Some((old_segment, old_frequency)) = old_entry {
            (old_segment, old_frequency)
        } else if self.remove_from_ghost(&key) {
            self.stats.ghost_hits += 1;
            (S3FifoSegment::Main, 0)
        } else {
            (S3FifoSegment::Small, 0)
        };

        self.evict_to_fit(weight);
        self.insert_resident(key.clone(), value, weight, segment);
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.frequency = frequency;
        }

        self.stats.admissions += 1;
        if replaced {
            S3FifoAdmission::Replaced
        } else {
            S3FifoAdmission::Stored
        }
    }

    /// Remove a resident value.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        let entry = self.entries.remove(key)?;
        self.current_weight = self.current_weight.saturating_sub(entry.weight);
        match entry.segment {
            S3FifoSegment::Small => {
                self.small_weight = self.small_weight.saturating_sub(entry.weight);
            }
            S3FifoSegment::Main => {
                self.main_weight = self.main_weight.saturating_sub(entry.weight);
            }
        }
        self.remove_resident_queue_refs(key);
        Some(entry.value)
    }

    fn remove_resident_queue_refs(&mut self, key: &K) {
        self.small.retain(|candidate| candidate != key);
        self.main.retain(|candidate| candidate != key);
    }

    fn insert_resident(&mut self, key: K, value: V, weight: usize, segment: S3FifoSegment) {
        let entry = CacheEntry {
            value,
            weight,
            segment,
            frequency: 0,
        };
        match segment {
            S3FifoSegment::Small => {
                self.small.push_back(key.clone());
                self.small_weight += weight;
            }
            S3FifoSegment::Main => {
                self.main.push_back(key.clone());
                self.main_weight += weight;
            }
        }
        self.current_weight += weight;
        self.entries.insert(key, entry);
    }

    fn evict_to_fit(&mut self, incoming_weight: usize) {
        while self.current_weight.saturating_add(incoming_weight) > self.config.max_weight {
            let prefer_small = self.small_weight > 0
                && (self.small_weight >= self.small_weight_target() || self.main_weight == 0);
            let evicted = if prefer_small {
                self.evict_small_once()
            } else {
                self.evict_main_once()
            };
            if !evicted {
                break;
            }
        }
    }

    fn evict_small_once(&mut self) -> bool {
        while let Some(key) = self.small.pop_front() {
            let Some(entry) = self.entries.get_mut(&key) else {
                continue;
            };
            if entry.segment != S3FifoSegment::Small {
                continue;
            }

            if entry.frequency > 0 {
                entry.frequency -= 1;
                entry.segment = S3FifoSegment::Main;
                self.small_weight = self.small_weight.saturating_sub(entry.weight);
                self.main_weight += entry.weight;
                self.main.push_back(key);
                return true;
            }

            let Some(removed) = self.entries.remove(&key) else {
                continue;
            };
            self.current_weight = self.current_weight.saturating_sub(removed.weight);
            self.small_weight = self.small_weight.saturating_sub(removed.weight);
            self.stats.evictions += 1;
            self.add_ghost(key);
            return true;
        }
        false
    }

    fn evict_main_once(&mut self) -> bool {
        while let Some(key) = self.main.pop_front() {
            let Some(entry) = self.entries.get_mut(&key) else {
                continue;
            };
            if entry.segment != S3FifoSegment::Main {
                continue;
            }

            if entry.frequency > 0 {
                entry.frequency -= 1;
                self.main.push_back(key);
                return true;
            }

            let Some(removed) = self.entries.remove(&key) else {
                continue;
            };
            self.current_weight = self.current_weight.saturating_sub(removed.weight);
            self.main_weight = self.main_weight.saturating_sub(removed.weight);
            self.stats.evictions += 1;
            return true;
        }
        false
    }

    fn small_weight_target(&self) -> usize {
        let percent = usize::from(self.config.small_queue_percent.clamp(1, 100));
        let target = self.config.max_weight.saturating_mul(percent) / 100;
        target.max(1).min(self.config.max_weight)
    }

    fn add_ghost(&mut self, key: K) {
        if self.config.ghost_capacity == 0 {
            return;
        }
        self.ghost.retain(|candidate| candidate != &key);
        self.ghost.push_back(key);
        while self.ghost.len() > self.config.ghost_capacity {
            self.ghost.pop_front();
        }
    }

    fn remove_from_ghost(&mut self, key: &K) -> bool {
        let original_len = self.ghost.len();
        self.ghost.retain(|candidate| candidate != key);
        self.ghost.len() != original_len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct ReplayReport {
        direct_storage_reads: u64,
        cache_storage_reads: u64,
        hits: u64,
        misses: u64,
        evictions: u64,
        ghost_hits: u64,
        max_weight_seen: usize,
        final_weight: usize,
    }

    impl ReplayReport {
        fn saved_storage_reads(&self) -> u64 {
            self.direct_storage_reads
                .saturating_sub(self.cache_storage_reads)
        }

        const fn candidate_beats_direct_reads(&self) -> bool {
            self.cache_storage_reads < self.direct_storage_reads
        }
    }

    fn replay_accesses(config: S3FifoConfig, accesses: &[usize]) -> ReplayReport {
        let mut cache = S3FifoCache::with_config(config);
        let mut max_weight_seen = 0;

        for &key in accesses {
            if cache.get(&key).is_none() {
                _ = cache.put(key, key, 1);
            }
            max_weight_seen = max_weight_seen.max(cache.current_weight());
        }

        let stats = cache.stats();
        ReplayReport {
            direct_storage_reads: u64::try_from(accesses.len()).unwrap_or(u64::MAX),
            cache_storage_reads: stats.misses,
            hits: stats.hits,
            misses: stats.misses,
            evictions: stats.evictions,
            ghost_hits: stats.ghost_hits,
            max_weight_seen,
            final_weight: cache.current_weight(),
        }
    }

    #[test]
    fn disabled_cache_records_misses_without_storing() {
        let mut cache = S3FifoCache::with_config(S3FifoConfig::disabled());

        assert_eq!(
            cache.put("issue-a", "payload", 1),
            S3FifoAdmission::RejectedDisabled
        );
        assert_eq!(cache.get(&"issue-a"), None);
        assert!(cache.is_empty());
        assert_eq!(cache.current_weight(), 0);
        assert_eq!(cache.stats().misses, 1);
        assert_eq!(cache.stats().admission_rejections, 1);
    }

    #[test]
    fn hits_and_misses_update_counters() {
        let mut cache = S3FifoCache::new(4);

        assert_eq!(cache.put("issue-a", 11, 1), S3FifoAdmission::Stored);
        assert_eq!(cache.get(&"issue-a"), Some(&11));
        assert_eq!(cache.get(&"missing"), None);

        let stats = cache.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.small_hits, 1);
        assert_eq!(stats.main_hits, 0);
    }

    #[test]
    fn oversized_items_are_rejected_and_do_not_break_memory_cap() {
        let mut cache = S3FifoCache::new(2);

        assert_eq!(cache.put("small", "ok", 1), S3FifoAdmission::Stored);
        assert_eq!(
            cache.put("huge", "nope", 3),
            S3FifoAdmission::RejectedOversized
        );

        assert!(cache.contains_key(&"small"));
        assert!(!cache.contains_key(&"huge"));
        assert!(cache.current_weight() <= cache.config().max_weight);
        assert_eq!(cache.stats().admission_rejections, 1);
    }

    #[test]
    fn evicted_small_entries_leave_ghost_evidence_for_main_admission() {
        let mut cache = S3FifoCache::with_config(S3FifoConfig {
            max_weight: 2,
            ghost_capacity: 4,
            ..S3FifoConfig::new(2)
        });

        assert_eq!(cache.put("a", "A", 1), S3FifoAdmission::Stored);
        assert_eq!(cache.put("b", "B", 1), S3FifoAdmission::Stored);
        assert_eq!(cache.put("c", "C", 1), S3FifoAdmission::Stored);

        assert!(!cache.contains_key(&"a"));
        assert!(cache.ghost_contains(&"a"));
        assert_eq!(cache.put("a", "A2", 1), S3FifoAdmission::Stored);

        assert_eq!(cache.segment_of(&"a"), Some(S3FifoSegment::Main));
        assert_eq!(cache.get(&"a"), Some(&"A2"));
        assert_eq!(cache.stats().ghost_hits, 1);
        assert!(cache.current_weight() <= cache.config().max_weight);
    }

    #[test]
    fn ghost_queue_respects_configured_capacity() {
        let mut cache = S3FifoCache::with_config(S3FifoConfig {
            max_weight: 1,
            ghost_capacity: 2,
            ..S3FifoConfig::new(1)
        });

        assert_eq!(cache.put("a", "A", 1), S3FifoAdmission::Stored);
        assert_eq!(cache.put("b", "B", 1), S3FifoAdmission::Stored);
        assert_eq!(cache.put("c", "C", 1), S3FifoAdmission::Stored);
        assert_eq!(cache.put("d", "D", 1), S3FifoAdmission::Stored);

        assert!(!cache.ghost_contains(&"a"));
        assert!(cache.ghost_contains(&"b"));
        assert!(cache.ghost_contains(&"c"));
        assert!(cache.contains_key(&"d"));
        assert!(cache.current_weight() <= cache.config().max_weight);
    }

    #[test]
    fn hit_small_entry_is_promoted_before_cold_neighbor() {
        let mut cache = S3FifoCache::new(2);

        assert_eq!(cache.put("hot", "H", 1), S3FifoAdmission::Stored);
        assert_eq!(cache.get(&"hot"), Some(&"H"));
        assert_eq!(cache.put("cold", "C", 1), S3FifoAdmission::Stored);
        assert_eq!(cache.put("new", "N", 1), S3FifoAdmission::Stored);

        assert!(cache.contains_key(&"hot"));
        assert!(!cache.contains_key(&"cold"));
        assert_eq!(cache.segment_of(&"hot"), Some(S3FifoSegment::Main));
        assert!(cache.current_weight() <= cache.config().max_weight);
    }

    #[test]
    fn repeated_admissions_stay_within_memory_cap() {
        let mut cache = S3FifoCache::new(8);

        for index in 0..32 {
            assert_eq!(cache.put(index, index * 10, 1), S3FifoAdmission::Stored);
            assert!(cache.current_weight() <= cache.config().max_weight);
        }

        assert_eq!(cache.len(), 8);
        assert_eq!(cache.current_weight(), 8);
        assert!(cache.stats().evictions > 0);
    }

    #[test]
    fn replacing_resident_entry_removes_stale_queue_slots() {
        let mut cache = S3FifoCache::new(2);

        assert_eq!(cache.put("a", "A1", 1), S3FifoAdmission::Stored);
        assert_eq!(cache.put("b", "B", 1), S3FifoAdmission::Stored);
        assert_eq!(cache.put("a", "A2", 1), S3FifoAdmission::Replaced);
        assert_eq!(cache.put("c", "C", 1), S3FifoAdmission::Stored);

        assert_eq!(cache.get(&"a"), Some(&"A2"));
        assert!(!cache.contains_key(&"b"));
        assert_eq!(cache.get(&"c"), Some(&"C"));
        assert!(cache.current_weight() <= cache.config().max_weight);
    }

    #[test]
    fn zipf_like_replay_beats_direct_reads() {
        let mut accesses = Vec::new();
        for cold_key in 10..42 {
            accesses.extend([0; 8]);
            accesses.extend([1; 4]);
            accesses.extend([2; 2]);
            accesses.push(cold_key);
        }

        let report = replay_accesses(S3FifoConfig::new(4), &accesses);

        assert!(report.candidate_beats_direct_reads());
        assert!(report.hits > report.misses);
        assert!(report.saved_storage_reads() > report.direct_storage_reads / 2);
        assert!(report.max_weight_seen <= 4);
    }

    #[test]
    fn bursty_replay_preserves_reused_entries_after_scan_gap() {
        let mut accesses = vec![42; 6];
        accesses.extend(100..132);
        accesses.extend([42; 6]);

        let report = replay_accesses(S3FifoConfig::new(4), &accesses);

        assert!(report.candidate_beats_direct_reads());
        assert!(report.hits >= 10);
        assert!(report.evictions > 0);
        assert!(report.max_weight_seen <= 4);
    }

    #[test]
    fn scan_heavy_replay_rejects_candidate_with_evidence() {
        let accesses: Vec<_> = (0..64).collect();

        let report = replay_accesses(S3FifoConfig::new(8), &accesses);

        assert!(!report.candidate_beats_direct_reads());
        assert_eq!(report.cache_storage_reads, report.direct_storage_reads);
        assert_eq!(report.hits, 0);
        assert!(report.evictions > 0);
        assert!(report.max_weight_seen <= 8);
    }

    #[test]
    fn disabled_replay_matches_direct_reads_without_residency() {
        let accesses = [0, 1, 0, 2, 0, 3, 0, 4];

        let report = replay_accesses(S3FifoConfig::disabled(), &accesses);

        assert_eq!(report.cache_storage_reads, report.direct_storage_reads);
        assert_eq!(report.hits, 0);
        assert_eq!(report.ghost_hits, 0);
        assert_eq!(report.final_weight, 0);
    }
}
