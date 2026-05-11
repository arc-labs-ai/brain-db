# Phase 3 — Task 3.12: Cross-crate integration test

**Classification:** moderate. No new product code — pure test work proving the storage + metadata layers compose end-to-end. Touches one new test file in `brain-metadata` plus the phase exit checklist. Last sub-task of Phase 3.

**Spec:** `spec/05_storage_arena_wal/08_recovery.md` (recovery contract); `spec/07_metadata_graph/08_transactions.md`. Cross-checked `brain_storage::recovery::recover` signature (`(&mut ArenaFile, &Path, [u8; 16], &mut dyn MetadataSink)`) and `Wal::create` / `Wal::append` to confirm the test harness can drive everything from outside the crates.

## 1. Scope

In:

- `crates/brain-metadata/tests/recovery_integration.rs` (new):
  - Test helpers: `fresh_env(dir)` returns `(ArenaFile, Wal, MetadataDb)` opening fresh files at predictable subpaths; `build_record(payload, ts, agent_id)` constructs a `WalRecord` ready for `Wal::append`.
  - **Scenario A — basic write-and-recover.** Append a small batch of `Encode` + `Link` records via `Wal::append`. Drop arena + wal + MetadataDb. Reopen all three. Call `recover(&mut arena, wal_dir, shard_uuid, &mut metadata)`. Assert the metadata tables (memories, texts, edges_out, edges_in, idempotency, model_fingerprints, slot_versions, next_lsn) reflect the records.
  - **Scenario B — checkpoint shortens replay.** Append records 1..=5, then CheckpointBegin/CheckpointEnd advancing `durable_lsn` to 5, then records 6..=8. Crash. Reopen — `MetadataDb::durable_lsn()` returns 5 from the checkpoint row. Recover skips 1..=5 and applies only 6..=8. Assert the report's `records_skipped == 5`, `records_replayed == 3`.
  - **Scenario C — transaction bracket: committed vs aborted.** Append TXN_BEGIN + Encode + Encode + TXN_COMMIT (the records inside survive) AND a second TXN_BEGIN + Encode + TXN_ABORT (the inner Encode is discarded). Crash + recover. Assert: the two committed memories are present, the aborted one is not.
  - **Scenario D — partial transaction at WAL tail.** Append a TXN_BEGIN + an Encode but **no** TXN_COMMIT (simulating a crash mid-transaction). Crash + recover. Assert: the orphan Encode is discarded; the recovery report's `records_discarded == 2`.
  - **Scenario E — recover is idempotent.** After Scenario A's recovery completes, call `recover` again on the same env. Assert: the second recover reports `records_skipped == N`, no rows duplicated.
  - **Scenario F — durable_lsn survives MetadataDb close + reopen.** After Scenario B's recovery, close `MetadataDb`, reopen it on the same path. `durable_lsn()` still returns 5 (or whatever the latest checkpoint wrote).
  - **Scenario G — 100-iteration property-style replay.** Seeded RNG generates a sequence of 5–20 records mixing Encode (with optional edges), Link, Forget, and occasional CheckpointBegin/End. Write through `Wal::append`, drop, reopen, recover. Assert two invariants per iteration: (1) `MetadataDb.durable_lsn()` after recover >= the last seen `CheckpointEnd.durable_lsn` if any (≤ the highest applied LSN); (2) recovery is idempotent — a second recover yields the same final state. Satisfies the phase exit criterion ("Recovery integration test passes 100 random-seed iterations").

Out (deferred):

- **HNSW recovery integration** — spec §05/08 §6 mentions HNSW state checks; HNSW is Phase 5. Out of scope.
- **Concurrent recovery + live writes** — recovery is a startup-time one-shot per spec §05/08; live coexistence isn't a v1 concern.
- **Chaos-test variants** (kill-during-fsync, partial-segment corruption) — spec §16/06 prescribes these but they're Phase 12+ territory; 3.12 establishes the happy-path foundation.
- **`MetadataSinkError::Corruption` injection** — wires up only via deliberate file corruption; not exercised in 3.12.

## 2. Spec quotes that bind the design

> **§05/08 §recovery contract:** "the sink consumes WAL records and updates tables idempotently. Multiple apply calls with the same `(lsn, payload)` must produce the same effect." → Scenario E pins idempotency on the full integration path.
>
> **§05/08 §durable_lsn skip:** "Recovery skips records whose `lsn <= durable_lsn()`." → Scenario B pins this with explicit replayed/skipped counts.
>
> **§05/09 §12.1:** "An incomplete checkpoint (BEGIN without END) is ignored on recovery." → Scenario D's symmetrical case for transactions (BEGIN without COMMIT discards).
>
> **§07/08 §13:** "Recovery sees the TXN_BEGIN/TXN_COMMIT brackets and applies records atomically." → Scenarios C + D pin both the commit and abort sides.

## 3. Design decisions

### 3.1 Test harness lives entirely outside the `src/` tree

`crates/brain-metadata/tests/recovery_integration.rs` is an **integration test** in Cargo's sense — it compiles as a separate binary, only sees `brain_metadata`'s **public** API. This is the right place to prove end-to-end behaviour because:

- It can't reach into `pub(crate)` fields (e.g. `MetadataDb.durable_lsn`), so it must observe only what real callers see (`durable_lsn()` accessor, `read_txn` + `open_table` for assertions).
- It validates that the public API is actually sufficient for the integration. If a test needs something not yet public, that's a signal to expose it.

### 3.2 Hand-built records via `WalRecord` public fields

`WalRecord` exposes public fields (lsn, kind, flags, timestamp_ns, agent_id_lo64, payload). The test constructs records directly via struct-literal syntax, then `payload.encode_to_bytes()` for the bytes. Avoids needing a "WalRecord builder" API just for tests.

The LSN field gets overwritten by `Wal::append` (it assigns the next monotonic LSN), so the test sets `lsn: Lsn(0)` and reads back the returned `Lsn`.

### 3.3 Crash simulation: drop the values

Rust's `Drop` model means dropping a `Wal` runs its shutdown path; dropping `ArenaFile` munmaps; dropping `MetadataDb` closes the redb file. No partial-state simulation in 3.12 — just clean drops then reopens, modelling a graceful crash where the on-disk state is whatever was durably flushed. Spec §16/06 chaos tests (kill-during-syscall) are a separate Phase 12 effort.

### 3.4 `Scenario G` uses a fixed seed strategy, not `proptest`

`proptest` would be the idiomatic crate, but for 100 deterministic iterations a simple `StdRng::seed_from_u64(i)` loop is enough and avoids adding a proptest harness for a single test. Seeds `0..100` give deterministic, reproducible iterations; a failure prints the seed for re-running.

This may evolve into a real proptest in Phase 12 with shrinking. v1 stays minimal — passes the phase exit criterion without overshooting.

### 3.5 ArenaFile capacity sized to records

The test creates a small arena (say 64 slots = 64 * 1600 = 102_400 bytes) — enough for the few memories per scenario. Slot indexes in the test memories stay under this capacity. If a scenario needs more slots, `ArenaFile::open` takes a capacity parameter.

### 3.6 Assertions read via `MetadataDb::read_txn`

The integration test treats `MetadataDb` exactly like a real caller would: open a read txn, open the table by its `TableDefinition` constant, look up a key. This validates that the public surface (txn + table constants from `brain_metadata::tables`) is usable end-to-end. No reaching into `pub(crate)`.

### 3.7 ShardId / shard_uuid handling

`recover` takes a `shard_uuid: [u8; 16]` matching the WAL's stored uuid. Test fixtures use a fixed `[0; 16]` for simplicity (or a deterministic uuidv4 seed); the WAL is created with that uuid via `Wal::create(dir, shard_uuid)`.

### 3.8 No new `Cargo.toml` deps

`tempfile` is already a dev-dependency of `brain-metadata`. brain-storage is already a path dep. `rand` (for Scenario G) — `proptest` already depends on `rand` indirectly, but proptest isn't pulled into this test. **Add `rand` as a dev-dependency** if it isn't already; check `Cargo.toml`. If `rand` is workspace-managed, use the workspace dep.

## 4. Files touched

- `crates/brain-metadata/tests/recovery_integration.rs` (new) — ~400 LOC including all 7 scenarios.
- `crates/brain-metadata/Cargo.toml` — add `rand` dev-dependency if not present.
- `docs/phases/phase-03-metadata.md` — flip 3.12 to ✅ post-implementation; flip the phase exit checklist's "Recovery integration test passes 100 random-seed iterations" to ✅.
- `docs/phases/phase-03-metadata.md` — also flip the **Phase exit checklist** items (sub-tasks complete, verify green, 13 tables present), and add the `phase-3-complete` git tag.

No SD entries.

## 5. Test execution

```
docker run --rm -v "$(pwd)":/workspaces/brain ... brain-dev:latest \
  bash -c "cargo test -p brain-metadata --test recovery_integration"
```

Expected: 7 tests pass (6 deterministic + 1 looped 100×).

Plus the full `just verify` equivalent for phase exit.

## 6. Commit

Branch: `feature/brain-metadata`. AUTONOMY §5 format:

```
test(brain-metadata): cross-crate recovery integration (sub-task 3.12)
```

Body summarises the 7 scenarios + phase-exit checklist completion + the `phase-3-complete` tag.

Optional separate commit for the tag — typically a tagging commit is just the tag itself (`git tag phase-3-complete`), no commit needed.

## 7. Done when

- [ ] `tests/recovery_integration.rs` exists with 6 deterministic scenarios + 1 100-iteration loop.
- [ ] All 7 tests green in the Linux container.
- [ ] `just verify` green workspace-wide.
- [ ] Phase exit checklist in `docs/phases/phase-03-metadata.md` fully ticked.
- [ ] Tag `phase-3-complete` applied to the commit.

PLAN READY.
