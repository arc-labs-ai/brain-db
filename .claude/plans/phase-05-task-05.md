# Sub-task 5.5 — LRU cache (text → vector)

Wraps any [`Dispatcher`] (from 5.4) with an LRU cache keyed by BLAKE3-16(text). Cache-hit path is < 1 µs; cache-miss path delegates to the wrapped dispatcher (typically `CpuDispatcher`, eventually `GpuDispatcher`).

Implements spec `04_embedding_layer/05_caching.md` faithfully:
- Key = 16-byte BLAKE3 of text bytes.
- Value = `(vector, fingerprint, inserted_at)`.
- Fingerprint mismatch on lookup → treat as miss (no auto-clean; LRU ages stale entries).
- LRU eviction via the `lru` crate.
- `cache_size = 0` → cache disabled, pure passthrough (spec §13).
- Hit / miss / eviction counters for observability (spec §10).

## 0. Spec grounding

| Spec | Says |
|---|---|
| §04/05 §1 | `CueCache { map: LruCache<TextHash, CachedEmbedding> }`; `CachedEmbedding { vector, fingerprint, inserted_at }` |
| §04/05 §2 | Key = `BLAKE3(text)[..16]`. Collision prob @ 10⁶ entries ≈ 10⁻¹⁹ — acceptable |
| §04/05 §3 | On lookup: fingerprint mismatch → discard, treat as miss |
| §04/05 §5 | Default capacity = **10 000 entries** |
| §04/05 §6 | LRU only; LFU / 2Q / ARC rejected |
| §04/05 §7 | Use the `lru` crate; "per-shard, not global" |
| §04/05 §8 | Stale entries are *not* auto-removed; they age out |
| §04/05 §10 | Stats: hits / misses / evictions / size / hit_rate |
| §04/05 §11 | Cache covers cues only — single texts. Tokenisations and arena vectors not cached |
| §04/05 §13 | `cache_size = 0` disables caching entirely |

The "per-shard, not global" line in §7 plus our `Dispatcher: Send + Sync` contract means: in v1 the `CachingDispatcher` is `Send + Sync` (so it can sit behind an `Arc` like any other dispatcher), with synchronisation owned internally. When per-shard Glommio executors arrive in Phase 9, each shard gets its own `CachingDispatcher` instance — the trait surface doesn't change.

## 1. Scope

**In scope for 5.5:**
- `CachingDispatcher<D>` generic over any `D: Dispatcher`. Implements `Dispatcher` itself, composable.
- LRU keyed by `[u8; 16]` (BLAKE3-16 of text bytes), value = `CachedEmbedding { vector, fingerprint, inserted_at }`.
- `cache_size: usize` knob; `0` → pure passthrough.
- Stats: `CacheStats { hits, misses, evictions, size }` — atomic counters readable without locking.
- `blake3_hash_text(&str) -> [u8; 16]` pure helper in `fingerprint.rs` (companion to existing `blake3_hash_file`).
- Pure-Rust unit tests using a `CountingMockDispatcher` (extending the mock pattern proven in 5.4): hit/miss accounting, capacity + eviction, fingerprint-mismatch as miss, `cache_size = 0` disables, stats accessor.
- Integration test gated on `BRAIN_EMBED_MODEL_DIR`: real `CpuDispatcher` wrapped; same text twice → second call returns bit-identical vector (sanity, not just cosine); both vectors unit-norm.

**NOT in scope (later sub-tasks / phases):**
- Async / Glommio integration — Phase 9.
- `ADMIN_STATS` opcode — Phase 9 server.
- Cache invalidation hooks for migration — Phase 8/9.
- `embed_batch` cache-aware split (hits + misses combined). For v1, `embed_batch` is a pure passthrough; see §3.7 for why.
- Per-shard cache instances — that's a runtime composition decision, not a 5.5 deliverable.

## 2. Module surface

```rust
// crates/brain-embed/src/cache.rs

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use lru::LruCache;
use parking_lot::Mutex;

use crate::dispatcher::Dispatcher;
use crate::error::EmbedError;
use crate::fingerprint::blake3_hash_text;
use crate::forward::VECTOR_DIM;

/// Default cache size per spec §04/05 §5.
pub const DEFAULT_CACHE_SIZE: usize = 10_000;

#[derive(Clone, Copy, Debug)]
struct CachedEmbedding {
    vector: [f32; VECTOR_DIM],
    fingerprint: [u8; 16],
    inserted_at: Instant,
}

#[derive(Debug, Default)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub size: u64,
}

/// Wraps any [`Dispatcher`] with a text-hash → vector LRU cache.
///
/// `cache_size = 0` → cache disabled, pure passthrough (spec §13).
pub struct CachingDispatcher<D: Dispatcher> {
    inner: D,
    /// `None` when `cache_size = 0`; the cache is disabled.
    state: Option<Arc<Mutex<LruCache<[u8; 16], CachedEmbedding>>>>,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
}

impl<D: Dispatcher> CachingDispatcher<D> {
    pub fn new(inner: D, cache_size: usize) -> Self;
    pub fn stats(&self) -> CacheStats;
    pub fn clear(&self); // for migrations / tests
    pub fn inner(&self) -> &D;
}

impl<D: Dispatcher> Dispatcher for CachingDispatcher<D> {
    fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError>; // cached
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError>; // passthrough
    fn fingerprint(&self) -> [u8; 16];
}
```

Re-exports from `lib.rs`: `CacheStats`, `CachingDispatcher`, `DEFAULT_CACHE_SIZE`.

Add to `fingerprint.rs`:
```rust
pub fn blake3_hash_text(text: &str) -> [u8; 16];
```

## 3. Implementation decisions

### 3.1 Why `Mutex<LruCache>` (not `RwLock`)

`lru::LruCache::get` requires `&mut self` because it bumps the entry to MRU. So a "read" still mutates. `RwLock` doesn't help. `parking_lot::Mutex` (already a workspace dep via brain-index) is the right primitive; uncontended `lock()` is ~25 ns, miss path is 5–10 ms anyway, so lock contention is irrelevant unless thousands of threads hit the same cache. If that ever becomes a bottleneck, switch to a sharded-mutex pattern later — not now.

### 3.2 Stats as `AtomicU64`, not behind the mutex

Hits/misses/evictions are incremented under the mutex (since we already hold it for the cache op). But the *read* path (`stats()`) doesn't need to acquire the mutex — atomic load is enough. This makes observability cheap and avoids stalling cache work if an operator polls stats hot.

`size` is read out of the cache itself when the mutex is held (during `stats()`). Reading it racy-without-lock would require an additional atomic; not worth it, since `stats()` is a debug / admin call, not a hot path.

### 3.3 Fingerprint mismatch ≠ delete

Per spec §8, stale entries are not auto-removed. The lookup returns `None` (miss); the entry sits until LRU evicts it. The miss path then *replaces* it on insertion (same key, new fingerprint). Net effect: stale entries serve as inert ballast for at most a short while; eventually overwritten or evicted.

Implementation: when fingerprint mismatches on lookup, we `peek` (don't bump LRU) so the stale entry doesn't gain undeserved freshness. Then proceed to miss path.

Actually `lru::LruCache::peek` exists and takes `&self` ... no wait, looking at the crate docs, `peek` returns `Option<&V>` and takes `&mut self` too. So even `peek` would need the mutex. But it does NOT bump the LRU position. We use it.

### 3.4 `cache_size = 0` short-circuit

`LruCache::new(NonZeroUsize::new(0))` panics — `NonZeroUsize` rejects zero. So our `new()` either constructs the LRU with `NonZeroUsize::new(cache_size).unwrap()` (when `cache_size > 0`) or stores `None` in `state` (when zero). The `embed()` impl checks `state.is_none()` first and forwards unconditionally.

Spec §13 says "the substrate skips the cache entirely". We honor that: no hash computation, no mutex, no stats incremented (a disabled cache has no hits or misses to count — the wrap is pure passthrough).

Wait, should disabled cache still count misses? Spec §10 lists `cache_hit_rate = hits / (hits + misses)`. If disabled, both stay zero. That's the cleanest reading; document it.

### 3.5 `inserted_at: Instant` — what for

Spec §1's struct has this field but the rest of the spec never reads it. It exists so the future migration path (or an admin stat) can answer "when was this cached?". We populate it on insert; nothing reads it in v1. Keeping the field in the struct for forward-compat doesn't cost anything (16 bytes per entry on 64-bit).

### 3.6 Why hash text bytes raw, not the normalised form

Spec §2's key is "BLAKE3 of the input text". The input is whatever the caller passes — exactly the bytes BERT will tokenise. Hashing the raw bytes means:
- "Hello" and "HELLO" hash differently → different cache entries, but BERT (uncased) tokenises them identically → same vector after inference. Wasted cache slot.
- "  hello  " and "hello" hash differently → same observation.

This is the spec's choice (it says "the input text"), and it's the right one for correctness: the cache stores what was actually inferred from exactly these bytes. Identical bytes → identical inference, period. The duplicate cache cost for trivial whitespace differences is fine.

### 3.7 Why `embed_batch` is a pure passthrough

A cache-aware batch implementation has to:
1. Hash every input.
2. Check the cache for each.
3. Collect indices of misses + their texts.
4. Call `inner.embed_batch(&miss_texts)` if any.
5. Insert each miss result.
6. Rebuild the output in original order.

This is correct but moves a noticeable chunk of logic into the cache. Spec §11 names "cues" — text submitted by `ENCODE` / `RECALL` / etc., one per request from the client's perspective. Phase 7's cognitive ops call `embed`, not `embed_batch`. So the gain from a cache-aware `embed_batch` is theoretical until some caller wants it.

Decision: `CachingDispatcher::embed_batch` forwards to `inner.embed_batch` and skips cache. Document it. If a future caller needs cache-aware batching, it's a contained follow-up.

### 3.8 No `EmbedError` changes

The cache adds no new failure modes. Hash computation is infallible (BLAKE3 over a `&str` can't fail). Mutex acquisition can't fail (parking_lot panics on poisoning, but we never panic while holding it). Eviction is silent.

### 3.9 `Arc` around the inner state vs not

`CachingDispatcher` is `Send + Sync` (required by the trait). The cache state itself is `Mutex<LruCache>` — `Send + Sync`. Wrapping it in `Arc` is only useful if the cache is shared between multiple `CachingDispatcher` instances. For v1 there's one `CachingDispatcher` per shard, so the `Arc` is unnecessary. But the `Arc` makes future "clone the wrapper to share the same cache" trivial without a re-architecture. Cost: one extra deref per lookup. Negligible.

Choice: keep the `Arc<Mutex<...>>` for forward-compatibility. Same cost as a bare `Mutex<...>` in the common (uncloned) case.

### 3.10 Why `D: Dispatcher` generic, not `dyn Dispatcher`

Performance: a generic enables monomorphisation, the compiler can inline the `inner.embed()` call into the cache miss path. A trait object would require a vtable dispatch on every miss. The miss path is 5–10 ms (inference dominates), so the vtable cost is in the noise — but we don't pay it either way with monomorphisation. The generic also keeps `Send + Sync` propagation automatic.

Phase 7 / Phase 9 callers can still erase the type if they want (`Box<dyn Dispatcher>` over the whole `CachingDispatcher<CpuDispatcher>`); they're free to.

### 3.11 Test harness: `CountingMockDispatcher`

Build on the mock pattern from 5.4. The cache's correctness is the *cache* logic, not the inference; a mock that returns a deterministic-per-text vector and counts inner calls is exactly enough:

```rust
struct CountingMockDispatcher {
    fp: [u8; 16],
    calls: AtomicU64,
    // text → vector mapping: vector encodes the text's first byte for traceability
    map: ...
}
```

We assert `mock.calls == N` after a sequence of `cache.embed(...)` calls to prove the cache is doing its job.

### 3.12 Risks

- **`lru` crate API drift**: 0.12 → 0.13 changed some signatures. We pin to `0.12` in workspace deps, matching the orientation plan. If `0.13` is preferred, swap before implementation.
- **`LruCache::put` returning the evicted entry**: we use the return value to count evictions. If the API ever stops returning it, the test catches the regression.
- **`AtomicU64` ordering**: `Relaxed` is fine for counters that have no causal dependencies; we use it. Reads via `.load(Relaxed)` may miss the most-recent increment by a few nanoseconds — irrelevant for stats.

## 4. New dependency

Add to workspace `Cargo.toml`:
```toml
lru = "0.12"
```

And to `crates/brain-embed/Cargo.toml`:
```toml
lru.workspace = true
parking_lot.workspace = true
```

(`parking_lot` is already a workspace dep — brain-index uses it. Just declare it here.)

That's the only new dep in Phase 5.

## 5. Files written / changed

```
Cargo.toml                                     [edit: + lru workspace dep]
crates/brain-embed/Cargo.toml                  [edit: + lru, + parking_lot]
crates/brain-embed/src/cache.rs                [new]
crates/brain-embed/src/fingerprint.rs          [edit: + blake3_hash_text]
crates/brain-embed/src/lib.rs                  [edit: mod + re-exports]
crates/brain-embed/tests/cache.rs              [new — gated on BRAIN_EMBED_MODEL_DIR]
```

## 6. Verify checklist

- `cargo build -p brain-embed` clean.
- `cargo test -p brain-embed` — existing 35 + ~7 new (1 hash helper + 5–6 cache unit tests + gated cache integration).
- `cargo clippy -p brain-embed --all-targets -- -D warnings` clean.
- `cargo fmt -p brain-embed` no diff.
- Workspace builds (excluding pre-existing brain-storage macOS errors): `cargo check -p brain-embed`.

## 7. Commit message (draft)

```
feat(brain-embed): LRU cache for text → vector (sub-task 5.5)

CachingDispatcher<D> wraps any inner Dispatcher with a per-shard
LRU cache keyed by BLAKE3-16(text). Implementation faithful to
spec §04/05:

- Default capacity 10 000 (§5); cache_size = 0 disables entirely (§13).
- Cache value is (vector, fingerprint, inserted_at) (§1).
- Fingerprint mismatch on lookup → miss; stale entries age out via
  LRU rather than being scanned and removed (§8).
- LRU eviction via the `lru` crate (§7).
- Atomic counters for hits / misses / evictions; observable without
  contending the cache mutex.
- embed_batch is a pure passthrough — spec §11 cache covers cues
  (single texts); cache-aware batching is a contained follow-up.
- Adds blake3_hash_text helper.

New workspace dep: lru 0.12.

Tests: CountingMockDispatcher proves hit / miss / eviction /
disabled-cache / fingerprint-mismatch behaviour. Integration test
(gated on BRAIN_EMBED_MODEL_DIR) confirms wrapping CpuDispatcher
returns bit-identical vectors on second call.

Verify: cargo build/test/clippy -p brain-embed.
```

## 8. Out-of-scope flags (re-confirm)

- No async API.
- No `ADMIN_STATS` plumbing — Phase 9.
- No migration cache-clear hooks — Phase 8/9.
- No cache-aware `embed_batch` — deferred.
- No SIMD / hashbrown / custom hasher — `lru`'s default `RandomState` is fine.

---

PLAN READY.
