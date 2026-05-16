# 25.01 Audit Tables

redb table layout for the audit logs (extraction, resolution,
merge). Phase 20.4 lands `EXTRACTOR_AUDIT_TABLE` + its three
indexes; phase 16 / 18 already laid the `ENTITY_RESOLUTION_AUDIT`
and `MERGE_LOG` tables.

Cross-references:
- [`./00_purpose.md`](./00_purpose.md) §"The audit log" — narrative.
- [`../22_extractors/05_audit.md`](../22_extractors/05_audit.md) —
  extractor audit row spec.

## 1. `EXTRACTOR_AUDIT_TABLE`

Primary record. One row per extraction call.

```rust
pub const EXTRACTOR_AUDIT_TABLE:
    TableDefinition<'static, u128, ExtractionAuditRow> =
    TableDefinition::new("extractor_audit");

#[derive(Archive, Serialize, Deserialize, ...)]
pub struct ExtractionAuditRow {
    pub audit_id: u128,                 // UUIDv7
    pub memory_id: u128,
    pub extractor_id: u32,
    pub extractor_version: u32,
    pub schema_version: u32,
    pub started_at_unix_nanos: u64,
    pub completed_at_unix_nanos: u64,
    pub status: u8,
    pub status_reason: String,
    pub outputs: Vec<OutputRefRow>,     // ≤ 64; overflow → follow-on row (post-v1)
    pub cost_micro_usd: u64,            // 0 in phase 20
    pub model_metadata: Vec<u8>,        // rkyv-archived blob; empty for non-LLM
    pub input_hash: [u8; 32],           // BLAKE3
}

pub struct OutputRefRow {
    pub kind: u8,                       // 1=Entity, 2=Statement, 3=Relation, 4=EntityMention
    pub id: u128,
}
```

Primary key is `audit_id`; UUIDv7 means insertion order ≈ time
order, so an iteration over the table is roughly newest-last.

## 2. Indexes

Three secondary indexes, each `()`-valued (the row data lives in
the primary table; indexes only support lookups).

```rust
pub const EXTRACTOR_AUDIT_BY_MEMORY:
    TableDefinition<'static, (u128, u128), ()> =
    TableDefinition::new("extractor_audit_by_memory");
// Key: (memory_id, audit_id) — iterating `(mem_id, 0)..(mem_id+1, 0)`
// returns all audits for one memory in time order.

pub const EXTRACTOR_AUDIT_BY_EXTRACTOR:
    TableDefinition<'static, (u32, u128), ()> =
    TableDefinition::new("extractor_audit_by_extractor");
// Key: (extractor_id, audit_id) — per-extractor history.

pub const EXTRACTOR_AUDIT_BY_TIME:
    TableDefinition<'static, (u64, u128), ()> =
    TableDefinition::new("extractor_audit_by_time");
// Key: (started_at_unix_nanos, audit_id) — global time-window scans.
```

Triple-write per extraction: primary + three indexes, all in the
same wtxn.

## 3. Phase 16 placeholder

Phase 15.1 laid down a placeholder `EXTRACTOR_AUDIT_TABLE` with a
narrower row (just `extractor_id` + `memory_id` + `outputs_blob`).
Phase 20.4 widens to the §1 shape — same table name, new value
type. Since v1 hasn't shipped, no migration concern; the existing
placeholder simply gets the wider row layout.

## 4. `ENTITY_RESOLUTION_AUDIT_TABLE` (phase 16)

Pre-existing; lives in `crates/brain-metadata/src/tables/knowledge/
audit.rs`. Same indexing pattern (by-entity + by-time). Phase 20
doesn't touch it.

## 5. `MERGE_LOG_TABLE` (phase 16.7)

Pre-existing; survivor-entity merges. Phase 20 doesn't touch it.

## 6. Retention

Per spec §00 §"Retention":

| Table | Default retention |
|---|---|
| `EXTRACTOR_AUDIT_TABLE` + indexes | 90 days |
| `ENTITY_RESOLUTION_AUDIT_TABLE` | 90 days |
| `MERGE_LOG_TABLE` | Forever |

The sweeper that deletes expired audit rows + their index entries
lands in phase 22+ (tracked in §27/07 Q4).

## 7. Performance

Spec §16/02 §2.6:

| Operation | p50 | p99 |
|---|---|---|
| Audit-row write (primary + 3 indexes, single wtxn) | 200 µs | 1 ms |
| `audit_by_memory(mem_id, limit=100)` | 500 µs | 2 ms |
| `audit_by_extractor(ext_id, limit=100)` | 500 µs | 2 ms |
| `audit_recent_failures(since, limit=100)` | 1 ms | 5 ms |

Bench in phase 20.10.

## 8. Sample query

```rust
// "Show me extractor X's most recent failures."
let mut failures = audit_by_extractor(&rtxn, ext_id, 1000)?
    .into_iter()
    .filter(|r| r.status == ExtractionStatus::Failure as u8)
    .take(20)
    .collect::<Vec<_>>();
failures.sort_by_key(|r| std::cmp::Reverse(r.started_at_unix_nanos));
```

Phase 22+ admin op `ADMIN_GET_EXTRACTION_AUDIT` wraps this with
proper filtering + pagination over the wire.

## 9. Atomicity invariants

- Extractor outputs (entity / statement / relation rows) and the
  audit row share one wtxn. Either both commit or neither does.
- Index inserts share the same wtxn as the primary insert.
- The audit row's `outputs: Vec<OutputRefRow>` is captured AFTER
  the output writes but BEFORE the wtxn commit, so the IDs in
  `outputs` are guaranteed durable.
