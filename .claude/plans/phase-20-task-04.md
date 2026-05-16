# 20.4 — Audit log

Widen `EXTRACTOR_AUDIT_TABLE` to the spec §22/05 §1 / §25/01 §1
shape, add three secondary indexes, and provide a query API in
`brain-metadata::audit_ops`.

The §15.1 placeholder is replaced wholesale; v1 hasn't shipped, no
migration concern. Existing `extraction_outcome::{SUCCESS, FAILURE,
SKIPPED}` constants become a stable u8 discriminant table that
matches `brain_extractors::ExtractionStatus` byte-for-byte.

## Files written / modified

| Path | Change |
|---|---|
| `crates/brain-metadata/src/tables/knowledge/audit.rs` | Widen `ExtractionAudit` row; add 3 index tables; bump `impl_redb_rkyv_value` type_name suffix to `::v2`. |
| `crates/brain-metadata/src/audit_ops.rs` | New: `audit_write`, `audit_get`, `audit_by_memory`, `audit_by_extractor`, `audit_recent`, `audit_recent_failures`, `AuditOpError`. |
| `crates/brain-metadata/src/lib.rs` | Module + re-exports. |
| `crates/brain-metadata/Cargo.toml` | (no change — uses existing deps). |

## Storage shape

```rust
pub const EXTRACTOR_AUDIT_TABLE:
    TableDefinition<'static, [u8; 16], ExtractionAudit> =
    TableDefinition::new("extractor_audit");

pub const EXTRACTOR_AUDIT_BY_MEMORY_TABLE:
    TableDefinition<'static, ([u8; 16], [u8; 16]), ()> =
    // Key: (memory_id_bytes, audit_id_bytes)
    TableDefinition::new("extractor_audit_by_memory");

pub const EXTRACTOR_AUDIT_BY_EXTRACTOR_TABLE:
    TableDefinition<'static, (u32, [u8; 16]), ()> =
    // Key: (extractor_id, audit_id_bytes)
    TableDefinition::new("extractor_audit_by_extractor");

pub const EXTRACTOR_AUDIT_BY_TIME_TABLE:
    TableDefinition<'static, (u64, [u8; 16]), ()> =
    // Key: (started_at_unix_nanos, audit_id_bytes)
    TableDefinition::new("extractor_audit_by_time");

#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct ExtractionAudit {
    pub audit_id_bytes: [u8; 16],
    pub memory_id_bytes: [u8; 16],
    pub extractor_id: u32,
    pub extractor_version: u32,
    pub schema_version: u32,
    pub started_at_unix_nanos: u64,
    pub completed_at_unix_nanos: u64,
    pub status: u8,                 // ExtractionStatus discriminant
    pub status_reason: String,
    pub outputs: Vec<OutputRef>,    // ≤ 64; overflow tracked in §22/07 Q9
    pub cost_micro_usd: u64,        // 0 for pattern/classifier in phase 20
    pub model_metadata: Vec<u8>,    // rkyv blob; empty for non-LLM
    pub input_hash: [u8; 32],       // BLAKE3 of memory.text
}

pub struct OutputRef {
    pub kind: u8,                   // 1=Entity, 2=Statement, 3=Relation, 4=EntityMention
    pub id: [u8; 16],
}
```

Status byte values (stable; never reassigned):
- `1` = Success
- `2` = Failure
- `3` = SkippedBudget
- `4` = SkippedFilter
- `5` = SkippedDuplicate
- `6` = SkippedDisabled

These match `brain_extractors::ExtractionStatus::as_u8()` exactly.
brain-metadata declares them as `pub mod extraction_status { pub
const SUCCESS: u8 = 1; ... }` so callers without a dep on
brain-extractors can write rows.

Output-kind byte values:
- `1` = Entity
- `2` = Statement
- `3` = Relation
- `4` = EntityMention

## audit_ops API

```rust
pub fn audit_write(
    wtxn: &WriteTransaction,
    audit: &ExtractionAudit,
) -> Result<(), AuditOpError>;

pub fn audit_get(
    rtxn: &ReadTransaction,
    audit_id: AuditId,
) -> Result<Option<ExtractionAudit>, AuditOpError>;

/// Audits for one memory, newest-first. Capped by `limit`.
pub fn audit_by_memory(
    rtxn: &ReadTransaction,
    memory_id: MemoryId,
    limit: usize,
) -> Result<Vec<ExtractionAudit>, AuditOpError>;

/// Audits by extractor, newest-first.
pub fn audit_by_extractor(
    rtxn: &ReadTransaction,
    extractor_id: u32,
    limit: usize,
) -> Result<Vec<ExtractionAudit>, AuditOpError>;

/// All audits started ≥ `since_unix_nanos`, newest-first. Capped.
pub fn audit_recent(
    rtxn: &ReadTransaction,
    since_unix_nanos: u64,
    limit: usize,
) -> Result<Vec<ExtractionAudit>, AuditOpError>;

/// `audit_recent` filtered to `status == Failure`.
pub fn audit_recent_failures(
    rtxn: &ReadTransaction,
    since_unix_nanos: u64,
    limit: usize,
) -> Result<Vec<ExtractionAudit>, AuditOpError>;
```

All reads return newest-first by descending `audit_id` (UUIDv7 →
time-ordered).

```rust
#[derive(thiserror::Error, Debug)]
pub enum AuditOpError {
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),
    #[error("audit_write: outputs exceeds cap of {cap} (got {got})")]
    OutputsOverCap { cap: usize, got: usize },
}
```

`audit_write` enforces `outputs.len() ≤ 64`; overflow returns
`OutputsOverCap` (caller can split into multiple rows later; phase
20 ships the cap, the follow-on-row mechanism is §22/07 Q9).

Read paths treat `TableDoesNotExist` as `Ok(empty)` — fresh DBs
that haven't written an audit yet still respond to queries (same
pattern as 19.5's schema_store).

## Atomicity

`audit_write` writes the primary row + three index rows in the
caller's `wtxn`. The caller commits the same transaction that
produced the outputs (entities / statements / relations), so the
audit row and its referenced output rows commit together.

`AuditOpError::OutputsOverCap` aborts before any write — the
caller's wtxn is untouched.

## Tests

`audit_ops.rs` tests (skipped under miri, require tempdir + redb):

1. Round-trip via `audit_write` + `audit_get`.
2. Three index tables populated on write.
3. `audit_by_memory` returns newest-first; multiple audits for
   one memory.
4. `audit_by_extractor` newest-first.
5. `audit_recent(since=0)` returns all.
6. `audit_recent_failures` filters status.
7. Output-cap rejection: 65 entries → `OutputsOverCap { cap: 64,
   got: 65 }`; the wtxn is rollable.
8. Empty database (no audit table yet) — every read returns
   `Ok(empty)`.
9. Status byte table — `extraction_status::*` constants match
   `brain_extractors::ExtractionStatus::as_u8()` (consume via the
   dep that brain-server already pulls in transitively).

`tables/knowledge/audit.rs` round-trip test gets updated for the
widened row.

The pre-existing `extractor_audit` row in `knowledge_compat.rs`
phase-15 closure test continues to be empty on substrate-only
runs (unchanged invariant).

## Out of scope

- Audit log sweeper (90-day retention enforcement) — phase 22+,
  tracked in §27/07 Q4.
- Output-overflow follow-on rows — §22/07 Q9.
- LLM `model_metadata` shape — phase 21.
- Wire `ADMIN_GET_EXTRACTION_AUDIT` op — §25/07 Q1, phase 22+
  admin.
- ENCODE integration — phase 20.6.

## Single commit

`feat(metadata): 20.4 — extractor audit log + indexes`

## Verification

```
just docker cargo test -p brain-metadata --lib audit
just docker cargo test -p brain-metadata --lib tables::knowledge::audit
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
cargo clippy --target x86_64-unknown-linux-gnu --workspace --all-targets -- -D warnings
```
