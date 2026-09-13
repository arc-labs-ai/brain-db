//! LRU cache for text → vector.
//!
//! Behaviour:
//!
//! - Key = `BLAKE3(text)[..16]`.
//! - Value = `(vector, fingerprint, inserted_at)`.
//! - Fingerprint mismatch on lookup → miss; stale entries are *not*
//!   auto-removed, they age out via LRU.
//! - LRU eviction via the `lru` crate.
//! - `cache_size = 0` → cache disabled, pure passthrough.
//! - Hit / miss / eviction counters as `AtomicU64` so observers don't
//!   contend the cache mutex.
//!
//! Wraps any [`Dispatcher`]. Implements `Dispatcher` itself, so
//! it composes cleanly: `CachingDispatcher<CpuDispatcher>` is what a
//! shard holds; ops see only the trait.
//!
//! `embed_batch` is a pure passthrough; the cache keys on single
//! "cue" texts (via `ENCODE` / `RECALL` / etc.). A cache-aware batch
//! implementation is a contained follow-up if it's ever wanted.

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use lru::LruCache;
use parking_lot::Mutex;

use crate::dispatcher::Dispatcher;
use crate::error::EmbedError;
use crate::fingerprint::blake3_hash_text;
use crate::model::VECTOR_DIM;

/// Default cache size.
pub const DEFAULT_CACHE_SIZE: usize = 10_000;

/// One LRU entry. The `inserted_at` field is reserved for future
/// migration / admin tooling; v1 reads only `vector` and `fingerprint`.
#[derive(Clone, Copy, Debug)]
struct CachedEmbedding {
    vector: [f32; VECTOR_DIM],
    fingerprint: [u8; 16],
    #[allow(dead_code)] // reserved for future migration / admin tooling
    inserted_at: Instant,
}

/// Snapshot of cache counters. Cheap to construct; read without
/// holding the cache mutex (counters are atomics).
#[derive(Debug, Default, Clone, Copy)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub size: u64,
}

impl CacheStats {
    /// `hits / (hits + misses)` — `None` when the cache has done no
    /// work (disabled cache or just-created instance).
    #[must_use]
    pub fn hit_rate(&self) -> Option<f64> {
        let total = self.hits + self.misses;
        if total == 0 {
            None
        } else {
            #[allow(clippy::cast_precision_loss)]
            Some(self.hits as f64 / total as f64)
        }
    }
}

/// LRU-cached wrapper around any [`Dispatcher`]. Generic so the inner
/// dispatcher's methods inline into the miss path.
pub struct CachingDispatcher<D: Dispatcher> {
    inner: D,
    /// `None` when `cache_size = 0` — cache disabled, pure passthrough.
    #[allow(clippy::type_complexity)]
    state: Option<Arc<Mutex<LruCache<[u8; 16], CachedEmbedding>>>>,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
}

impl<D: Dispatcher> CachingDispatcher<D> {
    /// Wrap `inner` with an LRU of capacity `cache_size`. A capacity
    /// of `0` disables the cache entirely.
    #[must_use]
    pub fn new(inner: D, cache_size: usize) -> Self {
        let state =
            NonZeroUsize::new(cache_size).map(|cap| Arc::new(Mutex::new(LruCache::new(cap))));
        Self {
            inner,
            state,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
        }
    }

    /// Wrap `inner` with the default capacity (10 000).
    #[must_use]
    pub fn with_default_size(inner: D) -> Self {
        Self::new(inner, DEFAULT_CACHE_SIZE)
    }

    /// Read the current counters. Atomic load; does not contend the
    /// cache mutex (size is computed under the lock).
    #[must_use]
    pub fn stats(&self) -> CacheStats {
        let size = self.state.as_ref().map_or(0, |s| s.lock().len() as u64);
        CacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            size,
        }
    }

    /// Drop every entry. Useful after a model migration or for tests.
    /// Has no effect when the cache is disabled.
    pub fn clear(&self) {
        if let Some(state) = &self.state {
            state.lock().clear();
        }
    }

    /// Borrow the wrapped inner dispatcher. Escape hatch for callers
    /// that need to bypass the cache (e.g. forced re-embed during
    /// migration).
    #[must_use]
    pub fn inner(&self) -> &D {
        &self.inner
    }
}

impl<D: Dispatcher> Dispatcher for CachingDispatcher<D> {
    fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        // Synchronous embed — no `.await` inside — so `.entered()` guards
        // are safe; nothing interleaves to steal the span.
        let _embed_span = tracing::info_span!("brain.embed").entered();
        // Disabled cache: pure passthrough.
        let Some(state) = &self.state else {
            return self.inner.embed(text);
        };

        let key = blake3_hash_text(text);
        let current_fp = self.inner.fingerprint();

        // Hit path. `peek` doesn't bump LRU — we promote only on
        // confirmed hits below.
        {
            let _cache_span = tracing::info_span!("brain.embedder.cache_lookup").entered();
            let mut guard = state.lock();
            if let Some(entry) = guard.peek(&key) {
                if entry.fingerprint == current_fp {
                    let vector = entry.vector;
                    // Bump to MRU on a real hit.
                    guard.get(&key);
                    drop(guard);
                    self.hits.fetch_add(1, Ordering::Relaxed);
                    return Ok(vector);
                }
                // Fingerprint mismatch: fall through as miss. Don't
                // delete — stale entries age out via LRU.
            }
        }

        // Miss path.
        self.misses.fetch_add(1, Ordering::Relaxed);
        let vector = self.inner.embed(text)?;
        let entry = CachedEmbedding {
            vector,
            fingerprint: current_fp,
            inserted_at: Instant::now(),
        };
        {
            let mut guard = state.lock();
            // `push` returns `Some((evicted_key, evicted_value))`
            // when a capacity-driven eviction happens. (`put` would
            // only return the old value on same-key overwrite — not
            // what we want to count here.)
            if let Some((evicted_key, _)) = guard.push(key, entry) {
                // Skip the "same-key overwrite" case: `push` returns
                // the old entry then too, but we don't want to count
                // it as an eviction.
                if evicted_key != key {
                    self.evictions.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        Ok(vector)
    }

    /// Pure passthrough; cache covers cues (single texts).
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        self.inner.embed_batch(texts)
    }

    fn fingerprint(&self) -> [u8; 16] {
        self.inner.fingerprint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    /// Mock dispatcher that counts calls and returns a deterministic
    /// vector per (text, fingerprint). Lets us assert exactly how
    /// many times the cache called through.
    struct CountingMock {
        fp: [u8; 16],
        calls: AtomicU64,
    }

    impl CountingMock {
        fn new(fp: [u8; 16]) -> Self {
            Self {
                fp,
                calls: AtomicU64::new(0),
            }
        }
        fn calls(&self) -> u64 {
            self.calls.load(Ordering::Relaxed)
        }
        fn vector_for(&self, text: &str) -> [f32; VECTOR_DIM] {
            let mut v = [0.0f32; VECTOR_DIM];
            // Encode the first 4 bytes of the text + first byte of
            // fp into the vector so different inputs are
            // distinguishable.
            let bytes = text.as_bytes();
            for (i, slot) in v.iter_mut().take(8).enumerate() {
                *slot = *bytes.get(i).unwrap_or(&0) as f32;
            }
            v[VECTOR_DIM - 1] = self.fp[0] as f32;
            v
        }
    }

    impl Dispatcher for CountingMock {
        fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(self.vector_for(text))
        }
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
            self.calls.fetch_add(texts.len() as u64, Ordering::Relaxed);
            Ok(texts.iter().map(|t| self.vector_for(t)).collect())
        }
        fn fingerprint(&self) -> [u8; 16] {
            self.fp
        }
    }

    fn fp(b: u8) -> [u8; 16] {
        [b; 16]
    }

    #[test]
    fn miss_then_hit_counts_one_inner_call() {
        let mock = CountingMock::new(fp(0x11));
        let cache = CachingDispatcher::new(mock, 10);

        let v1 = cache.embed("hello").unwrap();
        let v2 = cache.embed("hello").unwrap();
        assert_eq!(v1, v2);
        assert_eq!(cache.inner().calls(), 1, "second call must hit cache");

        let stats = cache.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.evictions, 0);
        assert_eq!(stats.size, 1);
        assert!((stats.hit_rate().unwrap() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn distinct_texts_all_miss() {
        let mock = CountingMock::new(fp(0x22));
        let cache = CachingDispatcher::new(mock, 10);
        for t in ["a", "b", "c", "d"] {
            cache.embed(t).unwrap();
        }
        assert_eq!(cache.inner().calls(), 4);
        assert_eq!(cache.stats().misses, 4);
        assert_eq!(cache.stats().hits, 0);
        assert_eq!(cache.stats().size, 4);
    }

    #[test]
    fn capacity_enforced_via_lru_eviction() {
        let mock = CountingMock::new(fp(0x33));
        let cache = CachingDispatcher::new(mock, 2);

        cache.embed("a").unwrap(); // [a]
        cache.embed("b").unwrap(); // [b, a]
        cache.embed("c").unwrap(); // [c, b]  — `a` evicted
        cache.embed("a").unwrap(); // miss again, [a, c] — `b` evicted

        let stats = cache.stats();
        assert_eq!(stats.size, 2, "cache must respect capacity");
        // 4 distinct keys all missed; the re-fetch of "a" also missed
        // because it had been evicted.
        assert_eq!(stats.misses, 4);
        assert_eq!(stats.hits, 0);
        assert!(stats.evictions >= 2, "evictions = {}", stats.evictions);
    }

    #[test]
    fn cache_size_zero_disables_entirely() {
        let mock = CountingMock::new(fp(0x44));
        let cache = CachingDispatcher::new(mock, 0);

        cache.embed("x").unwrap();
        cache.embed("x").unwrap();
        cache.embed("x").unwrap();

        // Every call must go to the inner dispatcher.
        assert_eq!(cache.inner().calls(), 3);
        let stats = cache.stats();
        assert_eq!(stats.hits, 0);
        assert_eq!(stats.misses, 0);
        assert_eq!(stats.evictions, 0);
        assert_eq!(stats.size, 0);
        assert!(stats.hit_rate().is_none());
    }

    #[test]
    fn fingerprint_change_makes_lookup_miss() {
        // Simulate a model change by swapping fingerprints between
        // the cache's `inner.fingerprint()` and the entry already in
        // the cache. We do this by populating the cache via mock A,
        // then wrapping mock B (same vectors-from-text but different
        // fp) around the *same* cache state — but easier: cache the
        // entry, then directly mutate the inner mock's fp.
        //
        // CountingMock holds its fp by value; we can't mutate it
        // mid-flight. Instead, construct two caches that share a
        // disambiguated text and check that a cache built around fp
        // 0x55 doesn't see fp 0x56's stored vector.
        //
        // The structural test we *can* run: after stale entry exists,
        // a re-embed under a different fp produces a miss + overwrite,
        // not a corrupted hit.
        let cache = CachingDispatcher::new(CountingMock::new(fp(0x55)), 4);
        let v_a = cache.embed("ping").unwrap();
        assert_eq!(cache.stats().hits, 0);
        assert_eq!(cache.stats().misses, 1);

        // Now hand the cache a different inner with a different fp.
        // We can't swap `inner`, so we test the inverse: pre-populate
        // cache A's slot with a stale fingerprint by hand.
        let cache_b = CachingDispatcher::new(CountingMock::new(fp(0x56)), 4);
        // Inject a stale entry directly via the state. (Test-only.)
        if let Some(state) = &cache_b.state {
            state.lock().put(
                blake3_hash_text("ping"),
                CachedEmbedding {
                    vector: v_a,
                    fingerprint: fp(0x55), // STALE
                    inserted_at: Instant::now(),
                },
            );
        }

        let v_b = cache_b.embed("ping").unwrap();
        // The stale entry must NOT be returned — it has a different fp.
        assert_ne!(v_a, v_b, "stale fp entry must not satisfy lookup");
        let stats = cache_b.stats();
        assert_eq!(stats.hits, 0, "fingerprint mismatch counts as miss");
        assert_eq!(stats.misses, 1);
        // The miss path overwrote the stale entry, so size stays 1.
        assert_eq!(stats.size, 1);
    }

    #[test]
    fn clear_drops_entries() {
        let cache = CachingDispatcher::new(CountingMock::new(fp(0x77)), 10);
        cache.embed("a").unwrap();
        cache.embed("b").unwrap();
        assert_eq!(cache.stats().size, 2);
        cache.clear();
        assert_eq!(cache.stats().size, 0);
        // Counters survive clear — they're observability, not state.
        let stats = cache.stats();
        assert_eq!(stats.misses, 2);
    }

    #[test]
    fn batch_is_passthrough_and_uncached() {
        let cache = CachingDispatcher::new(CountingMock::new(fp(0x88)), 10);
        let out = cache.embed_batch(&["a", "b", "c"]).unwrap();
        assert_eq!(out.len(), 3);
        // Batch is a passthrough; the cache should be untouched.
        assert_eq!(cache.stats().misses, 0);
        assert_eq!(cache.stats().hits, 0);
        assert_eq!(cache.stats().size, 0);
        // The mock should see 3 batched calls (passthrough).
        assert_eq!(cache.inner().calls(), 3);

        // A subsequent single embed for "a" must still miss — the
        // batch did NOT populate the cache.
        cache.embed("a").unwrap();
        assert_eq!(cache.stats().misses, 1);
        assert_eq!(cache.stats().hits, 0);
    }
}
