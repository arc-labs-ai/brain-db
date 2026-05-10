# 09.03 RECALL

The RECALL primitive: find memories by similarity.

## 1. Semantic contract

```
RECALL(cue_text, agent_id, k, filter, ...) → Vec<RecallResult>
```

The substrate:

1. Embeds the cue text into a vector.
2. Searches the HNSW index for nearest neighbors.
3. Filters candidates by the supplied filter.
4. Returns up to K results, sorted by similarity.

## 2. The arguments

### cue_text

The query. Embedded with the same model used for stored memories.

The cue can be a single word, a sentence, a longer document — whatever the agent thinks is a useful query. Note that very short cues may be ambiguous and produce broad results; very long cues are truncated by the embedder.

### agent_id

The owning agent. Returns are scoped to this agent's memories.

### k

How many results. Default 10. Max 1000.

### filter

A `RecallFilter`:

```rust
struct RecallFilter {
    kind: Option<MemoryKind>,         // Episodic / Semantic / Consolidated
    contexts: Option<Vec<ContextRef>>,// Limit to specific contexts
    min_salience: Option<f32>,
    max_age: Option<Duration>,
    fingerprint_match: bool,          // Default true; same model only
    tags: Option<Vec<String>>,        // Custom tags from metadata
    custom: Vec<FilterRule>,          // Arbitrary metadata filters
}
```

Most filters are optional; defaults are permissive.

### include_text

Whether to return the memory text in the response. Default false.

If true, the substrate fetches text from the metadata store. Adds ~50 µs per result.

### include_metadata

Whether to include extra metadata fields. Default false.

### consistency

Either `Eventual` (default) or `ReadAfterWrite`.

With ReadAfterWrite, the recall waits for the most recent writes to be searchable.

### confidence_min

Optional. Filter results with similarity score below this threshold. Useful when the agent only wants strong matches.

## 3. The response

```rust
struct RecallResponse {
    results: Vec<RecallResult>,
    partial: bool,                    // True if some shards failed
    total_candidates: usize,          // Pre-filter count (for diagnostics)
}

struct RecallResult {
    memory_id: MemoryId,
    score: f32,                       // [-1, 1]; higher = more similar
    text: Option<String>,             // If include_text
    metadata: Option<MemoryMetadata>, // If include_metadata
    context_id: ContextId,
    kind: MemoryKind,
}
```

Results are sorted by score, descending.

## 4. Score semantics

Score = `1 - cosine_distance(cue_vec, mem_vec)` for normalized vectors.

Range: typically 0 to 1 in practice (vectors don't usually point opposite). 1.0 means identical; 0.0 means orthogonal; negative means opposite (rare).

Heuristic interpretation:
- > 0.9: very similar (often near-duplicate).
- 0.7-0.9: similar topic, related content.
- 0.5-0.7: same general area.
- < 0.5: weakly related.

These aren't strict thresholds; they depend on the model and the corpus. Agents tune `confidence_min` to their use case.

## 5. The "fewer than K" case

If the substrate finds fewer than K matching memories, the response has fewer than K results. This is normal for:
- Small or new agents.
- Selective filters.
- Very specific cues.

It's not an error.

## 6. The "empty result" case

Zero results. Possible if:
- The agent has no memories.
- All memories are tombstoned.
- All memories have a different model fingerprint (after a model upgrade).
- The filter is too restrictive.

The response is an empty list, not an error.

## 7. Filter semantics

Filters are AND-combined:

```
result matches filter ⇔
  (filter.kind is None or result.kind == filter.kind)
  AND (filter.contexts is None or result.context in filter.contexts)
  AND (filter.min_salience is None or result.salience >= filter.min_salience)
  AND ... 
```

For OR semantics (e.g., "Episodic or Semantic"), use multiple filters and merge in the agent.

## 8. The "fingerprint_match" default

By default, RECALL returns memories with the current model's fingerprint. Memories from older models are excluded.

This is a safety feature: cross-model similarity isn't meaningful.

To search across models (rarely useful, mostly for debugging or migration), set `fingerprint_match: false`.

## 9. The "salience" effect

Currently, RECALL returns purely by similarity score. Salience is filtered (if `min_salience` is set) but doesn't directly affect ranking.

A future option (open question): blend salience and similarity in ranking. Not in v1.

## 10. The "recency" effect

Similar: recency (age) is filterable but doesn't affect ranking. The substrate doesn't auto-favor recent memories.

If the agent wants recent-favoring, it can:
- Use `max_age` to filter.
- Re-rank results in the agent layer.

## 11. The "context boost" effect

The agent might want memories in the current context to rank higher. Brain doesn't do this automatically. The agent can:

1. RECALL with no context filter; get K results.
2. RECALL with the context filter; get K results.
3. Merge in the agent layer with weights.

Or use a single RECALL with explicit `contexts: Some([current])`.

## 12. The "across-shard" recall

For agents whose data spans multiple shards (rare), RECALL fans out:

- Each shard runs its sub-recall in parallel.
- Results are merged by score.

The response is the global top K.

This is transparent to the agent — it sees a single result list.

## 13. Latency

For typical workloads (single-shard, K=10, no complex filter):

- p50: ~10 ms.
- p99: ~25 ms.

For larger K or complex filters: latency rises proportionally. K=100 takes ~15 ms typical; K=1000 takes ~30 ms.

For cross-shard recalls (2-3 shards): p99 rises to ~30-50 ms.

## 14. Throughput

A shard handles ~5K-20K RECALLs per second. Limited by:

- Embedder throughput (with cache, much higher).
- HNSW search latency.

For higher throughput, scale shards.

## 15. The "include_text=true" cost

Including text fetches each result's text from the metadata store:

- Per-result cost: ~5-20 µs (cache-dependent).
- For K=10: ~100 µs additional.
- For K=100: ~1 ms additional.

For very large texts (~MB each), the response size grows correspondingly.

## 16. The "include_metadata=true" cost

Similar to text, but for the extra metadata fields. Usually small (~tens of bytes per memory).

## 17. The "tags" filter

Tags are agent-defined strings stored in the memory's metadata. Filter:

```
filter.tags = Some(vec!["urgent".to_string(), "personal".to_string()])
```

Returns memories that have ALL the specified tags (intersection). For "any of these tags" (union), make multiple recalls.

Tags are filtered post-search; selective tag filters need higher ef_search (substrate handles automatically).

## 18. The "score-only" mode

For agents that want just IDs and scores (no text, no metadata), the default is fine — text and metadata are off by default. The response is small and fast.

## 19. The "no result" semantics

If the agent gets zero results, possible interpretations:

- The agent has no relevant memories.
- The cue is unusual (no similar memories).
- The filter is too tight.

The substrate doesn't distinguish these. The agent decides what to do — broaden the cue, relax the filter, or accept no results.

## 20. The "two-stage" pattern

Some agents do:

1. RECALL with K=100 to get a broad set.
2. Re-rank with custom logic on the agent side.

The substrate's K=100 isn't much more expensive than K=10. The agent gets flexibility.

For very large K (>100), make sure to consider cost (K=1000 is ~3× the cost of K=10).

---

*Continue to [`04_plan.md`](04_plan.md) for PLAN.*
