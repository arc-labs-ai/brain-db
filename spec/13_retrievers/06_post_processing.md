# 13.06 Post-Processing (Enrichment, Rerank, Traversal)

> **TL;DR.** Three post-processing stages applied after RRF fusion and before final response: per-hit enrichment (attach entities / statements / relations to each hit via the include_graph side-channel, opt-in via the request flag), always-on cross-encoder rerank (bge-reranker-base, 110M params) for the top-K whenever the model is loaded — gated only by the deploy-time `config.rerank.enabled` load switch, with no per-request toggle — and multi-hop relation traversal with depth caps and cycle detection.

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

A first-class, always-on post-fusion stage that re-ranks the top of the RRF-fused list with a cross-encoder model. Whenever the cross-encoder is loaded on a shard, every RECALL and QUERY reranks automatically — there is no per-request flag. The only control is the deploy-time `config.rerank.enabled` load gate: when the operator turns it off, no model loads and the pipeline returns RRF-only ordering (no error).

Where it sits in the pipeline:

```
retrievers → RRF fusion → filter chain → [recency boost] → [rerank, if cross-encoder loaded] → [merge/diversity, if list intent] → limit
```

The rerank fires on the top-50 fused candidates only. It surfaces a re-ordered head that the limit then cuts to `top_k`.

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
  a factoid that slips the detector keeps its top hit. (Resolves
  OQ-AN-9, which nominated MMR for this slot.)
- `top_k` still bounds the returned count; MMR changes *which* items
  fill the slots.

## Model

bge-reranker-base.

- **Architecture:** cross-encoder over (query, candidate) pairs; scores each pair against the query in a single forward pass.
- **Params:** ~110M.
- **License:** MIT.

bge-reranker-base is the production-default cross-encoder for retrieval in the field — best precision-per-MB among the MIT-licensed options and small enough to keep CPU rerank cost in budget for the 50→10 cut.

## Triggering

The rerank has no client toggle. A bare recall reranks automatically when the shard has the cross-encoder loaded:

```rust
let response = brain.recall()
    .cue("budget pushback in Q4")
    .top_k(10)
    .execute()
    .await?;
```

The only control is the deploy-time load gate `config.rerank.enabled`. When it is on (the default) and the model loads cleanly, the executor inserts a rerank step into the execution DAG between fusion and the filter chain for every query. When the operator sets `config.rerank.enabled = false`, no model loads, the step is never inserted, and the pipeline matches the RRF-only path exactly. Clients read the `rerank` bit from `GET_CAPABILITIES` purely to know whether the results they get back are reranked.

## Operation

Per call:

1. Take the top-50 of the fused list (or fewer if fusion returned fewer).
2. For each candidate, build a `(query, candidate.text)` pair where `candidate.text` is the memory surface form or the statement's natural-language rendering.
3. Score all pairs in a single batched forward pass.
4. Sort by cross-encoder score descending; emit the top-10.
5. The top-10 carries through the filter chain as if it had emerged from fusion directly — the rest of the pipeline is unaware.

Step 3 is the cost driver. On CPU, 50 pairs through bge-reranker-base land at ~6-9 ms p99 at typical text lengths.

## Latency budget

A rerank-enabled shard widens the RECALL p99 target from 20 ms to ~30 ms — the entire cost added by the rerank lands in that delta. Because rerank is always-on, that wider budget is the default budget for any shard with the model loaded. Operators who cannot afford the tail set `config.rerank.enabled = false` at spawn, which returns the shard to the 20 ms RRF-only path.

## Why a cross-encoder and not a bi-encoder

Bi-encoders (the same family the embedder uses) score query and candidate independently and dot-product. Cross-encoders score them jointly through a single forward pass, attending across the boundary.

For the rerank position — small candidate set, latency budget already widened — cross-encoders are the field-standard winner on precision. The bi-encoder embedding already runs upstream at retrieval; running another bi-encoder for rerank would be redundant.

## Gating discipline

The rerank is **gated** in two senses:

1. **Deploy-time load gate.** `config.rerank.enabled` decides whether the cross-encoder loads at shard spawn. When it loads, every query reranks; when the operator opts out, no model loads and recall is RRF-only. There is no per-request toggle.
2. **Top-50 cut.** Only the top of the fused list pays the rerank cost — the rest of the corpus is unaffected.

The load gate plus the top-50 cut are what bound the always-on rerank's cost: operators who need the tighter latency target turn the load gate off; everyone else pays only for re-scoring the head of the list.

## Configuration

```
[recall.rerank]
model = "bge-reranker-base"
top_n_in = 50           # candidates fed into the reranker
top_k_out = 10          # candidates emitted to the filter chain
batch_size = 50         # one forward pass per call by default
```

`top_n_in` and `top_k_out` can be tuned per deployment; the defaults reflect the design point above.

## Observability

Per-call metrics on every RECALL / QUERY served by a rerank-enabled shard:

- `rerank_latency_seconds` — histogram of the rerank step's wall time.
- `rerank_input_count` — how many candidates entered the reranker (usually 50, sometimes less if fusion returned fewer).
- `rerank_position_change` — histogram of `|rank_after - rank_before|` for the emitted top-10. Large average movement is a signal the rerank is doing meaningful work; near-zero is a signal it's redundant for the current query distribution.

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

## 10. Open questions

See [`../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md):

- Q4 — Should TRAVERSE return path-edge metadata (the relation_id +
  type at each hop), or just terminal-entity sets? Currently:
  path-edge metadata. Counter: simpler returns are cheaper.
- Q6 — Cross-shard traversal coordination.
- Q7 — Weight-aware shortest-path (uses `confidence` as edge weight).

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
