# 05.03 Read Pipeline

The read-side cognitive primitives: RECALL (similarity search), PLAN (graph paths from start to goal), and REASON (supporting and contradicting evidence). These all start from a vector lookup and then differ in how they traverse the edge graph.

## RECALL

The RECALL primitive is Brain's **sole primary read verb**: ask the memory a
question, get the answer. **One verb, one code path** — every request walks the
same pipeline regardless of whether a user schema has been declared. There is no
second client read verb; RECALL returns the answer as a membership shape
(Single / Many / None), not a ranked candidate list the caller has to sift.

### 1. Semantic contract

```
RECALL(cue_text, agent_id, filter, max_results, ...) → RecallAnswer
```

Brain runs a single pipeline on every request:

```
RECALL → validate → embed cue → fan out to three retrievers
       (semantic / lexical / graph, all always-wired)
       → RRF fusion (k=60)
       → filter chain (tombstone, kind, context, temporal,
         confidence, salience, supersession)
       → metadata enrichment from redb
       → cross-encoder rerank (always-on when the model is loaded)
       → membership: keep the answer set inside a relevance band,
         shape by cardinality → Single / Many / None
       → wire response
```

The three retrievers are mandatory shard wiring — they are never `None`. The
cross-encoder rerank runs on every read whenever the model is loaded; there is
no request flag. The only control is the deploy-time `config.rerank.enabled`
load gate — when the operator opts out, no model loads and the pipeline returns
RRF-only ordering (no error). Schema declarations do not gate any stage of this
pipeline. They only narrow what `STATEMENT_CREATE` / `RELATION_CREATE` and
predicate-aware filters accept.

Crucially, membership is computed over the **full filtered candidate pool**, not
a fixed top-K window. The relevance band (not a count) decides which memories
belong to the answer; `max_results` is only a safety ceiling on how many members
are returned, never the criterion that shapes the answer.

### Recall scope — space (default) vs namespace-wide

RECALL carries a `scope` selector:

```rust
enum RecallScope { Space, Namespace }   // wire default: Space
```

- **`Space`** (default, unchanged): the request is served by the single shard the
  caller's `(namespace, space)` hashes to (`shard_for_space`), and the pipeline
  above runs once on that shard.
- **`Namespace`**: the request spans **every space in the caller's own namespace**.
  Because a namespace's spaces hash across shards (shared-nothing shards, each with
  its own indexes), the connection layer **fans the RECALL out to all shards in
  parallel**; each shard runs the retrieval pipeline with its scope filter widened
  from `(namespace, space)` to `namespace_id` only (all spaces it holds), and
  returns **raw scored candidates** (per-lane scores, not yet shaped). The router
  then **globally merges** the per-shard pools (RRF), and the membership / precision
  shaping runs **once over the merged pool** — so `Single`/`Many`/`None` and the
  precision decision are computed globally, not per shard.

Authorization reuses the existing `RECALL` permission: a key that may RECALL may
request `scope = Namespace`. **Tenant-isolation invariant (non-negotiable):** a
namespace-wide RECALL returns only rows of the caller's own `namespace_id`. Each
shard's filter pins `namespace_id`, and the router never merges across namespaces —
`scope = Namespace` can never surface another tenant's data. `act_as` still selects
the effective identity; the namespace-wide span is over the *effective* namespace.

`max_results` remains a global safety ceiling on the merged answer; each shard is
additionally bounded to a per-shard top-K during fan-out so the merge cost stays
proportional to one shard's work, not the whole corpus.

### MEMORY_LIST — enumeration, not search

`MEMORY_LIST` (`0x0027`) is a distinct read *kind*: a non-ranked, paginated
enumeration of the caller's `(namespace, agent)` memories in a stable order. It
does **not** run the retrieval pipeline — no cue, no embedding, no RRF, no
rerank, no relevance suppression. Where RECALL answers *"what is relevant to this
query"*, MEMORY_LIST answers *"what is stored here"*: it walks the tenant-scoped
`created_at` timeline index and returns a page plus an opaque, signed keyset
cursor (seek pagination, never offset — page N costs the same as page 1 and pages
are stable under concurrent writes). Filters (kind, tombstone state, created-time
range, salience range) are applied during the scan; changing any filter or the
sort invalidates an in-flight cursor (`stale_cursor`). It never crosses the
`(namespace, agent)` boundary and never aggregates or counts the whole pool.

### GRAPH_FETCH — typed-graph export, not search

`GRAPH_FETCH` (`0x0163`) is the enumeration analogue for the *typed graph*: a
non-ranked, paginated export of the caller's `(namespace, agent)` entities and
the edges between them, shaped as a node/edge set a graph-explorer UI renders
directly. Like MEMORY_LIST it does **not** run the retrieval pipeline. It
paginates over the subject-anchored statement index — the one typed-graph index
that is `(namespace, agent)`-prefixed — and *derives* the entity set from
traversal (statement subjects/objects, plus the relation and mention neighbours
of those entities) rather than a dedicated per-agent entity index; a
fully-isolated entity therefore does not surface. The default layer is the
concept map (entity nodes + `Relation`/`Fact` edges); `include_statements` adds
value-object statement nodes and `include_memories` adds source-memory nodes
with `Mentions` edges. The cursor is opaque and signed over the layer toggles.
Because an entity can be reached on more than one page, the response guarantees
**completeness, not disjointness**: every node/edge appears in at least one page,
may repeat across pages, and carries a stable 16-byte id so the client dedups by
id. It never crosses the `(namespace, agent)` boundary.

### MEMORY_INSPECT — one memory's write story, not search

`MEMORY_INSPECT` (`0x0028`) is a single-memory point read: given one
`memory_id`, it returns that memory's text plus the durable **write-artifact
bundle** — the per-stage record of what the write built. It does **not** run
the retrieval pipeline; it is a keyed lookup, not a query. Where RECALL answers
*"what is relevant"* and MEMORY_LIST answers *"what is stored"*, MEMORY_INSPECT
answers *"how was this one memory built"*.

The response carries `found`, the `memory_id`, the `text`, and an
`EncodeStageArtifact` bundle with the same shape the live ENCODE trace uses for
its per-stage `artifact`: the embedding `vector`, the stored `record` (kind,
salience, times, dims, text length), the analyzed `keyword_fields` (the exact
terms the `memory_text` index matches on), the generated `hype_questions`, and
the typed `graph` (nodes + edges). The bundle is persisted in the
`memory_artifacts` table (§10.9a) and populated incrementally: the sync fields
(vector, record, keywords) are present the instant the write acks; the graph and
HyPE fields fill in as the async workers settle. A memory whose async stages
have not yet run therefore returns `found = true` with those fields still empty
— the same "how far along is this write" signal the ENCODE trace's drain window
exposes, but readable for any memory at any later time.

Scope is enforced by the memory's own `(namespace, agent)` owner: a
`memory_id` owned by another tenant reads as `found = false`, indistinguishable
from a missing one — an id never leaks cross-tenant. Requires the `RECALL`
capability bit. A hard-forgotten or reclaimed memory returns `found = false`;
its bundle is purged with the memory (§10.9a).

#### In-transaction read-your-writes overlay

When `req.txn_id` is set, the txn's pending ENCODE buffer is overlaid on the committed retrieval result before the response is built:

- Tombstoned ids in the buffer drop committed hits.
- Pending encodes are scored against the cue vector and merged with the committed list.
- The combined list is re-sorted by similarity (descending); membership shaping then runs over it, bounded only by the `max_results` safety ceiling.

This is the single read-your-writes path; the same overlay runs whether or not a schema is active.

### 2. The arguments

#### cue_text

The query. Embedded with the same model used for stored memories.

The cue can be a single word, a sentence, a longer document — whatever the agent thinks is a useful query. Note that very short cues may be ambiguous and produce broad results; very long cues are truncated by the embedder.

#### agent_id

The owning agent. Returns are scoped to this agent's memories.

**Under `act_as` the scope is the effective agent.** When the request carries the `act_as` field (a trusted service principal reading on behalf of a tenant agent; defined in [`../04_wire_protocol/04_handshake.md`](../04_wire_protocol/04_handshake.md) §"Per-request identity (`act_as`)"), the `agent_filter` isolation scopes to the **effective** `(namespace, agent_id)` named in `act_as`, never to the connection principal. A `RECALL` under `act_as` therefore returns the **effective identity's** memories only — never the service principal's own, and never any other tenant's. The isolation boundary follows the effective identity for every read, exactly as the write path stamps rows with it.

#### max_results

A **safety ceiling** on how many members a `Many` answer may carry — not a
ranking knob and not the criterion that shapes the answer. `0` ⇒ server default;
an explicit value caps the returned member count. Max 1000. The answer's shape
(Single / Many / None) comes from the relevance band over the full candidate
pool (§3), never from this number.

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

RECALL answers with memories — one, several, or none. The `answer_kind`
carries which; the memory list holds the members. There is no
retrieval-mechanism vocabulary in the response (no "episodic", no "grounded") —
how the router found the memories is an internal concern the caller never sees.

```rust
struct RecallAnswer {
    answer_kind: AnswerKind,          // Single | Many | None
    memories: Vec<RecallResult>,      // 0 for None; see §"Lead vs. retained
                                       // membership" below for Single/Many —
                                       // the count is NOT always 1 / 2+
    partial: bool,                    // True if some shards failed
    total_candidates: usize,          // Pre-filter count (for diagnostics)
}

enum AnswerKind {
    Single,                           // exactly one memory is the answer
    Many,                             // several memories together are the answer
    None,                             // no memory answers the cue (explicit absence)
}

struct RecallResult {
    memory_id: MemoryId,
    score: f32,                       // [-1, 1]; higher = more similar (provenance)
    text: Option<String>,             // If include_text
    metadata: Option<MemoryMetadata>, // If include_metadata
    context_id: ContextId,
    kind: MemoryKind,
}
```

#### The membership band (how the shape is decided)

The answer set is the memories that fall inside a **relevance band** over the
full filtered candidate pool, not the top-K by rank:

- Let `top` = the best relevance score in the pool. A candidate belongs to the
  answer iff it clears both an absolute floor and a relative band around `top`
  (`score ≥ ABS_FLOOR` **and** `score ≥ top × REL_BAND`). Lexical- or
  graph-confirmed hits, and grounded source memories for a resolved
  subject/predicate, are admitted the same way.
- `answer_kind` is then pure cardinality of that set: `0 → None`, `1 → Single`,
  `2+ → Many`. When several members all assert the same value, the router may
  collapse them to a single `Single`.
- `max_results` (§2) only caps the size of a `Many`; it never turns a `Many`
  into a `Single` by truncation, and never suppresses the band.

#### Lead vs. retained membership (grounded commit)

When the typed-graph (grounded) answer is anchor-scoped, clears the strong-match
floor, and its source memory is cross-lane corroborated, its value is
**committed** as the lead: `answer_kind` is set from the committed shape
(`Single` for one committed value, `Many` for a committed enumerated set)
independently of the raw membership count, and the committed memory (or
memories, for a `Many` commit) is moved to the **front** of `memories` in
committed order.

Critically, the rest of the relevance-band membership is **retained below the
lead, never discarded** — a `Single` answer's `memories` can therefore contain
more than one entry. This is deliberate, not a defect: an incorrect commit
(wrong subject, stale value) can only **mis-order** the response, because the
real answer — if it's anywhere in the band — is still present in the retained
tail. Silently dropping the tail was tried and reverted after it caused a
grounded-first regression (a loose predicate-name match on the wrong subject
hijacked the answer with no episodic fallback to catch it).

The caller-facing contract is therefore:

- **`Single`**: `memories[0]` is the committed answer. Any further entries
  (`memories[1..]`) are retained relevance-band context, not part of the
  answer — present for provenance/fallback, not for display as additional
  results. A client that wants "the answer, nothing else" reads only index 0.
- **`Many` via an uncommitted band** (no grounded commit fired): every entry in
  `memories` is part of the answer, per the pure-cardinality rule above — this
  is the common case and matches the original contract.
- **`Many` via a committed enumerated set**: the committed set leads (in
  committed/recency order); anything appended after it is retained context,
  not part of the enumerated answer. The wire does not currently carry an
  explicit boundary count between the committed set and the retained tail — a
  client that needs to draw that line precisely should treat this case the
  same as the uncommitted `Many` (all of `memories` as answer-relevant) until a
  `lead_count`-style field is added; this is an open follow-up, not yet
  implemented.

Absence is explicit (`None`, empty list), never a fabricated guess.
`RecallResult.score` and the other retrieval fields are **provenance** — they
say why a member surfaced; they are not a ranking the caller is expected to
re-sort or threshold.

### 4. Score semantics

Score = `1 - cosine_distance(cue_vec, mem_vec)` for normalized vectors.

Range: typically 0 to 1 in practice (vectors don't usually point opposite). 1.0 means identical; 0.0 means orthogonal; negative means opposite (rare).

Heuristic interpretation:
- > 0.9: very similar (often near-duplicate).
- 0.7-0.9: similar topic, related content.
- 0.5-0.7: same general area.
- < 0.5: weakly related.

These aren't strict thresholds; they depend on the model and the corpus. Agents tune `confidence_min` to their use case.

### 5. The small-answer case

A `Single` answer, or a `Many` with only a handful of members, is normal — the
band admits exactly the memories that answer the cue, however few. This is
common for:
- Small or new agents.
- Selective filters.
- Very specific cues.

It's not an error, and it is not "fewer than requested" — there is no requested
count. `max_results` only caps the upper end.

### 6. The `None` case

`answer_kind = None`, empty member list. Possible if:
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

The agent might want to restrict the answer to the current context. Use a single
RECALL with explicit `contexts: Some([current])` — the filter chain scopes the
candidate pool before membership runs. The agent does not merge or re-rank result
lists in its own layer; the DB returns the answer.

### 12. The "across-shard" recall

For agents whose data spans multiple shards (rare), RECALL fans out:

- Each shard runs its sub-recall in parallel, returning raw scored candidates (not shaped).
- The router merges the per-shard pools with **global RRF** over each hit's within-shard rank (see the "Recall scope — space vs namespace-wide" section above and [`../13_retrievers/01_rrf_fusion.md`](../13_retrievers/01_rrf_fusion.md)).

The membership answer is computed over the merged global candidate pool.

This is transparent to the agent — it sees a single answer.

### 13. Latency

For typical workloads (single-shard, no complex filter):

- p50: ~10 ms.
- p99: ~25 ms.

Latency scales with the candidate-pool size the retrievers fan out over and the
filter complexity, not with a caller-chosen result count — there isn't one. A
larger answer set (a broad `Many`) costs marginally more to project.

For cross-shard recalls (2-3 shards): p99 rises to ~30-50 ms.

### 14. Throughput

A shard handles ~5K-20K RECALLs per second. Limited by:

- Embedder throughput (with cache, much higher).
- HNSW search latency.

For higher throughput, scale shards.

### 15. The "include_text=true" cost

Including text fetches each result's text from the metadata store:

- Per-member cost: ~5-20 µs (cache-dependent).
- A handful of members: ~100 µs additional; a broad `Many`: proportionally more.

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

### 19. The `None` semantics

`answer_kind = None` is Brain's explicit "no memory answers this" — absence is a
first-class answer, never a fabricated guess. Possible causes:

- The agent has no relevant memories.
- The cue is unusual (nothing clears the relevance band).
- The filter is too tight.

Brain doesn't distinguish these causes on the wire. The agent decides what to do
— broaden the cue, relax the filter, or accept the `None`.

### 20. No client-side re-ranking

Brain does not expose a "fetch a broad top-K and re-rank on the agent side"
pattern — that is the SaaS-search shape this DB rejects. The heavy lifting
(fusion, rerank, membership) happens server-side at read time; the answer comes
back already shaped (Single / Many / None). The agent consumes the answer, it
does not re-sort or threshold a candidate list. `RecallResult.score` is
provenance, not a ranking the caller is expected to act on.

### 21. The RECALL trace (`trace: true`)

RECALL carries the same `trace: bool` observability toggle used across the read
primitives (see [`02_write_pipeline.md`](02_write_pipeline.md) §17, "API
convention — `wait` for writes, `trace` for reads" — a read has nothing to wait
for, so it carries a single boolean rather than a `wait`-shaped enum). The
default, `trace: false` (the common case, omitted from the wire map), is the
fast path described in §1-§20 above, unchanged: the response's trace field is
`None`, and the pipeline pays nothing for it — no per-item collection, no extra
allocation, no shape change to the hot path.

`trace: true` returns a populated `RecallTrace` describing every stage of the
pipeline in full per-item detail: not just the aggregate counts a caller could
already infer from the final answer, but which specific candidate each
retriever lane surfaced, which specific memory each filter step dropped, and
exactly what the cross-encoder reordered. There is one knob, not two — a caller
opting into tracing always gets the full per-item picture; there is no
separate size-minimized or id-only detail level to request instead.

```rust
struct RecallTrace {
    retrievers: Vec<RecallTraceRetriever>,
    filter_chain: RecallTraceFilterChain,
    rerank: Option<RecallTraceRerank>,  // None when no cross-encoder is loaded
    total_latency_ms: f64,
    fusion: Option<RecallTraceFusion>,  // full-detail only; None if fusion produced nothing
}
```

#### 21a. Per-retriever candidates

Each of the three always-wired lanes (semantic / lexical / graph) already
reported its terminal status, latency, and an aggregate `candidate_count`. It
now also reports the candidates themselves:

```rust
struct RecallTraceRetriever {
    name: RetrieverNameWire,
    status: RecallTraceRetrieverStatus,     // Success | Skipped | Timeout | Failure
    status_detail: String,                  // skip reason / error message
    latency_ms: f64,                        // 0.0 when skipped
    candidate_count: u32,                   // aggregate count, always present
    candidates: Vec<RecallTraceCandidate>,  // full-detail only
}

struct RecallTraceCandidate {
    memory_id: WireMemoryId,
    text: String,      // full-detail only; truncated server-side
    score: f32,        // this lane's own raw score for this item
}
```

`candidates` is empty on `trace: false` and holds the lane's raw hits **before**
RRF fusion, in the lane's own rank order, on `trace: true` — the same
population `candidate_count` already summarized, now with id, text, and score
attached instead of collapsed to a length. This is what makes it possible to
see, e.g., that the lexical lane surfaced memory X at rank 3 with its own
BM25-derived score of 0.42, independent of whatever rank X ended up at after
fusion.

#### 21b. Per-filter-step drops

The filter chain's survivor counts (`before`, `after_type`, `after_temporal`,
`after_confidence`, `after_tombstone`, `after_supersession`, `after_as_of`,
`after_limit`) are unchanged — always present, regardless of `trace`. Each step
now also carries exactly which ids it removed. Four of the seven drop lists are
plain memory-id lists; the last three are kind-tagged, because they can drop
`Statement` and `Relation` items too — not just `Memory` ones:

```rust
struct RecallTraceFilterChain {
    before: u32,
    after_type: u32,
    after_temporal: u32,
    after_confidence: u32,
    after_tombstone: u32,
    after_supersession: u32,
    after_as_of: u32,
    after_limit: u32,
    // full-detail only; empty on trace: false and on any step that dropped nothing
    dropped_by_type: Vec<WireMemoryId>,
    dropped_by_temporal: Vec<WireMemoryId>,
    dropped_by_confidence: Vec<WireMemoryId>,
    dropped_by_tombstone: Vec<WireMemoryId>,
    // kind-tagged — see below
    dropped_by_supersession: Vec<RecallTraceDroppedId>,
    dropped_by_as_of: Vec<RecallTraceDroppedId>,
    dropped_by_limit: Vec<RecallTraceDroppedId>,
}

/// One id a filter-chain step dropped, tagged with which item-kind
/// id-space it belongs to.
struct RecallTraceDroppedId {
    kind: RankedItemKindWire,
    id: u128,
}

enum RankedItemKindWire {
    Memory = 0,
    Statement = 1,
    Entity = 2,
    Relation = 3,
}
```

An empty `dropped_by_*` Vec means that step removed nothing at all (the
survivor count didn't shrink between the prior step and this one); a non-empty
one names precisely which ids that step removed. This is the difference
between knowing "confidence filtering went from 26 to 25" and knowing
"confidence filtering dropped memory `<id>`" — the latter is what turns "why
didn't memory X make the answer" from a guess into a lookup.

**Why the split.** The filter chain (`crates/brain-planner/src/retrieval/filters/logic.rs`)
runs over the fused set produced by RECALL's three retriever lanes, and that
set is not Memory-only — the graph lane can surface `Statement`- and
`Relation`-backed candidates alongside the semantic/lexical lanes' `Memory`
hits, so every filter step is written generically over the full
`RankedItemId` union (`Memory` / `Statement` / `Entity` / `Relation`).
`dropped_by_type` / `dropped_by_temporal` / `dropped_by_confidence` /
`dropped_by_tombstone` stay plain `Vec<WireMemoryId>` because, for RECALL,
these four steps only ever drop `Memory` items in practice. The remaining
three are different in kind, not just in practice:

- **Supersession** is a concept the filter code defines as not applying to
  `Memory` / `Entity` at all — `filter_supersession` passes those two kinds
  through unconditionally ("Memory / Entity have no supersession concept"),
  so any live supersession drop is definitionally a `Statement` or
  `Relation`.
- **As-of** (bi-temporal record-time filtering) is scoped narrower still:
  only `Statement` carries a record-time invalidation timestamp today —
  `filter_as_of` passes `Memory` / `Entity` / `Relation` through because
  bi-temporal validity is "a statement-layer property today" — so an as-of
  drop is definitionally a `Statement`.
- **Limit** truncation runs last, after all six filters, over the fully
  fused-and-filtered survivor list, which by then can hold any item kind
  that made it through the chain — so its drops need the same kind tag as
  supersession and as-of, for the same reason (a plain `WireMemoryId` can't
  represent a dropped `Statement` or `Relation`).

#### 21c. Pre-/post-rerank order

```rust
struct RecallTraceRerank {
    applied: bool,
    candidates: u32,
    latency_ms: f64,
    before_order: Vec<WireMemoryId>,  // full-detail only: fused order immediately before rerank
    after_order: Vec<WireMemoryId>,   // full-detail only: final order after the cross-encoder
}
```

`before_order` and `after_order` show exactly what the cross-encoder moved —
not just that it ran (`applied`) and how many candidates it scored
(`candidates`), but the concrete before/after permutation. Both are empty on
`trace: false`, when `rerank` is `None` (no cross-encoder loaded on this
shard), and when `applied = false` (loaded, but nothing in the fused list had
fetchable text to score).

#### 21d. Per-fused-item lane contribution

```rust
struct RecallTraceFusion {
    items: Vec<RecallTraceFusionItem>,
}

struct RecallTraceFusionItem {
    memory_id: WireMemoryId,
    rrf_score: f32,
    lane_scores: Vec<(RetrieverNameWire, f32)>,
}
```

`RecallTrace.fusion` has no aggregate-count precedent — nothing in the
`trace: false`-equivalent counts summarized fusion below the per-retriever
level at all. On `trace: true`, for each item RRF admitted to the candidate
pool, `lane_scores` lists which of the three lanes contributed to it and that
lane's own raw score, so a caller can see that a given fused item was surfaced
by both semantic (0.81) and graph (0.65) but not lexical, and how that
combination produced its `rrf_score`. `fusion` is `None` when tracing wasn't
requested or when fusion produced no items.

#### 21e. Why full detail, not counts-only or id-only

Tracing exists to debug recall accuracy — to answer "why did memory X end up
in, or stay out of, the answer," which an aggregate count can never answer.
Because `trace: true` is opt-in and only exercised by a caller who has already
decided the extra cost is worth it (an eval harness, a debugging console — never
the default production path), the trace returns full per-item detail, including
text, rather than a size-minimized id-only variant. This mirrors
`include_text`'s existing per-final-result fetch (§2, "include_text"), just
applied to every stage's candidates instead of only the final answer set.

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

### 13. The "explain" option (superseded)

This section originally described an `explain=true` option returning the
intermediate frontier expansions, paths considered but not returned, and a
per-path scoring breakdown. No such field was ever implemented on
`PlanRequest`. The real, shipped mechanism for this is the `trace: bool`
flag — see §19, "The PLAN trace (`trace: true`)" — which returns the full
BFS-explored node set (both directions) and every meeting point found,
including ones the `max_paths` cap excluded from the result. Kept here only
so old references to "the explain option" land somewhere; new integrations
should read §19 directly.

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

### 19. The PLAN trace (`trace: true`)

PLAN carries the same `trace: bool` opt-in observability toggle as RECALL
(§21 above) and REASON (§20 below): `pub trace: bool` on `PlanRequest`,
defaulting to `false` and omitted from the wire map in that case. The default
path is byte-for-byte unchanged — the bidirectional BFS already tracks full
per-node visited-map state and per-neighbor alignment scores internally, but
today collapses them to a scalar `nodes_explored` count and a capped
`meeting_points` list before they reach the wire; `trace: false` continues to
discard that detail with zero extra allocation.

`trace: true` populates `PlanResponseFrame.trace: Option<PlanTrace>` on the
**final** frame only (`is_final: true`); intermediate streamed `PlanStep`
frames are unaffected.

```rust
struct PlanTrace {
    explored: Vec<PlanTraceNode>,
    meeting_points: Vec<PlanTraceMeetingPoint>,
}
```

#### 19a. Explored nodes (both BFS directions)

```rust
struct PlanTraceNode {
    memory_id: WireMemoryId,
    text: String,
    direction: PlanTraceDirection,   // Forward (rooted at start) | Backward (rooted at goal)
    depth: u32,
    parent_edge: Option<WireMemoryId>,  // None for the root of each direction
    alignment_score: Option<f32>,       // set when order_by_goal_proximity scored this node
}
```

`explored` is the full visited-map contents of `run_bidirectional_bfs`
(`brain-planner/src/executor/path.rs`), from **both** the forward search
(rooted at `start`) and the backward search (rooted at `goal`) — not just the
scalar `nodes_explored` count the non-traced response already reports. Each
entry carries which direction found it, its BFS depth, the id of the parent
node it was reached from (`None` only for the two roots), and — when
`order_by_goal_proximity` scored it — the per-neighbor alignment score that
today is used only to reorder candidates and otherwise discarded.

#### 19b. Meeting points (found vs. included)

```rust
struct PlanTraceMeetingPoint {
    memory_id: WireMemoryId,
    text: String,
    included_in_result: bool,
}
```

Every node where the forward and backward frontiers connected is listed,
whether or not it survived the `max_paths` cap. `included_in_result: true`
marks the meeting points that made it into a returned `Path`; `false` marks
ones the cap dropped. This turns "why didn't the shorter path show up" into a
lookup instead of a guess — the meeting point is visible in the trace even
when the response's `paths` list doesn't contain it.

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

### 13. The "explain" option (superseded)

This section originally described an `explain=true` option returning why
each evidence item was selected, which edges were traversed, and per-edge
confidence. No such field was ever implemented on `ReasonRequest`. The real,
shipped mechanism for this is the `trace: bool` flag — see §20, "The REASON
trace (`trace: true`)" — which returns the full considered/dropped edge
walk, the per-item score breakdown (base similarity, decay, weight product,
alignment), and whether the topic-alignment centroid was computed at all.
Kept here only so old references to "the explain option" land somewhere;
new integrations should read §20 directly.

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

### 20. The REASON trace (`trace: true`)

REASON carries the same `trace: bool` toggle as RECALL (§21 above) and PLAN
(§19 above): `pub trace: bool` on `ReasonRequest`, defaulting to `false` and
omitted from the wire map in that case. The fast default path (§1-§19 above)
is unchanged — `resolve_base`, `walk_outward`, `filter_and_trim`, and
`topic_alignment_factor` already compute this detail internally and discard
it today; `trace: false` keeps discarding it with zero extra allocation.

> **Note on §13's `explain` option:** §13 above describes an `explain=true`
> option with similar intent ("why each evidence item was selected, which
> edges were traversed, per-edge confidence") but no such field exists on the
> implemented `ReasonRequest` — it was never built under that name. `trace`
> is the real, shipped mechanism for this; §13 should likely be reconciled
> or marked superseded by the owner rather than left as a second,
> unimplemented description of the same capability.

`trace: true` populates `ReasonResponseFrame.trace: Option<ReasonTrace>` on
the **final** frame only; intermediate streamed `InferenceStep` frames are
unaffected.

```rust
struct ReasonTrace {
    base: ReasonTraceBase,
    walk: ReasonTraceWalk,
    scoring: Vec<ReasonTraceScoreBreakdown>,
    centroid: ReasonTraceCentroid,
}
```

#### 20a. Base candidates

```rust
struct ReasonTraceBase {
    candidates: Vec<ReasonTraceCandidate>,
}
struct ReasonTraceCandidate {
    memory_id: WireMemoryId,
    text: String,
    score: f32,
}
```

`base.candidates` is the full HNSW hit set `resolve_base` returns, not just
the subset that seeded `walk_outward` — the same "everything a lane
surfaced, not the collapsed count" precedent as RECALL's per-retriever
`candidates` (§21a).

#### 20b. The edge walk: considered and dropped-by-kind

```rust
struct ReasonTraceWalk {
    considered: Vec<ReasonTraceEdgeCandidate>,
    dropped_by_edge_kind: Vec<ReasonTraceEdgeCandidate>,
    dropped_by_tombstone: Vec<ReasonTraceIdWithText>,
    dropped_by_visited: Vec<ReasonTraceIdWithText>,
    dropped_by_confidence: Vec<ReasonTraceScoredId>,
    dropped_by_max_supporting: Vec<ReasonTraceIdWithText>,
    dropped_by_max_contradicting: Vec<ReasonTraceIdWithText>,
}
struct ReasonTraceEdgeCandidate {
    memory_id: WireMemoryId,
    text: String,
    edge_kind: EdgeKindWire,
    depth: u32,
    from_memory_id: WireMemoryId,
    raw_score: f32,
}
struct ReasonTraceIdWithText { memory_id: WireMemoryId, text: String }
struct ReasonTraceScoredId { memory_id: WireMemoryId, text: String, score: f32 }
```

`considered` is every edge `walk_outward` visited at every node, from both
the supporting-side and contradicting-side traversals, before any pruning —
the walk's own analogue of a retriever lane's raw candidate list; a caller
can tell supporting from contradicting entries by `edge_kind`. The remaining
fields are that same walk's prune reasons, one bucket per prune point in the
executor: edge-kind filter, tombstoned target, an already-visited target,
sub-`confidence_threshold` score (in `filter_and_trim`), and the two post-hoc
trim caps on the surviving supporting/contradicting sets per inference step.
Every bucket carries the dropped memory's real text, not a bare id — same
"understand why, not just which id" rationale as RECALL's trace (§21e).

#### 20c. Score breakdown

```rust
struct ReasonTraceScoreBreakdown {
    memory_id: WireMemoryId,
    text: String,
    base_similarity: f32,
    decay: f32,
    weight_product: f32,
    alignment: f32,
    final_score: f32,
}
```

One entry per surviving evidence item, un-collapsing the multiplicative
score `topic_alignment_factor` folds into the single `EvidenceItem.score` /
`InferenceStep.confidence` value the non-traced response reports. This
exposes, e.g., that a low final score came from a weak `alignment` term
rather than a stale `decay` term, without the caller having to guess at the
factorization.

#### 20d. Centroid computed/skipped

```rust
struct ReasonTraceCentroid {
    computed: bool,
    skipped_reason: Option<String>,
}
```

`build_base_centroid` silently returns `None` on several paths (a singleton
base set, a `ByText` observation, missing text, or an embed error), logged
only at `tracing::debug!` today. The trace surfaces that outcome on the wire:
`computed: false` plus a `skipped_reason` string names which of those paths
fired, instead of leaving the caller to infer from an absent alignment term
whether topic-alignment scoring ran at all.

Both PLAN's and REASON's trace payload follow the same "full detail,
including text, not a size-minimized id-only variant" rationale as RECALL's
trace — see §21e above.

---

*Continue to [`04_transactions.md`](04_transactions.md) for transactional brackets.*
