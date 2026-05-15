# Entity Storage Layout

## redb tables

### `entities`
```
key:   EntityId (16 bytes)
value: rkyv-serialized Entity (canonical_name, aliases, type, attributes, mention_count, timestamps, merged_into, embedding_version)
```

### `entity_by_canonical_name`
```
key:   (entity_type_id: u32, normalized_name: String)
value: EntityId
```
Secondary index for tier-1 exact resolution.

### `entity_aliases`
```
key:   (entity_type_id: u32, normalized_alias: String, entity_id: EntityId)
value: () (membership only)
```
Aliases index for tier-1 resolution. Composite key allows the same alias to map to entities of different types.

### `entity_trigrams`
```
key:   (entity_type_id: u32, trigram: [u8; 3], entity_id: EntityId)
value: ()
```
Trigram index for tier-2 fuzzy resolution. Each entity's canonical_name contributes its trigrams. Similarity scored at query time via index intersection + Jaccard.

### `entity_mentions`
```
key:   (entity_id: EntityId, memory_id: MemoryId)
value: MentionMetadata (offset in memory text, confidence, extractor_id)
```
Reverse: which memories mention each entity. Used for graph queries and provenance.

### `entity_resolution_audit`
```
key:   AuditId (16 bytes)
value: rkyv-serialized ResolutionAudit
```

### `entity_merge_log`
```
key:   (timestamp_unix_nanos: u64, merge_id: [u8; 16])
value: MergeRecord
```

`MergeRecord` carries the **complete diff** between pre-merge and post-merge state. Unmerge ([`./04_unmerge.md`](./04_unmerge.md)) replays this diff in reverse:

```rust
pub struct MergeRecord {
    pub merge_id_bytes: [u8; 16],
    pub survivor_bytes: [u8; 16],
    pub merged_bytes: [u8; 16],

    // Pre-merge / post-merge identity.
    pub merged_at_unix_nanos: u64,
    pub grace_period_until_unix_nanos: u64,
    pub confidence: f32,
    pub reason: String,                                // ≤ 4 KiB; operator-supplied
    pub actor_kind: u8,                                // 0 = System, 1 = Agent
    pub actor_agent_bytes: [u8; 16],                   // [0;16] when actor_kind=System

    // Diffs against the survivor (replayed in reverse by unmerge).
    pub aliases_added: Vec<String>,                    // aliases merged contributed to survivor
    pub trigrams_added: Vec<[u8; 3]>,                  // trigrams contributed (derived from
                                                       // aliases_added + merged.canonical_name)
    pub attribute_conflicts: Vec<AttributeConflictRecord>,

    // Re-routing counts (lists deferred to overflow rows when large).
    pub statements_rerouted: u32,                      // phase 17+: populated by re-route step
    pub relations_rerouted: u32,                       // phase 18+: populated by re-route step
    pub mention_count_added: u32,                      // survivor.mention_count += this on merge

    // Status.
    pub finalized: u8,                                 // 0 = reversible, 1 = grace expired / unmerge invalid
    pub unmerged_at_unix_nanos: u64,                   // 0 = still merged
    pub unmerged_by_actor_kind: u8,                    // 0 if !unmerged
    pub unmerged_by_agent_bytes: [u8; 16],             // [0;16] if !unmerged or actor=System
}

pub struct AttributeConflictRecord {
    pub attribute_key: String,
    pub survivor_value_blob: Vec<u8>,                  // rkyv-encoded original survivor value
    pub merged_value_blob: Vec<u8>,                    // rkyv-encoded original merged value
    pub policy: u8,                                    // 1=survivor_wins, 2=merged_wins,
                                                       // 3=newest_wins, 4=concat_text, 5=reject_merge
    pub outcome: u8,                                   // 1=KeptSurvivor, 2=ReplacedWithMerged,
                                                       // 3=Concatenated
}
```

**Phase scope:** the `statements_rerouted` / `relations_rerouted` **counts** are written from phase 16.7 onward (currently always `0` since statement/relation tables don't exist until phases 17/18). The per-statement / per-relation **lists** (needed for unmerge to know which rows to re-route back) live in a sibling `entity_merge_audit_overflow` table introduced in phase 17 when statements land:

```
ENTITY_MERGE_AUDIT_OVERFLOW
key:   ([u8; 16] merge_id, u32 chunk_index)
value: MergeAuditOverflow { rerouted_statement_ids: Vec<[u8; 16]>, rerouted_relation_ids: Vec<[u8; 16]> }
```

Pre-phase-17, the overflow table is declared but never written.

**Versioning:** the `MergeRecord` rkyv shape introduced in phase 16.7 is `v2`. Phase 16.5 stubbed a `v1` shape with only `(survivor, merged, timestamps, confidence, finalized)`; no production deployments exist on `v1` so the migration is a straight replacement, not a rolling upgrade. See [`../../crates/brain-metadata/src/tables/knowledge/merge.rs`](../../crates/brain-metadata/src/tables/knowledge/merge.rs).

## Entity embedding HNSW

A per-shard HNSW index, separate from the main memory HNSW:

- Index: `entity_embeddings.hnsw`
- Vector dim: 384 (same as memory)
- Parameters: M=16, ef_construction=100 (lower than memory; entity count is smaller), ef_search=64
- Tombstoned entities are removed via the standard HNSW tombstone+rebuild cycle.

## Storage costs

For a deployment with N entities, each averaging:
- canonical_name: 30 chars
- aliases: 5 × 30 chars = 150 chars
- 5 attributes averaging 50 bytes each = 250 bytes
- embedding: 1536 bytes

Per entity: ~2 KB in the main table.
Plus index entries: ~200 bytes per entity across all indexes.
Plus HNSW: ~3 KB per entity (vector + HNSW links).

Total: ~5 KB per entity. 100K entities = 500 MB. 1M entities = 5 GB.

This is small relative to memory storage (memories are typically 2 KB of text + 1.6 KB slot = ~4 KB each, with M memories typically >> N entities).

## Read paths

| Query | Path |
|---|---|
| Get entity by ID | redb `entities` lookup (O(log N) seek) |
| Exact name resolution | `entity_by_canonical_name` or `entity_aliases` lookup |
| Fuzzy resolution | `entity_trigrams` intersection of candidate trigrams, scored |
| Embedding resolution | Entity HNSW search |
| All memories mentioning entity | `entity_mentions` prefix scan |
| All statements with subject = entity | `statements_by_subject` index (see `19_statements/`) |
| All relations involving entity | `relations_by_from` + `relations_by_to` (see `20_relations/`) |

## Write paths

Entity creation (new):
1. Generate EntityId (UUIDv7).
2. Write to `entities`.
3. Write to `entity_by_canonical_name`.
4. Write trigrams to `entity_trigrams`.
5. Embed and write to entity HNSW (async, doesn't block).
6. Commit redb transaction.

All steps except 5 are in a single redb transaction (single-writer-per-shard discipline from the substrate). Step 5 is async; embedding-based resolution may miss this entity for a few seconds.

Entity update (rename, attribute change):
1. Read current entity.
2. Compute delta (which indexes need update).
3. Update `entities`.
4. Update `entity_by_canonical_name` (remove old, add new) if canonical_name changed.
5. Update `entity_trigrams` (remove old set, add new) if canonical_name changed.
6. Queue async re-embedding.

Entity merge: see `01_resolution.md` for the full procedure.
