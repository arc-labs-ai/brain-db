# 18.4 — `relation_ops` module

Layer-2 CRUD + cardinality-driven supersession + symmetric
canonicalisation + dual-index population. Mirrors `statement_ops`
(17.4) and `entity_ops` precedents.

## Spec refs

- `spec/20_relations/00_purpose.md` — schema + operations.
- `spec/20_relations/01_cardinality.md` — auto-supersession rules.
- `spec/20_relations/02_symmetric.md` — canonical ordering +
  dual-index population.
- `spec/20_relations/03_storage.md` — per-op write paths.
- `spec/20_relations/05_evidence.md` — flat evidence vec, reverse
  index.

## Reads-only

- `crates/brain-metadata/src/statement_ops.rs` — closest precedent.
- `crates/brain-metadata/src/entity_ops.rs` — entity CRUD pattern.
- `crates/brain-metadata/src/tables/knowledge/relation.rs` — 15.1
  scaffolding (4 tables, RelationMetadata).
- `crates/brain-core/src/knowledge/relation.rs` — `Relation` + 
  `canonical_pair` from 18.2.

## Key design decisions

### D1 — Widen `RelationMetadata` for `chain_root_bytes`

15.1 row carries supersedes/superseded_by but no `chain_root`.
Brain-core `Relation` (18.2) has `chain_root: RelationId`. Add the
field; bump archive id to v2. Self-rooted for un-superseded rows
(`chain_root_bytes == relation_id_bytes`).

### D2 — Projection helpers `metadata_from_relation` / `relation_from_metadata`

Mirror `statement.rs` pattern. Single source of truth for the
Relation ↔ RelationMetadata mapping.

### D3 — Symmetric canonicalisation at the storage boundary

`relation_create` reads `RelationType.is_symmetric` (callers pass
the bool via the brain-core `Relation.is_symmetric` mirrored field;
in-process callers and 18.7 handler set it from the type lookup
before calling).

When `is_symmetric`:
1. Reorder `(from, to)` via `canonical_pair`.
2. Index in BOTH `RELATIONS_BY_FROM` and `RELATIONS_BY_TO` at BOTH
   endpoints — so `relation_list_from(canonical_to)` and
   `relation_list_to(canonical_from)` find the row.

For asymmetric: index `from` only in BY_FROM and `to` only in BY_TO.

### D4 — Cardinality auto-supersession

`relation_create` performs the pre-create lookup per §20/01 §2:

```
match cardinality {
    ManyToMany       => no lookup; just insert.
    ManyToOne        => lookup (from, type, current=1) in BY_FROM.
    OneToMany        => lookup (to,   type, current=1) in BY_TO.
    OneToOne         => both lookups.
}

if found.len() == 0 → insert N.
if found.len() == 1 → relation_supersede(wtxn, found[0], &N, now).
if found.len() >= 2 → StorageInvariantViolated (impossible by
                       construction; surface error).

For OneToOne with both sides held by DIFFERENT existing relations:
  → InvalidArgument("two-sided cardinality conflict").
  Caller must explicitly tombstone or supersede.
```

The handler (18.7) maps `InvalidArgument` to the wire error
`RELATION_CARDINALITY_VIOLATION`.

### D5 — `relation_supersede` mirrors `statement_supersede`

- Load old; pre-conditions (not tombstoned, not already
  superseded, kind/type/subject match).
- Compute new `chain_root` (inherit or self).
- Bump version.
- Update old in place + flip is_current bit in both directional
  indexes (including symmetric dual-side).
- Insert new statement + all indexes.
- All in one wtxn.

### D6 — `relation_list_from / _to` handle symmetric transparently

API: `relation_list_from(rtxn, entity, type_filter, current_only)`.

Implementation:
- Range-scan `RELATIONS_BY_FROM` at `(entity_bytes, type, current_bit)`.
- For each hit, load `RelationMetadata` from `RELATIONS_TABLE`.
- For symmetric relations: the row is indexed under BOTH endpoints
  in BY_FROM (per §20/02 §3) — so the query also finds rows where
  `entity == canonical_to`. No additional union needed; the spec
  pre-arranges this at write time.
- Return `Vec<Relation>` (projected via `relation_from_metadata`).

`type_filter`: `None` → any type; `Some(t)` → match `relation_type_id`.

### D7 — Flat evidence; no overflow

Per §20/05, relations have flat `Vec<MemoryId>` evidence (no
per-entry metadata, no overflow row). Per evidence entry on create,
write one row to `RELATIONS_BY_EVIDENCE_TABLE`. Supersede + tombstone
preserve reverse-index entries (audit + FORGET cascade need them).

### D8 — Lighter validation than `statement_ops`

Validations:
- `from_entity / to_entity` exist (via `entity_ops::entity_get`).
- `relation_type_id` exists (via `relation_type_get`).
- `from_entity != to_entity` UNLESS the relation_type explicitly
  allows self-loops. For v1, ALL types allow self-loops (no
  restriction). The TRAVERSE algorithm (§20/04) handles self-loops
  via the visited set.
- `confidence` in `[0, 1]`.
- For symmetric: enforce canonical ordering after canonical_pair.
- `relation_type.from_type / to_type` constraints on entity types
  if set.

## Plan

### Step 1 — Widen `RelationMetadata` + add helpers

In `tables/knowledge/relation.rs`:

- Add `chain_root_bytes: [u8; 16]` field.
- Bump archive id to `…::v2`.
- Add `chain_root() -> RelationId` accessor.
- Add `confidence_bucket()` helper (deferred-not-needed for v1;
  relations don't currently have a confidence-bucket index).
- Add `metadata_from_relation(r: &Relation) -> RelationMetadata`.
- Add `relation_from_metadata(m: &RelationMetadata) -> Relation`.
- Update existing tests.

### Step 2 — `relation_ops.rs` module

`crates/brain-metadata/src/relation_ops.rs`. ~600 lines.

Functions:

```rust
pub fn relation_create(
    wtxn: &WriteTransaction,
    r: &Relation,                // brain-core value type
    now_unix_nanos: u64,
) -> Result<RelationId, RelationOpError>;

pub fn relation_get(
    rtxn: &ReadTransaction,
    id: RelationId,
) -> Result<Option<Relation>, RelationOpError>;

pub fn relation_supersede(
    wtxn: &WriteTransaction,
    old_id: RelationId,
    new_relation: &Relation,
    now_unix_nanos: u64,
) -> Result<RelationId, RelationOpError>;

pub fn relation_tombstone(
    wtxn: &WriteTransaction,
    id: RelationId,
    now_unix_nanos: u64,
) -> Result<(), RelationOpError>;

pub struct RelationListFilter {
    pub relation_type: Option<RelationTypeId>,
    pub current_only: bool,
    pub limit: usize,
}

pub fn relation_list_from(
    rtxn: &ReadTransaction,
    entity: EntityId,
    filter: &RelationListFilter,
) -> Result<Vec<Relation>, RelationOpError>;

pub fn relation_list_to(
    rtxn: &ReadTransaction,
    entity: EntityId,
    filter: &RelationListFilter,
) -> Result<Vec<Relation>, RelationOpError>;

pub fn relation_history(
    rtxn: &ReadTransaction,
    anchor: RelationId,
) -> Result<Vec<Relation>, RelationOpError>;

pub fn relations_with_evidence(
    rtxn: &ReadTransaction,
    memory_id: MemoryId,
) -> Result<Vec<RelationId>, RelationOpError>;
```

Errors:

```rust
pub enum RelationOpError {
    Storage(redb::StorageError),
    Table(redb::TableError),
    NotFound(RelationId),
    AlreadyExists(RelationId),
    UnknownRelationType(RelationTypeId),
    UnknownEntity(EntityId),
    InvalidArgument(&'static str),
    AlreadySuperseded(RelationId, RelationId),
    AlreadyTombstoned(RelationId),
    KindMismatch { old: RelationTypeId, new: RelationTypeId },
    EndpointMismatch,
    CardinalityViolation { variant: Cardinality, conflicting: Vec<RelationId> },
    DecodeFailed,
    RelationTypeOp(RelationTypeOpError),
    EntityOp(EntityOpError),
}
```

### Step 3 — Tests

Colocated unit tests (~25):

- `create_asymmetric_round_trips`.
- `create_symmetric_canonicalises_ordering`.
- `create_symmetric_indexes_both_sides`.
- `create_self_loop_allowed`.
- `create_unknown_relation_type_rejected`.
- `create_unknown_endpoint_rejected`.
- `create_endpoint_type_mismatch_rejected` (when relation_type
  constrains from/to to a specific entity_type).
- `create_many_to_one_auto_supersedes`.
- `create_one_to_many_auto_supersedes_on_to_side`.
- `create_one_to_one_supersedes_either_side`.
- `create_one_to_one_two_sided_conflict_errors`.
- `create_many_to_many_no_supersession`.
- `supersede_explicit`.
- `supersede_event_kind_doesnt_apply` (relations have no kind
  enum — different invariants).
- `supersede_endpoint_mismatch_rejected`.
- `tombstone_flips_is_current_in_both_directional_indexes`.
- `tombstone_preserves_evidence_index`.
- `list_from_entity`.
- `list_to_entity`.
- `list_from_symmetric_either_side`.
- `list_with_type_filter`.
- `list_current_only`.
- `history_walks_chain`.
- `relations_with_evidence_returns_dependent_ids`.
- `cardinality_freed_after_tombstone`.

### Step 4 — Re-exports

`lib.rs`:

```rust
pub mod relation_ops;
pub use relation_ops::{
    relation_create, relation_get, relation_history, relation_list_from,
    relation_list_to, relation_supersede, relation_tombstone,
    relations_with_evidence, RelationListFilter, RelationOpError,
};
```

## Verify

```
cargo zigbuild --target x86_64-unknown-linux-gnu -p brain-metadata --tests
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
cargo clippy --target x86_64-unknown-linux-gnu -p brain-metadata --all-targets -- -D warnings
```

## Risks

- **Largest sub-task by surface**. ~25 tests + 4 secondary indexes
  + dual-index symmetric. May split into 18.4a / 18.4b if it grows
  unwieldy.
- **Cross-shard `RELATIONS_BY_TO` writes** for symmetric relations
  where canonical_from + canonical_to are on different shards —
  documented as same-shard-only TODO in `relation_create`.
- **`RelationOpError::CardinalityViolation`** carries `Vec<RelationId>`;
  the handler (18.7) maps to `RELATION_CARDINALITY_VIOLATION`
  wire error.
- **`RelationListFilter.limit`** ships with default cap 1000 (same
  as ENTITY_LIST / STATEMENT_LIST).

## Out of scope

- Traversal (18.5).
- Wire structs (18.6).
- Handlers (18.7).
- SDK builders (18.8).
- Cross-shard reverse-index writes — phase 23.
- Entity-merge re-routing — phase 23 (or 18.9 if scope allows).
