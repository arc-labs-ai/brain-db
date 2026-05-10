# 08.03 RECALL Planning

How the planner builds an execution plan for a RECALL request.

## 1. The RECALL request shape

```rust
struct RecallRequest {
    cue_text: String,             // Required; gets embedded
    agent_id: AgentId,
    k: usize,                     // Default 10; max 1000
    filter: AnnFilter,            // Per [06.09 Filtering]
    confidence_min: Option<f32>,  // Filter by similarity score
    include_text: bool,           // Whether to return full memory text
    include_metadata: bool,       // Whether to include extra metadata fields
    consistency: Consistency,     // Eventual / ReadAfterWrite
    request_id: Option<RequestId>,
}
```

## 2. Routing

A RECALL is agent-scoped: search within an agent's memories. The agent's data lives on a specific shard (or, for very large agents, multiple shards).

```rust
fn resolve_shards(agent_id: AgentId) -> Vec<ShardId> {
    let primary = router.shard_for(agent_id);
    let extras = router.extras_for(agent_id);  // Empty for typical agents
    [primary].into_iter().chain(extras).collect()
}
```

For most agents, this returns one shard. Cross-shard agents are rare and require fan-out.

## 3. Per-shard sub-plan

For each shard:

```rust
struct ShardSearchStep {
    shard_id: ShardId,
    embedding_step: EmbeddingStep,    // Shared across shards (reuse same embedding)
    ann_search: AnnSearchStep,
    metadata_lookup: MetadataLookupStep,
    filter_apply: FilterStep,
}
```

The embedding step is shared — we embed the cue once, then use the same vector across shards.

## 4. Picking ef_search

The substrate's HNSW search ([06.04 Search](../06_ann_index/04_search.md)) takes an `ef` parameter. Bigger ef = better recall but slower.

The planner picks ef:

```rust
fn pick_ef(req: &RecallRequest, shard_stats: &ShardStats) -> usize {
    let mut ef = config.default_ef_search;  // 64

    // Bigger K wants more candidates
    if req.k > 50 {
        ef = ef.max(req.k * 4);
    }

    // Filter selectivity
    let selectivity = estimate_selectivity(&req.filter, shard_stats);
    if selectivity < 0.5 {
        ef = ef.max((ef as f32 / selectivity) as usize);
    }

    // Cap to avoid pathological values
    ef = ef.min(config.max_ef_search);  // 500

    ef
}
```

For a typical agent-scoped RECALL with K=10 and no filter: ef=64.

For a RECALL with a selective filter (only 10% of memories match): ef = 64 / 0.1 = 640, capped to 500.

For a high-K RECALL (K=100): ef = max(64, 100*4) = 400.

## 5. The over_factor

To account for filtered candidates, the planner sets an over_factor:

```rust
let over_factor = (1.0 / selectivity).max(1.0).min(8.0);
let candidates_to_request = (req.k as f32 * over_factor) as usize;
```

`candidates_to_request` is the number of candidates the substrate asks HNSW for. Typically 10-100; capped at 1000 (HNSW gets less efficient at high K).

## 6. Filter pre/post

Most filters are post-search (HNSW returns candidates, then filter). Some can be pre-applied:

- Fingerprint filter: applied during HNSW post-processing via slot metadata (very fast; effectively pre).
- Tombstone filter: always pre-applied (the substrate skips tombstoned slots).

The plan describes which filter rules to apply at which stage:

```rust
enum FilterStage {
    PreFilter,    // Skip tombstoned slots; check inline metadata
    PostFilter,   // After candidate gathering
}
```

## 7. Confidence threshold

If `confidence_min` is set (e.g., 0.7), the planner adds a post-filter:

```rust
results.retain(|r| r.score >= req.confidence_min);
```

This is applied after merging results from all shards.

## 8. Fan-out for cross-shard

If multiple shards are involved:

```rust
struct CrossShardPlan {
    shards: Vec<ShardSearchStep>,    // One per shard
    merge: MergeStep {
        gather_top: usize,            // K * over_factor per shard
        final_top: usize,             // Final K
        sort_by: SortKey::Score,
    },
}
```

The substrate fans out: each shard runs its sub-plan in parallel; results are merged.

For a 2-shard agent: ~2× the per-shard latency (parallel) plus merge overhead. Typically negligible.

For 10+ shard agents (very large): the merge step matters more; the substrate may stream results from shards as they arrive rather than waiting for all.

## 9. The "K from each shard" sizing

When fanning out, each shard returns its top K (or top K * over_factor). The merge produces the global top K.

But we don't know in advance which shard has the global top K. So we ask each shard for K * sqrt(N) where N is the shard count, to ensure the global top K is captured.

For 2 shards: ask each for K * 1.4 ≈ 1.5K.
For 10 shards: ask each for K * 3.2.

This is conservative and adequate. More elaborate sampling-based approaches exist but aren't worth the complexity for typical N (1-3 shards per agent).

## 10. Read-after-write

If `consistency = ReadAfterWrite`, the planner adds a wait step:

```rust
struct ReadAfterWriteStep {
    wait_for_lsn: u64,    // The agent's last write LSN
    timeout_ms: u32,
}
```

The executor waits for the shard's HNSW to catch up to the LSN before searching. Detailed in [06.08 Concurrency](../06_ann_index/08_concurrency.md) §Read-after-write.

## 11. The text-include decision

If `include_text = true`, the planner adds a text-fetch step:

```rust
struct TextFetchStep {
    memory_ids: Vec<MemoryId>,    // From results
    parallel: bool,               // Yes, batch in one read txn
}
```

The text fetch is from the metadata store's `texts` table. With K=10, ~50 µs.

If `include_text = false` (the default), the step is omitted.

## 12. Full plan example

```rust
RecallPlan {
    embedding: EmbeddingStep {
        text: req.cue_text,
        cache_lookup: true,
    },
    shards: vec![ShardSearchStep {
        shard_id: agent's shard,
        ann_search: AnnSearchStep {
            ef: 64,
            k: 80,         // K * over_factor
            filter: PreFilter { fingerprint, tombstone },
        },
        metadata_lookup: MetadataLookupStep {
            include_extra: req.include_metadata,
        },
        filter_apply: FilterStep {
            stage: PostFilter,
            rules: req.filter.post_rules(),
        },
    }],
    merge: MergeStep {
        sort_by: Score,
        final_top: 10,
        confidence_min: req.confidence_min,
    },
    text_fetch: if req.include_text { Some(...) } else { None },
    response: ResponseStep {
        include_text: req.include_text,
        include_metadata: req.include_metadata,
    },
}
```

## 13. Plan validity

The planner ensures:

- ef ≥ K.
- ef ≤ max_ef_search.
- candidates_to_request ≤ 1000.
- filter rules are well-formed.

If invalid, the planner returns an error (not the planner's fault; user-supplied K too high).

## 14. Plan caching

For very-hot RECALL patterns (e.g., a chatbot's "recent context" RECALL), plan caching could amortize the planning cost.

Not implemented in v1. Plan time is < 50 µs; caching would save tens of microseconds. Not worth the complexity.

## 15. The "explain plan" facility

For debugging, an admin operation `ADMIN_EXPLAIN_PLAN` runs the planner without executing and returns the plan. Useful for:

- Verifying the planner's choices.
- Estimating costs before running expensive queries.

The output is human-readable plan text, structured for tooling.

---

*Continue to [`04_encode_planning.md`](04_encode_planning.md) for ENCODE planning.*
