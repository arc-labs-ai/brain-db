# 04.05 The Cue Cache

Embedding inference is the embedding layer's expensive operation. Repeated cues (e.g., similar queries from the same agent) shouldn't pay the inference cost twice. This file specifies the LRU cache that absorbs repeats.

## 1. The cache

A least-recently-used cache keyed by text hash, with normalized vectors as values.

```rust
struct CueCache {
    map: LruCache<TextHash, CachedEmbedding>,
}

struct CachedEmbedding {
    vector: [f32; 384],
    fingerprint: ModelFingerprint,
    inserted_at: Instant,
}

type TextHash = [u8; 16];  // BLAKE3-derived
```

## 2. Cache key: text hash

Cache keys are 16-byte hashes of the input text, not the text itself.

The hash function: [BLAKE3](https://github.com/BLAKE3-team/BLAKE3), truncated to 16 bytes.

Why hash:

- **Compact.** 16 bytes per key vs potentially kilobytes of text.
- **Fast comparison.** Hashmap lookup compares 16 bytes; full-text comparison is O(text length).
- **Privacy.** The cache holds hashes, not original text — useful for memory dumps and logs.

Why BLAKE3:

- Faster than SHA-256 on modern CPUs.
- Cryptographically strong (collision-resistant).
- Pure Rust implementation; no FFI.

The 16-byte truncation keeps keys compact. Collision probability at this length is ~10^-19 per pair; for any realistic cache size (~10^6 entries), the probability of a collision in the cache is essentially zero.

## 3. Cache hits

When a cue cache hit occurs:

1. Compute the text hash.
2. Look up in the cache.
3. If present, validate the cached entry's fingerprint matches the current model's fingerprint.
4. Return the cached vector.

A fingerprint mismatch means the model has been changed; the cached entry is stale and discarded.

Cache-hit latency: < 1 µs (hash + hashmap lookup + comparison). Three orders of magnitude faster than a cache miss.

## 4. Cache misses

When a cue cache miss occurs:

1. Compute the text hash.
2. Run the full embedding pipeline (tokenize → infer → normalize).
3. Insert the result into the cache before returning.

Cache-miss latency: 5–10 ms (dominated by inference).

## 5. Cache size

The cache size is configurable. Default: **10,000 entries**.

At 384 × 4 bytes per vector + 16 bytes per key + small overhead, each entry is ~1.6 KiB. 10,000 entries = ~16 MiB.

This is small relative to the model's resident size (~130 MiB) and the arena's working set. The cache is a small win on memory but a large win on latency for hot queries.

Sizing considerations:

- **Higher hit rate** is desirable for repeated agent queries.
- **Memory budget** is bounded by configuration.
- **Eviction overhead** (LRU's bookkeeping) scales with cache size.

For most deployments, the default 10K is fine. High-QPS deployments with consistent query patterns may benefit from 100K. Very-low-QPS deployments don't need a large cache (the hit rate's already high or the cache is irrelevant).

## 6. Eviction policy: LRU

When the cache is full and a new entry is inserted, the least-recently-used entry is evicted.

LRU is the right policy for this workload:

- Hot queries get repeatedly accessed; they stay in the cache.
- Cold queries age out.
- New queries displace older queries.

We considered:

- **LFU** (least-frequently-used) — could keep hot-but-old entries. More bookkeeping; marginal gain for our workload.
- **Random eviction** — simpler. Slightly worse hit rate.
- **2Q / ARC** — adaptive policies. More complex; marginal gain.

LRU's simplicity wins.

## 7. Implementation: lru crate

The substrate uses the [`lru`](https://github.com/jeromefroe/lru-rs) crate from Rust crates.io. It provides a HashMap-backed LRU with O(1) put, get, and eviction.

The cache is per-shard, not global. Each shard's executor maintains its own cache. This:

- Avoids cross-core synchronization (the cache is owned by one executor).
- Keeps cache content relevant to the shard's queries.
- Sums to roughly the same total memory as a single global cache, since per-shard hot sets are mostly disjoint.

## 8. The fingerprint check

Every cache entry includes the fingerprint of the model that produced it. On lookup:

```rust
fn lookup(&self, key: &TextHash) -> Option<&[f32; 384]> {
    let entry = self.map.get(key)?;
    if entry.fingerprint != self.current_fingerprint {
        return None;  // Stale; treat as miss
    }
    Some(&entry.vector)
}
```

Stale entries (different fingerprint) are not auto-removed; they age out via LRU. Auto-removal would require scanning the cache on fingerprint change, which is expensive.

## 9. Cache invalidation on model change

When the model is changed (via migration or restart with a different model), the cache becomes invalid:

- Restart drops the cache entirely (it's in-memory).
- Migration: the cache is invalidated by fingerprint mismatch on lookup. Old entries don't return; they age out.

For the migration path, the substrate optionally clears the cache explicitly to free memory faster. This is a minor optimization; not strictly necessary.

## 10. Cache stats

The cache maintains statistics for observability:

- **`cache_hits`** — count of successful lookups.
- **`cache_misses`** — count of misses.
- **`cache_evictions`** — count of evictions.
- **`cache_size`** — current number of entries.
- **`cache_hit_rate`** — `hits / (hits + misses)` over a rolling window.

Exposed via `ADMIN_STATS`. A typical hit rate is 30–70% depending on workload — repeated queries cached well, novel queries always miss.

## 11. What's not cached

The cache is for cues only — text submitted by clients in `ENCODE`, `RECALL`, `PLAN`, `REASON`. It's keyed on text.

What's not cached:

- **Stored vectors** — those live in the arena, accessed via mmap. The page cache handles their hot/cold management.
- **Tokenizations** — the tokenizer is fast enough that re-tokenizing on cache miss isn't a meaningful cost.
- **Inference activations** — these are intermediate model state; caching them doesn't help.

## 12. Cache poisoning

The cache could be poisoned if, for example, two requests with the same text hash but different intentions resulted in different vectors being cached. This doesn't happen because:

- The text hash is the input to the model's inference; the model is deterministic.
- The cache key is the text, not any per-request state.
- Different agents querying the same text get the same vector — that's by design, since vectors are per-text, not per-agent.

The cache is shared across agents' queries within a shard. There's no privacy concern because the cache holds vectors, not text — and an attacker who could read the cache could already read agent state through the substrate's normal access controls.

## 13. The case against caching

Some workloads see almost no cache hit rate:

- An agent that processes a continuous stream of unique queries.
- A research workload encoding diverse content.
- A system test with synthetic random text.

For these, the cache is overhead — every query misses, the cache fills with cold entries that get evicted on the next query. The substrate's behavior is correct (cache returns no useful answers; full inference path runs every time), just not helpful.

The configuration knob to disable caching is `cache_size = 0`. The substrate skips the cache entirely. For typical agent workloads (where some queries repeat), the default-on cache is the right choice.

## 14. The case for caching beyond cues

Could the cache extend beyond cues — e.g., to cache RECALL results?

- **RECALL result caching** would be useful for repeated identical queries, but RECALL results depend on the current state (memories may have been added or removed). Cache invalidation becomes complex.
- **Plan caching** — already done at the planner layer (separate from the embedding cache). See [08. Query Planner](../08_query_planner/) §Plan Cache.

For v1, only cue text is cached. Other caching is deferred.

---

*Continue to [`06_batching_gpu.md`](06_batching_gpu.md) for GPU batching.*
