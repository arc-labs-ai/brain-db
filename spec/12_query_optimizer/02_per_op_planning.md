# 12.02 Per-Operation Planning

How the planner builds execution plans for each core operation: RECALL, ENCODE, PLAN/REASON, and FORGET. Each section describes the request shape, plan structure, key decisions (ef_search, sharding, traversal depth), and latency profile.

## RECALL Planning

How the planner builds an execution plan for a RECALL request.

### 1. The RECALL request shape

The planner consumes the wire request as-is; the authoritative field list is in
[04. Wire Protocol](../04_wire_protocol/05_frame_layouts.md) §3. The inputs that
change the plan are:

| Input | Effect on the plan |
|---|---|
| `cue_text` | the embedding step; also mined for subject candidates and temporal expressions |
| `subject_name` | adds an explicit subject candidate to the grounded overlay |
| `scope` | `Space` → one shard; `Namespace` → fan out across every shard |
| `txn_id` | selects the txn path (read-your-writes over the pending buffer) instead of the retrieval path |
| `max_results` | caps the member count; also sizes the candidate pool and therefore the beam |
| `kind_filter` / `session_filter` / `salience_floor` / `age_bound` / `confidence_threshold` | the filter chain; selectivity feeds the beam choice |
| `as_of_record_time_unix_nanos` | the bi-temporal resolution point, and the recency reference |
| `include_text` / `include_edges` / `include_graph` | post-ranking fetch steps; no effect on retrieval |
| `trace` | retains the per-stage data the pipeline computes anyway |

There is no `consistency` input and no `k` separate from `max_results`: a read is
either inside a transaction or it is not, and the member count is capped by one
field.

### 2. Routing

Routing depends on the request's `scope`:

- **`Space` (the default)** — one shard, resolved by `shard_for_space`: an
  operator override pin if one exists, otherwise `BLAKE3(space_id) % shard_count`.
  A space's data lives on exactly one shard; multi-shard spaces are deferred
  past v1, so there is no extras list to chain.
- **`Namespace`** — fan out to **every** shard. Each shard widens its scope
  filter from `(namespace, space)` to `namespace_id` alone, and the merged pool
  is shaped once rather than per shard. The fan-out spans every space in the
  caller's own namespace and never another tenant's.

```rust
fn resolve_shards(scope: RecallScope, space_id: SpaceId) -> Vec<ShardId> {
    match scope {
        RecallScope::Space     => vec![routing.shard_for_space(space_id)],
        RecallScope::Namespace => (0..shard_count).collect(),
    }
}
```

### 3. Per-shard sub-plan

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

The embedding step is shared — Brain embeds the cue once, then uses the same vector across shards.

### 4. Picking ef_search

Brain's HNSW search ([09. HNSW operations](../09_indexing/02_hnsw_operations.md)) takes an `ef` parameter. Bigger ef = better recall but slower.

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

For a typical space-scoped RECALL with K=10 and no filter: ef=64.

For a RECALL with a selective filter (only 10% of memories match): ef = 64 / 0.1 = 640, capped to 500.

For a high-K RECALL (K=100): ef = max(64, 100*4) = 400.

### 5. The over_factor

To account for filtered candidates, the planner sets an over_factor:

```rust
let over_factor = (1.0 / selectivity).max(1.0).min(8.0);
let candidates_to_request = (req.k as f32 * over_factor) as usize;
```

`candidates_to_request` is the number of candidates Brain asks HNSW for. Typically 10-100; capped at 1000 (HNSW gets less efficient at high K).

### 6. Filter pre/post

Most filters are post-search (HNSW returns candidates, then filter). Some can be pre-applied:

- Fingerprint filter: applied during HNSW post-processing via slot metadata (very fast; effectively pre).
- Tombstone filter: always pre-applied (Brain skips tombstoned slots).

The plan describes which filter rules to apply at which stage:

```rust
enum FilterStage {
    PreFilter,    // Skip tombstoned slots; check inline metadata
    PostFilter,   // After candidate gathering
}
```

### 7. Confidence threshold

If `confidence_min` is set (e.g., 0.7), the planner adds a post-filter:

```rust
results.retain(|r| r.score >= req.confidence_min);
```

This is applied after merging results from all shards.

### 8. Fan-out for cross-shard

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

Brain fans out: each shard runs its sub-plan in parallel; results are merged.

For a 2-shard agent: ~2× the per-shard latency (parallel) plus merge overhead. Typically negligible.

For 10+ shard agents (very large): the merge step matters more; Brain may stream results from shards as they arrive rather than waiting for all.

### 9. The "K from each shard" sizing

When fanning out, each shard returns its top K (or top K * over_factor). The merge produces the global top K.

But Brain does not know in advance which shard has the global top K. So each shard is asked for K * sqrt(N) where N is the shard count, to ensure the global top K is captured.

For 2 shards: ask each for K * 1.4 ≈ 1.5K.
For 10 shards: ask each for K * 3.2.

This is conservative and adequate. More elaborate sampling-based approaches exist but aren't worth the complexity for typical N (1 shard per space in v1).

### 10. Read-your-writes

There is no `consistency` input and no wait-for-LSN step. Read-your-writes is a
property of being **inside a transaction**: a RECALL carrying a `txn_id` runs
the txn path, which overlays the transaction's pending buffer on the committed
snapshot.

That overlay is also why the txn path is a different plan rather than a flag on
the normal one: the lexical and graph retrievers do not see the pending buffer,
so a request that must observe uncommitted writes cannot be served by the fused
retrieval path at all.

Outside a transaction, a write is visible to a subsequent read once its ack has
landed; the derivation stages that follow it (edges, extraction, HyPE) settle
asynchronously and are observable through `SUBSCRIBE`, not by blocking the
read.

### 11. The text-include decision

If `include_text = true`, the planner adds a text-fetch step:

```rust
struct TextFetchStep {
    memory_ids: Vec<MemoryId>,    // From results
    parallel: bool,               // Yes, batch in one read txn
}
```

The text fetch is from the metadata store's `texts` table. With K=10, ~50 µs.

If `include_text = false` (the default), the step is omitted.

### 12. Full plan example

```rust
RecallPlan {
    embedding: EmbeddingStep {
        text: req.cue_text,
        cache_lookup: true,
    },
    shards: vec![ShardSearchStep {
        shard_id: the space's shard,
        ann_search: AnnSearchStep {
            ef: 64,
            k: 80,         // K * over_factor
            filter: PreFilter { fingerprint, tombstone },
        },
        metadata_lookup: MetadataLookupStep {},
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
    graph_fetch: if req.include_graph { Some(...) } else { None },
    response: ResponseStep {
        include_text: req.include_text,
        include_edges: req.include_edges,
        include_graph: req.include_graph,
    },
}
```

### 13. Plan validity

The planner ensures:

- ef ≥ K.
- ef ≤ max_ef_search.
- candidates_to_request ≤ 1000.
- filter rules are well-formed.

If invalid, the planner returns an error (not the planner's fault; user-supplied K too high).

### 14. Plan caching

For very-hot RECALL patterns (e.g., a chatbot's "recent session" RECALL), plan caching could amortize the planning cost.

Not implemented. Plan time is < 50 µs; caching would save tens of microseconds. Not worth the complexity.

### 15. The "explain plan" facility

For debugging, an admin operation `ADMIN_EXPLAIN_PLAN` runs the planner without executing and returns the plan. Useful for:

- Verifying the planner's choices.
- Estimating costs before running expensive queries.

The output is human-readable plan text, structured for tooling.

## ENCODE Planning

How the planner builds an execution plan for an ENCODE request.

### 1. The ENCODE request shape

```rust
struct EncodeRequest {
    text: String,                // The content to encode
    space_id: SpaceId,
    session: ContextRef,         // SessionId or session name
    kind: MemoryKind,            // Episodic / Semantic
    salience_initial: f32,       // [0, 1], default 1.0
    metadata: ExtraMetadata,     // User-defined key-values; small
    edges: Vec<EdgeSpec>,        // Edges to create alongside this memory
    request_id: RequestId,       // Required for idempotency
}
```

### 2. Routing

ENCODE is space-scoped: the new memory belongs to the requesting space's shard.

```rust
fn resolve_shard(space_id: SpaceId) -> ShardId {
    router.shard_for(space_id)
}
```

Multi-shard spaces are deferred past v1: a space's data lives on exactly one shard, so there is no per-encode shard selection.

### 3. The encode plan structure

```rust
struct EncodePlan {
    shard: ShardId,
    idempotency_check: IdempotencyCheckStep,
    embedding: EmbeddingStep,
    context_resolution: ContextResolutionStep,
    allocation: SlotAllocationStep,
    wal_append: WalAppendStep,
    apply: ApplyStep,
    edges: Vec<EdgeStep>,
    response: ResponseStep,
}
```

The plan describes the full encode pipeline as a sequence of steps.

### 4. Phase 1: Idempotency check

Before doing work, check if this RequestId has been seen:

```rust
fn idempotency_check(req: &EncodeRequest) -> Either<EncodeResponse, ()> {
    let cached = idempotency.get(&req.request_id);
    match cached {
        Some(entry) => Left(entry.cached_response),  // Replay
        None => Right(()),                            // Proceed
    }
}
```

If a cached response exists, return it; skip the rest of the plan. If not, proceed.

This check runs in a brief read transaction.

### 5. Phase 2: Embedding

Embed the text:

```rust
struct EmbeddingStep {
    text: String,
    cache_lookup: true,    // Check the cue cache
}
```

The embedder ([07. Embedding Layer](../07_embedding/00_purpose.md)) is called. May hit the cache (~10% hit rate) or compute fresh (~5-10 ms).

### 6. Phase 3: Session resolution

If the request specifies a session by name, resolve to a SessionId:

```rust
fn resolve_context(req: &EncodeRequest) -> SessionId {
    match req.session {
        ContextRef::Id(id) => id,
        ContextRef::Name(name) => {
            metadata.get_context_by_name(req.space_id, name)
                .or_else(|| create_new_session(req.space_id, name))
        }
    }
}
```

If the session doesn't exist, it's created (in the same write transaction as the encode).

### 7. Phase 4: Slot allocation

Allocate a slot in the arena ([08. Arena](../08_storage/01_arena.md) § Byte-Level Layout):

```rust
struct SlotAllocationStep {
    arena_grow_if_needed: bool,    // Yes; arena grows if at capacity
}
```

Allocation is a fast atomic operation. If the arena is near-full, growth is triggered (asynchronous; doesn't block this encode).

### 8. Phase 5: WAL append

Append the encode record to the WAL:

```rust
struct WalAppendStep {
    record: EncodeRecord {
        memory_id: MemoryId,    // Constructed from slot_id + version
        space_id: SpaceId,
        session_id: SessionId,
        text_bytes: Vec<u8>,
        vector: [f32; 384],     // 1.5 KB
        kind: MemoryKind,
        salience_initial: f32,
        edges: Vec<EdgeSpec>,
        request_id: RequestId,
    },
    fsync: true,    // Group commit
}
```

The WAL append is the durability barrier. After fsync, the encode is durable.

### 9. Phase 6: Apply

Apply the durable record to in-memory state:

```rust
struct ApplyStep {
    arena_write: bool,         // Write the vector to the slot
    metadata_write: bool,      // Insert into memories, texts tables
    hnsw_insert: bool,         // Insert into HNSW
}
```

Each sub-step happens after the durability barrier:

- Arena write: memcpy the vector to the slot. ~0.001 ms.
- Metadata write: insert in redb (in a write transaction). ~0.5 ms.
- HNSW insert: add node to the in-memory HNSW. ~0.1-1 ms.

### 10. Phase 7: Edges

For each edge in the request:

```rust
struct EdgeStep {
    edge: EdgeSpec,
    insert_in_metadata: true,    // Insert in edges and edges_reverse tables
}
```

Edge inserts are part of the same write transaction as the metadata write. Atomic.

If the edge's target memory doesn't exist, the edge is rejected (logged as a warning; the encode proceeds without it).

### 11. Phase 8: Response

Build the response:

```rust
struct ResponseStep {
    memory_id: MemoryId,
    persistent_id: bool,         // Yes; client uses this for future references
    edge_results: Vec<EdgeResult>,
}
```

The response confirms the encode and returns the new MemoryId.

### 12. The "encode now, edges later" option

If the request has many edges (>64), the planner may split the encode:

- Encode first (a fast initial response).
- Process edges in subsequent transactions.

The response indicates which edges were processed; the client may retry the rest.

Brain caps at 64 edges per encode and rejects requests with more. The split-encode mode is a future enhancement.

### 13. Plan size

A typical EncodePlan is ~500 bytes. Each step is small.

For very large texts (~1 MB), the plan size is dominated by the text itself (passed by reference, not copy, so still small).

### 14. Special encode cases

#### 14.1 Re-embedding

If the agent re-embeds an existing memory (model migration):

- This is a new ENCODE with a special `MIGRATE_OF: <existing_memory_id>` field.
- The new memory is created; the old is marked stale.

Detailed in [07. Migration](../07_embedding/06_migration.md).

#### 14.2 Bulk encode

For high-throughput bulk imports, the wire protocol's `ENCODE_BATCH` opcode (a list of ENCODE requests in one frame). The planner produces a batch plan:

- Single embedding batch (efficient).
- Single WAL group commit.
- Single metadata write transaction.
- Single HNSW batch insert.

Latency per memory drops to ~1-2 ms when batched.

#### 14.3 Implicit session

If `session` is unspecified:

```rust
let session_id = metadata.get_or_create_context(space_id, "_default");
```

Default sessions are reserved for this purpose ([10. Substrate Tables](../10_metadata/03_substrate_tables.md) § Sessions Table).

### 15. Plan validation

The planner ensures:

- The text is non-empty and within size limits.
- The kind is valid (not Consolidated; that's worker-only).
- The salience is in [0, 1].
- Edges have valid kinds and weights.
- The space's quotas allow another memory.

Invalid plans return errors immediately; no work is done.

### 16. The encode latency

For a typical encode (no batching):

| Phase | Latency |
|---|---|
| Idempotency check | 5-10 µs |
| Embedding (cache hit) | 5 µs |
| Embedding (cache miss) | 5-10 ms |
| Session resolution | 5 µs |
| Slot allocation | 1 µs |
| WAL append + fsync | 0.5 ms (group commit) |
| Apply (arena, metadata, HNSW) | 1-2 ms |
| Edges (10 edges) | 0.5 ms |
| Response | 50 µs |
| **Total (cache miss)** | **~7-13 ms** |
| **Total (cache hit)** | **~2-3 ms** |

Embedding dominates the no-cache case.

### 17. The "lazy edges" option

For agents that ENCODE first and LINK later (a common pattern), Brain doesn't add overhead — both paths are first-class.

But for agents that always encode with edges, embedding+single-write is more efficient than two round trips. Brain makes the in-encode edges path fast.

## PLAN and REASON Planning

How the planner builds plans for the higher-level operations.

### 1. Higher-level vs lower-level

ENCODE, RECALL, FORGET, LINK are direct operations on Brain's primitives. PLAN and REASON are higher-level — they compose multiple primitive operations.

- **PLAN**: given a goal, find a sequence of memories that connect to it via the graph.
- **REASON**: given a query, find supporting and contradicting memories.

These don't introduce new primitive types of work; they orchestrate RECALL and graph traversal.

### 2. The PLAN request shape

```rust
struct PlanRequest {
    goal_text: String,           // What to plan toward
    space_id: SpaceId,
    starting_state: Option<String>,  // Current state
    max_depth: usize,            // How many graph hops
    max_results: usize,          // Total plan elements
    edge_kinds: Vec<EdgeKind>,   // Which edges to follow (default: CAUSED, FOLLOWED_BY)
    request_id: Option<RequestId>,
}
```

The semantics: "starting from memories similar to the current state, traverse the graph along edges of the specified kinds, returning paths to memories similar to the goal".

### 3. The PLAN execution

The planner builds a multi-step plan:

```rust
struct PlanPlan {
    embedding: EmbeddingStep,             // Embed both starting_state and goal
    starting_recall: RecallStep,          // Find memories near starting_state
    goal_recall: RecallStep,              // Find memories near goal
    traversal: TraversalStep,             // BFS/DFS along edges
    scoring: ScoringStep,                 // Rank found paths
    response: ResponseStep,
}
```

Each step is a sub-task; they run in sequence with some parallelism (the two RECALLs can be parallel).

### 4. PLAN traversal

The traversal step:

```rust
fn traverse(
    starts: Vec<MemoryId>,
    goals: Vec<MemoryId>,
    edge_kinds: Vec<EdgeKind>,
    max_depth: usize,
) -> Vec<Path> {
    // Bidirectional BFS:
    //   forward from starts (following edges in their direction)
    //   backward from goals (following edges against their direction)
    //   intersect to find paths
    
    let mut forward_frontier = starts.clone();
    let mut backward_frontier = goals.clone();
    let mut found_paths = Vec::new();

    for depth in 0..max_depth {
        let next_forward = expand_frontier(forward_frontier, edge_kinds, Direction::Forward);
        let next_backward = expand_frontier(backward_frontier, edge_kinds, Direction::Backward);

        // Check intersection
        for memory in next_forward.intersect(&next_backward) {
            found_paths.push(reconstruct_path(starts, memory, goals));
        }
        if !found_paths.is_empty() && depth >= 1 {
            break;
        }

        forward_frontier = next_forward;
        backward_frontier = next_backward;
    }
    found_paths
}
```

The traversal uses the metadata store's `edges` and `edges_reverse` tables for graph queries.

### 5. Bidirectional BFS

For typical agent graphs (many memories, sparse connectivity), bidirectional BFS is much more efficient than unidirectional. Each direction explores `b^(d/2)` nodes (branching factor `b`, total depth `d`), versus `b^d` for unidirectional.

For typical graphs with `b ≈ 8` and `d ≈ 4`, that's 64+64 = 128 nodes vs 4096. ~30× savings.

### 6. Path scoring

Multiple paths may exist between starts and goals. Brain scores them:

```rust
fn score_path(path: &Path) -> f32 {
    let length_score = 1.0 / (path.length as f32);
    let edge_score = path.edges.iter().map(|e| e.weight).product();
    let salience_score = path.memories.iter().map(|m| m.salience).product().powf(1.0 / path.memories.len() as f32);
    
    length_score * edge_score * salience_score
}
```

Higher score = more useful path. Brain returns the top N paths.

### 7. Search vs traversal

The starting and goal RECALLs use HNSW for vector similarity. The traversal uses the metadata's edge tables for graph hops.

These are different storage layers:
- HNSW: in-memory vector index.
- Edge tables: redb B-trees.

The planner orchestrates the alternation: vector similarity at the boundaries, graph hops in the middle.

### 8. The REASON request shape

```rust
struct ReasonRequest {
    query_text: String,
    space_id: SpaceId,
    max_supporting: usize,       // Default 5
    max_contradicting: usize,    // Default 5
    request_id: Option<RequestId>,
}
```

REASON returns:
- Memories supporting the query (similar in vector space, with positive supports/derived_from edges).
- Memories contradicting the query (with CONTRADICTS edges, or significantly different in vector space with high salience).

### 9. The REASON execution

```rust
struct ReasonPlan {
    embedding: EmbeddingStep,
    base_recall: RecallStep,                  // Top similar memories
    supports_traversal: TraversalStep,        // Follow SUPPORTS, DERIVED_FROM edges
    contradicts_traversal: TraversalStep,     // Follow CONTRADICTS edges
    aggregation: AggregationStep,             // Score and rank
    response: ResponseStep,
}
```

The two traversals run in parallel after the base RECALL.

### 10. The "explainable" response

REASON's response includes evidence:

```rust
struct ReasonResponse {
    supporting: Vec<EvidenceItem>,
    contradicting: Vec<EvidenceItem>,
    confidence: f32,    // Aggregate; based on relative weights
}

struct EvidenceItem {
    memory_id: MemoryId,
    text: Option<String>,
    score: f32,
    edge_path: Vec<EdgeKind>,  // How this connects to the query
}
```

This shape makes the reasoning interpretable — agents can show their work.

### 11. Cost considerations

PLAN and REASON do more work than RECALL:

| Operation | Typical latency |
|---|---|
| RECALL (K=10) | 10-15 ms |
| PLAN (max_depth=4) | 30-100 ms |
| REASON (default) | 30-50 ms |

The latency is mostly graph traversal. For deep traversals (max_depth > 5), latency grows quickly. The planner caps depth conservatively.

### 12. Caching of intermediate results

For PLAN and REASON, intermediate results (e.g., the starting RECALL's outputs) might be useful in subsequent calls. Brain doesn't cache them — each call is independent.

If a workload makes many PLAN/REASON calls with the same starting state, an external caching layer can amortize.

### 13. The "max_results" cap

PLAN and REASON limit the response size:

- PLAN: `max_results` total path nodes returned.
- REASON: `max_supporting + max_contradicting` evidence items.

These caps protect against pathological queries (a goal connected to thousands of paths). The defaults are conservative: 10-20 results.

### 14. Plan validity

The planner checks:

- max_depth ≤ 10 (hard limit; deeper isn't useful and is expensive).
- edge_kinds are valid.
- max_results ≤ 100.

Out-of-bounds → error response.

### 15. The "explain" facility

Both PLAN and REASON benefit from explainability. The response can include:

- The intermediate RECALL results.
- The traversal path.
- The scoring breakdown.

This is opt-in via `explain=true` in the request. It increases response size but helps debugging.

### 16. The "no result" path

If the traversal finds no paths (PLAN) or no evidence (REASON), the response is empty (or a partial/uncertain answer). This isn't an error — it just means Brain has nothing to offer.

For workloads that need confident answers, the response includes a `confidence` score the agent can threshold on.

## FORGET Planning

How the planner builds an execution plan for a FORGET request.

### 1. The FORGET request shape

```rust
struct ForgetRequest {
    target: ForgetTarget,         // What to forget
    space_id: SpaceId,
    mode: ForgetMode,             // Soft / Hard
    request_id: RequestId,
}

enum ForgetTarget {
    Memory(MemoryId),
    Memories(Vec<MemoryId>),
    Filter(ForgetFilter),         // E.g., all memories in a session with salience < 0.1
}

enum ForgetMode {
    Soft,    // Tombstone; reclaim after grace
    Hard,    // Tombstone, zero vector and text immediately
}
```

### 2. Two planning paths

#### 2.1 Forget by ID (or list of IDs)

```rust
struct ForgetByIdPlan {
    memory_ids: Vec<MemoryId>,
    mode: ForgetMode,
    shard_routes: Vec<(ShardId, Vec<MemoryId>)>,    // Group by shard
    per_shard: Vec<ForgetShardStep>,
}
```

The planner groups memory IDs by shard and produces a sub-plan per shard. Each shard processes its memories independently.

#### 2.2 Forget by filter

```rust
struct ForgetByFilterPlan {
    filter: ForgetFilter,
    mode: ForgetMode,
    discovery_step: DiscoveryStep,    // First, list matching memories
    forget_step: ForgetByIdStep,      // Then, forget them
}
```

The plan first discovers matching memories (a query-like step) then forgets them.

### 3. The per-shard forget step

```rust
struct ForgetShardStep {
    shard_id: ShardId,
    memory_ids: Vec<MemoryId>,
    wal_records: Vec<ForgetRecord>,    // One per memory
    metadata_updates: Vec<MetadataUpdate>,
    arena_updates: Vec<ArenaUpdate>,   // Tombstone flags
    hnsw_updates: Vec<HnswUpdate>,     // Mark removed
}
```

The shard processes the IDs in a single batch:
- Single WAL group commit.
- Single metadata write transaction.
- Batched arena tombstones.
- Batched HNSW marks.

For a batch of 1000 IDs: ~5-10 ms total.

### 4. Hard forget specifics

Hard forget zeros the vector and text:

```rust
struct HardForgetStep {
    arena_zero_vectors: Vec<u64>,    // Slot IDs to zero
    text_zero: Vec<MemoryId>,        // Texts to zero out
}
```

Zeroing happens in addition to tombstoning. Performed before the WAL fsync — the WAL record indicates "hard forget" so recovery knows to apply zeroing too.

### 5. The "forget by filter" two-phase

Phase 1: discovery.

```rust
struct DiscoveryStep {
    filter: ForgetFilter,
    list_max: usize,                 // Cap to avoid runaway
    use_metadata_iteration: bool,    // Iterate metadata table; not HNSW
}
```

Listing matching memories is a metadata-table operation. Brain iterates the relevant table (e.g., scan `memories` and apply the filter). For large shards, this is expensive.

Phase 2: forget.

After discovery, Brain has a list of memory IDs. It runs the "forget by ID" path on them.

### 6. The bulk-forget cap

`ForgetFilter` is bounded:

- A single FORGET request can affect at most 100,000 memories.
- Beyond this, the request fails with `TooManyMemories`.

For larger bulk operations, the operator uses `ADMIN_CONTEXT_DELETE` (which does its own staged processing) or scripts a sequence of capped FORGETs.

### 7. The idempotency check

Before doing work, check the idempotency table:

```rust
fn idempotency_check(req: &ForgetRequest) -> Either<ForgetResponse, ()> {
    if let Some(prior) = idempotency.get(&req.request_id) {
        return Left(prior.cached_response);
    }
    Right(())
}
```

Same as for ENCODE. If duplicate, replay; else proceed.

### 8. The shard routing

Each memory ID is routed to its shard:

```rust
fn route(memory_ids: Vec<MemoryId>) -> Vec<(ShardId, Vec<MemoryId>)> {
    let mut grouped = HashMap::new();
    for id in memory_ids {
        let shard = router.shard_for_memory(id);
        grouped.entry(shard).or_insert_with(Vec::new).push(id);
    }
    grouped.into_iter().collect()
}
```

The MemoryId encodes the shard ([02. Memory](../02_data_model/02_memory.md)), so routing is O(1) per ID.

### 9. The cross-shard fan-out

For a forget across shards, the planner produces parallel sub-plans. The executor runs them in parallel.

If one shard fails (write error), the other shards' forgets still proceed. The response indicates per-memory-ID success/failure.

### 10. The per-memory error tolerance

If a memory-ID isn't found (already forgotten or never existed), Brain logs and continues:

```rust
match metadata.get(&memory_id) {
    Some(m) if m.is_active() => proceed_with_forget(m),
    Some(m) => log_warning("Memory already tombstoned"),    // No-op
    None => log_warning("Memory not found"),                // No-op
}
```

The response indicates which IDs were processed.

This makes FORGET idempotent at the per-ID level: re-forgetting a tombstoned memory is a no-op.

### 11. Cascading edge handling

When a memory is forgotten, what happens to its edges?

- Outgoing edges from the forgotten memory: tombstoned (the source is gone).
- Incoming edges to the forgotten memory: tombstoned.

The maintenance worker eventually cleans up tombstoned edges. Until then, queries that traverse these edges see them as "leading to a tombstoned memory" and skip.

### 12. Cascade options

A future option:

- **Cascade forget**: forgetting memory M also forgets memories that DERIVED_FROM M.
- **Restrict forget**: if memory M has incoming DERIVED_FROM edges, forgetting M is rejected (would orphan derived memories).

Both are nuanced semantics that need careful design. Brain does not impose them currently; deletes are unconstrained from Brain's perspective.

### 13. The arena tombstone vs metadata

The arena tombstone is set first (in-memory; before the WAL). This prevents new searches from returning the slot during the brief window before the metadata is updated.

If the arena tombstone is set but the WAL fsync fails, recovery rolls back the in-memory state (the WAL record was never committed).

### 14. The HNSW update timing

HNSW node removal (the `mark_removed` flag) happens after the metadata commit. Ordering:

```
1. WAL fsync of FORGET record.
2. Arena slot tombstoned.
3. Metadata commit (memory's flags updated, forgot_at set).
4. HNSW node marked removed.
5. Acknowledge.
```

If a crash happens between 3 and 4, recovery re-applies step 4 by replaying from the WAL.

### 15. The grace period and reclaim

Soft FORGET starts a grace period. After the grace (default 7 days), the maintenance worker reclaims the slot:

```rust
fn reclaim(memory_id: MemoryId) {
    let mut wtxn = db.begin_write()?;
    delete_from_memories(memory_id);
    delete_from_texts(memory_id);
    delete_associated_edges(memory_id);
    increment_slot_version(memory_id.slot_id());
    wtxn.commit()?;

    add_to_arena_free_list(memory_id.slot_id());
}
```

The reclaim is a separate operation, not part of the original FORGET's plan.

### 16. The forget latency

For a single FORGET:

| Phase | Latency |
|---|---|
| Idempotency check | 5-10 µs |
| WAL append + fsync | 0.3 ms (group commit) |
| Arena tombstone | 0.001 ms |
| Metadata commit | 0.5 ms |
| HNSW mark removed | 0.1 ms |
| Response | 50 µs |
| **Total** | **~1 ms** |

Hard forget adds ~0.001 ms (zeroing). Negligible.

For a batched forget of 100 IDs: ~3-5 ms total (single WAL commit; single metadata transaction).

### 17. The plan size

A typical FORGET plan is ~200 bytes for a single ID; ~10 KB for 1000 IDs (mostly the ID list).

### 18. Plan validation

The planner checks:

- Memory IDs are well-formed.
- The caller's space owns the memories (the `space_id` on the row must match the request's effective space).
- The request's RequestId is set.
- For filter mode: the filter is well-formed.

Invalid → error response immediately.

---

*Continue to [`03_cost_estimation.md`](03_cost_estimation.md) for cost estimation.*
