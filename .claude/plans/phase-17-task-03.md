# 17.3 — Predicate registry + interning + built-in registration

## Goal

Stand up the **predicate registry** in `brain-metadata` so phase 17.4's
`statement_ops::statement_create` can validate the `(kind, object-type)`
combo a caller's statement uses against a declared predicate. Phase 19's
schema DSL is what populates user predicates; phase 17.3 only owns the
built-ins (`brain:is_a`, `brain:has_name`, `brain:mentions`,
`brain:related_to`) and the intern/lookup API.

## Spec refs

- `spec/19_statements/00_purpose.md` §"Predicate vocabulary" — fields,
  built-in list, object-type-constraint examples.
- `spec/26_knowledge_storage/00_purpose.md` — table catalog for the
  knowledge layer (predicates already listed).
- `spec/19_statements/03_storage.md` — redb table assumptions
  `statement_create` is built against.

## Reads-only files

- `crates/brain-metadata/src/tables/knowledge/predicate.rs` (current
  `PredicateDefinition` shape, minimal).
- `crates/brain-core/src/knowledge/statement.rs` (the `Predicate`
  value type from 17.2 — single source of truth for fields).
- `crates/brain-metadata/src/db.rs` (`seed_builtin_entity_types`
  precedent for the bootstrap pattern at `MetadataDb::open`).

## Plan

### Step 1 — Expand `PredicateDefinition` row

The 15.1 `PredicateDefinition` is too thin for §19/00. Bring it to
parity with `brain_core::knowledge::Predicate`:

```rust
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct PredicateDefinition {
    pub predicate_id: u32,
    pub namespace: String,
    pub name: String,
    pub kind_constraint: u8,             // 0 = any / 1 = Fact / 2 = Pref / 3 = Event
    pub object_type_constraint_byte: u8, // 0 = any / 1 = Entity / 2 = Value / 3 = Memory / 4 = Statement
    pub schema_version: u32,
    pub description: String,
    pub created_at_unix_nanos: u64,
}
```

Add `to_predicate(&self) -> brain_core::knowledge::Predicate` and
`from_predicate(p: &Predicate, created_at: u64) -> Self` converters so
ops code uses brain-core's value type, persistence uses the rkyv row.

### Step 2 — Add `predicates_by_qname` lookup table

Spec §19/00 implies `(namespace, name)` lookup. Today: full-scan only.
Add:

```rust
// key = "namespace:name", value = predicate_id (u32)
pub const PREDICATES_BY_QNAME_TABLE: TableDefinition<'static, &str, u32>;
```

Single secondary index. `predicate_intern` writes both atomically;
`predicate_lookup_by_qname` reads from this. Keeps ops O(log n).

### Step 3 — `predicate_ops.rs` module

New file `crates/brain-metadata/src/predicate_ops.rs`. Public surface:

```rust
pub enum PredicateOpError {
    Storage(redb::StorageError),
    Table(redb::TableError),
    InvalidIdentifier { reason: &'static str },
    AlreadyExists { qname: String, existing_id: PredicateId },
    NotFound { qname: String },
}

pub fn predicate_intern(
    wtxn: &WriteTransaction,
    namespace: &str,
    name: &str,
    kind_constraint: Option<StatementKind>,
    object_type_constraint_byte: u8,
    schema_version: u32,
    description: &str,
    now_unix_nanos: u64,
) -> Result<PredicateId, PredicateOpError>;

pub fn predicate_lookup_by_qname(
    rtxn: &ReadTransaction,
    namespace: &str,
    name: &str,
) -> Result<Option<Predicate>, PredicateOpError>;

pub fn predicate_get(
    rtxn: &ReadTransaction,
    id: PredicateId,
) -> Result<Option<Predicate>, PredicateOpError>;

pub fn predicate_list(
    rtxn: &ReadTransaction,
    namespace_filter: Option<&str>,
) -> Result<Vec<Predicate>, PredicateOpError>;
```

**ID allocation:** scan-max-and-increment on intern. Predicate
creation is rare (mostly at schema upload); keep it simple instead of
introducing a counter row. Reserve `0` as sentinel "unset".

**Idempotency on intern:** if `(namespace, name)` already present and
ALL constraint fields match → return existing id (no error). If
already present and any constraint differs → `AlreadyExists`.

### Step 4 — Identifier validation

Spec §19/00 doesn't pin the grammar. Codify here:

- `namespace`: `[a-z][a-z0-9_]*`, max 32 chars.
- `name`: `[a-z][a-z0-9_]*`, max 64 chars.
- Bare ASCII; no `:` (it's the qname separator), no Unicode.
- Trim is **not** done — caller passes pre-trimmed.

`InvalidIdentifier` on violation. One unit test per rejection class.

### Step 5 — Built-in registration at `MetadataDb::open`

Mirror `seed_builtin_entity_types`. New function
`seed_builtin_predicates` in `db.rs`:

| qname | kind_constraint | object_type | description |
|---|---|---|---|
| `brain:is_a` | Fact | Entity | "Subject is an instance of the object entity type" |
| `brain:has_name` | Fact | Value(Text) | "Subject's canonical name" |
| `brain:mentions` | Fact | Any | "Subject mentions object — generic" |
| `brain:related_to` | Fact | Entity | "Generic relation between subject and object" |

Idempotency: probe `predicates_by_qname` first; only insert missing
rows. `MetadataDb::open` calls `seed_builtin_predicates` AFTER
`seed_builtin_entity_types`.

### Step 6 — Tests

`predicate_ops` unit tests, colocated:

- `intern_fresh` — first call returns id=1, row visible via `_get` and `_lookup_by_qname`.
- `intern_idempotent` — second call with identical args returns same id.
- `intern_conflict` — second call with different constraint → `AlreadyExists`.
- `lookup_missing` — `_lookup_by_qname` returns `None` for absent qname.
- `list_by_namespace` — `_list(Some("brain"))` returns the 4 built-ins after seed.
- `invalid_namespace` × 4 — empty, uppercase, leading digit, contains `:`.
- `invalid_name` × 3 — empty, too long, contains hyphen.
- `to_predicate_round_trip` — `from_predicate` → `to_predicate` round-trips.

`db.rs` integration tests:

- `builtin_predicates_seeded_on_fresh_open` — 4 rows, all `brain:*`.
- `builtin_predicates_seed_idempotent` — re-open keeps row count at 4.
- `builtin_predicates_skip_when_present` — pre-seeding `brain:is_a` with
  different fields → seed leaves existing row alone (idempotent, not
  forced overwrite).

### Step 7 — Re-exports

`lib.rs`:

```rust
pub mod predicate_ops;
pub use predicate_ops::{
    predicate_get, predicate_intern, predicate_list, predicate_lookup_by_qname,
    PredicateOpError,
};
```

## Files written

| Path | Change |
|---|---|
| `crates/brain-metadata/src/tables/knowledge/predicate.rs` | Expand `PredicateDefinition` to 8 fields; add `PREDICATES_BY_QNAME_TABLE`; add `to_predicate` / `from_predicate`. Update round-trip test. |
| `crates/brain-metadata/src/predicate_ops.rs` | New. Intern + lookup + list + validation. |
| `crates/brain-metadata/src/db.rs` | Add `seed_builtin_predicates`, call from `MetadataDb::open`. |
| `crates/brain-metadata/src/lib.rs` | `pub mod predicate_ops; pub use ...`. |

## Files NOT written this sub-task

- `statement_ops.rs` — 17.4.
- Predicate wire opcode — out of scope; predicates are exposed via
  `SCHEMA_UPLOAD` (phase 19) not a dedicated opcode.
- SDK predicate helpers — none planned for phase 17 (handled by
  derive macro in phase 19).

## Verification gate

```
cargo test -p brain-metadata predicate_ops
cargo test -p brain-metadata db::tests::builtin_predicate
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
cargo clippy -p brain-metadata --all-targets -- -D warnings
```

All clean before committing.

## Commit message draft

```
feat(brain-metadata): predicate registry + interning (17.3)

PredicateDefinition expanded to match §19/00 — adds kind_constraint,
object_type_constraint, schema_version, description. New
predicates_by_qname index for O(log n) namespace+name lookup.

predicate_ops exposes intern (with idempotency by qname+constraints),
lookup_by_qname, get, list. Identifier validation is conservative:
[a-z][a-z0-9_]*, max 32/64 chars, ASCII only.

MetadataDb::open seeds 4 built-in predicates (brain:is_a,
brain:has_name, brain:mentions, brain:related_to) following the
Person bootstrap pattern from 16.1. Idempotent re-open is verified.
```

## Risks

- **PredicateDefinition shape change is an on-disk break.** No
  production deployment of phase 17.x exists yet — fine to widen the
  rkyv row. Bumping the per-row archive id keeps rkyv strict-checks
  honest; reuse `impl_redb_rkyv_value!` with `…::v2`.
- **Identifier grammar codification** sets a precedent; phase 19's
  schema DSL parser will inherit these rules (one source of truth).
- **`scan-max-and-increment` allocator** is acceptable because predicate
  creation is rare (one burst at SCHEMA_UPLOAD). If predicate count
  ever exceeds ~10k, swap to a counter row — out of scope for v1.

## Out of scope (this sub-task)

- Statement CRUD (17.4).
- Schema DSL hooks that populate user predicates (phase 19).
- Wire opcode for runtime predicate registration (deliberately absent —
  predicates only mutate via SCHEMA_UPLOAD).
- Per-predicate decay overrides (§19/06 Q8, post-v1.0).
