# Sub-task 16.1 — Entity record types + alias promotion + Person bootstrap

> Per-sub-task plan. Plan-first convention.

## Goal

Lay the type foundation for Phase 16. Three concrete pieces:

1. **brain-core public types** — `Entity`, `EntityType`,
   `EntityAttributes`, `ResolverTier`, `ResolutionOutcome`,
   `ResolverConfig`. Pure value types, no I/O. Mirrors the
   `Memory` (brain-core) vs `MemoryMetadata` (brain-metadata)
   split.
2. **`EntityMetadata` schema bump** — promote `aliases_blob:
   Vec<u8>` to `aliases: Vec<String>` (per spec §18/00 entity
   schema). `attributes_blob` stays opaque awaiting phase 19.
   `type_name` bumps `::v1` → `::v2`.
3. **Person bootstrap** — `MetadataDb::open` seeds a built-in
   `Person` `EntityTypeDefinition` row with `EntityTypeId(1)` when
   the `entity_types` table is empty. Idempotent; safe to call on
   existing shards (no-op if any row is present).

After this sub-task: types compile, round-trip rkyv tests pass,
`MetadataDb::open` reliably has a `Person` row available. No CRUD
operations yet — those are 16.2.

## Spec references

1. `spec/18_entities/00_purpose.md` — Entity record schema
   (canonical_name / aliases / attributes / mention_count /
   timestamps / merged_into / embedding_version / type).
2. `spec/18_entities/01_resolution.md` — `ResolutionOutcome`,
   `ResolverTier`, `ResolverConfig` shapes (we lift these into
   brain-core so 16.5 can plug them in).
3. `crates/brain-core/src/memory.rs` — pattern precedent (high-
   level `Memory` value vs on-disk `MemoryMetadata`).
4. `crates/brain-metadata/src/tables/knowledge/entity.rs` —
   the table this sub-task amends.

## Pre-flight findings

### F-1 — `Entity` (brain-core) vs `EntityMetadata` (brain-metadata) split

The phase doc says `Entity`, `EntityType`, `EntityAttributes`
belong in brain-core. brain-metadata already has `EntityMetadata`
(15.1). The natural split:

- **brain-core** holds the public API value types — no rkyv, no
  redb. Used by SDK consumers, resolver logic, wire-protocol
  request/response structs.
- **brain-metadata** holds the on-disk row (rkyv-archived,
  redb-stored). Reads/writes convert at the boundary via
  `From<Entity> for EntityMetadata` and `TryFrom<EntityMetadata>
  for Entity`.

This mirrors the substrate exactly:

| Layer | Memory | Entity |
|---|---|---|
| Public API value | `brain_core::Memory` | `brain_core::Entity` (new) |
| On-disk row | `brain_metadata::tables::memory::MemoryMetadata` | `brain_metadata::tables::knowledge::entity::EntityMetadata` (15.1) |

### F-2 — `aliases` promotion

`EntityMetadata` from 15.1 has `aliases_blob: Vec<u8>` (opaque).
The resolver's tier-1 lookup needs to match aliases by string
equality (after normalization). Storing them as `Vec<String>` is
the natural representation; redb-row size grows by ~30 bytes per
alias (rkyv `Vec<String>` encoding).

Schema-version bump: `"brain_metadata::EntityMetadata::v1"` →
`"::v2"`. redb refuses to open a file written with `::v1` after
the bump — but 15.5's regression confirmed no existing data, so
the bump is safe pre-1.0.

### F-3 — `attributes` stays opaque (deferred to phase 19)

`attributes_blob: Vec<u8>` stays opaque. The typed `Value` union
needs `EntityType`'s attribute schema, which lands in phase 19
(schema DSL).

brain-core exposes `EntityAttributes(Vec<u8>)` newtype so callers
can write `entity.attributes(EntityAttributes(blob))` without
phase-19 typing in scope. Phase 19 adds typed accessors on the
same type.

### F-4 — Person bootstrap

The `entity_types` table needs at least one row before Phase 16's
CRUD validates entity_type_id. Without it:

- 16.2's `entity_put` checks `entity_type_id` exists in
  `entity_types` → fails for every call.
- Tests can't insert an entity without first manually seeding the
  table.

Cleanest fix: `MetadataDb::open` seeds Person automatically. The
seed is idempotent — if any row exists in `entity_types`, skip
(phase 19's user uploads already populated the registry). On a
fresh shard, Person is inserted as `EntityTypeId(1)` with
`name = "Person"`, `schema_blob = []` (empty until phase 19
populates it), `created_at_unix_nanos = now()`.

A schema-version table or a `bootstrap_complete` marker isn't
needed — emptiness is sufficient signal.

### F-5 — `ResolverConfig` lifts into brain-core

The phase doc puts the resolver itself in brain-core (16.5). For
that to work, `ResolverConfig`, `ResolutionOutcome`,
`ResolverTier` need to be in brain-core too (the resolver returns
them). 16.1 ships the types so 16.5 only writes the algorithm.

### F-6 — brain-core dependency surface stays minimal

The new types use `Vec`, `Option`, `String`, primitives, and
existing brain-core types (`EntityId`, etc.). No new external
deps. brain-core remains pure-value-type per its module-doc
charter.

## Design decisions

### D1 — Module layout in brain-core

```
brain-core/src/
├── knowledge/
│   ├── mod.rs       (existing)
│   ├── ids.rs       (existing)
│   ├── kinds.rs     (existing)
│   ├── entity.rs    NEW
│   └── resolver.rs  NEW (types only in 16.1; algorithm in 16.5)
```

Existing `knowledge/mod.rs` adds `pub mod entity; pub mod
resolver;` + re-exports.

`brain-core/src/lib.rs` adds the new types to the root re-exports
(`Entity`, `EntityType`, `EntityAttributes`, `ResolverConfig`,
`ResolutionOutcome`, `ResolverTier`, `TypeConstraint`).

### D2 — `Entity` value type

```rust
pub struct Entity {
    pub id: EntityId,
    pub entity_type: EntityTypeId,
    pub canonical_name: String,
    pub normalized_name: String,
    pub aliases: Vec<String>,
    pub attributes: EntityAttributes,
    pub mention_count: u32,
    pub created_at_unix_nanos: u64,
    pub updated_at_unix_nanos: u64,
    pub merged_into: Option<EntityId>,
    pub embedding_version: u32,
    pub flags: u32,
}
```

Constructor: `Entity::new_active(id, entity_type, canonical_name,
normalized_name, created_at_unix_nanos)` — mirrors
`EntityMetadata::new_active` from 15.1.

Two helper getters: `is_merged()` (returns `merged_into.is_some()`),
`has_alias(&str)` (linear scan; OK for typical alias counts ≤ 32
per spec).

### D3 — `EntityType` value type

```rust
pub struct EntityType {
    pub id: EntityTypeId,
    pub name: String,
    pub attribute_schema_blob: Vec<u8>,
    pub created_at_unix_nanos: u64,
}

impl EntityType {
    pub const PERSON_ID: EntityTypeId = EntityTypeId(1);
    pub const PERSON_NAME: &'static str = "Person";
}
```

The associated consts make the bootstrap row referenceable from
test code and from 16.2's CRUD without re-deriving the magic
number.

### D4 — `EntityAttributes` newtype

```rust
pub struct EntityAttributes(pub Vec<u8>);

impl EntityAttributes {
    pub const fn empty() -> Self { Self(Vec::new()) }
    pub fn is_empty(&self) -> bool { self.0.is_empty() }
    pub fn into_bytes(self) -> Vec<u8> { self.0 }
    pub fn as_bytes(&self) -> &[u8] { &self.0 }
}
```

Newtype keeps the typed boundary; phase 19 adds typed accessors.

### D5 — Resolver types in brain-core (algorithm in 16.5)

```rust
pub enum ResolverTier {
    Exact = 0,
    Fuzzy = 1,
    Embedding = 2,
    Llm = 3,
    Created = 4,
}

pub enum TypeConstraint {
    Strict,
    Hint,
    None,
}

pub enum ResolutionOutcome {
    Resolved {
        entity: EntityId,
        confidence: f32,
        tier: ResolverTier,
    },
    Ambiguous {
        audit_id: AuditId,
        candidates: Vec<(EntityId, f32)>,
    },
    Created {
        entity: EntityId,
    },
}

pub struct ResolverConfig {
    pub enable_exact: bool,
    pub enable_fuzzy: bool,
    pub fuzzy_threshold: f32,
    pub enable_embedding: bool,
    pub embedding_threshold: f32,
    pub embedding_top_k: usize,
    pub enable_llm: bool,
    pub llm_threshold: f32,
    pub create_confidence: f32,
    pub type_constraint: TypeConstraint,
}

impl Default for ResolverConfig {
    fn default() -> Self {
        // Per spec §18/01 §Configuration.
        Self {
            enable_exact: true,
            enable_fuzzy: true,
            fuzzy_threshold: 0.85,
            enable_embedding: true,
            embedding_threshold: 0.78,
            embedding_top_k: 5,
            enable_llm: false,
            llm_threshold: 0.85,
            create_confidence: 0.6,
            type_constraint: TypeConstraint::Hint,
        }
    }
}
```

No algorithm; 16.5 will add `fn resolve(...) -> ResolutionOutcome`
either as a free function or as a method on a `Resolver` struct
that owns trait objects for storage + embedding.

### D6 — `EntityMetadata::aliases_blob` → `aliases: Vec<String>`

```rust
// brain-metadata/src/tables/knowledge/entity.rs
pub struct EntityMetadata {
    // ...
-   pub aliases_blob: Vec<u8>,
+   pub aliases: Vec<String>,
    // ...
}

impl_redb_rkyv_value!(EntityMetadata, "brain_metadata::EntityMetadata::v2");
```

Constructor `new_active(...)` initializes `aliases: Vec::new()`.
`add_alias(&mut self, alias: String)` helper added (used by 16.2's
rename path: old canonical_name moves to aliases).

### D7 — Conversion at the boundary

```rust
// brain-metadata-side
impl From<&Entity> for EntityMetadata { ... }
impl From<&EntityMetadata> for Entity { ... }
```

Owned/borrowed split deliberate: writers usually have a `&Entity`,
readers usually want an owned `Entity`. `From` impls per
direction.

These live on the brain-metadata side because `EntityMetadata` is
local there (orphan rule: can implement foreign traits for local
types).

### D8 — Person bootstrap in `MetadataDb::open`

```rust
pub fn open(path: impl AsRef<Path>) -> Result<Self, MetadataDbError> {
    let db = Database::create(&path)?;
    let schema_version = open_or_init_schema(&db)?;

    // Existing checkpoint seed (substrate)...

    // Sub-task 16.1: bootstrap a built-in Person EntityType if the
    // registry is empty. Idempotent — skipped on shards with any
    // existing type (phase 19 SCHEMA_UPLOAD will own the registry).
    seed_builtin_entity_types(&db)?;

    // ...
}
```

`seed_builtin_entity_types` opens a write transaction, scans
`entity_types`, returns Ok(()) if any row exists, else inserts
the Person row.

### D9 — Tests

In `brain-core/src/knowledge/entity.rs`:
- `entity_new_active_sets_defaults` — constructor produces an
  unmerged, mention_count=0 entity.
- `entity_attributes_empty` — `EntityAttributes::empty().is_empty()`.
- `entity_has_alias_scans_vec` — `has_alias` linear scan finds
  inserted aliases.
- `entity_type_person_id_is_one` — `EntityType::PERSON_ID ==
  EntityTypeId(1)`.

In `brain-core/src/knowledge/resolver.rs`:
- `resolver_config_default_matches_spec` — every threshold
  matches §18/01.
- `resolution_outcome_round_trip_through_clone` — `Clone`
  preserves variants.

In `brain-metadata/src/tables/knowledge/entity.rs`:
- Update the existing `entities_round_trip` test for the new
  field layout.
- New: `aliases_round_trip` — insert with 3 aliases, read back,
  assert vec equality.
- New: `entity_metadata_v1_to_v2_breaks` — *not* a test; document
  the bump in the module-doc.

In `brain-metadata/src/db.rs`:
- `person_entity_type_seeded_on_open` — open a fresh DB, read
  `entity_types`, assert exactly one row with id=1, name="Person".
- `person_seed_is_idempotent` — open twice, assert still one
  row (not duplicated).
- `person_seed_skipped_when_registry_nonempty` — pre-insert a
  different EntityType, open DB, assert no Person row added.

In `brain-metadata/src/tables/knowledge/entity_type.rs`:
- Constructor helper for the Person row that 16.1's seed code
  reuses.

## File plan

- `crates/brain-core/src/knowledge/entity.rs` — **new**.
  ~80 lines + tests.
- `crates/brain-core/src/knowledge/resolver.rs` — **new**.
  ~120 lines + tests.
- `crates/brain-core/src/knowledge/mod.rs` — extend with `pub
  mod entity; pub mod resolver;` + re-exports.
- `crates/brain-core/src/lib.rs` — extend the
  `pub use knowledge::{...}` re-export list.
- `crates/brain-metadata/src/tables/knowledge/entity.rs` —
  promote `aliases_blob` → `aliases`; bump version; update tests;
  add From/Into impls against `brain_core::Entity`.
- `crates/brain-metadata/src/tables/knowledge/entity_type.rs` —
  add Person-row constructor.
- `crates/brain-metadata/src/db.rs` — call
  `seed_builtin_entity_types` in `open`; tests for seed.

No new dependencies in any crate.

## Done-when

- `cargo zigbuild --target x86_64-unknown-linux-gnu --workspace
  --tests` clean.
- All new tests pass type-checking; existing tests stay green.
- One commit:
  `feat(core,metadata): 16.1 — entity types + alias promotion + Person bootstrap`.

## Risk register

| Risk | Mitigation |
|---|---|
| Schema-version bump (`v1` → `v2`) breaks shards with existing data | 15.5 confirmed no existing data on substrate-only shards. Pre-1.0 we bump freely. |
| Person seed runs on every open and slows startup | `seed_builtin_entity_types` opens *one* read txn to check emptiness; only opens a write txn on actually-empty registries. Cost: ~30 µs on shards with the row already present. |
| `From<&Entity> for EntityMetadata` loses information (round-trip not exact) | Field-set is symmetric (Entity and EntityMetadata have the same conceptual fields, just encoded differently for aliases). Test asserts round-trip equality. |
| `ResolverConfig` defaults drift from spec | Test asserts every default field against the spec value; if spec changes, test fails first. |
| Phase 19 SCHEMA_UPLOAD wants to *replace* the Person seed | Coordinated convention: first SCHEMA_UPLOAD inserts user-declared types as id=2+ (id=1 reserved for Person). Phase 19's plan documents this. |
| `Entity::has_alias` is O(n) — slow if aliases >> 32 | Spec caps aliases at 32 per entity. n=32 linear scan is faster than a HashSet for these sizes. |

## Open questions for your approval

1. **`Entity` value type in brain-core (D2)** — pure-data struct
   with `pub` fields, mirroring `Memory`? **Recommended: yes.**
2. **`EntityAttributes` newtype (D4)** — thin `Vec<u8>` wrapper
   today, typed accessors in phase 19? **Recommended: yes.** Hides
   the encoding from callers without committing to a typed shape
   prematurely.
3. **rkyv `EntityMetadata` schema bump (D6)** — `::v1` → `::v2`
   on aliases promotion? **Recommended: yes.** No existing data;
   pre-1.0; bump is free.
4. **Resolver types in brain-core now (D5)** — types only in
   16.1; algorithm in 16.5? **Recommended: yes.** Keeps 16.5
   focused on the algorithm; tests in 16.1 don't need 16.5's
   `Resolver` struct to verify defaults.
5. **Person ID convention** — `EntityTypeId(1)` for the seeded
   Person, user types start at `2`? **Recommended: yes.** Stable
   ID makes test fixtures trivial; phase 19 documents the
   convention.
6. **`brain_core::TypeConstraint::Hint` as default** —
   per spec §18/01? **Recommended: yes.** Phase-19-time
   per-extractor override changes this; default matches spec.

## Workflow

On your nod: implement, run `cargo zigbuild --target
x86_64-unknown-linux-gnu --workspace --tests`, commit, then stop
and draft 16.2's plan (`brain-metadata::entity_ops` — typed CRUD
over the entity tables).
