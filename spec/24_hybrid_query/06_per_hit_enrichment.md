# Per-Hit Graph Enrichment on RECALL

## Purpose

When the client sets `include_graph = true` on a `RecallRequest`, the server attaches a knowledge-layer side-channel to each `MemoryResult`. The side-channel surfaces the entities, statements, and relations the substrate's extractor pipeline associated with the recalled memory, so a client can render rich context (who is mentioned, what's been claimed, how those entities relate) without issuing follow-up `ENTITY_GET` / `STATEMENT_LIST` / `RELATION_LIST` calls per hit.

`include_graph` is independent of `include_edges`: edges carry substrate-layer memory→memory edges; the graph side-channel carries knowledge-layer enrichment.

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

- **`None`** — the memory never went through extractors. This is the substrate-only deployment posture (no schema declared) and pre-schema memories on hybrid deployments. The server signals this by returning `None` rather than an empty `GraphEnrichment`.
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

This gives the correct behaviour for both substrate-only deployments (no knowledge tables → `None`) and per-memory granularity (memories encoded before the schema was uploaded never gained `Mentions` edges → `None`; memories encoded after gain `Some`).

## Interaction with the substrate vs hybrid paths

The enrichment payload is identical on both server-side paths. The substrate path opens its own `ReadTransaction`; the hybrid path reuses the transaction already open for the `MEMORIES_TABLE` scan (no double-lock). Both paths populate `MemoryResult.graph` exactly when `req.include_graph` is set; the rest of `MemoryResult` (`similarity_score`, `confidence`, `fused_score`, `contributing_retrievers`, …) is unaffected.

## Cost note

`include_graph = true` adds, per hit:

- 1 prefix scan of the unified edge table at `(memory, Mentions, *)`.
- For each mentioned entity (capped at 16): 1 `entity_get`, 1 `ENTITY_TYPES_TABLE` get.
- 1 prefix scan of `STATEMENTS_BY_EVIDENCE_TABLE` at `(memory, *)`. For each statement returned (capped at 5 after sorting): 1 `statement_get`, 1 `predicate_get`, 1–2 `entity_get`.
- For each mentioned entity: 2 prefix scans (outgoing + incoming) of the unified edge table; for each typed-relation row kept (capped at 5 overall): 1 `relation_type_get`, 2 `entity_get`.

All on the same `ReadTransaction`. Cost is dominated by entity-count fanout per hit; for typical extractor output (2–5 entities per memory) the overhead is ~10–30 redb point/range reads per hit.

Clients sensitive to RECALL latency leave `include_graph` off and issue targeted follow-up queries when a particular result deserves enrichment.
