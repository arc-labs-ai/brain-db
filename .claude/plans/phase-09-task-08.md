# Sub-task 9.8 — Wire Phase-8 seams to real impls

**Reads:** `spec/05_storage_arena_wal/09_checkpointing.md` §1-§3, `spec/05_storage_arena_wal/10_wal_retention.md` (if present), `spec/06_ann_index/06_rebuild.md`, audit `docs/phases/phase-09-glommio-port.md` §4 + §8.5.
**Phase doc:** `docs/phases/phase-09-server.md` §9.8.
**Done when:** Three of the four Phase-8 seams have real, shard-backed impls; the fourth (CacheEvictionSource) stays `Disabled*` with a documented rationale.

---

## 1. Background

Sub-task 9.7b registered all 12 Phase-8 workers against the per-shard `WorkerScheduler` with `Disabled*` source defaults (`DisabledRebuildSource`, `DisabledWalRetentionSource`, `DisabledSnapshotSource`, `DisabledCacheEvictionSource`). The workers compile + run but their `Disabled*` adapters short-circuit immediately with `*Source::Disabled`. 9.8 plugs in real adapters that interact with the shard's actual state.

### 1.1 The Send + Sync bound cascade — again

All four source traits today declare:
```rust
pub trait WalRetentionSource: Send + Sync + 'static { ... }
pub trait RebuildSource<const D: usize>: Send + Sync + 'static { ... }
pub trait SnapshotSource: Send + Sync + 'static { ... }
pub trait CacheEvictionSource: Send + Sync + 'static { ... }
```

A real adapter holding `Rc<Wal>` (Wal is `!Send`) or `Arc<dyn WriterHandle>` (WriterHandle is `!Send + !Sync` post-9.7a) breaks the bound. The cascade we hit in 9.7a applies again here. Resolution: **drop `Send + Sync` from all four trait declarations** (audit §4 / §8.5 logic extends naturally to these per-shard sources). The Disabled* impls and the future-return types' `+ Send` bounds also drop.

This is the **first** thing 9.8 does, before any adapter work.

### 1.2 Scope: 3 of 4 adapters real, 1 stays Disabled

| Seam | Adapter strategy | Why now / why later |
| ---- | --------------- | --------- |
| **RebuildSource** | Real: scan the per-shard arena's occupied slots, yield `(MemoryId, vector)` pairs | Cheap (mmap walk); HNSW rebuild is the single biggest correctness improvement Phase 8 left on the table |
| **WalRetentionSource** | Real: `WalReader::open` for `list_segments`, `std::fs::remove_file` for `delete_segment`, `MetadataDb::durable_lsn()` for `current_checkpoint` | Spec §05/10 retention is operationally important; gates WAL disk growth |
| **SnapshotSource** | Real: orchestrate `write_checkpoint` → `arena.msync_all` → HNSW snapshot save (if present) → metadata copy via `std::fs::copy`; manage snapshot directory layout | Spec §05/09 is the durability path; 9.8 makes restart-from-snapshot real |
| **CacheEvictionSource** | Stays `DisabledCacheEvictionSource` | No `CachingDispatcher` is constructed in the shard yet (9.7b uses `NopDispatcher`). 9.10's frame dispatcher introduces a real Dispatcher; cache wires alongside |

---

## 2. The trait Send-drops

`crates/brain-workers/src/{hnsw_maint,wal_retention,snapshot,cache_evict}.rs`:

1. Trait declaration: drop `Send + Sync + 'static` → keep `'static` only (still required because `Arc<dyn Trait>` lives in worker state across `.await`s; we'd drop `'static` too only if every adapter is borrowed not owned, which isn't our shape).

   Actually after 9.7a's WorkerScheduler is per-shard / Glommio: `Arc<dyn Trait>` no longer requires `Send + Sync` for the dyn. The `'static` bound is still needed for the worker's stored field to outlive cycles.

2. Future-return aliases (`SnapshotFuture<'_, D>`, `CheckpointFuture<'_>`, etc.) drop their `+ Send` bound.

3. Disabled* impls drop their inner `+ Send` future bounds.

4. Any `fn require<T: Send + Sync>()` assertions that test the source types: drop them.

This is mechanical — same shape as 9.7a's WriterHandle cascade, smaller scope.

---

## 3. The real adapters (brain-server)

New module: `crates/brain-server/src/shard_adapters.rs`. Three adapters, all `!Send + !Sync` by construction.

### 3.1 `ArenaRebuildSource<D>`

Holds `Rc<RefCell<ArenaFile>>` (read-only borrow during the scan).

```rust
pub(crate) struct ArenaRebuildSource<const D: usize> {
    arena: Rc<RefCell<ArenaFile>>,
}

impl<const D: usize> RebuildSource<D> for ArenaRebuildSource<D> {
    fn snapshot_vectors(&self) -> SnapshotFuture<'_, D> {
        let arena = self.arena.clone();
        Box::pin(async move {
            let arena = arena.borrow();
            let mut out = Vec::new();
            for idx in 0..arena.capacity_slots() {
                let s = arena.slot(idx);
                if s.is_occupied() && !s.is_tombstoned() {
                    let mid = MemoryId::pack(/*shard*/ 0, idx, s.metadata.slot_version);
                    out.push((mid, s.vector));
                }
            }
            Ok(out)
        })
    }
}
```

Issue: ArenaFile is held mutably by the main loop (the allocator's `alloc` takes `&mut ArenaFile`). Sharing the arena with the rebuild source requires either:
- (a) Wrapping in `Rc<RefCell<ArenaFile>>` — main loop borrows mutably between awaits, rebuild source borrows immutably during its scan. Single-threaded executor; borrows can interleave cleanly across awaits.
- (b) Snapshotting vector data into a separate buffer that the rebuild source owns.

**Recommendation: (a).** Wrap `ArenaFile` in `Rc<RefCell<...>>` in `Shard`; main loop uses `borrow_mut()` briefly per op; rebuild source uses `borrow()` during its scan. Borrows must be released before any `.await` (same discipline as `Wal`'s `RefCell<WalInner>`).

The shard_id is needed to construct MemoryIds correctly — pass it via the adapter struct.

### 3.2 `WalDirRetentionSource`

Holds the WAL directory path + shard UUID + a way to learn the current `durable_lsn`.

```rust
pub(crate) struct WalDirRetentionSource {
    wal_dir: PathBuf,
    shard_uuid: [u8; 16],
    metadata: SharedMetadataDb,   // for durable_lsn lookup
}

impl WalRetentionSource for WalDirRetentionSource {
    fn current_checkpoint(&self) -> CheckpointFuture<'_> {
        Box::pin(async move {
            let lsn = self.metadata.lock().durable_lsn();
            Ok(CheckpointDesc { durable_lsn: lsn, ... })
        })
    }

    fn list_segments(&self) -> SegmentListFuture<'_> {
        Box::pin(async move {
            let reader = WalReader::open(&self.wal_dir, self.shard_uuid)?;
            let segs = reader.segments().iter()
                .map(|s| SegmentDesc { segment_id: s.segment_seq, ... })
                .collect();
            Ok(segs)
        })
    }

    fn delete_segment(&self, segment_id: u64) -> DeleteFuture<'_> {
        Box::pin(async move {
            let path = self.wal_dir.join(format!("{:010}.wal", segment_id));
            std::fs::remove_file(&path)?;
            Ok(())
        })
    }
}
```

Caveat: deleting a WAL segment file is fine if no `Wal` handle is currently reading from it. Per spec §05/10, segments are deleted only after they're fully past `durable_lsn`. The active segment is never on the deletion list (its `segment_seq == active_segment_seq` would be excluded by `decide_deletions`).

### 3.3 `ShardSnapshotSource`

Holds references to all shard state needed to take a snapshot. Most complex of the three.

```rust
pub(crate) struct ShardSnapshotSource {
    shard_id: ShardId,
    shard_uuid: [u8; 16],
    data_dir: PathBuf,         // <data_dir>/<shard_id>/snapshots/
    arena: Rc<RefCell<ArenaFile>>,
    wal: Rc<RefCell<Option<Wal>>>,    // Option so we can drain on shutdown
    metadata: SharedMetadataDb,
}

impl SnapshotSource for ShardSnapshotSource {
    fn take_snapshot(&self) -> TakeFuture<'_> {
        Box::pin(async move {
            // 1. wal.write_checkpoint(plan).await — async
            // 2. arena.msync_all() — sync
            // 3. std::fs::copy(arena.bin → snapshots/<id>/arena.bin) — sync
            // 4. metadata.copy(snapshots/<id>/metadata.redb) — sync if redb supports
            // 5. (HNSW snapshot save — deferred, no save_snapshot in 9.7b yet)
            // 6. Write a snapshot manifest
            // 7. Return SnapshotDesc { id, ... }
        })
    }

    fn list_snapshots(&self) -> ListFuture<'_> { ... }
    fn delete_snapshot(&self, id: SnapshotId) -> DeleteFuture<'_> { ... }
}
```

Snapshot directory layout:
```
<data_dir>/<shard_id>/snapshots/
  <snapshot_id>/
    arena.bin       (copy)
    metadata.redb   (copy via std::fs::copy or redb's backup API)
    manifest.toml   ({shard_uuid, durable_lsn, taken_at, ...})
```

Open question: HNSW snapshot save — Phase 6 may have shipped `HnswIndex::save_snapshot(path)` already; check during impl. If not, defer that part of the snapshot to a follow-up.

### 3.4 CacheEvictionSource — stays Disabled

Documented in the adapter module. Real adapter lands when 9.10's frame dispatcher constructs a `CachingDispatcher` per shard. Until then `DisabledCacheEvictionSource` keeps the worker harmless.

---

## 4. Shard struct refactor

To share the arena + wal with the adapters, `Shard` switches from owning these directly to wrapping them in `Rc<RefCell<...>>`:

```rust
struct Shard {
    shard_id: ShardId,
    arena: Rc<RefCell<ArenaFile>>,        // NEW: was ArenaFile
    allocator: SlotAllocator,             // unchanged (per-shard, owned)
    wal: Rc<RefCell<Option<Wal>>>,        // NEW: was Option<Wal>; Rc-cell so adapters can share
    ops: Arc<OpsContext>,                 // unchanged
    scheduler: Option<WorkerScheduler>,   // unchanged
}
```

Main loop's `AllocSlot` / `AppendWalRecord` handlers borrow / borrow_mut briefly per op; borrows drop before each `.await`.

**Critical invariant:** never hold a `borrow_mut()` across `.await`. Same discipline as `Wal`'s internal `RefCell<WalInner>` from 9.6a.

---

## 5. Spawn flow delta

After 9.7b's full-stack construction:

```rust
// inside Glommio closure (after OpsContext is built):

let arena_cell = Rc::new(RefCell::new(arena));
let wal_cell = Rc::new(RefCell::new(Some(wal)));

let rebuild_source: Arc<dyn RebuildSource<{ VECTOR_DIM }>> =
    Arc::new(ArenaRebuildSource::new(shard_id, arena_cell.clone()));

let wal_retention_source: Arc<dyn WalRetentionSource> =
    Arc::new(WalDirRetentionSource::new(
        wal_dir_for_executor.clone(),
        shard_uuid,
        metadata.clone(),
    ));

let snapshot_source: Arc<dyn SnapshotSource> =
    Arc::new(ShardSnapshotSource::new(
        shard_id,
        shard_uuid,
        dir.join("snapshots"),
        arena_cell.clone(),
        wal_cell.clone(),
        metadata.clone(),
    ));

let cache_eviction_source: Arc<dyn CacheEvictionSource> =
    Arc::new(DisabledCacheEvictionSource);

let mut scheduler = WorkerScheduler::new();
register_phase8_workers(
    &mut scheduler,
    ops.clone(),
    rebuild_source,
    wal_retention_source,
    snapshot_source,
    cache_eviction_source,
)?;
```

`register_phase8_workers` gains four `Arc<dyn ...>` parameters (one per seam) instead of constructing the Disabled* defaults inline.

But wait — `Arc<dyn !Send>` is now Send only iff the dyn is Sync, which it isn't. So these are `Arc<dyn ...>` where the dyn is `!Send + !Sync`. The Arc itself is `!Send` then. Same lint trigger as 9.7b's `Arc<OpsContext>` — covered by the file-level `#[allow(clippy::arc_with_non_send_sync)]`.

---

## 6. Tests

### 6.1 Trait Send-drop sanity

Compile-time: a `Rc`-containing adapter satisfies the trait. Document via inline tests in each `*_adapters` module.

### 6.2 WAL retention smoke

`tests/shard.rs`:
- spawn shard with WAL_SEGMENT_SIZE_BYTES bumped down (tiny segments to force rollover)
- append records until segment 0 + 1 + 2 exist
- manually advance `metadata.durable_lsn` past segment 1's end
- ping (keeps shard alive while the worker runs)
- sleep > worker interval (or use a fast-cadence test config)
- verify segment 0 + 1 are deleted, segment 2 remains

Hmm: forcing this test deterministically requires either fast worker intervals OR a way to trigger the worker manually. Both are surface-area additions. **Simpler 9.8 test:** unit-test `WalDirRetentionSource::list_segments` + `delete_segment` directly without going through the worker.

### 6.3 Rebuild smoke

Similar: unit-test `ArenaRebuildSource::snapshot_vectors` against a hand-populated arena. Verify it returns the right `(memory_id, vector)` pairs.

### 6.4 Snapshot smoke

Unit-test `ShardSnapshotSource::take_snapshot` → `list_snapshots` → `delete_snapshot`. Verify directory layout, manifest contents.

---

## 7. Sizing

| File | Action | LOC |
| ---- | ------ | --- |
| `crates/brain-workers/src/{hnsw_maint,wal_retention,snapshot,cache_evict}.rs` | Edit (drop Send + Sync from 4 traits + future aliases) | ~50 (mechanical) |
| `crates/brain-server/src/shard_adapters.rs` | NEW (3 adapters + unit tests) | ~500 |
| `crates/brain-server/src/shard.rs` | Edit (Arc/Rc<RefCell<...>> + register_phase8_workers signature) | ~100 |
| `crates/brain-server/tests/shard.rs` | Edit (1-2 new integration tests) | ~80 |

Total: ~700 LOC. Single commit.

---

## 8. Risks

| Risk | Mitigation |
| ---- | ---------- |
| `Rc<RefCell<ArenaFile>>` for the arena means the main loop's `alloc()` takes `borrow_mut()` — if any future holds a `borrow()` across that point, panic at runtime | Audit borrow lifetimes in main loop. The discipline is identical to `Wal`'s `RefCell<WalInner>`; documented inline. |
| The 4 source trait Send-drops cascade through Phase-8's *existing* tests | Tests use `DisabledX` (zero-sized). Drop the `+ Send` bounds on test fixture impls too — mechanical. |
| Snapshot's HNSW save isn't implemented | Mark the HNSW step `// TODO(9.12)` and proceed with arena + metadata copy. Recovery doesn't strictly need the HNSW snapshot (can rebuild from arena via RebuildSource) — see spec §05/09 §4. |
| redb's snapshot/backup API isn't in the version we use | Fall back to `std::fs::copy` while holding a redb read-txn (which acquires a read lock; copy bytes; release). Less than ideal but works for v1. |
| Workers actually fire and break things | The default Phase-8 worker intervals are minutes-to-hours; the smoke test wouldn't reach them anyway. No production risk. |
| HnswMaintenanceWorker actually calls rebuild_source → tries to rebuild HNSW → may not have a `swap` API exposed | Check `SharedHnsw::swap` from 9.7b imports. If not yet wired, the worker's rebuild path would error — accept and `// TODO(9.12)` it. |

---

## 9. Done criteria

- [ ] `RebuildSource`, `WalRetentionSource`, `SnapshotSource`, `CacheEvictionSource` traits drop `Send + Sync`.
- [ ] `+ Send` dropped from each future-return alias.
- [ ] `Disabled*` impls compile + their futures match the new bounds.
- [ ] `crates/brain-server/src/shard_adapters.rs` ships three real adapters.
- [ ] `Shard.arena` and `Shard.wal` switch to `Rc<RefCell<...>>`.
- [ ] `register_phase8_workers` takes 4 `Arc<dyn …>` source parameters.
- [ ] Unit tests for each adapter pass.
- [ ] `just docker-verify` green workspace-wide.
- [ ] Audit doc §12 status row for §8.5 (already partially done by 9.7a) extends to cover the 4 worker-source traits.
- [ ] Phase doc 9.8 marked `[x]`.

---

## 10. What 9.8 explicitly defers

- **HNSW snapshot save/restore** — Phase-6 surface may not yet exist; `// TODO(9.12)`.
- **Real `CacheEvictionSource`** — waits for 9.10's `CachingDispatcher`.
- **Worker-driven snapshot lifecycle in production** — the worker tests merely verify the adapters' contracts; real restart-from-snapshot is 9.17's E2E smoke.
- **Snapshot manifest schema versioning** — first cut; v2 adds version bumps.

---

*Implement on approval.*
