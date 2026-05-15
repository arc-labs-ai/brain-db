# 19.03 Statement Storage Layout

redb tables backing the statement layer. All 8 tables already declared in [`../../crates/brain-metadata/src/tables/knowledge/statement.rs`](../../crates/brain-metadata/src/tables/knowledge/statement.rs) — this file documents the layout authoritatively + the read / write paths.

Cross-references:
- [`./00_purpose.md`](./00_purpose.md) — value-type schema.
- [`./01_supersession.md`](./01_supersession.md) — supersession chain mechanics.
- [`./05_evidence.md`](./05_evidence.md) — evidence ref + overflow.
- [`../26_knowledge_storage/00_purpose.md`](../26_knowledge_storage/00_purpose.md) — overall knowledge-storage catalog.

## 1. Tables

### 1.1 `statements` (primary)

```
key:   StatementId.to_bytes() ([u8; 16])
value: StatementMetadata
```

Primary lookup. `StatementMetadata` is the rkyv-archived row carrying every statement field. See `crates/brain-metadata/src/tables/knowledge/statement.rs::StatementMetadata`.

### 1.2 `statements_by_subject`

```
key:   (subject_entity_bytes: [u8; 16], kind: u8, predicate_id: u32, is_current: u8)
value: StatementId.to_bytes()
```

Compound key lets "what's Priya's current role?" be a point lookup at `(priya_id, Fact, role_predicate_id, 1)`.

`is_current = 1` iff `superseded_by.is_none() && !tombstoned && valid_at(now)`. The bit is **derived** — supersession / tombstone / validity-time-out flips it; the underlying StatementMetadata also has the source-of-truth fields.

### 1.3 `statements_by_predicate`

```
key:   (predicate_id: u32, kind: u8, confidence_bucket: u8)
value: StatementId.to_bytes()
```

`confidence_bucket = floor(confidence * 10).clamp(0, 10)`. Coarse quantisation so the index is dense (11 buckets) but still useful for "all high-confidence Facts with predicate `manages`".

### 1.4 `statements_by_object_entity`

```
key:   (object_entity_bytes: [u8; 16], kind: u8)
value: StatementId.to_bytes()
```

Reverse index for "what statements have X as their object?". Populated only when `object` is the `Entity(...)` variant — `Value` / `Memory` / `Statement` objects skip this index.

### 1.5 `statements_by_event_time`

```
key:   (event_at_unix_nanos: u64, subject_entity_bytes: [u8; 16])
value: StatementId.to_bytes()
```

Time-range queries for Events. `event_at` only — populated only for `kind == Event`.

The compound second-component (subject) disambiguates same-time events about the same subject.

### 1.6 `statements_by_evidence`

```
key:   (memory_id_bytes: [u8; 16], statement_id_bytes: [u8; 16])
value: ()
```

Reverse index: "which statements reference memory M as evidence?". Used by FORGET cascade: when memory M is forgotten / retracted, the substrate finds all dependent statements and decides per-kind whether to tombstone, supersede, or just record provenance loss.

Population: one row per `(MemoryId, StatementId)` pair in `evidence.inline` (or every `MemoryId` reachable from `evidence.Overflow`). See [`./05_evidence.md`](./05_evidence.md) for cascade semantics.

### 1.7 `statement_chain`

```
key:   (chain_root_bytes: [u8; 16], version: u32)
value: StatementId.to_bytes()
```

Supersession chain. Prefix-scan `(chain_root, *)` returns the full chain in version order. See [`./01_supersession.md`](./01_supersession.md) §4.

### 1.8 `evidence_overflow`

```
key:   EvidenceOverflowId.to_bytes() ([u8; 16])
value: EvidenceOverflow { memory_ids: Vec<[u8; 16]>, extractor_ids: Vec<u32> }
```

For statements with > 8 evidence memories (the inline cap). See [`./05_evidence.md`](./05_evidence.md).

## 2. Per-create index writes

`statement_create` (in `brain-metadata::statement_ops`) writes to multiple tables in one redb txn:

```text
For each new Statement S:
  1. STATEMENTS_TABLE.insert(S.id, StatementMetadata::from(S))
  2. STATEMENTS_BY_SUBJECT_TABLE.insert(
         (S.subject_bytes, S.kind, S.predicate_id, is_current_bit), S.id_bytes)
  3. STATEMENTS_BY_PREDICATE_TABLE.insert(
         (S.predicate_id, S.kind, confidence_bucket(S.confidence)), S.id_bytes)
  4. if let Object::Entity(eid) = S.object:
         STATEMENTS_BY_OBJECT_ENTITY_TABLE.insert((eid_bytes, S.kind), S.id_bytes)
  5. if S.kind == Event:
         STATEMENTS_BY_EVENT_TIME_TABLE.insert(
             (S.event_at_unix_nanos, S.subject_bytes), S.id_bytes)
  6. For each mem_id in evidence.inline:
         STATEMENTS_BY_EVIDENCE_TABLE.insert((mem_id_bytes, S.id_bytes), ())
     For overflow_id in evidence.Overflow:
         load EvidenceOverflow; iterate memory_ids; same insertion
  7. STATEMENT_CHAIN_TABLE.insert((S.chain_root_bytes, S.version), S.id_bytes)
```

Total: 7 index writes (plus per-evidence inserts) on a typical create.

## 3. Per-supersede index updates

In addition to `statement_create` of the new statement, `statement_supersede` also updates the **old** statement:

```text
old.superseded_by = Some(new.id)
old.valid_to_unix_nanos = new.extracted_at_unix_nanos  (per spec §01 §3.2)

Rewrite old in STATEMENTS_TABLE.

Remove old's STATEMENTS_BY_SUBJECT_TABLE entry with is_current=1;
re-insert with is_current=0.
```

Other indexes (`by_predicate`, `by_object_entity`, `by_event_time`, `by_evidence`) don't care about `is_current` and stay unchanged.

## 4. Per-tombstone index updates

```text
Set fields:
  tombstoned = true
  tombstoned_at_unix_nanos = now
  tombstone_reason = reason byte

Rewrite in STATEMENTS_TABLE.

Re-insert into STATEMENTS_BY_SUBJECT_TABLE with is_current=0 (flipping
the bit; the lookup for "current state of X" no longer finds this row).
```

Reverse-evidence index entries (§1.6) are **preserved** so audit / cascade can still find the tombstoned statement.

## 5. Per-retract reclamation

`statement_retract` is the hard-delete variant. It:

1. Tombstones as in §4.
2. Schedules zero-out after `RETRACT_GRACE_NANOS` (default 30 days) — handled by the periodic GC worker (phase 21+).
3. At reclamation: remove from **all** tables except the audit row in `entity_resolution_audit` (kind discriminator `STATEMENT_RETRACTED`).

`STATEMENTS_BY_EVIDENCE_TABLE` is also stripped — the dependency is gone since the row no longer exists.

## 6. Storage costs

For a deployment with M statements averaging:

- Fixed fields (`StatementMetadata`): ~256 bytes.
- `object` (tagged union): 16-64 bytes typical.
- Inline evidence: 0-128 bytes (8 × 16-byte MemoryIds, max).
- Indexes: ~200 bytes per statement across all 6 secondary indexes.

Total: ~500-700 bytes per statement primary row + indexes. 10M statements ≈ 5-7 GB.

Plus statement HNSW (phase 21 populates): ~3 KB per statement (1536-byte vector + HNSW links).

## 7. Read paths

| Query | Path |
|---|---|
| Get statement by id | `STATEMENTS_TABLE` point lookup (O(log M)). |
| Current state for `(subject, predicate)` | `STATEMENTS_BY_SUBJECT_TABLE` point lookup at `(subject, kind, predicate_id, 1)`. |
| History of a chain | `STATEMENT_CHAIN_TABLE` prefix scan at `(chain_root, *)`. |
| All Facts with predicate X | `STATEMENTS_BY_PREDICATE_TABLE` prefix scan at `(predicate_id, Fact, *)`. |
| What references entity X as object? | `STATEMENTS_BY_OBJECT_ENTITY_TABLE` prefix scan at `(X_bytes, *)`. |
| Events in time range | `STATEMENTS_BY_EVENT_TIME_TABLE` range scan. |
| Statements depending on memory M | `STATEMENTS_BY_EVIDENCE_TABLE` prefix scan at `(M_bytes, *)`. |

## 8. Write paths summary

| Operation | Tables written |
|---|---|
| `statement_create` | `STATEMENTS` + 6 indexes + chain (7 inserts) + 1 per evidence memory |
| `statement_supersede` | All `statement_create` writes for new + 2 rewrites (old `STATEMENTS` + flip `is_current` in `STATEMENTS_BY_SUBJECT`) |
| `statement_tombstone` | `STATEMENTS` rewrite + flip `is_current` in `STATEMENTS_BY_SUBJECT` (2 writes) |
| `statement_retract` | tombstone-equivalent at write time; reclaim later |

All operations execute inside one redb `WriteTransaction`. Commit makes them atomic — half-completed indexes never observed.

## 9. Sharding

Statements live on the **subject's** shard. `statement_create` routes to that shard via the substrate's routing table (per `spec/01_system_architecture/`).

Cross-shard concerns:
- `statements_by_object_entity` is on the **object** entity's shard. A statement with `subject` on shard A and `object` on shard B writes its `by_object_entity` index entry to shard B's redb. Phase 17 implementation handles this via the existing cross-shard write path (substrate-level: `WriterHandle::route_index_write`).
- Cross-shard joins (e.g. "all statements where subject = X and object = Y") aren't first-class; the hybrid query router (phase 23) fans out.

## 10. Concurrency

Per-shard single-writer discipline ([substrate §05](../05_storage_arena_wal/)) makes concurrent statement writes on the same shard impossible at the storage layer. Cross-shard writes coordinate via the same routing mechanism the substrate uses for cross-shard edges (phase 9.11+).

## 11. Migration

The 8 statement tables are declared but were never written before phase 17. No prior deployments exist; the v2 redb schema is what 17.4 first writes to.

When phase 21's embedding worker comes online, it writes to the statement HNSW out-of-band (not in a redb txn). The HNSW file lives at `statement_embeddings.hnsw` per shard.

## 12. Tests

Storage-layer test coverage (phase 17.4):

- Round-trip every `StatementMetadata` field via `STATEMENTS_TABLE`.
- Index consistency: after a `statement_create`, the row is reachable via all 6 secondary indexes.
- After supersede: old appears in chain at `version=N`, new at `version=N+1`; `is_current` bit flipped.
- After tombstone: `is_current=0`; row still reachable via `STATEMENTS_BY_EVIDENCE_TABLE`.
- After retract grace period: row physically gone; audit row retained.
- Concurrent transactions on the same shard correctly serialise (single-writer discipline holds).
