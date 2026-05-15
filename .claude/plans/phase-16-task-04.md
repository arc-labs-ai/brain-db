# Sub-task 16.4 — Trigram index + Jaccard scoring

> Per-sub-task plan. Plan-first convention.

## Goal

Make the `entity_trigrams` table actually do something. Two pieces:

1. **Trigram primitives** — extract a deduplicated trigram set from a
   normalized name; compute Jaccard similarity between two sets;
   read/write the redb index. New module `brain-metadata::trigram_ops`.
2. **Integrate into entity_ops** — `entity_put`, `entity_update`,
   `entity_rename`, `entity_add_alias`, `entity_remove_alias`,
   `entity_tombstone` all need to keep `entity_trigrams` in sync.
   Done internally so the public API doesn't grow.

After 16.4: the resolver's tier-2 algorithm (16.5) has the
primitives it needs — `extract_trigrams`, `jaccard`,
`lookup_candidates_by_trigram`.

## Reading list

1. `spec/18_entities/02_storage.md` — `entity_trigrams` key shape.
2. `spec/18_entities/01_resolution.md` § Tier 2 — Jaccard threshold,
   query algorithm.
3. `crates/brain-metadata/src/tables/knowledge/entity.rs` —
   `ENTITY_TRIGRAMS_TABLE` declared in 15.1 (key shape needs a
   schema change; see F-1).
4. `crates/brain-metadata/src/entity_ops.rs` — the integration
   target.

## Pre-flight findings

### F-1 — Spec ↔ 15.1 schema drift on trigram key shape

Spec §18/02:

> `entity_trigrams`
> key: `(entity_type_id: u32, trigram: [u8; 3], entity_id: EntityId)`

15.1 declared:

```rust
pub const ENTITY_TRIGRAMS_TABLE: TableDefinition<
    'static,
    (u32, &'static str, [u8; 16]),
    (),
> = TableDefinition::new("entity_trigrams");
```

`&'static str` instead of `[u8; 3]`. Drift introduced because 15.1
mirrored the `&str` pattern from `entity_aliases` (which is
spec-correct for variable-length strings). Trigrams are
fixed-3-byte; the spec is right.

**Decision: change the key shape in 16.4 to `(u32, [u8; 3],
[u8; 16])`.** Safe pre-1.0 (15.5 confirmed no on-disk knowledge
data anywhere). Required test fixture update in
`tables/knowledge/entity.rs` (one literal).

Document the drift fix in the commit message — falls under
"sub-task fixes a 15.1 schema bug" not "phase 16 changes spec
direction."

### F-2 — Trigram extraction strategy: pg_trgm-style

The standard postgresql/pg_trgm approach (well-understood, no
research needed):

1. Split the normalized name into whitespace-separated words.
2. For each word, pad: `"  " + word + " "` (two leading, one
   trailing).
3. Extract every 3-byte window from each padded word.
4. Return a deduplicated `HashSet<[u8; 3]>` across all words.

Example: `"priya patel"` →

- Word `"priya"` padded → `"  priya "`
- Trigrams: `"  p"`, `" pr"`, `"pri"`, `"riy"`, `"iya"`, `"ya "`
- Word `"patel"` padded → `"  patel "`
- Trigrams: `"  p"`, `" pa"`, `"pat"`, `"ate"`, `"tel"`, `"el "`
- Deduplicated set: 11 trigrams

Operates on **bytes**, not Unicode code points. ASCII names work
exactly. Unicode names may have multi-byte code points sliced by
3-byte windows — that's pg_trgm's standard behavior; trigrams are
opaque byte buckets. Doesn't break matching as long as both query
and index extract the same way.

### F-3 — Jaccard similarity

```rust
fn jaccard(a: &HashSet<[u8; 3]>, b: &HashSet<[u8; 3]>) -> f32 {
    let intersection = a.intersection(b).count();
    let union = a.len() + b.len() - intersection;
    if union == 0 { 0.0 } else { intersection as f32 / union as f32 }
}
```

Returns `[0.0, 1.0]`. Spec §18/01 § Tier 2 default threshold is
`0.85` for "single hit accepted as Resolved." Threshold lives in
`ResolverConfig`; this module only computes the score.

### F-4 — Write path: extract from canonical_name AND aliases

An entity's trigram set is the union of trigrams from its
`canonical_name` AND every alias. Spec §18/02 example: "Each
entity's canonical_name contributes its trigrams." Aliases too —
the resolver should match `"priya"` as well as `"priya patel"`.

For `entity_put` (16.2):

```rust
let mut all = HashSet::new();
all.extend(extract_trigrams(&normalize_name(&entity.canonical_name)));
for alias in &entity.aliases {
    all.extend(extract_trigrams(&normalize_name(alias)));
}
// Write one row per trigram in `all`.
```

### F-5 — Update path: delta between old + new trigram sets

`entity_update` already computes the alias delta and the
canonical-name change in 16.2. The trigram delta follows: compute
old trigrams (from `current.canonical_name + current.aliases`),
compute new trigrams (from `next.canonical_name + next.aliases`),
remove `old - new`, add `new - old`. Single redb txn.

`entity_rename`, `entity_add_alias`, `entity_remove_alias`
funnel through `entity_update` already (16.2), so the trigram
delta path covers them transparently.

### F-6 — Tombstone tears down trigrams too

16.2's `entity_tombstone` already tears down `entity_by_canonical_name`
and `entity_aliases`. 16.4 adds `entity_trigrams` to the same
teardown — the tombstoned entity must not surface in tier-2
candidate scans.

### F-7 — Read path: candidates by trigram

Tier 2 query algorithm (per spec §18/01):

1. Extract trigrams of the candidate query.
2. For each trigram, range-scan `entity_trigrams` at prefix
   `(type_id, trigram, *)` → collect EntityIds.
3. Union all candidate EntityIds across trigrams.
4. For each candidate EntityId, fetch its trigram set (re-extract
   from canonical_name + aliases) and compute Jaccard.
5. Return candidates sorted by Jaccard descending.

16.4 ships steps 1–3 as primitives (`extract_trigrams`,
`jaccard`, `lookup_candidates_by_trigram`). The full algorithm
composition lands in **16.5** (resolver).

For step 4, re-extracting trigrams per candidate is simpler than
maintaining a reverse trigram index. Cost: O(candidates × name_len);
for tier-2's typical candidate-count (≤50) and name_len (~30
chars), well under 1 ms. Phase 14 may add a precomputed reverse
cache; 16.4 doesn't.

### F-8 — Module placement

`brain-metadata::trigram_ops` (new). Free functions matching the
existing `entity_ops` style. The integration helpers stay in
`entity_ops.rs` (private; call into `trigram_ops`).

## Design decisions

### D1 — Change `ENTITY_TRIGRAMS_TABLE` key shape

```rust
// Before (15.1):
pub const ENTITY_TRIGRAMS_TABLE:
    TableDefinition<'static, (u32, &'static str, [u8; 16]), ()> = ...;

// After (16.4):
pub const ENTITY_TRIGRAMS_TABLE:
    TableDefinition<'static, (u32, [u8; 3], [u8; 16]), ()> = ...;
```

Update the 15.1 test that uses the table — one literal change.

### D2 — `trigram_ops` API

```rust
// Extraction.
pub fn extract_trigrams(normalized: &str) -> HashSet<[u8; 3]>;
pub fn trigrams_of_entity(entity: &Entity) -> HashSet<[u8; 3]>;

// Similarity.
pub fn jaccard(a: &HashSet<[u8; 3]>, b: &HashSet<[u8; 3]>) -> f32;

// redb writes.
pub fn index_entity_trigrams(
    wtxn: &WriteTransaction,
    type_id: EntityTypeId,
    entity_id: EntityId,
    trigrams: &HashSet<[u8; 3]>,
) -> Result<(), TrigramOpError>;

pub fn remove_entity_trigrams(
    wtxn: &WriteTransaction,
    type_id: EntityTypeId,
    entity_id: EntityId,
    trigrams: &HashSet<[u8; 3]>,
) -> Result<(), TrigramOpError>;

// redb read.
pub fn lookup_candidates_by_trigram(
    rtxn: &ReadTransaction,
    type_id: EntityTypeId,
    trigram: [u8; 3],
) -> Result<Vec<EntityId>, TrigramOpError>;

// Convenience: collect all candidates across the query's trigram set.
pub fn candidates_for_query(
    rtxn: &ReadTransaction,
    type_id: EntityTypeId,
    query_normalized: &str,
) -> Result<HashSet<EntityId>, TrigramOpError>;
```

`candidates_for_query` is the union step from F-7; one call from
the resolver. Returns deduplicated EntityIds for downstream Jaccard
scoring.

### D3 — `TrigramOpError`

```rust
#[derive(thiserror::Error, Debug)]
pub enum TrigramOpError {
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),
}
```

Smaller than `EntityOpError` — trigram ops don't validate entity
types or check for duplicates; they're pure index maintenance.

### D4 — Integration with `entity_ops`

`entity_ops.rs` gains private helper:

```rust
fn write_entity_trigrams_inside_wtxn(
    wtxn: &WriteTransaction,
    type_id: EntityTypeId,
    entity_id: EntityId,
    canonical_name: &str,
    aliases: &[String],
) -> Result<(), EntityOpError> { ... }
```

Called from `entity_put` (insert), in `entity_update` (delta
remove + add), `entity_tombstone` (tear down).

`EntityOpError` gains a `TrigramOp(#[from] TrigramOpError)`
variant.

### D5 — Word-padding strategy

```rust
pub fn extract_trigrams(normalized: &str) -> HashSet<[u8; 3]> {
    let mut out = HashSet::new();
    for word in normalized.split_whitespace() {
        let mut padded = Vec::with_capacity(word.len() + 3);
        padded.extend_from_slice(b"  ");
        padded.extend_from_slice(word.as_bytes());
        padded.push(b' ');
        for window in padded.windows(3) {
            if let Ok(arr) = <[u8; 3]>::try_from(window) {
                out.insert(arr);
            }
        }
    }
    out
}
```

Empty input → empty set (no trigrams). Single-character word
(`"x"`) → padded becomes `"  x "` → trigrams `{"  x", " x "}` —
2 trigrams. Tested explicitly.

### D6 — Tests

`trigram_ops.rs`:

- `extract_trigrams_pg_trgm_style` — assert exact set for
  `"priya"` (single word) and `"priya patel"` (two words).
- `extract_trigrams_empty_string` — empty set.
- `extract_trigrams_single_char` — `"x"` → 2 trigrams.
- `extract_trigrams_unicode_does_not_panic` — `"straße"` produces
  some byte-level trigrams; assert len > 0 (specific shape is
  pg_trgm convention, not a contract).
- `jaccard_identical_sets_is_one`.
- `jaccard_disjoint_sets_is_zero`.
- `jaccard_empty_empty_is_zero` — both empty → 0 (avoids 0/0).
- `jaccard_partial_overlap` — `{a, b, c}` vs `{b, c, d}` → 2/4 = 0.5.
- `index_then_lookup_round_trips` — write 3 trigrams for one
  EntityId; `lookup_candidates_by_trigram` finds it for each.
- `remove_clears_index_rows` — index then remove; lookup empty.
- `candidates_for_query_unions_across_trigrams` — two entities
  share one trigram; query union returns both.
- `lookup_filters_by_type_id` — same trigram under two different
  types; query filters to the requested type.

`entity_ops.rs` (additional):

- `entity_put_writes_trigrams` — after put, lookup_candidates_by
  any trigram of the canonical_name returns the EntityId.
- `entity_put_aliases_contribute_trigrams` — canonical "X" with
  alias "Priya" → lookup by `"pri"` finds the entity.
- `entity_rename_updates_trigrams` — after rename, old-name
  trigrams gone (unless they overlap with new name + aliases),
  new-name trigrams present.
- `entity_tombstone_removes_trigrams` — tombstoned entity not in
  any trigram lookup.

## File plan

- `crates/brain-metadata/src/trigram_ops.rs` — **new**, ~250
  lines + tests.
- `crates/brain-metadata/src/lib.rs` — `pub mod trigram_ops;` +
  re-exports.
- `crates/brain-metadata/src/tables/knowledge/entity.rs` — change
  `ENTITY_TRIGRAMS_TABLE` key from `(u32, &'static str, [u8; 16])`
  to `(u32, [u8; 3], [u8; 16])`; update the 15.1 trigram test
  literal.
- `crates/brain-metadata/src/entity_ops.rs` — add private
  `write_entity_trigrams_inside_wtxn` /
  `remove_entity_trigrams_inside_wtxn` helpers; call them from
  `entity_put`, `entity_update`, `entity_tombstone`; add
  `TrigramOp(#[from] TrigramOpError)` to `EntityOpError`.

No new dependencies.

## Done-when

- `cargo zigbuild --target x86_64-unknown-linux-gnu --workspace
  --tests` clean.
- All new tests compile + the existing tests in 16.1/16.2 stay
  green (the entity_put round-trip now also exercises trigram
  writes).
- 15.5's `knowledge_compat` substrate-only regression still
  passes — substrate ops don't touch trigram tables.
- One commit:
  `feat(metadata): 16.4 — trigram index + Jaccard scoring`.

## Risk register

| Risk | Mitigation |
|---|---|
| Changing `ENTITY_TRIGRAMS_TABLE` key shape breaks 15.1's test fixture | Update the one fixture literal in the same commit. No on-disk data anywhere (15.5 confirmed). |
| Unicode multi-byte trigrams silently change matching characteristics | Spec is byte-level; documented as intentional. Test `extract_trigrams_unicode_does_not_panic` asserts non-empty; no exact-shape assertion. |
| Trigram write amplification on entity_put — N trigrams = N redb inserts | Typical name (~10 chars) produces ~12 trigrams; 12 redb inserts inside one txn is cheap (~50 µs). Acceptable. |
| Re-extracting candidate trigrams on every query is O(candidates × name_len) | Spec accepts this for tier 2. Reverse-trigram cache is a phase-14 perf knob, not a 16.4 blocker. |
| `EntityOpError` gains a new variant; downstream `match` exhaustive arms break | grep confirms only the tests in `entity_ops.rs` itself match on `EntityOpError`; no external exhaustive match sites yet. |
| `candidates_for_query` returns *all* type-matching entities if the query is short (lots of shared trigrams) | Resolver in 16.5 applies the Jaccard threshold to discard low-score candidates. 16.4 returns a candidate set, not a result set. |

## Open questions for your approval

1. **Schema change on `ENTITY_TRIGRAMS_TABLE`** — change key from
   `&'static str` to `[u8; 3]` per spec? **Recommended: yes.**
   Spec-aligned; pre-1.0 with no data; one-literal fixture update.
2. **`trigram_ops` separate module, integrated via private helpers
   in `entity_ops`** — public API of entity_ops doesn't change?
   **Recommended: yes.** Callers don't have to know trigram details;
   integration is invisible.
3. **No reverse-trigram cache (F-7)** — re-extract per candidate
   on tier-2 queries, defer the cache to phase 14? **Recommended:
   yes.** Cost is sub-millisecond for typical name lengths;
   premature optimization otherwise.
4. **Pad each word `"  WORD "`, not the whole string** — pg_trgm
   convention? **Recommended: yes.** Word-boundary matches are
   what makes trigram similarity useful for entity names.

## Workflow

On your nod: implement, run `cargo zigbuild --target
x86_64-unknown-linux-gnu --workspace --tests`, commit as
`feat(metadata): 16.4 — trigram index + Jaccard scoring`, then
stop and draft 16.5's plan (resolver tiers 1+2+3 — the heaviest
sub-task of Phase 16).
