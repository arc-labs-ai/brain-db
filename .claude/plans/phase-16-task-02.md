# Sub-task 16.2 — redb entity CRUD operations

> Per-sub-task plan. Plan-first convention.

## Goal

Make the entity tables queryable + writable through a typed, redb-
transaction-aware API. After this sub-task:

- Entities can be **inserted** (`entity_put`) with their indexes
  (canonical_name + aliases) updated atomically.
- Entities can be **fetched** by `EntityId`, by canonical name, or
  by alias.
- Entities can be **renamed**: the old canonical_name moves into the
  alias set; indexes update transactionally.
- Entities can be **tombstoned**: secondary indexes are torn down so
  the resolver never sees the row again, but the primary record
  stays for audit / unmerge in 16.7.
- A small **list_by_type** scan supports tests; full filter
  (`name_prefix`, `mention_count_min`) is phase 18+ work.

What this sub-task does NOT do:

- Trigram index writes — that's **16.4**. `entity_put` does not
  touch `entity_trigrams` yet; 16.4 layers in a separate
  `trigram_ops::index_entity_name` call on top.
- Entity-embedding HNSW — that's **16.3**.
- Merge / unmerge — that's **16.7**.
- Wire protocol — that's **16.6**.
- Resolver — that's **16.5** (which consumes 16.2's read paths).

## Reading list

1. `spec/18_entities/02_storage.md` — table layout, read/write paths.
2. `spec/18_entities/00_purpose.md` — entity lifecycle: rename moves
   old canonical_name into aliases; merge is separate.
3. `crates/brain-metadata/src/sink.rs` — precedent for multi-table
   ops inside one redb transaction.
4. `crates/brain-metadata/src/tables/knowledge/entity.rs` — the
   table sigs we operate against (post-16.1 state).

## Pre-flight findings

### F-1 — Tier-1 exact-match index is single-value

`entity_by_canonical_name` is keyed by `(entity_type_id, &str)` →
`[u8; 16]` (the EntityId bytes). Single value per key. Implication:
two entities with the same `(type, normalized_canonical_name)` in
the same shard **cannot both have rows in this index**.

Two options:
- (a) Allow the row, but reject the second `entity_put` with
  `DuplicateCanonicalName`. The resolver's tier-1 `lookup_exact`
  returns at most 1 hit.
- (b) Promote the key to `(type, name, EntityId)` with value `()`
  (multi-value). The resolver returns a `Vec<EntityId>` from
  tier-1 and falls through to tier-2.

Spec §18/01's pseudo-code is explicitly designed for "(b)" — the
resolver handles `match exact_hits.len() { 1 => Resolved, 0 =>
proceed, _ => fall through }`. But spec §18/02 unambiguously
specifies single-value:

> `entity_by_canonical_name`
> key: `(entity_type_id, normalized_name)`
> value: `EntityId`

Reconciliation: spec treats "(a)" as the design. The tier-1 `_ =>`
arm of the pseudo-code handles a theoretical alias-side collision
(an alias matching another entity's canonical_name), not a primary
canonical_name collision. **Decision: enforce uniqueness on insert
— return `EntityOpError::DuplicateCanonicalName`.**

This is the right call also because a duplicate `(type,
canonical_name)` is almost always a bug at the call site:

- Pre-extraction: the resolver should have found the existing
  entity instead of creating a new one.
- Post-rename: the rename should have failed atomically.

Returning a typed error keeps the contract honest.

### F-2 — Aliases are multi-value

`entity_aliases` keys on `(entity_type_id, &str, [u8; 16])` with
value `()`. Same alias → multiple entities is fine. `entity_put`
inserts one row per alias under the entity's own EntityId.

### F-3 — Name normalization is the responsibility of the ops layer

Spec §18/01: `let normalized = normalize(candidate); // trim,
lowercase, collapse whitespace`. Both write and read paths must
normalize. We expose `normalize_name(&str) -> String` and call it
internally in `entity_put`, `entity_lookup_by_canonical_name`,
`entity_lookup_by_alias`. Callers can also call it directly for
external-facing APIs (wire layer in 16.6).

Edge cases: Unicode lowercase (use `str::to_lowercase`, which is
locale-independent and handles full Unicode), tab/newline as
whitespace, leading/trailing whitespace, internal multi-space
collapse to single space.

### F-4 — Update path detects index deltas

`entity_update` takes a full `&Entity` (the caller's desired new
state). The function:

1. Reads the current row.
2. If `canonical_name` changed → remove old row from
   `entity_by_canonical_name`, add new row. Also push the **old**
   canonical_name onto the new entity's aliases (per spec §18/00:
   "Mutable; old values move into `aliases`"). The caller can
   override by passing aliases that already contain the new tail
   (we deduplicate).
3. Compute alias delta: `{old} - {new}` to remove from
   `entity_aliases`, `{new} - {old}` to add.
4. Bump `embedding_version` if `canonical_name` changed (so the
   embedding worker in phase 21 re-embeds).
5. Bump `updated_at_unix_nanos` to `now()` (caller provides;
   `entity_ops` doesn't read the clock).
6. Write the new primary row.

All in one `WriteTransaction`. No partial state visible.

### F-5 — Tombstone tears down secondary indexes

Tombstoned entities should never appear in resolver lookups.
Cheapest implementation: on `entity_tombstone(wtxn, id)`:

1. Read the current row.
2. Remove the canonical_name index entry.
3. Remove every alias index entry.
4. Set `flags |= TOMBSTONED` and `tombstoned_at_unix_nanos = now()`.
5. Write the primary row back (so `entity_get` still works for
   audit, and 16.7's unmerge can rehydrate).

This makes tier-1 / tier-2 lookups O(1) faster than "scan + filter
by flag." Trade-off: tombstoned entities are not listed by
`entity_list_by_type` either, which is the intended user behavior.

### F-6 — Entity-type-id existence check

`entity_put` validates that `entity_type_id` exists in the
`entity_types` registry. Without this, callers can write entities
with a phantom type that's never been declared. 16.1's seed
guarantees `EntityTypeId(1)` (Person) is always present; phase 19
uploads add more.

Cost: one extra `get` in the write transaction (~1 µs).

### F-7 — `entity_list_by_type` is small-list-only for 16.2

Spec lists `entity_list(filter: type, name_prefix, mention_count_min)`
as a wire opcode. 16.2 ships only the simplest form
(`entity_list_by_type(rtxn, type_id)`) which scans the primary
table and filters in-memory. For 1K entities of one type that's
~5 ms.

Pagination + the richer filter shape land in **16.6** (wire-protocol
ENTITY_LIST handler) where we know what filters extractors
actually need.

## Design decisions

### D1 — New module `brain-metadata::entity_ops`

```
crates/brain-metadata/src/
├── lib.rs           (extend)
├── entity_ops.rs    NEW — free functions over WriteTransaction / ReadTransaction
└── ...
```

Free functions, not a struct. Mirrors the existing `sink.rs`
pattern (free `apply_*` helpers under one `impl MetadataDb`). The
caller controls transaction scope, which matters when the resolver
in 16.5 wants `entity_put` + a trigram-index write
(16.4) + an embedding-vector insert (16.3) in a single redb txn.

### D2 — API surface

```rust
// Write paths.
pub fn entity_put(
    wtxn: &WriteTransaction,
    entity: &Entity,
) -> Result<(), EntityOpError>;

pub fn entity_update(
    wtxn: &WriteTransaction,
    new_state: &Entity,
    now_unix_nanos: u64,
) -> Result<(), EntityOpError>;

pub fn entity_rename(
    wtxn: &WriteTransaction,
    id: EntityId,
    new_canonical_name: String,
    now_unix_nanos: u64,
) -> Result<(), EntityOpError>;

pub fn entity_add_alias(
    wtxn: &WriteTransaction,
    id: EntityId,
    alias: String,
    now_unix_nanos: u64,
) -> Result<(), EntityOpError>;

pub fn entity_remove_alias(
    wtxn: &WriteTransaction,
    id: EntityId,
    alias: &str,
    now_unix_nanos: u64,
) -> Result<(), EntityOpError>;

pub fn entity_tombstone(
    wtxn: &WriteTransaction,
    id: EntityId,
    now_unix_nanos: u64,
) -> Result<(), EntityOpError>;

// Read paths.
pub fn entity_get(
    rtxn: &ReadTransaction,
    id: EntityId,
) -> Result<Option<Entity>, EntityOpError>;

pub fn entity_lookup_by_canonical_name(
    rtxn: &ReadTransaction,
    type_id: EntityTypeId,
    candidate: &str,
) -> Result<Option<EntityId>, EntityOpError>;

pub fn entity_lookup_by_alias(
    rtxn: &ReadTransaction,
    type_id: EntityTypeId,
    candidate: &str,
) -> Result<Vec<EntityId>, EntityOpError>;

pub fn entity_list_by_type(
    rtxn: &ReadTransaction,
    type_id: EntityTypeId,
) -> Result<Vec<Entity>, EntityOpError>;

// Utility — exposed for callers (wire layer in 16.6 normalizes
// at the boundary).
pub fn normalize_name(s: &str) -> String;
```

`now_unix_nanos` is caller-supplied: avoids hidden side-effects on
the clock, makes tests deterministic, lets the wire layer pass a
single timestamp consistently across batched ops.

### D3 — `EntityOpError`

```rust
#[derive(thiserror::Error, Debug)]
pub enum EntityOpError {
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),
    #[error("transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),
    #[error("entity {0:?} not found")]
    NotFound(EntityId),
    #[error("entity type {0:?} not registered")]
    UnknownEntityType(EntityTypeId),
    #[error(
        "duplicate canonical_name {name:?} for entity_type {type_id:?} (existing id {existing:?})"
    )]
    DuplicateCanonicalName {
        type_id: EntityTypeId,
        name: String,
        existing: EntityId,
    },
}
```

Distinct from `MetadataDbError` (open-time only). Caller converts
via `?` since the variants `From<redb::*>` cover the common errors.

### D4 — Normalization function

```rust
pub fn normalize_name(s: &str) -> String {
    s.trim()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
```

- `trim()`: leading/trailing whitespace.
- `to_lowercase()`: Unicode-aware.
- `split_whitespace().join(" ")`: collapses any internal whitespace
  run (spaces, tabs, newlines) to a single space.

### D5 — Update path: rename moves old canonical_name into aliases

```rust
pub fn entity_update(wtxn, new_state, now) -> Result<()> {
    let current = entity_get_inside_wtxn(wtxn, new_state.id)?
        .ok_or(EntityOpError::NotFound(new_state.id))?;

    let mut next = new_state.clone();
    next.updated_at_unix_nanos = now;

    if current.canonical_name != next.canonical_name {
        // Move old canonical name into aliases (dedup).
        if !next.has_alias(&current.canonical_name) {
            next.aliases.push(current.canonical_name.clone());
        }
        next.embedding_version = current.embedding_version + 1;

        // Remove old canonical_name index entry.
        // Add new canonical_name index entry — fails if collision.
    }

    // Compute alias delta against `current.aliases`.
    let old_aliases: HashSet<_> = current.aliases.iter().collect();
    let new_aliases: HashSet<_> = next.aliases.iter().collect();
    let removed: Vec<_> = old_aliases.difference(&new_aliases).collect();
    let added: Vec<_> = new_aliases.difference(&old_aliases).collect();
    // Remove `removed` from entity_aliases, add `added`.

    // Write back primary row.
    Ok(())
}
```

### D6 — Tombstone teardown

```rust
pub fn entity_tombstone(wtxn, id, now) -> Result<()> {
    let current = entity_get_inside_wtxn(wtxn, id)?
        .ok_or(EntityOpError::NotFound(id))?;

    // Remove from canonical_name index.
    // Remove all alias index entries.
    // Set flag bit + timestamp.
    let mut next = current;
    next.flags |= flags::TOMBSTONED;
    // Record tombstone time in a way later phases can use; for
    // 16.2 we keep aliases empty (drained) on the primary row to
    // make "re-list aliases" unambiguous.
    next.aliases.clear();
    next.updated_at_unix_nanos = now;

    // Write back primary row.
    Ok(())
}
```

(`flags::TOMBSTONED` is a new bit added in this sub-task; bit 0 by
analogy with `MemoryMetadata::flags::ACTIVE` but inverted —
TOMBSTONED is set, not cleared. Document this in the flags
sub-module.)

### D7 — Flag bits module

Add a `flags` submodule inside `tables/knowledge/entity.rs`
analogous to `tables::memory::flags`:

```rust
pub mod flags {
    /// Bit 0: entity has been tombstoned.
    pub const TOMBSTONED: u32 = 1 << 0;
    /// Bit 1: entity has been merged into another (also surfaced via
    /// `merged_into_bytes.is_some()`; the flag is redundant but
    /// makes flag-scan filters trivial).
    pub const MERGED: u32 = 1 << 1;
    /// Bits 2..=31 reserved.
    pub const RESERVED_MASK: u32 = !(TOMBSTONED | MERGED);
}
```

`MERGED` lands here so 16.7 doesn't re-shape the module; the flag
is unused in 16.2.

### D8 — Tests

In `entity_ops.rs`:

- `normalize_name_lowercases_and_collapses` — full Unicode + tabs +
  multiple internal spaces → single normalized form.
- `entity_put_then_get_round_trips` — write a Person + alias-less
  entity; read it back equal.
- `entity_put_writes_alias_index` — write with 3 aliases; verify
  `entity_aliases` has 3 rows; `lookup_by_alias` returns the
  EntityId for each.
- `entity_put_validates_entity_type_exists` — unknown
  `entity_type_id` → `UnknownEntityType`. Use a `Person`-seeded
  fresh DB; an EntityTypeId(99) fails.
- `entity_put_rejects_duplicate_canonical_name` — two entities with
  same `(type, normalized_canonical_name)` → second errors with
  `DuplicateCanonicalName{existing}`.
- `entity_lookup_by_canonical_name_finds_inserted` — exact-match
  works after normalization.
- `entity_lookup_by_canonical_name_misses_after_rename` —
  rename removes the old index entry.
- `entity_rename_moves_old_canonical_name_to_aliases` — verifies
  the spec §18/00 "old values move into aliases" rule. Also
  asserts `embedding_version` bumped.
- `entity_update_alias_delta_writes_correct_rows` — start with
  aliases {A, B}; update to {B, C}; assert `entity_aliases` ends
  up with {B, C} rows.
- `entity_add_alias_dedup` — adding an alias that's already
  present is a no-op.
- `entity_remove_alias_removes_index_row` — `entity_aliases` row
  goes away after removal.
- `entity_tombstone_removes_from_indexes` — after tombstone,
  `lookup_by_canonical_name` and `lookup_by_alias` return None /
  empty.
- `entity_tombstone_preserves_primary_row` — `entity_get` still
  returns Some (for audit), flags now include TOMBSTONED.
- `entity_list_by_type_returns_only_matching` — insert 2 Person
  entities + 1 other-type entity; list_by_type(Person) returns 2.

## File plan

- `crates/brain-metadata/src/entity_ops.rs` — new module, ~350
  lines + tests.
- `crates/brain-metadata/src/lib.rs` — `pub mod entity_ops;` +
  re-export of `EntityOpError` and the function names.
- `crates/brain-metadata/src/tables/knowledge/entity.rs` — add the
  `flags` submodule. No schema bump.

No new dependencies.

## Done-when

- `cargo zigbuild --target x86_64-unknown-linux-gnu --workspace
  --tests` clean.
- All ~14 new tests in `entity_ops.rs` compile.
- Existing tests in `brain-core::knowledge` (18 from 16.1) and
  `brain-metadata::tables::knowledge::entity` (3) stay green.
- 15.5's `knowledge_compat` substrate-only regression still
  passes — adding the entity_ops module mustn't accidentally write
  to knowledge tables during substrate ops.
- One commit:
  `feat(metadata): 16.2 — typed CRUD for entities`.

## Risk register

| Risk | Mitigation |
|---|---|
| `entity_update` opens too many tables in one txn and exceeds redb's per-txn limit | redb has no fixed cap; the four tables we touch (entities, by_canonical_name, aliases, entity_types for the existence check) are well under any practical bound. |
| `to_lowercase()` allocates a String per call; hot path may matter | Entity create rate is low (<100/s) per phase plan; allocation is fine. Phase 20 extractors that batch can opt for a slice-based normalize_to fn if profiling shows it. |
| Tombstone-then-re-create-with-same-name fails on `DuplicateCanonicalName` | False alarm — tombstone removes the canonical_name index entry, so re-create finds an empty slot. Test covers this. |
| Updating aliases in a non-deterministic order corrupts the alias index | Use a HashSet-based delta computation; insertion order doesn't matter (aliases are a set conceptually). |
| `entity_list_by_type` scans full table — O(N) | Acceptable for tests with N < 1K. 16.6 wire layer adds pagination + filter shape; full filter implementation is phase 18+. |
| The `flags::TOMBSTONED` bit collides with a future flag from another phase | Document the bit layout in `tables/knowledge/entity.rs::flags`; reserved bits are explicit. |

## Open questions for your approval

1. **API as free functions (D1, D2)** — not methods on
   `MetadataDb`? **Recommended: free functions.** Callers compose
   them inside their own `WriteTransaction` for atomic multi-table
   work (16.3 HNSW + 16.4 trigram + this CRUD all in one txn).
2. **Caller-supplied `now_unix_nanos` (D2)** — entity_ops doesn't
   call the clock itself? **Recommended: yes.** Avoids hidden
   side-effects; matches the wire layer's pattern of a single
   timestamp per request.
3. **Duplicate canonical_name → error (F-1)** — vs. permitting
   collisions and letting the resolver disambiguate?
   **Recommended: error.** Spec §18/02 keys the index single-value;
   a duplicate is almost always a bug at the call site.
4. **Tombstone tears down secondary indexes (D6, F-5)** —
   vs. filtering on read? **Recommended: tear down.** Faster
   lookups; tombstoned-but-readable primary row preserves audit.
5. **`flags::TOMBSTONED` + `flags::MERGED` (D7)** — both shipped in
   16.2 even though `MERGED` is unused until 16.7? **Recommended:
   yes.** One module-shaping commit beats two.
6. **No trigram-index writes here (scope)** — 16.4 layers in
   the trigram write on top of `entity_put`? **Recommended: yes.**
   Phase plan splits 16.2 (CRUD) from 16.4 (trigram extraction +
   index) deliberately.

## Workflow

On your nod: implement, run `cargo zigbuild --target
x86_64-unknown-linux-gnu --workspace --tests`, commit as
`feat(metadata): 16.2 — typed CRUD for entities`, then stop and
draft 16.3's plan (entity HNSW per shard).
