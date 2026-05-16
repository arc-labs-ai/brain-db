# 22.05 Extraction Audit

Every extraction call — success, failure, or skip — writes one
`ExtractionAuditRow` to the `extractor_audit` redb table. Audits
are the substrate's source of truth for "what did extractor X
do to memory Y?".

Cross-references:
- [`./00_purpose.md`](./00_purpose.md) §"Audit log" — narrative.
- [`../25_provenance_versioning/01_audit_tables.md`](../25_provenance_versioning/01_audit_tables.md)
  — concrete redb shape.

## 1. The audit row

```rust
#[derive(Archive, Serialize, Deserialize, ...)]
pub struct ExtractionAuditRow {
    pub audit_id: u128,               // UUIDv7 — ordered by time.
    pub memory_id: u128,
    pub extractor_id: u32,
    pub extractor_version: u32,
    pub schema_version: u32,
    pub started_at_unix_nanos: u64,
    pub completed_at_unix_nanos: u64,
    pub status: u8,                   // enum byte; see §3.
    pub status_reason: String,        // "" on Success.
    pub outputs: Vec<OutputRefRow>,   // 0..N produced records.
    pub cost_micro_usd: u64,          // 0 for pattern/classifier; LLM in phase 21.
    pub model_metadata: Vec<u8>,      // rkyv-archived `ModelMetadata` blob; empty for non-LLM.
    pub input_hash: [u8; 32],         // BLAKE3 of memory.text — for idempotency probe.
}

pub struct OutputRefRow {
    pub kind: u8,                     // 1=Entity, 2=Statement, 3=Relation, 4=EntityMention.
    pub id: u128,                     // EntityId / StatementId / RelationId.
}
```

`audit_id` is UUIDv7 to give time-ordered iteration without a
separate timestamp index. Same convention as the
`entity_resolution_audit` table from phase 16.

## 2. Where it's stored

`spec/25_provenance_versioning/01_audit_tables.md` defines:

```rust
pub const EXTRACTOR_AUDIT_TABLE:
    TableDefinition<'static, u128, ExtractionAuditRow> =
    TableDefinition::new("extractor_audit");
```

Plus three index tables:

```rust
pub const EXTRACTOR_AUDIT_BY_MEMORY:
    TableDefinition<'static, (u128, u128), ()> =      // (memory_id, audit_id)
    TableDefinition::new("extractor_audit_by_memory");

pub const EXTRACTOR_AUDIT_BY_EXTRACTOR:
    TableDefinition<'static, (u32, u128), ()> =       // (extractor_id, audit_id)
    TableDefinition::new("extractor_audit_by_extractor");

pub const EXTRACTOR_AUDIT_BY_TIME:
    TableDefinition<'static, (u64, u128), ()> =       // (started_at, audit_id)
    TableDefinition::new("extractor_audit_by_time");
```

Phase 16 already lays down the `EXTRACTOR_AUDIT_TABLE` placeholder
(see [`../26_knowledge_storage/00_purpose.md`](../26_knowledge_storage/00_purpose.md));
phase 20 widens it to this shape and adds the three indexes.

## 3. Status enum

```rust
#[repr(u8)]
pub enum ExtractionStatus {
    Success = 1,
    Failure = 2,
    SkippedBudget = 3,        // LLM only (phase 21).
    SkippedFilter = 4,        // trigger condition was false.
    SkippedDuplicate = 5,     // idempotent re-run probe matched.
    SkippedDisabled = 6,      // extractor was disabled at dispatch time.
}
```

Discriminants are stable — never reassigned. New variants append.

## 4. Audit query API

```rust
pub fn audit_by_memory(
    rtxn: &ReadTransaction,
    memory_id: MemoryId,
) -> Result<Vec<ExtractionAuditRow>, AuditError>;

pub fn audit_by_extractor(
    rtxn: &ReadTransaction,
    extractor_id: ExtractorId,
    limit: usize,
) -> Result<Vec<ExtractionAuditRow>, AuditError>;

pub fn audit_recent_failures(
    rtxn: &ReadTransaction,
    since_unix_nanos: u64,
    limit: usize,
) -> Result<Vec<ExtractionAuditRow>, AuditError>;
```

Returned vectors are newest-first.

Wire surface for these queries lands in a phase 22+ admin op
(`ADMIN_GET_EXTRACTION_AUDIT`); phase 20 exposes the API
internally only.

## 5. Retention

Default 90 days, configurable per deployment. A periodic worker
(`audit_log_sweeper`, §27/00) iterates `EXTRACTOR_AUDIT_BY_TIME`
and deletes rows older than the cutoff plus their index entries.

Phase 20 ships the audit-write path and the read API; the sweeper
itself lands in phase 22+ (tracked as
[`../27_knowledge_workers/07_open_questions.md`](../27_knowledge_workers/07_open_questions.md)).

## 6. Atomicity

Audit row writes share the `wtxn` that produced the extracted
outputs:

```text
wtxn = db.begin_write()
  extractor produces ExtractedItem[]
  for each item: entity_put / statement_create / relation_create
  write_audit_row(wtxn, ExtractionAuditRow { outputs, ... })
wtxn.commit()
```

If any output write fails, the wtxn rolls back — both the outputs
AND the audit row disappear together. There's no "audit says
success but no output" or vice versa.

## 7. Performance budget

Spec §16/02 §2.6:

| Operation | p50 | p99 |
|---|---|---|
| `write_audit_row` (single wtxn cost) | 200 µs | 1 ms |

Three index inserts + one primary insert. Bench in phase 20.10.

## 8. Idempotency probe

Before running an extractor, the dispatcher probes:

```rust
audit_lookup(
    rtxn,
    memory_id,
    extractor_id,
    extractor_version,
    input_hash,
) -> Option<ExtractionAuditRow>
```

If `Some(row)` returns AND the caller didn't request replay, the
dispatcher skips the extractor entirely and re-emits the cached
outputs (or, for `Skipped*` audit rows, re-skips). One new audit
row with `status = SkippedDuplicate` is written so the operator can
see the probe fired.

## 9. Open questions

See [`./07_open_questions.md`](./07_open_questions.md). Notably:

- Q-audit-size — `outputs: Vec<OutputRefRow>` could be unbounded;
  v1 caps at 64 entries per row, overflow written to a follow-on
  row.
- Q-cost-tracking — `cost_micro_usd` is set to 0 in phase 20;
  phase 21 fills it for LLM.
