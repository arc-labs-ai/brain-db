# 20.03 Relation Storage Layout

redb tables backing the relation layer. All 4 tables already
declared in [`../../crates/brain-metadata/src/tables/knowledge/relation.rs`](../../crates/brain-metadata/src/tables/knowledge/relation.rs)
(phase 15.1) — this file documents the layout authoritatively + the
read / write paths.

Cross-references:
- [`./00_purpose.md`](./00_purpose.md) — value-type schema.
- [`./01_cardinality.md`](./01_cardinality.md) — supersession write
  paths.
- [`./05_evidence.md`](./05_evidence.md) — evidence vec + cascade.
- [`../26_knowledge_storage/00_purpose.md`](../26_knowledge_storage/00_purpose.md)
  — knowledge-storage catalog.

## 1. Tables

### 1.1 `relations` (primary)

```
key:   RelationId.to_bytes() ([u8; 16])
value: RelationMetadata
```

Primary lookup. `RelationMetadata` is the rkyv-archived row carrying
every relation field. See `crates/brain-metadata/src/tables/knowledge/relation.rs::RelationMetadata`.

### 1.2 `relations_by_from`

```
key:   (from_entity_bytes: [u8; 16], relation_type_id: u32, is_current: u8)
value: RelationId.to_bytes()
```

Outgoing-edges index. For asymmetric relations, populated only with
the row's actual `from`. For symmetric relations, populated with the
**canonical_from** (per [`./02_symmetric.md`](./02_symmetric.md) §3).

`is_current = 1` iff `superseded_by.is_none() && !tombstoned`.
Derived bit; the `RelationMetadata` carries the source-of-truth
fields.

### 1.3 `relations_by_to`

```
key:   (to_entity_bytes: [u8; 16], relation_type_id: u32, is_current: u8)
value: RelationId.to_bytes()
```

Incoming-edges index. For asymmetric relations, populated with
`to`. For symmetric relations, populated with **canonical_to** —
**plus** an entry under `(canonical_from, type, is_current)` so
either endpoint queries return the relation. See [`./02_symmetric.md`](./02_symmetric.md)
§3.

### 1.4 `relations_by_evidence`

```
key:   (memory_id_bytes: [u8; 16], relation_id_bytes: [u8; 16])
value: ()
```

Reverse index for the FORGET cascade. One row per
`(MemoryId, RelationId)` pair in `RelationMetadata.evidence_inline`.
When a memory is forgotten, this index finds all relations that
referenced it; the FORGET worker decides per-cardinality whether
to tombstone, supersede with reduced evidence, or just record
provenance loss.

### 1.5 Deferred: `relations_by_type`

The §00 schema lists a per-type index for "all current relations of
type T" queries. Not present in 15.1 scaffolding. Deferred to phase
18.x if traversal performance demands it — most queries filter by
`from / to` first, which already narrows the candidate set.

If added later, the key would be `(relation_type_id, is_current,
created_at_unix_nanos)` and the value `RelationId.to_bytes()`. The
`created_at` suffix gives time-ordered scans for admin queries.

## 2. Per-create index writes

`relation_create` (in `brain-metadata::relation_ops`) writes
to all relevant tables in one redb txn:

```text
For each new Relation R:
  1. RELATIONS_TABLE.insert(R.id, RelationMetadata::from(R))
  2. RELATIONS_BY_FROM_TABLE.insert(
        (effective_from_bytes, R.relation_type_id, is_current_bit),
        R.id_bytes)
  3. RELATIONS_BY_TO_TABLE.insert(
        (effective_to_bytes, R.relation_type_id, is_current_bit),
        R.id_bytes)
  4. For each mem_id in R.evidence_inline:
       RELATIONS_BY_EVIDENCE_TABLE.insert(
           (mem_id_bytes, R.id_bytes), ())
```

For symmetric relations, **both** endpoints get an entry in **both**
directional indexes — so the relation is reachable from either side
regardless of which directional table the query consulted. Total
index writes: 4 (BY_FROM × 2 + BY_TO × 2 if symmetric, else 1 each)
plus per-evidence inserts.

## 3. Per-supersede index updates

`relation_supersede` runs `relation_create` for the new relation,
then updates the **old** in place:

```text
old.superseded_by = Some(new.id)
old.valid_to_unix_nanos = new.extracted_at  (if not pinned)

Rewrite old in RELATIONS_TABLE.

Remove old's RELATIONS_BY_FROM entry at is_current=1;
re-insert at is_current=0.
Same for RELATIONS_BY_TO (including symmetric dual-index removal).
```

## 4. Per-tombstone index updates

```text
Set fields:
  tombstoned = true
  tombstoned_at_unix_nanos = now

Rewrite in RELATIONS_TABLE.

Re-insert in BY_FROM / BY_TO with is_current=0 (flipping the bit).
```

Reverse-evidence index entries (§1.4) are **preserved** so FORGET
cascade can still find tombstoned relations whose evidence is
being deleted.

## 5. Hard reclamation

V1.0 doesn't ship a `RELATION_RETRACT` opcode — tombstone is soft
by default; phase 21+ may add a GC sweeper analogous to the
statement retract path (§19/03 §5). Documented in
[`./06_open_questions.md`](./06_open_questions.md) Q5.

## 6. Storage costs

For a deployment with R relations averaging:

- Fixed fields (`RelationMetadata`): ~200 bytes.
- `properties_blob`: 0 bytes (phase 19 schema DSL).
- Inline evidence: 0–128 bytes (8 × 16-byte MemoryIds, max).
- Indexes: ~80 bytes per relation across BY_FROM + BY_TO + BY_EVIDENCE.

Total: ~400–500 bytes per relation primary row + indexes. 10M
relations ≈ 4–5 GB.

## 7. Read paths

| Query | Path |
|---|---|
| Get relation by id | `RELATIONS_TABLE` point lookup (O(log R)). |
| Outgoing for entity | `RELATIONS_BY_FROM_TABLE` prefix scan at `(entity, *)`. |
| Incoming for entity | `RELATIONS_BY_TO_TABLE` prefix scan at `(entity, *)`. |
| Filtered by type | Same with type byte in key. |
| Current-only | Prefix terminates at `is_current = 1`. |
| Relations dependent on memory M | `RELATIONS_BY_EVIDENCE_TABLE` prefix scan at `(M, *)`. |

## 8. Write paths summary

| Operation | Tables written |
|---|---|
| `relation_create` (asymmetric) | RELATIONS + BY_FROM + BY_TO + BY_EVIDENCE (4 + evidence count) |
| `relation_create` (symmetric)  | RELATIONS + BY_FROM × 2 + BY_TO × 2 + BY_EVIDENCE (5 + evidence count) |
| `relation_supersede` | All create writes for new + 2 rewrites (old RELATIONS + flip is_current in BY_FROM / BY_TO) |
| `relation_tombstone` | RELATIONS rewrite + flip is_current in BY_FROM / BY_TO |

All operations execute inside one redb `WriteTransaction`.

## 9. Sharding

Relations are sharded by `canonical_from` (or `from` for asymmetric)
EntityId. Cross-shard concerns:

- A symmetric relation with `canonical_from` on shard A and
  `canonical_to` on shard B: primary lives on A, but the
  `RELATIONS_BY_TO` entry for `canonical_to` belongs on B's shard.
  Phase 18 ships same-shard only; cross-shard reverse-index writes
  follow the entity-side path in phase 23.

## 10. Concurrency

Per-shard single-writer discipline ([substrate §05](../05_storage_arena_wal/))
serialises writes naturally. Cross-shard writes coordinate via the
same routing mechanism the substrate uses for cross-shard edges
(phase 9.11+).

## 11. Tests

Storage-layer test coverage (phase 18.4):

- Round-trip every `RelationMetadata` field via `RELATIONS_TABLE`.
- Index consistency: after `relation_create`, the row is reachable
  via BY_FROM, BY_TO, and BY_EVIDENCE for every evidence memory.
- Symmetric: dual-side BY_FROM + BY_TO populated.
- After supersede: old appears with `is_current=0`; new at `=1`.
- After tombstone: BY_FROM / BY_TO flipped; BY_EVIDENCE preserved.
- Cardinality conflict: ManyToOne second create supersedes first;
  index consistency holds across the supersede.
