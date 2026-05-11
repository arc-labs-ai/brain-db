# Phase 3 — Task 3.9: `checkpoints` table

**Classification:** simple. One table with one rkyv-derived value (six u64 fields, all `Copy`-able) and one helper for the spec-mandated "most recent checkpoint is the recovery target" query. The last spec'd table — after this lands, the metadata store has 13 of 13.

**Spec:** `spec/05_storage_arena_wal/09_checkpointing.md` §2 (full struct + table shape); `spec/07_metadata_graph/02_table_layout.md` §1 row 11 (catalog entry).

**Cross-checked:** `crates/brain-storage/src/wal/checkpoint.rs` already defines `CheckpointReport` (sub-task 2.12's output) — same conceptual record as the spec's `Checkpoint` struct but with two operational extras (`lsn_begin` / `lsn_end`) and one spec field missing (`metadata_version_at_checkpoint`). The 3.9 row type matches the spec; conversion from `CheckpointReport` is 3.11 (`MetadataSink`)'s concern.

## 1. Scope

In:

- `crates/brain-metadata/src/tables/checkpoint.rs` (new):
  - `CHECKPOINTS_TABLE: TableDefinition<'static, u64, CheckpointMeta>` — keyed by `checkpoint_id`, valued by the spec's `Checkpoint` struct.
  - `CheckpointMeta` — rkyv-derived: `checkpoint_id`, `durable_lsn`, `arena_capacity_at_checkpoint`, `metadata_version_at_checkpoint`, `started_at_unix_nanos`, `completed_at_unix_nanos`. All `u64`. Re-uses the established `::v1` type_name + AlignedVec workaround.
  - `pub fn latest(table: &ReadOnlyTable<u64, CheckpointMeta>) -> Result<Option<CheckpointMeta>, redb::StorageError>` — returns the row with the highest `checkpoint_id` (the recovery target per spec §05/09 §2), or `None` on an empty table. Iterates in reverse.
- `crates/brain-metadata/src/tables/mod.rs` — add `pub mod checkpoint;`.

Out (deferred):

- **Composition with `write_checkpoint`** (read `CheckpointReport` from brain-storage's WAL checkpoint, fill in `metadata_version_at_checkpoint` from `schema::CURRENT_SCHEMA_VERSION`, insert into `CHECKPOINTS_TABLE`) — `MetadataSink::apply(CheckpointEnd)` in 3.11.
- **Retention sweep** (delete checkpoints older than the recovery target per spec §6's retention policy) — Phase 8 maintenance worker.
- **Recovery integration** — 3.11 reads `latest()` to find the resume point, then replays WAL records after that LSN.
- **`From<&CheckpointReport>` conversion** — 3.11. Caller assembles `CheckpointMeta` from a `CheckpointReport` + the current schema version. Two crates each owning their own type keeps the boundary clean (brain-storage doesn't depend on brain-metadata's schema constants).
- **Concurrency / "multiple checkpoints in flight" handling** (spec §5 covers this at the system level) — coordination lives above the storage layer.

## 2. Spec quotes that bind the design

> **§05/09 §2 (the struct):**
> ```rust
> struct Checkpoint {
>     checkpoint_id: u64,                  // Monotonic counter
>     durable_lsn: u64,                    // All records up to and including this LSN are durable in arena+metadata
>     arena_capacity_at_checkpoint: u64,
>     metadata_version_at_checkpoint: u64,
>     started_at: u64,                     // unix nanoseconds
>     completed_at: u64,
> }
> ```
>
> **§05/09 §2 (the table):**
> ```
> table: checkpoints
> key: checkpoint_id (u64)
> value: Checkpoint struct
> ```
>
> **§05/09 §2 (multiple rows):** "Multiple checkpoints can exist; the substrate keeps the most recent one as the recovery target." → keys are monotonic; "most recent" = highest key; iter-reverse picks it.
>
> **§07/02 §1 row 11:** `checkpoints | u64 | CheckpointInfo | Checkpoint records`. ("`CheckpointInfo`" is the catalog's pet name; `Checkpoint` is the §05/09 §2 detail name. v1 picks `CheckpointMeta` to match the rest of brain-metadata's `*Metadata`/`*Info` naming.)

## 3. Design decisions

### 3.1 Time-field suffixing per established convention

Spec calls the time fields `started_at` and `completed_at` (both noted as unix nanoseconds). We suffix to `started_at_unix_nanos` / `completed_at_unix_nanos` — same renaming applied in 3.8's `ModelInfo` and consistently across 3.2/3.3/3.4/3.5. Not an SD (spec doesn't pin naming).

### 3.2 `CheckpointMeta`, not `CheckpointInfo` or `Checkpoint`

`Checkpoint` collides with brain-storage's WAL `Checkpoint` records — bad imports waiting to happen. `CheckpointInfo` is the spec catalog's pet name but doesn't match the rest of this crate (`MemoryMetadata`, `AgentMetadata`, `ContextMetadata`, `ModelInfo`). `CheckpointMeta` follows the metadata-row convention. Compiler-checked: zero name collisions with any current brain-* type.

### 3.3 `latest()` over `&ReadOnlyTable`, not `&Database`

`latest()` takes a *table handle*, matching the established pattern from 3.5's `prune_expired` and 3.7's `increment`: the caller already has a read transaction and the table open (because recovery composes multiple table lookups). A `&Database`-taking variant would force the helper to start its own read txn, which fights composition.

`ReadOnlyTable` (vs `&mut Table`) reflects that `latest()` is read-only — no writes, no mutation. Recovery reads checkpoints; only the checkpoint worker writes them.

### 3.4 Use `iter().rev().next()` or `range(..).next_back()` for "latest"

redb's `Range` is `DoubleEndedIterator`. `table.iter()?.next_back()` returns the highest-key row in O(log N) — single B-tree path to the rightmost leaf. Equivalent to `range(..).next_back()`. Pick whichever reads cleaner; both are O(log N).

### 3.5 No `From<CheckpointReport>` impl in this file

Tempting, but it would either (a) require brain-metadata to import brain-storage's WAL types (a backward dependency that breaks the layering — brain-metadata is below the WAL writer in the dep graph), or (b) require brain-storage to know about `CheckpointMeta` (the opposite, also wrong). The clean composition is in 3.11's `MetadataSink::apply(CheckpointEnd)` where both types are in scope.

`brain-metadata` depends on `brain-storage` already (for the `MetadataSink` trait). So technically (a) is possible. But: 3.11's `MetadataSink` impl is the one place that needs the conversion, and it lives in this same crate. Putting the conversion in `checkpoint.rs` would force the file to import a WAL type purely as a convenience — better to keep the table file focused and let 3.11 do the assembly.

### 3.6 Iter-key ordering is numerical, not lexicographic-bytes

Spec implies `checkpoint_id` is a monotonic u64 ("the most recent" = highest id). redb's built-in u64 `Key` impl encodes integers big-endian, so iteration order matches numerical order — same property pinned by 3.7's `range_scan_returns_in_order`. We pin it again here in a test (`latest_returns_max_id`) so a regression in either redb's key encoding or our key choice surfaces immediately.

## 4. Files touched

- `crates/brain-metadata/src/tables/checkpoint.rs` (new) — ~160 LOC including tests.
- `crates/brain-metadata/src/tables/mod.rs` — `pub mod checkpoint;` (alphabetical: between `agent` and `context`).
- `docs/phases/phase-03-metadata.md` — flip 3.9 to ✅ (no realignment this time — phase doc and spec catalog agree).

No edits to brain-core. No SD entry.

## 5. Tests (gated `#[cfg(all(test, not(miri)))]`)

1. **`insert_and_get_round_trips`** — write one `CheckpointMeta`, read back, structural equality.
2. **`all_fields_round_trip`** — non-trivial values for every field (non-zero `durable_lsn`, large `arena_capacity_at_checkpoint`, distinct `started_at` vs `completed_at`); checks rkyv didn't reorder anything silently.
3. **`update_overwrites`** — second insert at same `checkpoint_id` replaces (e.g., a checkpoint worker re-running).
4. **`missing_key_returns_none`** — vanilla.
5. **`multiple_checkpoints_coexist`** — three checkpoints with ids 1, 2, 3 each round-trip distinctly.
6. **`latest_returns_max_id`** — three rows inserted out of order (id=2, then id=10, then id=5); `latest()` returns the id=10 row. Pins the recovery-target semantics + the u64-numerical-ordering guarantee.
7. **`latest_returns_none_on_empty`** — fresh table, `latest()` is `Ok(None)`.
8. **`latest_after_update`** — insert id=5, then update id=5 with new content; `latest()` returns the updated row.
9. **`type_name_includes_v1`** — `format!("{:?}", <CheckpointMeta as Value>::type_name())` contains "v1".

## 6. Verification

Same Linux dev-container harness:

```
docker run --rm -v "$(pwd)":/workspaces/brain ... brain-dev:latest \
  bash -c "cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p brain-metadata"
```

Expected: 82 brain-metadata tests pass (73 prior + 9 new).

## 7. Commit

Branch: `feature/brain-metadata`. AUTONOMY §5 format:

```
feat(brain-metadata): checkpoints table (sub-task 3.9)
```

Body summarises: last spec'd table, `CheckpointMeta` six-u64 struct, `latest()` helper for the recovery-target query, deliberate non-implementation of `From<CheckpointReport>` (kept in 3.11), 9 new tests. **13 of 13 spec'd tables done.**

## 8. Done when

- [ ] `CHECKPOINTS_TABLE` defined; opens cleanly.
- [ ] `CheckpointMeta` round-trips with all six fields under non-trivial values.
- [ ] `latest()` returns the max-id row or `None` on empty; verified across insert-out-of-order + update-existing scenarios.
- [ ] 9 tests green; full brain-metadata suite green in the container.
- [ ] `docs/phases/phase-03-metadata.md` 3.9 flipped to ✅. Phase doc's broader "13 of 13" tally noted at the section level (post-implementation, since the phase doc edits ride with each sub-task commit).

PLAN READY.
