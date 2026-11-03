# 09.05 Filtering

How Brain restricts ANN search to a subset of memories — by model fingerprint, kind, session, salience range, etc.

## 1. The need for filtering

Most queries don't want to search all memories. Common filters:

- **Model fingerprint** — only memories embedded with the current model. Required for cross-model exclusion ([07.05 Fingerprinting](../07_embedding/05_fingerprinting.md)).
- **Kind** — episodic, semantic, or consolidated.
- **Session** — only memories in a specific session (or set of sessions).
- **Salience** — only memories above a threshold.
- **Recency** — only memories from the last N days.
- **Tombstone** — exclude tombstoned memories (always applied).

## 2. Post-search filtering

HNSW doesn't support filter expressions during traversal. Filters apply post-search:

```rust
fn filtered_search(query, k, filters) -> Vec<Result> {
    let candidates = hnsw.search(query, k * over_factor, ef);
    candidates.into_iter()
        .filter(|c| filters_match(c.memory_id, filters))
        .take(k)
        .collect()
}
```

`over_factor` is a multiplier ≥ 1: search returns more candidates than K, expecting some to be filtered out. For unrestricted filters (always-true), over_factor = 1. For selective filters (e.g., 10% pass rate), over_factor = 10.

## 3. The filter shape

```rust
struct AnnFilter {
    model_fingerprint: Option<ModelFingerprint>,
    kind: Option<MemoryKind>,
    session_ids: Option<Vec<SessionId>>,
    salience_min: Option<f32>,
    salience_max: Option<f32>,
    age_max: Option<Duration>,
    exclude_tombstoned: bool,    // default true
}

impl AnnFilter {
    fn matches(&self, memory_id: MemoryId, metadata: &MemoryMetadata) -> bool {
        // Apply each filter in turn; bail on first non-match
        if let Some(fp) = self.model_fingerprint {
            if metadata.fingerprint != fp { return false; }
        }
        // ... etc
        true
    }
}
```

Filters are passed to `search()`; the search applies them after candidates are gathered.

## 4. Fingerprint as inline metadata

For the most common filter (model fingerprint), Brain stores the fingerprint inline in the arena slot's metadata. This means filtering by fingerprint doesn't require a metadata-store lookup — it's a direct read from the slot's bytes.

```rust
fn slot_fingerprint(slot_id: u64) -> [u8; 16] {
    let offset = slot_offset(slot_id) + FINGERPRINT_OFFSET_IN_SLOT;
    let ptr = arena_base.add(offset) as *const [u8; 16];
    unsafe { *ptr }
}
```

Cost: ~10 ns. Fingerprint filter on every search candidate is essentially free.

## 5. Other filters require metadata lookup

Kind, session, salience, age — these live in the metadata store (redb). Filtering by them means:

```rust
fn metadata_for_filter(memory_id: MemoryId) -> Option<MemoryMetadata> {
    metadata_db.get_memory(memory_id)
}
```

Cost: ~1-10 µs (depending on cache state).

For a search returning 100 candidates with selective filters, the metadata lookups are 100 × ~5 µs = 500 µs. Significant but acceptable.

## 6. Filter selectivity and over_factor

Brain estimates filter selectivity to set `over_factor`:

| Filter | Typical pass rate | over_factor |
|---|---|---|
| Fingerprint (current model) | ~95% | 1.1 |
| Kind = Episodic | ~80% | 1.3 |
| Kind = Semantic | ~15% | 7 |
| Kind = Consolidated | ~5% | 20 |
| Session (specific) | varies | varies |
| Salience > 0.5 | ~30% | 4 |
| Combined (AND) | product | product |

Estimates come from per-shard statistics (Brain tracks how many memories of each kind exist).

## 7. Bailout on too-few-results

If even with the over_factor Brain doesn't gather enough filtered candidates, it re-queries with higher ef:

```rust
let mut ef = initial_ef;
let mut results = Vec::new();
while results.len() < k && ef < ef_max {
    let candidates = hnsw.search(query, k * over_factor, ef);
    results = filter(candidates, filters).take(k).collect();
    if results.len() < k {
        ef *= 2;  // Try a wider beam
    }
}
return results;
```

The `ef_max` is configurable (default 500). Beyond that, Brain gives up and returns whatever it found, even if fewer than K.

## 8. Session filter via inverted index

For very selective session filters (memory in a specific small session), Brain maintains an inverted index:

```
context_index[session_id] -> Set<MemoryId>
```

For these filters, Brain intersects the HNSW search results with the session's set:

```rust
fn search_with_session_filter(query, k, session_id) -> Vec<Result> {
    let context_set = context_index[session_id];
    let candidates = hnsw.search(query, k * 2, ef);
    candidates.into_iter()
        .filter(|c| context_set.contains(&c.memory_id))
        .take(k)
        .collect()
}
```

For very selective filters where the candidate set is much larger than the session set, Brain could iterate the session set instead — compute distance from query to each member, return top K. This is a brute-force fallback; useful when the session has < 10K memories.

## 9. The space_id filter

Every memory belongs to a space. ANN searches are inherently space-scoped — Brain routes the query to the space's shard ([16. Sharding & Clustering](../16_sharding/00_purpose.md)).

Within a shard, many spaces coexist. Brain filters by `space_id` implicitly — every search carries a scope filter, and it is never the caller's to omit.

## 10. Composed filters

Filters can be combined (AND'd):

```
filter = Filter {
    model_fingerprint: Some(current_fp),
    kind: Some(MemoryKind::Episodic),
    salience_min: Some(0.5),
    ...
}
```

All conditions must match for a candidate to pass. Single-pass evaluation: stop at the first non-match.

OR'd filters (e.g., "kind = Episodic OR kind = Semantic") are expressed as:

```
filter.kind = None;  // No filter
or use a custom matcher: |m| matches!(m.kind, Episodic | Semantic)
```

The data-model-side representation is an enum; expressions like "Episodic OR Semantic" are special-cased in the filter struct.

## 11. The cost of filtering

Per-result filtering cost:

- Fingerprint (slot metadata): ~10 ns.
- Kind, salience (metadata-store cached): ~50 ns.
- Kind, salience (metadata-store cold): ~5 µs.
- Session (set membership): ~50 ns.

For typical searches returning 100 candidates with 3-4 filters: ~1-10 µs total, dominated by metadata-store lookups.

## 12. Filter caching

Brain caches per-memory metadata for the duration of a query. If the same memory is checked against multiple filters, the metadata is fetched once.

A more aggressive cache (across queries) is not implemented — metadata can change (salience updates), and invalidation is complex.

## 13. Filter on MemoryId range

A specialized filter: "memories created after time T". The MemoryId's UUIDv7 prefix encodes creation time, so this is a fast comparison without metadata-store lookup:

```rust
fn after_time(id: &MemoryId, t: Timestamp) -> bool {
    let id_time = uuidv7_extract_time(id);
    id_time >= t
}
```

Cost: ~10 ns. Useful for "recent memories" queries.

## 14. The "no filter" fast path

For space_id-only queries (no other filters), Brain skips post-filter logic entirely:

```rust
if filter.is_minimal() {
    return hnsw.search(query, k, ef);
}
```

This shaves ~10 µs off the search latency. Most space-scoped queries with default settings hit this fast path.

---

*Continue to [`06_failure_modes.md`](06_failure_modes.md) for failure modes.*
