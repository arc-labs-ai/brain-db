# 10. Metadata + Graph Store

> **TL;DR.** Per-shard redb B-tree holding memory metadata, edges, contexts, idempotency, text, entities, statements, relations, predicates, and audit tables. Plus two tantivy indexes (memory text, statement text) and a separate LLM extractor cache, both active once a schema is declared. Provides random access, ACID transactions, MVCC isolation. The WAL is the source of truth; the metadata store is a derived representation maintained for fast lookups. Cascading FORGET, confidence aggregation across evidence, supersession chains, and re-extraction live here.

## Status

| Field | Value |
|---|---|
| Status | Draft |
| Audience | Storage-layer implementers; query planner authors |
| Voice | Hybrid (rationale + normative) |
| Depends on | [02. Data Model](../02_data_model/00_purpose.md), [08. Storage](../08_storage/00_purpose.md) |
| Referenced by | [09. Indexing](../09_indexing/00_purpose.md), [12. Query Optimizer](../12_query_optimizer/00_purpose.md), [15. Background Workers](../15_background_workers/00_purpose.md) |

## What this spec defines

The metadata store: the persistent home for everything that isn't a vector. Memory metadata, edges between memories, contexts, idempotency records, and bookkeeping for sharding.

The store is built on **redb**, a pure-Rust embedded ACID key-value store.

This document specifies the metadata + graph store. Together with the vector arena ([08. Storage](../08_storage/00_purpose.md)) and the WAL, these comprise Brain's persistent state.

## What this document covers

- The role of the metadata store in the architecture.
- The choice of redb as the embedded engine.
- The table layout: memories, edges, contexts, idempotency, contexts.
- How variable-length data (text) is stored.
- Transaction semantics within the metadata store.
- Concurrency between metadata operations and the rest of Brain.

## What this document does not cover

- **Vector storage.** Defined in [08. Storage](../08_storage/00_purpose.md).
- **The query planner that uses the metadata.** Defined in [12. Query Optimizer](../12_query_optimizer/00_purpose.md).
- **Background workers (consolidation, decay, etc.).** Defined in [15. Background Workers](../15_background_workers/00_purpose.md).

## 1. The role of the metadata store

The metadata store holds:

- **Memory metadata** — for each memory, its kind, context, salience, model fingerprint, slot ID, timestamps, etc.
- **Edges** — relationships between memories: CAUSED, FOLLOWED_BY, DERIVED_FROM, SIMILAR_TO, CONTRADICTS, SUPPORTS, REFERENCES, PART_OF.
- **Contexts** — named buckets memories belong to.
- **Idempotency** — for replay protection: maps RequestId to the resulting MemoryId.
- **Text** — memory text content (for ENCODE) and consolidated content.
- **Bookkeeping** — checkpoints, model fingerprints registry, agent metadata.

The store is per-shard: each shard has its own redb file. Cross-shard queries fan out to multiple shards and merge.

## 2. Why a separate store

Brain could have stored metadata in the WAL alone, replaying it on startup to build in-memory structures. Why a separate persistent store?

- **Random access.** Some operations (looking up a memory's metadata) are random-access; a B-tree is fast for this. Replaying the WAL is sequential.
- **Reduced startup time.** With persistent metadata, recovery only replays records since the last checkpoint, not the entire history.
- **Compact representation.** The WAL contains the history of mutations; the metadata store contains only the current state. The metadata store is much smaller.

The cost: an additional persistent structure to keep consistent with the WAL. The WAL is still the source of truth; the metadata store is a derived representation maintained for fast random access.

## 3. ACID requirements

The metadata store provides:

- **Atomicity** — multi-key updates within a transaction either all happen or none do.
- **Consistency** — invariants (e.g., edge endpoints exist) are preserved.
- **Isolation** — concurrent reads see a consistent snapshot.
- **Durability** — committed transactions survive crashes (with the WAL ensuring redb's commits sync correctly).

These are needed because:

- Encoding a memory with edges is multi-key (memory record + edge records); must be atomic.
- Searches concurrent with edits must see a consistent view.
- Crashes shouldn't leave half-applied state.

## 4. The redb dependency

[redb](https://github.com/cberner/redb) is the engine:

- Pure Rust (no native dependencies, no cgo).
- ACID transactions.
- B-tree indexed.
- MVCC for concurrency.
- Good documentation, active maintenance.

Brain uses redb over alternatives in [`01_redb_choice.md`](01_redb_choice.md).

## 5. Per-shard deployment

Each shard's metadata store is a single redb file. The full per-shard directory holds the arena, WAL, primary metadata redb, memory HNSW, plus the on-disk artifacts that activate once a schema is declared:

```
data/
  shard-000/
    arena.bin              ── memory slots ([08. Storage](../08_storage/00_purpose.md))
    arena.wal              ── write-ahead log ([08. Storage](../08_storage/00_purpose.md))
    metadata.redb          ── all redb tables (this section)
    memory.hnsw            ── memory vectors ([09. Indexing](../09_indexing/00_purpose.md))

    # active once a schema is declared
    statements.tantivy/    ── full-text index over statements
    memory_text.tantivy/   ── full-text index over memory text
    entity.hnsw            ── entity-embedding HNSW
    statement.hnsw         ── statement-embedding HNSW
    llm_cache.redb         ── LLM extractor result cache
```

`metadata.redb` holds every table described in this spec — memory metadata, edges, contexts, idempotency, and (once a schema is declared) entities, statements, relations, predicates, audit. Different shards have different files; no cross-file or cross-shard queries.

## 6. Latency targets

- Memory metadata read (cached): < 1 µs.
- Memory metadata read (cold): < 10 µs.
- Single-row write within a transaction: < 10 µs.
- Transaction commit (with redb's internal sync): 0.1-1 ms.

These match the storage layer's latency budget. The metadata store contributes meaningful but bounded latency to writes; reads are negligible.

## 7. Size targets

For a typical 1M-memory shard:

| Table | Approx size |
|---|---|
| memories | ~150 MB (150 bytes × 1M) |
| edges | ~200 MB (8 edges/memory × 25 bytes/edge) |
| text | ~1 GB (1 KB/memory) |
| contexts | < 1 MB (few thousand contexts max) |
| idempotency | ~50 MB (50 bytes × 1M, with TTL) |
| **Total** | **~1.4 GB per 1M memories** |

Plus the vector arena (~1.5 GB) and HNSW (~150 MB), the total per-shard storage is ~3 GB per 1M memories. Operationally, plan for ~5 GB to give headroom.

Once a schema is declared, the same 1M-memory deployment with extraction density ~1 statement per 2 memories, ~10K entities, ~500 relations adds:

| Storage | Size |
|---|---|
| Entities table | ~50 MB |
| Statements table | ~150 MB |
| Relations table | ~5 MB |
| Entity HNSW | ~50 MB |
| Statement HNSW | ~2 GB |
| Memory tantivy | ~500 MB |
| Statement tantivy | ~100 MB |
| LLM cache | ~1 GB (configurable cap) |
| Audit logs | ~200 MB |
| **Additional total** | **~4 GB** |

Roughly doubles per-shard storage; acceptable for the capabilities gained.

## 8. The interface to the rest of Brain

The metadata store is accessed through a per-shard wrapper:

```rust
struct MetadataStore {
    db: redb::Database,
}

impl MetadataStore {
    fn get_memory(&self, id: MemoryId) -> Option<MemoryMetadata>;
    fn put_memory(&mut self, txn: &mut WriteTxn, m: &MemoryMetadata);
    fn list_edges(&self, source: MemoryId, kind: EdgeKind) -> Vec<Edge>;
    // ...
}
```

The wrapper hides redb's specifics. Higher layers (executors, planners) talk to this wrapper.

## 9. The metadata is not the source of truth

The WAL is the source of truth ([05.00 §4](../08_storage/00_purpose.md)). The metadata store is a derived representation:

- After WAL fsync, the metadata is updated.
- On crash, recovery replays the WAL to bring the metadata back into sync with the WAL.

If the metadata store is corrupted but the WAL is intact, recovery rebuilds the metadata from the WAL. This is slower but correct.

If the WAL is corrupted, recovery is more difficult (the metadata may not be consistent with itself). Backup/restore from snapshot is the answer.

## 10. The text storage decision

Memory text is stored in the metadata store (in a dedicated table) rather than alongside the vector in the arena. This is because:

- Text is variable-length; the arena's fixed-size slots aren't a good fit.
- Text isn't read on every search; metadata-store random access is fine.
- The metadata store's transactional semantics naturally protect text alongside memory metadata.

Detailed in [`03_substrate_tables.md`](03_substrate_tables.md) § Text Storage.

---

*Continue to [`01_redb_choice.md`](01_redb_choice.md) for the engine choice.*


## Provenance


## The provenance invariant

Every Statement, Relation, and Entity here has a traceable chain back to its source. The chain answers: "Where did this come from? Who derived it? When? Why?"

```
Memory(text) ──extracted_by──> Statement ──supersedes──> Statement (current)
                                  │
                                  └──evidence──> [Memory, Memory, Memory]
```

## What is tracked

For every derived record:

| Field | Meaning |
|---|---|
| `evidence: Vec<MemoryId>` | The source memories. |
| `extractor_id: ExtractorId` | Which extractor produced this. |
| `extractor_version: u32` | Pinned at extraction time. |
| `schema_version: u32` | Pinned at extraction time. |
| `extracted_at: u64` | When the extraction ran. |
| `model_metadata: Option<ModelMetadata>` | For LLM extractors: model name/version, token counts, cache hit/miss. |

For supersession chains:
- `chain_root: StatementId` — the first statement in the chain.
- `version: u32` — chain position (1 for root, 2 for first supersession, ...).
- `supersedes: Option<StatementId>` — back-pointer.
- `superseded_by: Option<StatementId>` — forward-pointer.

For tombstones:
- `tombstoned: bool`
- `tombstoned_at: Option<u64>`
- `tombstone_reason: TombstoneReason`
- `tombstoned_by: Option<Actor>`

## The audit log

A separate, append-only log records every derivation, supersession, tombstone, and merge:

```rust
struct AuditEntry {
    id: AuditId,                       // UUIDv7, ordered
    timestamp: u64,
    actor: Actor,                      // System(extractor) | User(agent_id) | Admin
    operation: AuditOp,
}

enum AuditOp {
    Extracted { memory_id, extractor_id, output_ids },
    Superseded { old, new, reason },
    Tombstoned { target, reason },
    Retracted { target, reason },
    Merged { survivor, merged, confidence },
    Unmerged { entity, restored_to },
    Renamed { entity, old_name, new_name },
    SchemaUpgraded { from_version, to_version },
}
```

Audit entries are durable (written through WAL) and queryable. Default retention: 90 days. Configurable.

## Cascading effects of FORGET

When a Memory is forgotten:

```
FORGET memory_x
  │
  ├─ Soft tombstone in the memory store
  │
  ├─ Lookup statements WHERE memory_x ∈ evidence
  │     For each affected statement:
  │       - Remove memory_x from evidence list
  │       - Recompute confidence (down-weighted)
  │       - If evidence list now empty:
  │           * If confidence_after >= threshold: keep, mark "stale_evidence"
  │           * Else: tombstone with reason=SourceMemoryForgotten
  │
  ├─ Lookup relations WHERE memory_x ∈ evidence (same logic)
  │
  ├─ Lookup entity_mentions WHERE memory_id=memory_x: remove
  │
  └─ Write audit entries for each cascade
```

This cascade is performed by a worker (the FORGET cascade worker), not synchronously. The triggering FORGET returns immediately; the cascade processes in background.

### Reversibility: the additive undo log

A soft FORGET (tombstone with a grace window) is reversible within grace via an **additive undo log**, not a pending-tombstone flag. The forward cascade applies its mutations immediately — it strips the forgotten memory from each dependent's evidence, drops the reverse-index row, recomputes confidence, and (when the memory was the sole evidence) tombstones the row with reason `SourceMemoryForgotten`. All of that is lossy, so before mutating each dependent the cascade writes one `ForgetUndoRecord` to the `forget_undo_log` table, capturing exactly what is needed to reverse the change: the stripped evidence entry, the row's prior confidence, its prior `is_current` / `tombstone_reason` / evidence-overflow id, and which outcome the cascade applied.

The undo log is keyed `(forgotten_memory_id, dependent_record_id)`, so every dependent of one forgotten memory forms a contiguous key range that the revert executor prefix-scans cheaply. The table is additive — materialized lazily on open, adding no column to any existing row and requiring no `CURRENT_SCHEMA_VERSION` bump.

An admin restore within the grace window replays these undo records: it re-attaches the evidence entry, re-adds the reverse-index row, recomputes confidence, re-keys the predicate bucket, and un-tombstones each row still carrying `SourceMemoryForgotten`. Each consumed undo row is deleted in the same write txn, so a re-run is a structural no-op (idempotent, crash-safe replay).

A **hard** FORGET writes no undo records and is irreversible by design (the privacy escape hatch). Once the grace window passes and slot reclamation runs, that memory's undo rows are reaped as well — so a soft FORGET also becomes irreversible after grace, and the journal cannot leak.

## Confidence aggregation across evidence

When a Statement has multiple supporting Memories, its confidence is aggregated:

```
confidence = 1 - Π_i (1 - c_i * decay(age_i))
```

Where:
- `c_i` is the per-evidence confidence (extractor's per-memory confidence).
- `decay(age_i)` reduces older evidence: `decay(t) = exp(-t / half_life)`.
- `half_life` is 90 days default for Facts, 30 days for Preferences, no decay for Events (Events are point-in-time and don't decay).

When evidence is added or removed, confidence is recomputed.

This formula:
- Bounded in [0, 1].
- Monotonic: adding consistent evidence raises confidence.
- Diminishing returns: 10 pieces of weak evidence don't equal 1 piece of strong.

## Stale extraction detection

Each Statement carries the `schema_version` and `extractor_version` it was produced under.

When the current `schema.version` or `extractor.version` advances, statements with older versions are flagged `stale`:

```rust
fn is_stale(statement: &Statement, current: &SchemaSnapshot) -> bool {
    statement.schema_version < current.schema_version
    || statement.extractor_version < current.extractor.version_for(statement.extractor_id)
}
```

Stale statements remain queryable. The query result surfaces staleness in metadata, so clients can decide whether to trust them or trigger re-extraction.

## Re-extraction workflow

Triggered manually or by the schema migration worker:

```
RE_EXTRACT memory_x with extractor_y schema v=5
  │
  ├─ Look up existing statements for (memory_x, extractor_y)
  │
  ├─ Run extractor_y v5 on memory_x
  │     Output: new statements
  │
  ├─ Diff: for each new statement:
  │     If matching old statement (same kind, subject, predicate, object):
  │       confidence_delta = new.confidence - old.confidence
  │       If similar (delta < threshold): mark old as "refreshed", update version
  │       If different (delta >= threshold): supersede old with new
  │     If new but no matching old: create
  │
  │     For each old not matched by new:
  │       Mark as "potentially retracted"; user review or auto-tombstone
  │
  └─ Audit entry written
```

For Events: re-extraction is non-destructive. New Events are added; old Events stay.

For Preferences: supersession applies straightforwardly.

For Facts: tricky. New contradicting Facts trigger a contradiction; same-direction Facts confirm the old.

## Version visibility in queries

By default, queries return:
- Current Statements (not superseded).
- Non-tombstoned.
- Not stale (unless `include_stale: true`).

Optional query parameters:
- `as_of: Timestamp` — return Statements as they would have appeared at this time (looks at chain by `version <= ?`).
- `include_superseded: true` — return all versions.
- `include_tombstoned: true` — return tombstoned with their reasons.

Note: `as_of` operates on valid_time (`valid_from`, `valid_to`, `extracted_at`). True bitemporal "as of transaction time" is deferred to future versions.

## Retention

| Record | Retention default |
|---|---|
| Active Statements/Relations/Entities | Forever |
| Tombstoned Statements/Relations | 30 days (then hard-deleted by sweeper) |
| Superseded Statements/Relations | Forever (kept for chain history) |
| Extraction audit logs | 90 days |
| Resolution audit logs | 90 days |
| Merge logs | Forever (small, valuable) |
| LLM extractor cache | 90 days |

All configurable per deployment.
