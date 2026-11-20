# 13.06 Post-Processing (Enrichment, Rerank, Traversal)

> **TL;DR.** Three post-processing stages applied after RRF fusion and before final response: per-hit enrichment (attach entities / statements / relations to each hit via the include_graph side-channel, opt-in via the request flag), cross-encoder rerank (`BAAI/bge-reranker-base`) over the top-50, **off by default and not part of the current release** — gated by the deploy-time `[rerank] enabled` load switch, with no per-request toggle — and multi-hop relation traversal with depth caps and cycle detection.

## Per-Hit Enrichment

## Purpose

When the client sets `include_graph = true` on a `RecallRequest`, the server attaches a typed-graph side-channel to each `MemoryResult`. The side-channel surfaces the entities, statements, and relations Brain's extractor pipeline associated with the recalled memory, so a client can render rich context (who is mentioned, what's been claimed, how those entities relate) without issuing follow-up `ENTITY_GET` / `STATEMENT_LIST` / `RELATION_LIST` calls per hit.

`include_graph` is independent of `include_edges`: edges carry memory→memory edges; the graph side-channel carries typed-graph enrichment (entities, statements, relations).

## Wire shape

```rust
struct MemoryResult {
    // ... existing fields (memory_id, similarity_score, …) …
    graph: Option<GraphEnrichment>,
}

struct GraphEnrichment {
    entities: Vec<EnrichedEntity>,
    statements: Vec<EnrichedStatement>,
    relations: Vec<EnrichedRelation>,
}

struct EnrichedEntity {
    id: [u8; 16],
    name: String,            // canonical_name from the entity table
    type_qname: String,      // "Person" / "namespace:typename"
}

struct EnrichedStatement {
    id: [u8; 16],
    subject_name: String,    // canonical_name; "(ambiguous)" when SubjectRef::Ambiguous
    predicate: String,       // qname form: "namespace:name"
    object_label: String,    // entity canonical_name, scalar repr, or memory:/statement: ref
    confidence: f32,
}

struct EnrichedRelation {
    from_name: String,       // canonical_name of the source entity
    predicate: String,       // qname of the relation type
    to_name: String,         // canonical_name of the target entity
}
```

## `graph = None` vs `graph = Some(empty)`

The two states are distinct and the server must preserve the distinction:

- **`None`** — the memory never went through extractors. This is the schemaless deployment posture (no schema declared) and applies to pre-schema memories on schema-declared deployments. The server signals this by returning `None` rather than an empty `GraphEnrichment`.
- **`Some(GraphEnrichment { entities: [], statements: [], relations: [] })`** — extractors ran but produced no entities for this memory (e.g. the text contained no recognised entity mentions). The presence of `Some` signals "this memory was processed."

The renderer surfaces the distinction in human output: `None` omits the section entirely; `Some(empty)` prints a muted "(no knowledge enrichment — extractor produced no entities/statements/relations)" line.

## Server-side query plan

For each hit, with the shard's metadata read transaction:

1. **Entities** — walk the unified edge table with `(NodeRef::Memory(memory_id), EdgeKindRef::Mentions, *)` to enumerate the `EntityId`s the extractor stamped on this memory. For each, point-look up `ENTITIES_TABLE` for the canonical name and `ENTITY_TYPES_TABLE` for the type name.
2. **Statements** — range-scan `STATEMENTS_BY_EVIDENCE_TABLE` at the prefix `(memory_id.to_be_bytes(), *)` to enumerate `StatementId`s sourced from this memory. For each, point-look up `STATEMENTS_TABLE`. Skip tombstoned rows. Resolve the subject's canonical name via `entity_get`, the predicate via `predicate_get`, and the object label by discriminating on `StatementObject`.
3. **Relations** — for each entity from step 1, walk both `walk_outgoing(NodeRef::Entity(id), None)` and `walk_incoming` and keep rows whose `EdgeKindRef` is `Typed(RelationTypeId)`. Resolve the relation type's qname via `relation_type_get`; resolve the two endpoints' canonical names.

All three steps share the same `&ReadTransaction` — one redb txn serves the entire enrichment batch.

## Caps

The server caps each list to keep the per-hit payload bounded:

| List | Cap | Selection signal |
|---|---|---|
| `entities` | 16 | first-seen order from the `Mentions` walk (which yields rows in `(kind, to, disambiguator)` byte order, deterministic) |
| `statements` | 5 | `confidence` descending; tombstoned excluded; `is_current` not enforced (a superseded-but-not-tombstoned statement is still evidence) |
| `relations` | 5 | `created_at_unix_nanos` descending across both directions |

A hit that legitimately involves more than 16 mentioned entities returns the first 16 by mention order; clients that want all of them issue `ENTITY_LIST` for the memory directly.

## Schema gating

The server gates by table presence + edge presence, not by declared schema:

- If `STATEMENTS_BY_EVIDENCE_TABLE` does not exist on the shard *and* `walk_outgoing(NodeRef::Memory(memory_id), Some(Mentions))` returns no rows → `graph = None`.
- Otherwise → `Some(GraphEnrichment { … })`, possibly with empty inner vectors.

This gives the correct behaviour for both schemaless deployments (no knowledge tables → `None`) and per-memory granularity (memories encoded before the schema was uploaded never gained `Mentions` edges → `None`; memories encoded after gain `Some`).

## Interaction with schemaless vs schema-declared paths

The enrichment payload is identical on both server-side paths. The schemaless path opens its own `ReadTransaction`; the schema-declared (retrieval) path reuses the transaction already open for the `MEMORIES_TABLE` scan (no double-lock). Both paths populate `MemoryResult.graph` exactly when `req.include_graph` is set; the rest of `MemoryResult` (`similarity_score`, `confidence`, `fused_score`, `contributing_retrievers`, …) is unaffected.

## Cost note

`include_graph = true` adds, per hit:

- 1 prefix scan of the unified edge table at `(memory, Mentions, *)`.
- For each mentioned entity (capped at 16): 1 `entity_get`, 1 `ENTITY_TYPES_TABLE` get.
- 1 prefix scan of `STATEMENTS_BY_EVIDENCE_TABLE` at `(memory, *)`. For each statement returned (capped at 5 after sorting): 1 `statement_get`, 1 `predicate_get`, 1–2 `entity_get`.
- For each mentioned entity: 2 prefix scans (outgoing + incoming) of the unified edge table; for each typed-relation row kept (capped at 5 overall): 1 `relation_type_get`, 2 `entity_get`.

All on the same `ReadTransaction`. Cost is dominated by entity-count fanout per hit; for typical extractor output (2–5 entities per memory) the overhead is ~10–30 redb point/range reads per hit.

Clients sensitive to RECALL latency leave `include_graph` off and issue targeted follow-up queries when a particular result deserves enrichment.

---

## Rerank

A post-fusion stage that re-ranks the top of the RRF-fused list with a cross-encoder model. When the cross-encoder is loaded on a shard, every RECALL (and the `QUERY_TRACE` debug op) reranks automatically — there is no per-request flag.

**It ships disabled.** `[rerank] enabled` defaults to `false`, and the published image carries no cross-encoder model. The reason is a correctness one, not a cost one: rerank only reorders candidates that retrieval already surfaced, so it cannot recover a memory the retrieval and grounding stages missed. Solving recall upstream is the work that matters; turning rerank on is how you A/B the ordering once that work is done.

Two consequences of being off by default:

- **Enabled-but-unloadable is a boot failure, not a degradation.** `enabled = true` without a cross-encoder model on disk is `ShardError::CrossEncoderInitFailed` at spawn. Turning it on in the stock image stops the server starting, which is the intended outcome — a silently-degraded reranker would make two shards with the same config return different orderings.
- **A disabled capability is reported, never silently substituted.** `GET_CAPABILITIES` reports `rerank: false`, and any request that reaches the shard asking for a capability it does not hold is refused with `CapabilityNotEnabled` rather than served a different ranking without explanation (`crates/brain-server/src/config/mod.rs:114-118`). On the `RECALL` read path this is structural rather than a runtime check — there is no rerank field for a client to set — so the honest signal is the capability bit, which clients read to know whether the ordering they received was reranked.

Where it sits in the pipeline:

```
retrievers → RRF fusion → filter chain → [recency boost] → [rerank, if cross-encoder loaded] → [merge/diversity, if list intent] → limit
```

The rerank fires on the top-50 fused candidates only. It surfaces a re-ordered head that the membership stage then shapes into the answer set (bounded by the `max_results` safety cap, not a caller `top_k`).

## Merge / diversity (MMR) — internal, list-intent only

A post-rerank stage that spreads near-duplicate memories so a set
question returns *distinct* members, not paraphrases of the most salient
one. It runs **only** when the router detected list/aggregation intent
(see [`./05_retrieval_query.md`](./05_retrieval_query.md) Rule 6) — there
is no client knob; the database decides. This is the read half of list
handling; the router's wider candidate pool is the coverage half (a
member that never reached fusion can't be diversified into the result).

- **Algorithm:** greedy Maximal Marginal Relevance over the top
  candidates (window 50). Each pick maximises `λ·rel(d) − (1−λ)·max
  redundancy(d, already-picked)`, `λ = 0.5`. Relevance is the rerank
  score (else fused score), min-max normalized; redundancy is **Jaccard
  over lowercased text token sets** (candidate vectors aren't reachable
  post-fusion without a retriever round-trip, and token overlap is a
  cheap near-duplicate signal).
- **Cannot regress a single answer:** greedy MMR's first pick is always
  the highest-relevance item, so a lone strong answer stays at rank 1.
  Only positions 2..N are reordered, and only on detected list intent —
  a factoid that slips the detector keeps its top hit.
- **Window-bounded:** `MMR_WINDOW = 50`, `MMR_LAMBDA_LIST = 0.5`
  (`crates/brain-planner/src/retrieval/diversity.rs:30-36`). The stage is
  `O(window² · tokens)`, so the window covers what a caller could
  plausibly receive plus headroom; items past it keep their incoming
  order behind the reordered head. A window of ≤ 2, or a non-positive
  λ, is a no-op.
- the membership band still decides the returned set; MMR changes *which* items
  fill the slots.

## Model

`BAAI/bge-reranker-base`.

- **Architecture:** a cross-encoder over `(query, candidate)` pairs — a sequence-classification head with `num_labels = 1`, scoring each pair jointly in one forward pass. The encoder is **XLM-RoBERTa**, not BERT, which matters to anyone loading weights: the classification token is `<s>`-based rather than BERT's position-0 `[CLS]` convention (`crates/brain-rerank/src/model.rs:1-10,77`).
- **Score:** the raw logit from the classification head; higher is more relevant. It is unbounded and its scale varies per pair, which is why the blend in §"Operation" normalizes per batch rather than using it directly.
- **Loading:** best-effort. The model directory resolves from `BRAIN_RERANK_MODEL_DIR`, then `$XDG_DATA_HOME`, then `$HOME/.local/share/brain/models/`. An absent directory yields no cross-encoder and an RRF-only pipeline; **corrupt weights are a hard error**, not a silent fallback.

bge-reranker-base is the production-default cross-encoder for retrieval in the field — strong precision-per-MB and small enough to keep CPU rerank cost in budget for a 50-pair window.

## Triggering

The rerank has no client toggle. A bare recall reranks automatically when the shard has the cross-encoder loaded:

```rust
let response = brain.recall()
    .cue("budget pushback in Q4")
    .top_k(10)
    .execute()
    .await?;
```

The only control is the deploy-time load gate `[rerank] enabled`, which is `false` by default. When an operator turns it on and the model loads cleanly, the executor runs a rerank step for every query, positioned **after** the filter chain and the recency boost and before the merge/diversity stage (`crates/brain-planner/src/retrieval/executor/logic.rs:721-790`). That position is the point: re-scoring a candidate the filter chain is about to drop, or re-sorting before a recency term is folded in, would spend cross-encoder time to produce an ordering a later stage overwrites. With the gate off — the shipped state — no model loads, the step never runs, and the pipeline is RRF-only. Clients read the `rerank` bit from `GET_CAPABILITIES` (`crates/brain-ops/src/handlers/capabilities.rs:62`) to know whether the results they get back are reranked.

## Operation

Per call:

1. Take the top-50 of the filtered list (or fewer if fewer survived).
2. For each candidate, resolve its text from the per-shard `texts` table and build a `(query, candidate.text)` pair. A candidate with no fetchable text — tombstoned mid-query, or a non-memory variant — is skipped and keeps its fused position.
3. Score the pairs through the cross-encoder, off the shard core.
4. **Re-sort by a blend, not by the model's score.** The sort key is
   `normalize(fused_score) + RERANK_ALPHA · normalize(rerank_logit)`, with
   `RERANK_ALPHA = 0.5` and both terms min-max normalized per batch
   (`crates/brain-planner/src/retrieval/rerank.rs:28-46,83`).
5. Candidates outside the window keep their RRF order and remain in the pool. Nothing is cut to a fixed head; the membership band decides the answer size, bounded only by `max_results`.

**Why a blend and not the logit.** Normalizing *both* terms is what makes `α` a real weight. Raw RRF scores sit around 0.01–0.05 while an un-normalized rerank term spans [0, 0.5], so combining them on their native scales would let the cross-encoder dominate by roughly 10× and effectively overrule fusion. On a common [0, 1] scale, fusion carries weight 1.0 and rerank 0.5: a full-span fusion lead (Δ = 1.0) cannot be overturned by the bounded rerank term (≤ 0.5), but rerank decisively breaks close fused ties. The cross-encoder is a re-scoring signal over a pool fusion already agreed on, not the primary one — multi-retriever consensus should not lose to one noisy logit on one pair.

Two degradations are deliberate. A flat batch (every logit equal) collapses the rerank term to 0 for every item, so the fused order simply wins. A score/candidate length mismatch skips the rerank entirely with a warning rather than applying it partially, since a partial application would silently treat unscored candidates as worst-in-batch.

Step 3 is the cost driver. On CPU, 50 pairs through bge-reranker-base land at ~6-9 ms p99 at typical text lengths. Like the embedder figure in §13/03, this is a per-core model-inference cost and is the number in this file most sensitive to the host it was measured on.

## Latency budget

A rerank-enabled shard widens the RECALL p99 target from 20 ms to ~30 ms — the entire cost added by the rerank lands in that delta. The shipped default is the 20 ms RRF-only path; an operator who enables rerank is accepting the wider budget for every read on that shard.

## Why a cross-encoder and not a bi-encoder

Bi-encoders (the same family the embedder uses) score query and candidate independently and dot-product. Cross-encoders score them jointly through a single forward pass, attending across the boundary.

For the rerank position — small candidate set, latency budget already widened — cross-encoders are the field-standard winner on precision. The bi-encoder embedding already runs upstream at retrieval; running another bi-encoder for rerank would be redundant.

## Gating discipline

The rerank is **gated** in two senses:

1. **Deploy-time load gate.** `[rerank] enabled` (default `false`) decides whether the cross-encoder loads at shard spawn. When it loads, every query reranks; in the shipped default no model loads and recall is RRF-only. There is no per-request toggle.
2. **Top-50 cut.** Only the top of the fused list pays the rerank cost — the rest of the corpus is unaffected.

The load gate plus the top-50 cut are what bound rerank's cost: the default deployment pays nothing, and an operator who enables it pays only for re-scoring the head of the list.

## Configuration

`[rerank]` has exactly one field (`crates/brain-server/src/config/mod.rs:416-429`):

```toml
[rerank]
enabled = false
```

Nothing else is configurable — not the model, not the intake width, not the
batch size. The section is `#[serde(deny_unknown_fields)]` like every other, so
adding a key to it is a startup parse error rather than an ignored line.

The intake width is a build constant, `RERANK_TOP_N = 50`
(`crates/brain-planner/src/retrieval/rerank.rs:28`). Keeping it out of config is
deliberate: it is the knob that decides how much cross-encoder work a read does,
and the cross-encoder dominates that read's wall time, so a deployment that
could raise it could make every read arbitrarily slow through a config edit.
Fifty is wide enough to rescue a strongly-relevant candidate that fusion
diluted, and bounded enough that the cost is a constant.

A candidate ranked outside the top 50 by fusion keeps its fused position. The
re-ordered head is **not** a final result cap: candidates below it keep their
RRF order and stay in the filtered pool the membership stage shapes, and the
answer size is decided by the relevance band, bounded only by `max_results`.

## Observability

The scrape surface carries exactly one rerank series:
`brain_query_rerank_invoked_total{shard}` — recalls where the rerank stage
reordered the fused list (`crates/brain-server/src/metrics/format.rs:1308-1318`).
It answers "is the stage running at all", which on a capability that ships
disabled is the question most likely to be wrong.

It carries no rerank **cost or quality** series: there is no `brain_rerank_*`
family, no rerank-latency histogram, and no score distribution. The step's wall
time is folded into the enclosing recall duration, so an operator cannot
separate rerank cost from retrieval cost on the dashboard.

What fills that gap is `QUERY_TRACE`, which reports the rerank's own duration
plus — in full-detail mode — the fused order and the post-rerank order for a
single query, so the reordering itself is inspectable
(`crates/brain-protocol/src/ops/memory.rs:1170-1186`). That shape fits the real
question, "is the rerank earning its latency on this query distribution?",
better than a scraped aggregate: the answer depends on *which* documents moved,
which a counter cannot show. A deployment that enables rerank and wants
continuous latency attribution has to add it.

---

## Traversal

How `RELATION_TRAVERSE` (opcode `0x0156`) explores the relation
graph from a starting entity, bounded by depth + branching factor +
cycle detection.

Cross-references:
- [`./00_purpose.md`](./00_purpose.md) §"Graph queries" — query
  patterns the traversal supports.
- [`../10_metadata/03_substrate_tables.md`](../10_metadata/03_substrate_tables.md) — read paths
  the traversal consumes (BY_FROM / BY_TO prefix scans on the relations table).
- [`./04_graph_retriever.md`](./04_graph_retriever.md) — dual-index reads for
  symmetric relations.
- [`../04_wire_protocol/08_typed_graph_frames.md`](../04_wire_protocol/08_typed_graph_frames.md)
  §9 — wire shape.

## 1. The contract

```rust
pub fn traverse(
    rtxn: &ReadTransaction,
    start: EntityId,
    type_filter: &[RelationTypeId],   // empty = any type
    direction: TraversalDirection,
    max_depth: u8,                    // capped at MAX_DEPTH
    max_branching_factor: u32,         // per-level cap
    current_only: bool,
) -> Result<Vec<TraversalPath>, RelationOpError>;

pub enum TraversalDirection {
    Outgoing,
    Incoming,
    Both,
}

pub struct TraversalPath {
    pub steps: Vec<TraversalStep>,    // length = path depth
}

pub struct TraversalStep {
    pub relation_id: RelationId,
    pub from: EntityId,
    pub to: EntityId,
    pub relation_type_id: RelationTypeId,
    pub depth: u8,                    // 1-indexed from start
}
```

## 2. Algorithm

Iterative BFS — depth-first would risk stack blowup on degenerate
graphs.

```text
visited: HashSet<EntityId>      // cycle detection
frontier: Vec<(EntityId, path)> // current level
paths: Vec<TraversalPath>       // accumulator

visited.insert(start)
frontier.push((start, []))

for current_depth in 1..=max_depth:
    next_frontier = []
    for (node, path_so_far) in frontier:
        neighbours = expand(node, type_filter, direction, current_only)
        if neighbours.len() > max_branching_factor:
            neighbours.truncate(max_branching_factor)
            // Per §6 below: log a tracing::warn for visibility.

        for (relation_id, other, rel_type) in neighbours:
            if visited.contains(other):
                continue  // cycle / re-entry, skip
            visited.insert(other)
            new_path = path_so_far.clone()
            new_path.push(TraversalStep {
                relation_id,
                from: if direction == Incoming { other } else { node },
                to:   if direction == Incoming { node }  else { other },
                relation_type_id: rel_type,
                depth: current_depth,
            })
            paths.push(TraversalPath { steps: new_path.clone() })
            next_frontier.push((other, new_path))

        if next_frontier.len() > max_branching_factor * (1 << current_depth):
            // Defensive cap on total state.
            return paths

    frontier = next_frontier
    if frontier.is_empty():
        break  // exhausted before max_depth

return paths
```

## 3. `expand`

Returns the neighbours of `node` reachable in one step via a
relation of any filtered type, in the requested direction.

```text
fn expand(node, type_filter, direction, current_only) -> Vec<(RelationId, EntityId, RelationTypeId)>:
    out = []
    if direction in {Outgoing, Both}:
        out += relation_list_from(rtxn, node, type_filter, current_only)
                .map(|r| (r.id, r.to_entity, r.relation_type))
    if direction in {Incoming, Both}:
        // For symmetric relations, list_from already returned the
        // edge (the relation is dual-indexed). list_to here would
        // duplicate — so for symmetric types, skip list_to.
        out += relation_list_to(rtxn, node, type_filter, current_only)
                .filter(|r| !r.is_symmetric_type)
                .map(|r| (r.id, r.from_entity, r.relation_type))

    // Dedup by (relation_id, other_entity).
    out.sort();
    out.dedup_by(|a, b| (a.0, a.1) == (b.0, b.1));
    out
```

## 4. Bounds

| Bound | Default | Cap | Rationale |
|---|---|---|---|
| `max_depth` | 3 | 5 | Per §13/00 §"Graph queries". Past 3 hops, denormalise. |
| `max_branching_factor` | 1000 | 10_000 | Truncates pathological super-nodes. |
| Total visited | 100_000 | — | Soft cap; if exceeded, the traversal stops early and returns what it has. |
| Wall-clock | 500 ms | — | Soft cap enforced by the handler via the planner's query budget. |

Caller-supplied bounds are clamped to the caps server-side.

## 5. Cycle detection

The `visited` set covers entity revisits. Self-loops (edge from
`A → A`) are visited once at depth 1 then never again — the second
visit short-circuits at the `visited.contains` check.

Symmetric back-edges are handled implicitly: once `B` is added to
`visited` by visiting it from `A`, traversal won't re-add an edge
`B → A` of the same symmetric relation.

## 6. Branching-factor diagnostics

When `neighbours.len() > max_branching_factor`, the implementation
emits a `tracing::warn!` with the node id, depth, type filter, and
the actual neighbour count. Operators can spot super-nodes via the
warn log and decide whether to denormalise.

## 7. Path enumeration

Each unique node found at each depth contributes one
`TraversalPath`. A node reachable via two distinct paths is reported
once (the first time it's visited); the second path is dropped.

For "all paths between A and B" semantics, callers iterate the
returned set and post-process. The query router may add explicit
"all paths up to depth N" if demand surfaces.

## 8. Wire response

`RELATION_TRAVERSE_RESP` (`0x01D6`) ships a single-frame snapshot:
`Vec<TraversalPathWire>` + `total_paths` + `truncated_by_*`
flags. Streaming + cursor resumption land here alongside
`STATEMENT_LIST` / `ENTITY_LIST` streaming.

## 9. Performance

For 1–2 hop queries: each `expand` runs a single `RELATIONS_BY_FROM`
(or `_TO`) prefix scan — O(log N + k) where k is the per-node
out-degree. Bounded by `max_branching_factor`.

For 3-hop queries with default branching (1000), the worst-case
visit count is 1 + 1000 + 1_000_000 = ~10^6 entities. Capped by
the `total_visited` soft cap (100k). Typical workloads have
out-degrees in the tens — visit counts stay under 10^3.

§19 perf targets: depth 1 p50 5ms, p99 25ms; depth 2 p50 15ms,
p99 50ms; depth 3 p50 30ms, p99 100ms.

## 10. Traversal boundaries

- **The return carries path-edge metadata, not terminal-entity sets.**
  Each hop reports its `relation_id` and `relation_type_id` alongside
  the endpoints (§1). A terminal-entity-only return would be cheaper to
  build and to frame, but it cannot answer *how* two entities connect —
  and that is what a traversal is asked for. The caller that only wants
  the reachable set reads the last step of each path.
- **Traversal is single-shard.** There is no cross-shard coordination:
  a traversal explores the relations its own shard owns. An edge whose
  other endpoint lives elsewhere terminates that path.
- **Path ranking is unweighted.** Hop distance is the only cost; a
  relation's `confidence` does not shorten or lengthen a path. A
  high-confidence three-hop route therefore ranks below a dubious
  two-hop one, and a caller that cares reads `confidence` off the
  returned steps and re-ranks.

## 11. Tests

Unit tests cover:

- One-hop outgoing.
- One-hop incoming.
- Two-hop with type filter.
- Three-hop with mixed types.
- Cycle: `A → B → A` returns one path at depth 1, no re-entry.
- Self-loop: `A → A` visited once.
- Symmetric edge: `A ↔ B` reachable from either side.
- Branching cap: 1001-out-degree node truncates at 1000 + emits warn.
- Direction filter: Outgoing-only excludes incoming edges.
- Empty type filter = any type.
- `current_only = true` excludes superseded / tombstoned.
- Disconnected graph: traversal from isolated node returns empty.
