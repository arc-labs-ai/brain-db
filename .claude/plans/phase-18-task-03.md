# 18.3 — Relation-type registry + interning

Stand up the relation-type registry in `brain-metadata` so phase
18.4's `relation_create` can validate `(from_type, to_type,
cardinality, is_symmetric)`. Mirrors the 17.3 predicate registry.

## Spec refs

- `spec/20_relations/00_purpose.md` §"Relation type declaration".
- `spec/26_knowledge_storage/00_purpose.md` §"relation_types".
- `spec/20_relations/07_references.md` — code-path table.

## Reads-only

- `crates/brain-metadata/src/tables/knowledge/relation_type.rs` —
  current minimal `RelationTypeDefinition` (15.1).
- `crates/brain-metadata/src/predicate_ops.rs` — 17.3 pattern to
  clone.
- `crates/brain-core/src/knowledge/relation.rs` — `RelationType`
  value type from 18.2 (source of truth for fields).

## Key design decisions

### D1 — Widen `RelationTypeDefinition` to match brain-core

The 15.1 row only carries `name + cardinality + is_symmetric +
from/to + created_at`. The brain-core `RelationType` adds:

- `namespace` (qname grammar parity with predicates).
- `schema_version`.
- `description`.

Bump rkyv archive id to v2. Pre-v1.0, no migration needed.

`from_entity_type_id` / `to_entity_type_id`: use `0 = any` sentinel
(EntityTypeId(0) isn't used — Person is 1; user types start at 2+).
`to_relation_type()` projects `0 → None`.

### D2 — `RELATION_TYPES_BY_QNAME_TABLE` secondary index

Parallels `PREDICATES_BY_QNAME_TABLE`. Key = `"namespace:name"`,
value = `RelationTypeId.raw()` (u32). O(log n) lookup.

### D3 — Identifier grammar

Reuse predicate grammar verbatim: `[a-z][a-z0-9_]*`, namespace ≤ 32
chars, name ≤ 64 chars, ASCII only. Shared `validate_identifier`
helper or duplicate (small enough to duplicate — keeps modules
self-contained).

### D4 — Idempotent intern

Same rule as predicates: if `(namespace, name)` exists with
identical constraint fields → return existing id. If exists with
diverging fields → `AlreadyExists` (caller resolves; schema upload
in phase 19 will overwrite via a different code path).

### D5 — Built-in `brain:related_to`

Seeded at `MetadataDb::open` via `seed_builtin_relation_types`.
Following the §20/00 §"Operations" pattern and parallel to predicate
built-ins:

```text
brain:related_to
  from_type: any (0)
  to_type:   any (0)
  cardinality: ManyToMany
  is_symmetric: false
  description: "Generic relation between two entities."
```

One built-in for v1; tests/integration use this. Phase 19's
`SCHEMA_UPLOAD` registers user types.

## Plan

### Step 1 — Widen `RelationTypeDefinition`

`crates/brain-metadata/src/tables/knowledge/relation_type.rs`:

- Add `namespace`, `description`, `schema_version` fields.
- Bump archive id to `…::v2`.
- Add `RELATION_TYPES_BY_QNAME_TABLE` const.
- Add `to_relation_type()` / `from_relation_type()` helpers.
- Add `encode_entity_type_id(Option<EntityTypeId>) -> u32` and
  inverse.
- Update round-trip test.

### Step 2 — `relation_type_ops.rs` module

`crates/brain-metadata/src/relation_type_ops.rs`. Functions:

- `relation_type_intern(wtxn, namespace, name, from_type, to_type,
   cardinality, is_symmetric, schema_version, description, now)
   -> RelationTypeId`.
- `relation_type_lookup_by_qname(rtxn, namespace, name) ->
   Option<RelationType>`.
- `relation_type_get(rtxn, id) -> Option<RelationType>`.
- `relation_type_list(rtxn, namespace_filter) -> Vec<RelationType>`.

ID allocation: scan-max-and-increment (rare creates). Reserve 0 as
sentinel.

Errors: `RelationTypeOpError` mirroring `PredicateOpError`
(Storage, Table, InvalidIdentifier, AlreadyExists).

### Step 3 — Seed `brain:related_to` at `MetadataDb::open`

Add `seed_builtin_relation_types` in `db.rs` after
`seed_builtin_predicates`. Same idempotent pattern.

### Step 4 — Re-exports

`crates/brain-metadata/src/lib.rs`:

```rust
pub mod relation_type_ops;
pub use relation_type_ops::{
    relation_type_get, relation_type_intern, relation_type_list,
    relation_type_lookup_by_qname, RelationTypeOpError,
};
```

### Step 5 — Tests

Colocated in `relation_type_ops.rs`:

- `intern_fresh_allocates_id_one`.
- `intern_idempotent_returns_same_id`.
- `intern_conflict_on_constraint_mismatch` (different cardinality).
- `intern_conflict_on_symmetric_mismatch`.
- `lookup_by_qname` (hit + miss).
- `list_by_namespace` filters.
- `invalid_namespace` × 3 (empty, uppercase, leading digit).
- `invalid_name` × 2 (empty, hyphen).
- `from_relation_type_round_trip`.

In `db.rs`:

- `builtin_relation_types_seeded_on_fresh_open`.
- `builtin_relation_types_seed_idempotent`.

## Verify

```
cargo zigbuild --target x86_64-unknown-linux-gnu -p brain-metadata --tests
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
cargo clippy --target x86_64-unknown-linux-gnu -p brain-metadata --all-targets -- -D warnings
```

## Commit message draft

```
feat(brain-metadata): relation-type registry + interning (18.3)

RelationTypeDefinition widened to match brain-core RelationType:
adds namespace, schema_version, description. rkyv archive bumped
to v2 (pre-v1.0). New RELATION_TYPES_BY_QNAME index for O(log n)
"namespace:name" lookup.

relation_type_ops exposes intern (idempotent on matching
constraints; conflict on diverging), lookup_by_qname, get, list.
Identifier grammar matches predicates: [a-z][a-z0-9_]*, max 32 ns
/ 64 name, ASCII.

MetadataDb::open seeds brain:related_to (any→any ManyToMany
asymmetric) following the predicate built-in pattern (17.3) and
Person entity-type bootstrap (16.1). Phase 19 SCHEMA_UPLOAD registers
user types against the same registry.

~12 unit tests + 2 db integration tests.

Plan: .claude/plans/phase-18-task-03.md.
```

## Risks

- **15.1 row schema change is an on-disk break.** No prod deployment
  of phase 18 exists; v2 archive id keeps rkyv strict-checks honest.
- **Identifier grammar duplication** between predicates and
  relation types. Could factor into a `validate_qname` shared
  helper later; ~20 lines each, not load-bearing.
