# 05.03 Read Pipeline

The read-side cognitive primitives: RECALL (similarity search), PLAN (graph paths from start to goal), and REASON (supporting and contradicting evidence). These all start from a vector lookup and then differ in how they traverse the edge graph.

## RECALL

The RECALL primitive: find memories by similarity. **One verb, one code path** — every request walks the same pipeline regardless of whether a user schema has been declared.

### 1. Semantic contract

```
RECALL(cue_text, agent_id, k, filter, ...) → Vec<RecallResult>
```

Brain runs a single pipeline on every request:

```
RECALL → validate → embed cue → fan out to three retrievers
       (semantic / lexical / graph, all always-wired)
       → RRF fusion (k=60)
       → filter chain (tombstone, kind, context, temporal,
         confidence, salience, supersession)
       → metadata enrichment from redb
       → optional cross-encoder rerank
         (if request.rerank == true AND CrossEncoderSlot::Enabled)
       → wire response
```

The three retrievers are mandatory shard wiring — they are never `None`. If `request.rerank == true` and the cross-encoder is `Disabled` (operator opt-out), the request fails fast with `CapabilityNotEnabled { capability: "rerank" }`; there is no silent fallback. Schema declarations do not gate any stage of this pipeline. They only narrow what `STATEMENT_CREATE` / `RELATION_CREATE` and predicate-aware filters accept.

#### In-transaction read-your-writes overlay

When `req.txn_id` is set, the txn's pending ENCODE buffer is overlaid on the committed retrieval result before the response is built:

- Tombstoned ids in the buffer drop committed hits.
- Pending encodes are scored against the cue vector and merged with the committed list.
- The combined list is re-sorted by similarity (descending) and trimmed to `top_k`.

This is the single read-your-writes path; the same overlay runs whether or not a schema is active.

### 2. The arguments

#### cue_text

The query. Embedded with the same model used for stored memories.

The cue can be a single word, a sentence, a longer document — whatever the agent thinks is a useful query. Note that very short cues may be ambiguous and produce broad results; very long cues are truncated by the embedder.

#### agent_id

The owning agent. Returns are scoped to this agent's memories.

#### k

How many results. Default 10. Max 1000.

#### filter

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

#### include_text

Whether to return the memory text in the response. Default false.

If true, Brain fetches text from the metadata store. Adds ~50 µs per result.

#### include_metadata

Whether to include extra metadata fields. Default false.

#### consistency

Either `Eventual` (default) or `ReadAfterWrite`.

With ReadAfterWrite, the recall waits for the most recent writes to be searchable.

#### confidence_min

Optional. Filter results with similarity score below this threshold. Useful when the agent only wants strong matches.

### 3. The response

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

### 4. Score semantics

Score = `1 - cosine_distance(cue_vec, mem_vec)` for normalized vectors.

Range: typically 0 to 1 in practice (vectors don't usually point opposite). 1.0 means identical; 0.0 means orthogonal; negative means opposite (rare).

Heuristic interpretation:
- > 0.9: very similar (often near-duplicate).
- 0.7-0.9: similar topic, related content.
- 0.5-0.7: same general area.
- < 0.5: weakly related.

These aren't strict thresholds; they depend on the model and the corpus. Agents tune `confidence_min` to their use case.

### 5. The "fewer than K" case

If Brain finds fewer than K matching memories, the response has fewer than K results. This is normal for:
- Small or new agents.
- Selective filters.
- Very specific cues.

It's not an error.

### 6. The "empty result" case

Zero results. Possible if:
- The agent has no memories.
- All memories are tombstoned.
- All memories have a different model fingerprint (after a model upgrade).
- The filter is too restrictive.

The response is an empty list, not an error.

### 7. Filter semantics

Filters are AND-combined:

```
result matches filter ⇔
  (filter.kind is None or result.kind == filter.kind)
  AND (filter.contexts is None or result.context in filter.contexts)
  AND (filter.min_salience is None or result.salience >= filter.min_salience)
  AND ... 
```

For OR semantics (e.g., "Episodic or Semantic"), use multiple filters and merge in the agent.

### 8. The "fingerprint_match" default

By default, RECALL returns memories with the current model's fingerprint. Memories from older models are excluded.

This is a safety feature: cross-model similarity isn't meaningful.

To search across models (rarely useful, mostly for debugging or migration), set `fingerprint_match: false`.

### 9. The "salience" effect

Currently, RECALL returns purely by similarity score. Salience is filtered (if `min_salience` is set) but doesn't directly affect ranking.

A future option (open question): blend salience and similarity in ranking. Not currently implemented.

### 10. The "recency" effect

Similar: recency (age) is filterable but doesn't affect ranking. Brain doesn't auto-favor recent memories.

If the agent wants recent-favoring, it can:
- Use `max_age` to filter.
- Re-rank results in the agent layer.

### 11. The "context boost" effect

The agent might want memories in the current context to rank higher. Brain doesn't do this automatically. The agent can:

1. RECALL with no context filter; get K results.
2. RECALL with the context filter; get K results.
3. Merge in the agent layer with weights.

Or use a single RECALL with explicit `contexts: Some([current])`.

### 12. The "across-shard" recall

For agents whose data spans multiple shards (rare), RECALL fans out:

- Each shard runs its sub-recall in parallel.
- Results are merged by score.

The response is the global top K.

This is transparent to the agent — it sees a single result list.

### 13. Latency

For typical workloads (single-shard, K=10, no complex filter):

- p50: ~10 ms.
- p99: ~25 ms.

For larger K or complex filters: latency rises proportionally. K=100 takes ~15 ms typical; K=1000 takes ~30 ms.

For cross-shard recalls (2-3 shards): p99 rises to ~30-50 ms.

### 14. Throughput

A shard handles ~5K-20K RECALLs per second. Limited by:

- Embedder throughput (with cache, much higher).
- HNSW search latency.

For higher throughput, scale shards.

### 15. The "include_text=true" cost

Including text fetches each result's text from the metadata store:

- Per-result cost: ~5-20 µs (cache-dependent).
- For K=10: ~100 µs additional.
- For K=100: ~1 ms additional.

For very large texts (~MB each), the response size grows correspondingly.

### 16. The "include_metadata=true" cost

Similar to text, but for the extra metadata fields. Usually small (~tens of bytes per memory).

### 17. The "tags" filter

Tags are agent-defined strings stored in the memory's metadata. Filter:

```
filter.tags = Some(vec!["urgent".to_string(), "personal".to_string()])
```

Returns memories that have ALL the specified tags (intersection). For "any of these tags" (union), make multiple recalls.

Tags are filtered post-search; selective tag filters need higher ef_search (the planner adjusts automatically).

### 18. The "score-only" mode

For agents that want just IDs and scores (no text, no metadata), the default is fine — text and metadata are off by default. The response is small and fast.

### 19. The "no result" semantics

If the agent gets zero results, possible interpretations:

- The agent has no relevant memories.
- The cue is unusual (no similar memories).
- The filter is too tight.

Brain doesn't distinguish these. The agent decides what to do — broaden the cue, relax the filter, or accept no results.

### 20. The "two-stage" pattern

Some agents do:

1. RECALL with K=100 to get a broad set.
2. Re-rank with custom logic on the agent side.

Brain's K=100 isn't much more expensive than K=10. The agent gets flexibility.

For very large K (>100), make sure to consider cost (K=1000 is ~3× the cost of K=10).

## PLAN

The PLAN primitive: find paths through the memory graph from a starting state to a goal.

### 1. Semantic contract

```
PLAN(goal_text, starting_state, agent_id, max_depth, edge_kinds, ...) → Vec<Path>
```

Brain:

1. Embeds the starting state and goal.
2. Finds memories near the starting state and memories near the goal.
3. Traverses the edge graph from start side and goal side (bidirectional BFS).
4. Returns paths where the two sides intersect.

A "path" is a sequence of memories connected by edges, leading from a start memory to a goal memory.

### 2. The arguments

#### goal_text

What the agent is planning toward. A description of the desired end state.

#### starting_state

What the agent is currently doing or thinking. A description of the present state.

If unspecified, Brain uses recent high-salience memories as starting points (defaulting to the agent's "implicit current state").

#### agent_id

The owning agent. Plans are scoped to this agent's memories and edges.

#### max_depth

How many graph hops to traverse. Default 4; max 10.

Greater depth = more thorough search but more cost. Brain caps at 10 to avoid pathological queries.

#### max_results

How many paths to return. Default 5; max 100.

#### edge_kinds

Which edge types to traverse. Default: `CAUSED, FOLLOWED_BY, DERIVED_FROM, PART_OF`. These are the "actionable" edges that suggest forward movement.

The agent can specify a different list — e.g., `[REFERENCES]` for citation chains.

#### scoring

Optional scoring weights:

```rust
struct PlanScoring {
    length_weight: f32,        // Default 1.0; longer paths penalized
    edge_weight_weight: f32,   // Default 1.0; edge weights matter
    salience_weight: f32,      // Default 0.5; salient memories preferred
}
```

### 3. The response

```rust
struct PlanResponse {
    paths: Vec<Path>,
    starting_memories: Vec<MemoryId>,    // What was used as start
    goal_memories: Vec<MemoryId>,        // What was used as goal
    confidence: f32,                     // Aggregate confidence
}

struct Path {
    nodes: Vec<MemoryId>,                // In order from start to goal
    edges: Vec<EdgeKind>,                // Edge types between nodes
    score: f32,                          // Higher = better path
    length: usize,                       // Number of hops
}
```

Paths are sorted by score, descending.

### 4. Path semantics

A path of length 3:

```
start_memory --CAUSED--> A --FOLLOWED_BY--> B --PART_OF--> goal_memory
```

The path connects (start_memory ≈ starting_state) to (goal_memory ≈ goal). Intermediate nodes are stepping stones.

The score reflects:
- Path length (shorter is generally better).
- Edge weights along the path.
- Salience of intermediate nodes.

### 5. Bidirectional BFS

The traversal:
- Forward: from each starting memory, follow edges in their forward direction.
- Backward: from each goal memory, follow edges in their reverse direction.
- Intersect: when forward and backward frontiers meet, a path is found.

Bidirectional cuts the cost from O(b^d) to O(b^(d/2)), where b is branching factor and d is depth.

For typical agent graphs (b≈8, d=4): ~64 nodes explored each way vs ~4000 unidirectional.

### 6. The "no paths found" case

If no path exists within max_depth, the response has empty `paths`:

- `paths: []`
- `starting_memories` and `goal_memories` are populated (so the agent can see what was attempted).
- `confidence: 0.0`.

This tells the agent: "I see your start and goal, but I can't connect them in my memory."

### 7. The "starting_state is empty" case

When starting_state is unspecified, Brain uses:

```
top-K most salient recent memories (default K=5, recency window 24h)
```

This is "what's on the agent's mind right now" — a soft proxy for the agent's current context.

For agents that want explicit control, always pass `starting_state`.

### 8. The "goal not encoded yet" case

The goal is a text description; it doesn't need to be a stored memory. Brain embeds the goal text and finds nearby memories as anchors for the goal side of the BFS.

If no memory is similar to the goal (low scores), the BFS has weak goal anchors. PLAN may return no paths.

### 9. Edge direction semantics

Edges have a defined direction (see [02.05 Edges](../02_data_model/05_edges.md)):

| Edge kind | Forward semantic |
|---|---|
| CAUSED | source led to target |
| FOLLOWED_BY | source then target |
| DERIVED_FROM | target derived from source |
| PART_OF | source is part of target |
| REFERENCES | source mentions target |
| ... | ... |

PLAN's forward traversal follows edges in their forward direction; backward traversal goes against. So a path:

```
A --CAUSED--> B
B --FOLLOWED_BY--> C
```

is "A caused B, then B was followed by C". Logical forward sequence.

### 10. Path scoring

```
score = length_score × edge_score × salience_score

length_score = 1 / path_length         (shorter is better)
edge_score = product(edge.weight)      (high-confidence edges matter)
salience_score = geomean(node.salience) (salient intermediate nodes preferred)
```

The score is in (0, 1]. Brain returns paths sorted by score.

The agent can re-rank in its own layer with custom weights if the default doesn't fit.

### 11. The "best_n_per_endpoint" rule

When multiple paths exist between the same start and goal, Brain returns up to N best (default 3 per start-goal pair). This avoids returning many similar paths.

For diverse-paths use cases (the agent wants alternatives, not just the best), the agent can request more (`max_results`) and Brain picks across endpoints.

### 12. Latency

For typical PLAN with max_depth=4:

- p50: ~30-50 ms.
- p99: ~80-100 ms.

The latency is dominated by:
- Two embeddings (start + goal, parallel): ~10 ms.
- Two RECALLs (parallel): ~10 ms.
- Graph traversal (~10-20 ms for typical graphs).

For deeper PLAN (max_depth=8): can reach 200+ ms. Brain's cost-budget check (in [12.03 Cost Estimation](../12_query_optimizer/03_cost_estimation.md)) may reject overly-expensive plans.

### 13. The "explain" option

With `explain=true`, the response includes:

- The intermediate frontier expansions.
- Paths that were considered but didn't make the top results.
- The scoring breakdown for each returned path.

Useful for debugging or showing reasoning to a human.

### 14. The "actionable edges" default

The default `edge_kinds: [CAUSED, FOLLOWED_BY, DERIVED_FROM, PART_OF]` are the "actionable" or "forward" edges. They suggest progression.

Other edges (REFERENCES, SIMILAR_TO, SUPPORTS, CONTRADICTS) are more associative; they're not great for planning.

For exploratory queries (e.g., "what's related to my goal?"), use REASON instead of PLAN.

### 15. The "stale plan" caveat

A PLAN's results reflect the current state of the graph. If the agent encodes new memories or links between calls, subsequent PLANs may give different results.

Brain doesn't cache PLAN results. Each call sees the current graph (eventual consistency, ~10 ms publication lag).

### 16. The "self-loop" guard

The traversal avoids self-loops:

- A path doesn't visit the same memory twice.
- The forward and backward expansions skip already-visited nodes.

This prevents infinite loops in cyclic graphs.

### 17. Failure modes

#### NoPathsFound

Not technically a failure — the response just has empty `paths`. The agent should handle this gracefully.

#### QueryTooExpensive

If the planner estimates the PLAN exceeds the cost budget (typically due to high max_depth + dense graph), it returns this error.

The agent should reduce max_depth or narrow the start/goal.

#### Timeout

If the traversal takes too long, Brain aborts and returns whatever paths it found so far. The response is marked `partial: true`.

### 18. The "PLAN as discovery" use case

PLAN is most useful when:

- The agent has built up a graph of CAUSED, FOLLOWED_BY, etc. relationships.
- The agent has a clear goal and wants to find a path.
- The graph is dense enough that paths exist.

For sparse graphs (few edges), PLAN often returns no paths. The agent should use RECALL or REASON instead.

For text-only memories without edges, PLAN is mostly useless. The graph is the planning substrate.

## REASON

The REASON primitive: find supporting and contradicting memories for a query.

### 1. Semantic contract

```
REASON(query_text, agent_id, max_supporting, max_contradicting, ...) → ReasonResponse
```

Brain:

1. Embeds the query text.
2. Finds memories near the query (the "base set").
3. From the base set, follows SUPPORTS / DERIVED_FROM edges to find supporting evidence.
4. From the base set, follows CONTRADICTS edges to find opposing evidence.
5. Aggregates and returns evidence with scores and confidence.

### 2. The arguments

#### query_text

The claim or question. Brain doesn't parse it as a logical proposition — it's just text to embed and lookup.

#### agent_id

The owning agent. Reasoning is scoped to this agent's memories.

#### max_supporting

How many supporting items. Default 5; max 50.

#### max_contradicting

How many contradicting items. Default 5; max 50.

#### include_text

Whether to return memory text in the response. Default true (REASON is meant to be interpretable).

#### confidence_min

Optional. Filter out evidence with low individual confidence (similarity score below threshold).

### 3. The response

```rust
struct ReasonResponse {
    supporting: Vec<EvidenceItem>,
    contradicting: Vec<EvidenceItem>,
    confidence: f32,                 // Aggregate; balance of evidence
    base_memories: Vec<MemoryId>,    // The seed memories
}

struct EvidenceItem {
    memory_id: MemoryId,
    text: Option<String>,
    score: f32,                      // Individual confidence (0..1)
    edge_path: Vec<EdgeKind>,        // How this connects to the query
    distance: usize,                 // Graph distance from base set
}
```

### 4. The "supporting" semantics

A memory is "supporting" if:

- It's directly similar (high score) to the query.
- AND/OR it's reached from the base set via SUPPORTS or DERIVED_FROM edges.

Both kinds of evidence are returned. Similarity-only support is weaker (just thematic relevance). Edge-traversed support is stronger (explicit assertion).

### 5. The "contradicting" semantics

A memory is "contradicting" if:

- It's reached from the base set via CONTRADICTS edges.
- OR it's similar in topic but with significantly different content (this is harder to detect; see § 11).

Brain primarily uses CONTRADICTS edges. Vector-distance-based contradiction is research-grade and not reliable enough.

### 6. The aggregate confidence

The aggregate `confidence` is roughly:

```
support_strength = sum(supporting.score)
contradict_strength = sum(contradicting.score)

confidence = (support_strength - contradict_strength) / (support_strength + contradict_strength)
```

Range: -1 (all contradicting) to +1 (all supporting). 0 means balanced.

This is a heuristic. Agents shouldn't use confidence as a hard truth value — it's a hint about the balance of evidence.

### 7. The "base memories" output

The response includes which memories were the seeds:

- `base_memories`: top similar memories to the query.
- These are the starting points for evidence traversal.

Agents can use this to verify Brain is reasoning about the right topic.

### 8. The "edge_path" output

For each evidence item, the response shows how it relates to the base:

- `[]`: directly similar (no edge traversal).
- `[SUPPORTS]`: one hop through a SUPPORTS edge.
- `[DERIVED_FROM, SUPPORTS]`: two hops.

Up to depth 2 by default. Longer paths are weaker evidence.

### 9. Latency

For typical REASON:

- p50: ~30 ms.
- p99: ~70 ms.

The latency is similar to PLAN but typically faster because depth is smaller (default 2 vs 4) and edge types are fewer.

### 10. The "no contradicting evidence" case

Often, REASON finds support but no contradictions. The response has `contradicting: []`. This indicates the agent's memory is consistent with the query.

If the agent's memory is biased (only one perspective is encoded), REASON's responses will be biased too. Brain doesn't fact-check the memory.

### 11. The "no support, no contradiction" case

If the query is about something the agent has no memory of:

- `supporting: []`
- `contradicting: []`
- `confidence: 0.0`
- `base_memories: []` (no similar memories found).

The agent should interpret this as "I don't know — I have no memory about this".

### 12. The vector-distance contradiction question

Vector distance as a contradiction signal has been considered:

- Memory M is similar to the query in topic (mid-range score).
- But its content vector points in a noticeably different direction.

This is research-grade. It tends to flag false positives (similar topic, different angle, but not actually contradicting).

Brain does not currently do this. CONTRADICTS edges (explicitly created by the agent or by a downstream LLM) are the contradiction signal.

A future enhancement: integrate with an LLM-based contradiction detector. Brain would generate candidate pairs (query + memory) and let an external LLM judge contradiction. Out of scope at present.

### 13. The "explain" option

With `explain=true`, the response includes:

- Why each evidence item was selected.
- Which edges were traversed.
- Per-edge confidence.

Useful for showing reasoning chains to humans or other systems.

### 14. The "different from PLAN" semantic

PLAN: "how do I get from A to B?" — finds connections.
REASON: "what supports/contradicts X?" — finds evidence.

Different goals, similar mechanics (both traverse the graph). The edge sets are different:

- PLAN: forward edges (CAUSED, FOLLOWED_BY).
- REASON: associative edges (SUPPORTS, CONTRADICTS, DERIVED_FROM).

### 15. The "REASON about a memory" pattern

A common pattern: the agent has a specific memory and wants evidence for or against it. Two approaches:

1. Use the memory's text as the query to REASON.
2. Use REASON-by-id (a future addition; not currently implemented).

Currently, the agent passes the memory's text. Brain embeds it and reasons; results may include the memory itself in the base set.

### 16. The "confidence is a hint" warning

The aggregate confidence is a rough indicator. It's not:

- A probability of truth.
- A measure of Brain's certainty about the world.
- A score the agent should use as a hard cutoff.

It reflects the balance of stored memories. If the memory is wrong, biased, or incomplete, the confidence is too.

Agents should treat confidence as one input among many, not the final word.

### 17. The "edge weight" effect

Edges have weights. REASON uses them in scoring:

```
evidence_strength = base_similarity × product(edge.weight along path)
```

A high-weight SUPPORTS edge contributes more than a low-weight one. Agents that create edges with calibrated weights get better REASON results.

### 18. The "REASON without memories" case

If the agent has zero memories matching the query:

- `base_memories: []`.
- `supporting: []`, `contradicting: []`.
- `confidence: 0.0`.

Brain isn't generating evidence — only retrieving. No memories means no reasoning.

### 19. The "small evidence" case

For agents with few memories on a topic, REASON returns weak evidence:

- A handful of items with low scores.
- Confidence near zero either way.

The agent can use this as a signal to seek more information (encode more memories from external sources, do web searches, etc.).

---

*Continue to [`04_transactions.md`](04_transactions.md) for transactional brackets.*
