# Phase 3 — Task 3.11: `impl MetadataSink for MetadataDb`

**Classification:** large. Touches two crates. Approximately 600–800 LOC of methodical translation from the 15 `WalPayload` variants to redb table writes, plus ~20–25 tests, plus a small `brain-storage` trait extension threading the WAL record timestamp through `apply`.

**Spec:** `spec/05_storage_arena_wal/08_recovery.md` (recovery contract); `spec/07_metadata_graph/08_transactions.md` §5–§11 (multi-table atomic writes inside one redb txn); each of the 15 payloads' originating spec section (e.g. §09/02 ENCODE, §09/06 FORGET, §07/06 idempotency, §05/09 checkpoints). Cross-checked `crates/brain-storage/src/wal/payload.rs` for the exact `WalPayload` variant shapes and `crates/brain-storage/src/recovery.rs` for the existing trait + bracketing.

## 1. Scope

In:

### 1.1 brain-storage trait extension

- Modify `brain_storage::recovery::MetadataSink::apply` signature:
  ```rust
  fn apply(&mut self, lsn: u64, timestamp_ns: u64, payload: &WalPayload) -> Result<(), MetadataSinkError>;
  ```
  New `timestamp_ns: u64` parameter carries the WAL record's wall-clock timestamp. Required so `CheckpointEnd → CheckpointMeta.completed_at_unix_nanos` (and similar timestamp-needing fields) get accurate values without buffering record state inside the sink.
- Update `brain_storage::recovery::InMemoryMetadataSink::apply` to the new signature (no-op body change — it ignores the timestamp).
- Update the recovery dispatch in `brain_storage::recovery::apply` to pass `record.timestamp_ns` through.
- Update `brain_storage::wal::checkpoint`'s tests if they call `sink.apply` directly (most use `recover` which is updated transparently).

This is a breaking trait change. Logged as SD-3.11-1: trait extension beyond what spec §05/08 prescribed (spec doesn't mandate any specific trait signature; it's a brain-storage internal API). Captures the deviation in `docs/spec-deviations.md`.

### 1.2 brain-metadata real sink

- `crates/brain-metadata/src/sink.rs` (new):
  - `impl MetadataSink for MetadataDb` — `durable_lsn()` + `apply(lsn, timestamp_ns, payload)`.
  - 15 private `apply_*` helpers, one per `WalPayload` variant.
- Extend `MetadataDb` (`crates/brain-metadata/src/db.rs`):
  - Cache `durable_lsn: u64` read from the `checkpoints` table via `latest()` at `open()` time. Returned by the trait's `durable_lsn()`.
  - `pending_checkpoints: HashMap<u64, u64>` (checkpoint_id → started_at_unix_nanos) for CheckpointBegin/End pairing. In-memory only; transient across restarts (any unpaired begin is discarded on next open, which is correct — spec §05/09 §12.1: incomplete checkpoint is ignored).
- `crates/brain-metadata/src/lib.rs` — `pub mod sink;`.
- `docs/spec-deviations.md` — log SD-3.11-1 (trait signature extension) and SD-3.11-2 (Reclaim's O(N) scan; see §3.6).

Out (deferred):

- **Cross-table edge-count maintenance** (spec §07/04 §5–§6: LINK/UNLINK updates `memories.edges_out_count`). Phase 8 maintenance worker reconciles; storage-layer atomicity not load-bearing for v1 — the count is observational.
- **Idempotency `request_hash` population** during Encode/Forget/etc. The hash needs Phase 9's canonicalised-request bytes; recovery doesn't have them. Recovery writes `request_hash: [0; 32]` placeholder. The hash is only used by the conflict-detection path (spec §07/06 §5), which is wire-layer; recovery just records that the RequestId was seen.
- **Model fingerprint `model_name` population** during Encode. The payload carries `embedding_model_fp` but not the human-readable name (it's set by ADMIN_REGISTER_MODEL or the embedding loader). Recovery writes `model_name: ""` placeholder.
- **TxnBegin / TxnCommit / TxnAbort handling** — recovery (`brain_storage::recovery::recover`) already buffers and applies bracketed records atomically (verified in `recovery.rs` lines 222–248). The sink sees only committed records in LSN order; these three variants are no-ops in `apply` (just bump next_lsn).
- **Hard-forget secure-erase** (zeroing text/vector). The Reclaim record cleans up the row; the actual zero happens at the arena level (`brain_storage::recovery::apply_to_arena` writes zeros to the slot when applying Reclaim) and would need `FALLOC_FL_PUNCH_HOLE` on redb's underlying file for true secure erasure (out of scope; Phase 8 worker).

## 2. Spec quotes that bind the design

> **§05/08 §recovery contract:** the sink consumes WAL records and updates tables idempotently. Multiple apply calls with the same `(lsn, payload)` must produce the same effect.
>
> **§07/08 §11 (multi-table atomicity):** "A single write transaction can update multiple tables atomically." → one redb txn per apply call, touching all the tables that one WAL record affects.
>
> **§09/02 ENCODE writes:** memory metadata + text + idempotency + edges + model fingerprint + slot version are all written in one logical operation.
>
> **§07/06 §3 (idempotency):** every state-mutating request records its RequestId. Recovery preserves this (subsequent retries of the same RequestId would have hit the idempotency check at the wire layer; replay maintains the record).
>
> **§02/03 §2.3 (MemoryId stability):** "A `MemoryId` that previously identified memory M never identifies a different memory." → on Reclaim, the slot_version increments. Recovery uses the WAL-recorded `new_version` directly via `insert`, not the `increment()` helper (which would advance beyond what the WAL recorded).
>
> **§05/09 §12.1:** an incomplete checkpoint (BEGIN without END) is ignored on recovery. → `pending_checkpoints` is in-memory; an unpaired begin discards cleanly on restart.

## 3. Design decisions

### 3.1 One redb write transaction per `apply` call

Each call to `apply(lsn, ts, payload)` does:
```rust
let wtxn = self.write_txn()?;
{
    // open whichever tables this variant needs
    // do the inserts/deletes
    // update next_lsn[()] = max(current, lsn + 1)
}
wtxn.commit()?;
```

Pros: simplest correctness story. After return, the sink's state is durable; a crash during recovery loses no progress beyond the last completed apply.

Cons: every record pays one redb commit (~0.5 ms on NVMe per spec §07/08 §9). 1M records → ~8 min recovery. **Acceptable for v1**; Phase 8 can introduce batching with explicit `flush()` if needed.

### 3.2 `next_lsn` updated on every apply

End of every apply: `next_lsn[()] = max(current, lsn + 1)`. After recovery completes, `next_lsn` reflects the highest applied LSN + 1, ready for live operation to resume.

### 3.3 `slot_versions` uses `insert` (overwrite) during recovery, not `increment`

3.7's `increment()` is for *live* slot reclamation. During recovery, the WAL already records the exact new version (`EncodePayload.memory_id.version()`, `ReclaimPayload.new_version`); we must replay it verbatim, not increment past it. Recovery path uses raw `table.insert(&slot_id, &new_version)`.

### 3.4 Sink state on `MetadataDb` (not a separate adapter type)

Phase doc literally says "impl MetadataSink for MetadataDb". To support this, `MetadataDb` gains:
- `durable_lsn: u64` — cached at open by `latest()?.map(|c| c.durable_lsn).unwrap_or(0)`.
- `pending_checkpoints: HashMap<u64, u64>` — checkpoint_id → started_at_unix_nanos.

Both fields are `&mut self`-only mutable (consistent with single-writer-per-shard via `&mut` on writes).

A separate `MetadataDbSink { inner: MetadataDb, state: SinkState }` wrapper would keep `MetadataDb` pure-table-access, but the phase doc and the simpler API both favour direct `impl`. Going with direct.

### 3.5 `MetadataSinkError` mapping

The `MetadataSinkError` enum (brain-storage) has `Transient(String)` and `Corruption(String)`. Our impl:
- `redb::StorageError`, `redb::TransactionError`, `redb::CommitError`, `redb::TableError` → `Transient`. These represent transient I/O failures; recovery retries.
- rkyv deserialise failures during a get (shouldn't happen during apply, but in defensive code paths) → `Corruption`.

Use `Display`-string formatting through `format!`.

### 3.6 Reclaim's memory-row cleanup is O(N) — log as SD-3.11-2

`ReclaimPayload` carries `slot_id`, `old_version`, `new_version` but NOT `memory_id`. To delete the memory row, we'd scan `memories` looking for one with `slot_id == X && slot_version == old_version`. O(N) per reclaim where N = number of memories.

Spec §07/02 §13 (`slot_versions` table) doesn't address how to find the memory row by slot. The wire/worker layer that emits Reclaim has the MemoryId in hand; carrying it forward in the payload would be the natural fix.

**v1 decision:** scan-based cleanup. Reclaims are rare during recovery (one per reclaimed slot, only after grace expiry); 1M memories × 1000 reclaims = 10^9 reads — acceptable for the recovery one-shot, painful for live ops (where Reclaim shouldn't be applied through this path anyway).

**SD-3.11-2 logged.** Reconciliation: extend `ReclaimPayload` with `memory_id: MemoryId` in a future Phase 2 amendment, then O(1) row-key delete.

### 3.7 Encode's `model_fingerprints` insert-if-absent

Spec §04/07 §5 says the substrate records each fingerprint when first seen. During recovery, on every Encode we check `table.get(&fp)?.is_none()` and insert only if absent. Subsequent Encodes with the same fp don't touch the row. `memory_count_at_fingerprint: 0` — Phase 8 worker reconciles by scanning memories. `model_name: ""` (not in payload).

### 3.8 Idempotency entries on every state-mutating WAL record

Per spec §07/06 §17 the idempotency-required ops are ENCODE/FORGET/LINK/UNLINK/UPDATE_KIND/UPDATE_CONTEXT/TXN_BEGIN/TXN_COMMIT. The sink writes an idempotency entry for each of these variants. The entry has:
- `response_kind` per 3.5's `response_kind` constants.
- `memory_id_bytes` where applicable (Some for ENCODE; None for LINK/UNLINK since they don't return a new memory).
- `response_payload: vec![]` (empty — recovery doesn't have the encoded response bytes; the conflict-detection path uses `request_hash` only).
- `request_hash: [0; 32]` (placeholder per §1 out-of-scope item).
- `created_at_unix_nanos: timestamp_ns`.

Variants that don't carry a `RequestId` (UpdateSalience, Reclaim, Consolidate, MigrateEmbedding, CheckpointBegin/End, TxnBegin/Commit/Abort) skip the idempotency table.

### 3.9 Per-variant write surface — summary table

| Variant | Tables touched on apply |
|---|---|
| Encode | memories, texts, idempotency, model_fingerprints, edges_out, edges_in, slot_versions, next_lsn |
| Forget | memories (set flags + forgot_at), idempotency, next_lsn |
| Link | edges_out, edges_in, idempotency, next_lsn |
| Unlink | edges_out, edges_in, idempotency, next_lsn |
| UpdateSalience | memories (set salience for each in batch), next_lsn |
| Reclaim | memories (scan+delete), texts (delete), edges_out/in (delete by source/target), slot_versions (set new_version), next_lsn |
| Consolidate | memories (insert), texts, slot_versions, next_lsn |
| UpdateKind | memories (set kind), idempotency, next_lsn |
| UpdateContext | memories (set context_id), idempotency, next_lsn |
| MigrateEmbedding | memories (set embedding_model_fp), next_lsn |
| CheckpointBegin | in-memory state only (pending_checkpoints[id] = started_at), next_lsn |
| CheckpointEnd | checkpoints (insert CheckpointMeta), next_lsn; update self.durable_lsn |
| TxnBegin/Commit/Abort | next_lsn only (recovery handles bracketing) |

### 3.10 Edge-table writes use the `link()`/`unlink()` helpers from 3.4

Encode and Link both go through `crate::tables::edge::link(&mut out, &mut in_, source, kind, target, &data)` which handles the symmetric-edge mirroring. Unlink mirrors via `unlink()`. Reclaim's edge cleanup (delete all edges where source or target == this memory) does manual range-scan since the helpers don't support "delete by source/target prefix" — TODO comment, follow-up for Phase 8.

## 4. Files touched

### brain-storage (trait extension)

- `crates/brain-storage/src/recovery.rs`:
  - Update `MetadataSink::apply` signature to include `timestamp_ns: u64`.
  - Update `InMemoryMetadataSink::apply` to match (ignore the new param).
  - Update the dispatch call in `apply()` (line 285): `sink.apply(record.lsn.raw(), record.timestamp_ns, payload)`.
- `crates/brain-storage/src/wal/checkpoint.rs`: tests that call `sink.apply` directly (if any) updated.

### brain-metadata (new sink)

- `crates/brain-metadata/src/sink.rs` (new) — ~500–600 LOC including tests.
- `crates/brain-metadata/src/db.rs` — add `durable_lsn: u64` and `pending_checkpoints` fields; load `durable_lsn` in `open()`.
- `crates/brain-metadata/src/lib.rs` — `pub mod sink;`.
- `docs/spec-deviations.md` — append SD-3.11-1 (trait extension) and SD-3.11-2 (Reclaim O(N) scan).
- `docs/phases/phase-03-metadata.md` — flip 3.11 to ✅, post-implementation.

## 5. Tests (gated `#[cfg(all(test, not(miri)))]`)

In `sink.rs`:

1. **`durable_lsn_fresh_is_zero`** — fresh `MetadataDb`; `durable_lsn() == 0`.
2. **`durable_lsn_persists_across_reopens`** — apply a CheckpointEnd at `durable_lsn = 100`, reopen the DB, `durable_lsn()` returns 100.
3. **`encode_writes_all_expected_tables`** — apply Encode; verify rows in `memories`, `texts`, `idempotency`, `model_fingerprints`, `edges_out`, `edges_in`, `slot_versions`, `next_lsn`.
4. **`encode_is_idempotent`** — apply same Encode twice (same lsn, same payload); state matches single-apply state.
5. **`forget_marks_memory_tombstoned`** — apply Encode then Forget; `memories[id].flags & HARD_FORGOTTEN != 0`, `forgot_at_unix_nanos == Some(ts)`.
6. **`link_writes_both_edge_tables`** — apply Link; rows in both `edges_out` and `edges_in`.
7. **`unlink_removes_both_edges`** — apply Link then Unlink; rows gone.
8. **`update_salience_changes_memory_salience`** — apply UpdateSalience batch of 3; each memory's salience updated.
9. **`reclaim_advances_slot_version_and_deletes_memory`** — Encode → Forget → Reclaim; `slot_versions[slot] == new_version`, memory row gone, text gone.
10. **`consolidate_inserts_new_memory`** — apply Consolidate; new memory row with kind=Consolidated.
11. **`update_kind_changes_memory_kind`** — apply UpdateKind; memory's kind field changed.
12. **`update_context_changes_memory_context`** — apply UpdateContext; memory's context_id changed.
13. **`migrate_embedding_changes_memory_fingerprint`** — apply MigrateEmbedding; memory's embedding_model_fp changed.
14. **`checkpoint_begin_tracks_pending`** — apply CheckpointBegin; verify the BEGIN didn't write any persistent table (we can't easily peek `pending_checkpoints`; instead verify CheckpointEnd uses the right started_at).
15. **`checkpoint_end_writes_meta_row`** — apply CheckpointBegin then CheckpointEnd; `checkpoints[id]` row exists with `started_at_unix_nanos` from BEGIN and `completed_at_unix_nanos` from END's timestamp_ns; `durable_lsn()` advances.
16. **`checkpoint_end_without_begin_uses_zero_started_at`** — apply CheckpointEnd directly (no BEGIN seen, e.g. across restart); row has `started_at = 0`, `completed_at = ts`, `durable_lsn` still advances.
17. **`txn_records_are_noops`** — apply TxnBegin/TxnCommit/TxnAbort; no rows added except next_lsn bump.
18. **`next_lsn_tracks_max_seen`** — apply 5 records with non-monotonic LSNs (e.g. 3, 5, 4, 7, 6); `next_lsn[()] == 8`.
19. **`apply_propagates_storage_errors`** — close the DB / corrupt it, attempt apply, expect `MetadataSinkError::Transient`.
20. **`encode_with_multiple_edges_writes_all`** — Encode with 3 edges in the payload; all 3 appear in `edges_out`/`edges_in`.

In `brain-storage/src/recovery.rs`:

21. **Updated `InMemoryMetadataSink` tests** — change call signatures to include `timestamp_ns`; ensure existing recovery tests pass.

## 6. Verification

```
docker run --rm -v "$(pwd)":/workspaces/brain ... brain-dev:latest \
  bash -c "cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p brain-storage && cargo test -p brain-metadata"
```

Expected:
- brain-storage tests still pass after the trait change (47 prior tests).
- brain-metadata: 91 + ~20 = ~111 tests.

## 7. Commit

Branch: `feature/brain-metadata`. AUTONOMY §5 format:

```
feat(brain-metadata): MetadataSink impl over MetadataDb (sub-task 3.11)
```

Body summarises: trait extension (timestamp_ns parameter, SD-3.11-1), real sink impl across 15 WAL variants, per-variant write surface, deliberate non-implementations (cross-table count maintenance, request_hash population, model_name population), 20+ new tests, Reclaim O(N) scan SD-3.11-2.

## 8. Done when

- [ ] `MetadataSink::apply` signature extended to include `timestamp_ns: u64`; all callers (recovery + `InMemoryMetadataSink`) updated; brain-storage tests still green.
- [ ] `impl MetadataSink for MetadataDb` covers all 15 `WalPayload` variants.
- [ ] `durable_lsn()` survives `MetadataDb::open` → close → reopen.
- [ ] 20+ tests cover every variant + idempotency + cross-reopen persistence.
- [ ] `just verify` green in container.
- [ ] `docs/spec-deviations.md` records SD-3.11-1 and SD-3.11-2.
- [ ] `docs/phases/phase-03-metadata.md` 3.11 flipped to ✅.

PLAN READY.
