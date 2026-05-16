# 18.2 — Relation value types in brain-core

New file `crates/brain-core/src/knowledge/relation.rs` with pure
value types backing the Layer-2 graph edges. Pure value types — no
I/O, no rkyv (storage shape lives in brain-metadata; wire shape
lives in brain-protocol).

## Spec refs

- `spec/20_relations/00_purpose.md` — schema.
- `spec/20_relations/01_cardinality.md` — Cardinality enum already
  in `kinds.rs`.
- `spec/20_relations/02_symmetric.md` §2 — canonical from/to
  ordering.

## Types

### `Relation`

19 fields matching spec §20/00 + supersession fields mirroring
`Statement`:

```rust
pub struct Relation {
    pub id: RelationId,
    pub relation_type: RelationTypeId,
    pub from_entity: EntityId,
    pub to_entity: EntityId,
    pub properties_blob: Vec<u8>,        // phase 19 schema DSL types
    pub confidence: f32,
    pub evidence: Vec<MemoryId>,         // flat per §05
    pub extractor_id: ExtractorId,
    pub extracted_at_unix_nanos: u64,
    pub valid_from_unix_nanos: Option<u64>,
    pub valid_to_unix_nanos: Option<u64>,
    pub version: u32,
    pub superseded_by: Option<RelationId>,
    pub supersedes: Option<RelationId>,
    pub chain_root: RelationId,
    pub tombstoned: bool,
    pub tombstoned_at_unix_nanos: Option<u64>,
    pub is_symmetric: bool,              // mirrored from RelationType for fast access
}
```

### `RelationType`

User-declared relation type. Phase 19 schema DSL builds these from
the DSL; phase 18.3 hand-registers built-ins.

```rust
pub struct RelationType {
    pub id: RelationTypeId,
    pub namespace: String,                // "brain", "acme", ...
    pub name: String,                     // "related_to", "reports_to"
    pub from_type: Option<EntityTypeId>,  // None = any
    pub to_type: Option<EntityTypeId>,    // None = any
    pub cardinality: Cardinality,
    pub is_symmetric: bool,
    pub schema_version: u32,
    pub description: String,
}
```

`canonical()` returns `"namespace:name"` (parallels Predicate).

### Helpers

- `canonical_pair(a, b) -> (EntityId, EntityId)` — returns `(a, b)`
  if `a < b` byte-wise, else `(b, a)`. Used by symmetric write
  path.
- `Relation::new_root(...)` — fresh relation, chain_root = id,
  version = 1, no supersedes / superseded_by / tombstone fields.
- `Relation::is_current(now)` — mirrors `Statement::is_current`.
- `Relation::is_chain_root()`.

## Tests

~10 unit tests:

- `relation_new_root` — defaults verified.
- `canonical_pair_sorts_ascending` × 2 (a<b, a>b).
- `canonical_pair_handles_equal` — same id → same pair.
- `is_current_true_when_active`.
- `is_current_false_after_tombstone`.
- `is_current_false_after_supersede`.
- `is_current_respects_valid_window`.
- `is_chain_root_self_referential`.
- `relation_type_canonical_form` — "ns:name".
- `cardinality_from_u8_round_trip` (already exists in kinds.rs;
  add a relation_type-side test for completeness).

## Verify

```
cargo test -p brain-core knowledge::relation
cargo clippy -p brain-core --all-targets -- -D warnings
```

## Out of scope

- rkyv (storage layer; phase 18.4 in tables/knowledge/relation.rs
  already has it from 15.1, will be widened).
- Wire types (18.6).
- Properties typed shape (phase 19 schema DSL).
