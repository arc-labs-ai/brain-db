# Sub-task 9.6 — Real WAL hookup

**Reads:** `spec/05_storage_arena_wal/06_wal_durability.md` §1, §11; `spec/05_storage_arena_wal/08_recovery.md` §§1-7; `spec/12_sharding_clustering/01_shard_model.md` §1-§5.
**Phase doc:** `docs/phases/phase-09-server.md` §9.6 (next after 9.6a's WAL io_uring port).
**Done when:** Each shard owns a real `Wal` on disk under `<data_dir>/<shard_id>/wal/`; recovers on respawn (via `brain_storage::recovery::recover`); `AppendWalRecord` stub op exercises `Wal::append` end-to-end on the Glommio executor.

---

## 1. Scope

After 9.5 each shard owns an arena. After 9.6a the WAL is io_uring-driven and async. 9.6 plugs the WAL into the shard alongside the arena: a fresh shard opens an empty WAL; a respawned shard recovers from its existing WAL before accepting new writes.

**In scope:**
- Add `Wal` to the `Shard` struct (alongside `ArenaFile` + `SlotAllocator`).
- On `spawn_shard`, **inside the executor**:
  1. Open arena (existing).
  2. Run `recover(&mut arena, wal_dir, shard_uuid, &mut sink)` with an `InMemoryMetadataSink` (transient — 9.7 swaps in the redb-backed sink).
  3. Open a `Wal` positioned at `next_lsn = report.next_lsn`.
- New `ShardRequest::AppendWalRecord { record, reply_tx }` that submits via `wal.append(record).await`.
- `ShardSpawnConfig` gains `wal: WalConfig` (defaults from `brain_storage::wal::WalConfig::default()`).
- Tests: fresh-shard WAL layout; restart-then-append continues from the right LSN; append returns durable LSN; drop+reopen via `WalReader` sees the records.

**Out of scope (later sub-tasks):**
- Real metadata sink (`MetadataDb`-backed) — 9.7.
- `OpsContext` integration (the writer trait, the publisher trait) — 9.7.
- Workers replay against the WAL — 9.7 plumbs the scheduler.
- HNSW rebuild from arena scan on recovery — 9.7 (the index lives there).
- Checkpoint worker — 9.8 (Phase 8 seam).
- Multi-segment rollover stress under real workload — exercised opportunistically by tests but not a 9.6 deliverable.

---

## 2. The shard directory after 9.6

```
<data_dir>/<shard_id>/
├── arena.bin            (9.5)
├── shard.uuid           (9.5)
└── wal/                 (NEW in 9.6)
    └── 0000000000.wal   (segment 0; new on fresh start, present on restart)
```

`Wal::create_with_config` (after 9.6a) builds segment-0 inside the directory. For restart, we need a way to **open an existing WAL**. Two paths:

(a) Pre-9.6 `Wal::create` rejects non-empty dirs with `DirectoryNotEmpty`. We extend `Wal` with a new entry point.
(b) Skip the open path entirely; for restart, the shard truncates the WAL dir and starts fresh.

**Choice: (a).** Add `Wal::open_existing(dir, shard_uuid, starting_lsn, config)` to `brain-storage` that:
- Enumerates existing segments via `WalReader::open`.
- Picks the highest-`segment_seq` segment as the active one (per spec §05/08 §4: "the segment containing `durable_lsn + 1`").
- Re-opens it for append at the end of its existing records (uses the `next_lsn` derived from recovery).
- Spins up a fresh `GroupCommitter` over that active segment.

Recovery (`brain_storage::recovery::recover`) computes `next_lsn`; we pass it into `open_existing`. Single source of truth for the LSN counter.

The (a) path requires a small addition to `brain-storage`. Per the plan-first workflow that's a code change in another crate; sub-task 9.6 owns it.

---

## 3. Shard struct + main loop

```rust
struct Shard {
    shard_id: ShardId,
    arena: ArenaFile,
    allocator: SlotAllocator,
    wal: Wal,              // NEW
}

pub(crate) enum ShardRequest {
    Ping { reply_tx: Sender<()> },
    AllocSlot { reply_tx: Sender<Result<(u64, SlotVersion), ShardOpError>> },
    /// Append an opaque WAL record. The reply carries the LSN once durable.
    /// Used by 9.6's integration tests; 9.7 wraps this inside RealWriterHandle.
    AppendWalRecord {
        record: WalRecord,
        reply_tx: Sender<Result<u64, ShardOpError>>,
    },
}

async fn shard_main_loop(mut shard: Shard, rx: Receiver<ShardRequest>) {
    info!(shard_id = shard.shard_id, "shard executor entering main loop");
    while let Ok(req) = rx.recv_async().await {
        match req {
            ShardRequest::Ping { reply_tx } => { /* unchanged */ }
            ShardRequest::AllocSlot { reply_tx } => { /* unchanged */ }
            ShardRequest::AppendWalRecord { record, reply_tx } => {
                let out = match shard.wal.append(record).await {
                    Ok(lsn) => Ok(lsn.raw()),
                    Err(e) => Err(ShardOpError::Wal(e)),
                };
                let _ = reply_tx.send_async(out).await;
            }
        }
    }
    // Clean shutdown: drain WAL committer + flush arena.
    if let Err(e) = shard.wal.shutdown_in_place().await {
        warn!(shard_id = shard.shard_id, error = %e, "wal shutdown failed");
    }
    if let Err(e) = shard.arena.msync_all() {
        warn!(shard_id = shard.shard_id, error = %e, "msync_all at shutdown failed");
    }
}
```

`Wal::shutdown` currently consumes `self`. Inside the main loop we have `&mut shard.wal` — can't move out. Two options:
- (i) Wrap the Wal in `Option<Wal>` inside the Shard. On shutdown, `.take()` and call `shutdown(self)`.
- (ii) Add `Wal::shutdown_in_place(&mut self)` that drains the committer without consuming.

(i) is simpler; the field type becomes `Option<Wal>`. Asserts on every access. Acceptable for the scaffold.

---

## 4. Spawn flow

```rust
pub fn spawn_shard(
    shard_id: ShardId,
    cfg: ShardSpawnConfig,
) -> Result<(ShardHandle, ShardJoiner), ShardError> {
    let dir = cfg.data_dir.join(shard_id.to_string());
    std::fs::create_dir_all(&dir)?;

    let shard_uuid = read_or_generate_uuid(&dir.join("shard.uuid"))?;
    let wal_dir = dir.join("wal");
    std::fs::create_dir_all(&wal_dir)?;

    // Arena (9.5).
    let arena_path = dir.join("arena.bin");
    let arena_initial_capacity = cfg.arena_initial_capacity_slots;
    let wal_config = cfg.wal_config.clone();

    // Spawn into Glommio executor; everything WAL-related happens there.
    let (tx, rx) = flume::bounded::<ShardRequest>(cfg.channel_capacity);
    let placement = ...;
    let join_handle = LocalExecutorBuilder::new(placement)
        .name(&format!("brain-shard-{shard_id}"))
        .spawn(move || async move {
            let mut arena = ArenaFile::open(&arena_path, shard_uuid, arena_initial_capacity)?;

            // Recovery: scan existing WAL, derive next_lsn, populate allocator.
            let mut sink = InMemoryMetadataSink::new();
            let (report, allocator) =
                recover(&mut arena, &wal_dir, shard_uuid, &mut sink)?;

            // Open or create the WAL at the recovered next_lsn.
            let wal = if report.records_replayed == 0 && report.records_skipped == 0 {
                Wal::create_with_config(&wal_dir, shard_uuid, wal_config).await?
            } else {
                Wal::open_existing(&wal_dir, shard_uuid, report.next_lsn, wal_config).await?
            };

            let shard = Shard { shard_id, arena, allocator, wal: Some(wal) };
            shard_main_loop(shard, rx).await;
            Ok::<_, ShardError>(())
        })?;

    Ok((ShardHandle { ... }, ShardJoiner { ... }))
}
```

Two things to verify during impl:
1. `ArenaFile::open` is sync — fine inside an async block (it doesn't block on I/O the way fsync does; mmap is fast).
2. `recover` is sync (per audit §3 / §6: reader stays sync). Acceptable here; only runs at startup.

The `Result<_, ShardError>` return from the spawn body means we need a way to surface init errors back to the caller of `spawn_shard`. Currently `spawn_shard` returns a `Result<(handle, joiner), ShardError>` synchronously; we lose the ability to fail late. Options:

- (i) Move the init steps (uuid, arena open, recovery) out of the executor and back to the caller's thread. They're all sync. The `Wal::create_or_open` and the executor spawn happen last. **Simpler.**
- (ii) Race the executor's init result through a oneshot channel; if init fails, return that error.

**Choice: (i).** Keep arena open + recovery on the caller thread; spawn the executor with already-recovered `arena + allocator + next_lsn`. The Glommio executor only owns Wal creation onward (because Wal needs to live on Glommio).

Revised flow:
```rust
pub fn spawn_shard(...) -> Result<(handle, joiner), ShardError> {
    // ... uuid + arena (sync, on caller thread) ...
    let mut arena = ArenaFile::open(...)?;
    let mut sink = InMemoryMetadataSink::new();
    let (report, allocator) = recover(&mut arena, &wal_dir, shard_uuid, &mut sink)?;
    let next_lsn = report.next_lsn;
    let must_open_existing = report.records_replayed > 0 || report.records_skipped > 0;

    // Spawn executor; Wal creation happens inside it.
    let join_handle = LocalExecutorBuilder::new(placement)
        .spawn(move || async move {
            let wal = if must_open_existing {
                Wal::open_existing(&wal_dir, shard_uuid, next_lsn, wal_config).await.expect("wal open")
            } else {
                Wal::create_with_config(&wal_dir, shard_uuid, wal_config).await.expect("wal create")
            };
            shard_main_loop(Shard { shard_id, arena, allocator, wal: Some(wal) }, rx).await;
        })?;
    Ok((handle, joiner))
}
```

The `.expect("wal open")` inside the executor closure is the unfortunate part — Glommio's `spawn(fn() -> F)` returns `ExecutorJoinHandle<()>`; init failures inside the closure become panics-in-thread, surfaced via `joiner.join()`. Acceptable for the scaffold; 9.9 might add a oneshot to bubble first-flush failures.

---

## 5. The `Wal::open_existing` addition (brain-storage)

```rust
// crates/brain-storage/src/wal/wal.rs

impl Wal {
    /// Open an existing WAL for append, resuming at `next_lsn`.
    ///
    /// Caller must have already run `recover()` to determine `next_lsn`;
    /// supplying a wrong value risks LSN reuse, which the WAL reader will
    /// then reject as corruption on the next recovery.
    pub async fn open_existing(
        dir: impl AsRef<Path>,
        shard_uuid: [u8; 16],
        next_lsn: u64,
        config: WalConfig,
    ) -> Result<Self, WalError> {
        let dir_path = dir.as_ref().to_path_buf();

        // Find the highest-segment_seq file. We re-use WalReader's
        // enumeration logic by opening it once.
        let reader = WalReader::open(&dir_path, shard_uuid)?;
        let last_segment = reader
            .segments()
            .last()
            .ok_or_else(|| WalError::DirectoryNotEmpty { dir: dir_path.clone() })?; // misnamed; use a new variant if needed
        let active_segment_seq = last_segment.segment_seq;
        let starting_lsn_in_segment = last_segment.starting_lsn;
        drop(reader);

        // Re-open the active segment for append.
        let seg_path = segment_path(&dir_path, active_segment_seq);
        let segment = WalSegment::open_for_append(&seg_path, shard_uuid, active_segment_seq, starting_lsn_in_segment).await?;

        // The committer needs to know how many bytes are already on disk
        // (sub-task 2.7 records this via WalReader; we extract from the
        // segment file's stat).
        // ...

        let committer = GroupCommitter::start(segment, config.group_commit);

        Ok(Self {
            inner: RefCell::new(WalInner {
                dir: dir_path,
                shard_uuid,
                next_lsn,
                active_segment_seq,
                bytes_in_active_segment: /* derived from segment file size */,
                committer: Some(committer),
                config,
            }),
        })
    }
}
```

`WalSegment::open_for_append` is new — it `BufferedFile::open`s an existing segment, reads + validates the header, and positions for further appends at the current file end. Sister to `create_new`.

This adds:
- ~80 LOC to `wal.rs` (`open_existing`).
- ~100 LOC to `segment.rs` (`open_for_append` + header validation against `WalSegment::create_new`).
- ~6 new tests across both.

---

## 6. ShardSpawnConfig delta

```rust
#[derive(Clone, Debug)]
pub struct ShardSpawnConfig {
    pub channel_capacity: usize,
    pub pin_cpu: Option<usize>,
    pub data_dir: PathBuf,
    pub arena_initial_capacity_slots: u64,
    // NEW:
    pub wal_config: brain_storage::wal::WalConfig,
}
```

`WalConfig: Clone + Copy`, so `Clone` derive on `ShardSpawnConfig` keeps working. Default uses `WalConfig::default()`.

---

## 7. Error / variant additions

```rust
// shard.rs

#[derive(Debug, thiserror::Error)]
pub enum ShardError {
    // ...existing...
    #[error("recovery failed: {0}")]
    Recovery(#[from] brain_storage::recovery::RecoveryError),
    #[error("wal error during init: {0}")]
    WalInit(#[from] brain_storage::wal::WalError),
}

#[derive(Debug, thiserror::Error)]
pub enum ShardOpError {
    // ...existing...
    #[error("wal append failed: {0}")]
    Wal(#[from] brain_storage::wal::WalError),
}
```

---

## 8. Tests (all Linux-gated, container-only)

### 8.1 Existing tests adapted

- `arena_first_spawn_creates_files` — now also asserts `wal/0000000000.wal` exists.
- `arena_alloc_returns_sequential_indices` — unchanged.
- `arena_uuid_persists_across_restarts` — unchanged.
- `spawn_unbound_and_join` — adjust if needed for new init order.

### 8.2 New tests

1. `wal_first_spawn_creates_segment_zero` — fresh shard → `<dir>/<id>/wal/0000000000.wal` exists, contains the 4 KB header.
2. `append_wal_record_returns_lsn` — spawn → submit `AppendWalRecord` → reply has LSN 1, next call LSN 2.
3. `wal_persists_across_restart` — spawn → append 3 records → drop/join → respawn → next append has LSN 4.
4. `wal_records_visible_to_reader_after_shutdown` — spawn → append → drop/join → open `WalReader` on the dir → assert 3 records with monotone LSNs.
5. `restart_with_existing_wal_recovers_allocator` — spawn → append a record that allocates a slot (encode-shaped payload would normally; for 9.6 stub: AllocSlot then AppendWalRecord with a record asserting that slot) → restart → next AllocSlot returns the *next* free index (or pops a reclaimable PENDING_WRITE slot). Sanity: doesn't crash; allocator is consistent with arena state.

The brain-storage side: add 3-5 tests for `Wal::open_existing` + `WalSegment::open_for_append` covering header revalidation, segment-seq filename mismatch, shard-uuid mismatch.

---

## 9. File-by-file

| File | Action | LOC |
| ---- | ------ | --- |
| `crates/brain-storage/src/wal/segment.rs` | Edit | +~120 (`open_for_append` + tests) |
| `crates/brain-storage/src/wal/wal.rs` | Edit | +~100 (`open_existing` + tests) |
| `crates/brain-server/src/shard.rs` | Edit | +~150 (Wal field, init path, AppendWalRecord variant + handler, errors) |
| `crates/brain-server/tests/shard.rs` | Edit | +~150 (5 new tests) |

Single commit. Subject: `feat(brain-server): WAL hookup (sub-task 9.6)`.

Total: ~520 LOC. About half of 9.6a's size.

---

## 10. Risks

| Risk | Mitigation |
| ---- | ---------- |
| `recover()` runs sync on the spawn thread; on a large WAL it could block the caller for seconds | Spec §05/08 §2 budgets this; the connection layer's spawn happens at startup, not on the hot path. Acceptable. Phase 11 perf work can move it onto a blocking pool if needed. |
| `Wal::open_existing` accidentally clobbers in-flight records | We're not opening a *live* WAL — we're respawning after a clean shutdown / crash. `recover()` already established the prefix-truncation point; `open_existing` resumes after it. The shard's single-writer discipline means only one Wal handle exists per shard at a time. |
| `WalSegment::open_for_append` reads via `BufferedFile::open` but the file was written by the previous run (kernel page cache cold) | `BufferedFile::open` is async + io_uring; cold read is fine. |
| `InMemoryMetadataSink` is throw-away — recovery does the work and gets discarded | Yes, on purpose. 9.7 replaces with `MetadataDbSink` and reuses the recovery path. Document inline. |
| Recovery report's `next_lsn` disagrees with the file's actual end-of-records | The WAL reader stops at the first CRC failure (per spec §05/08 §4); `next_lsn = highest_valid_lsn + 1`. Recovery is the source of truth; `open_existing` honors it. |
| Tests can't assert "restart at LSN 4" because the recovery sink throws state away | Tests use `WalReader::open` directly after drop to read the bytes; they assert LSN sequence end-to-end. The 9.7 plan adds the real-sink restart test. |

---

## 11. Done criteria

- [ ] `Wal::open_existing` + `WalSegment::open_for_append` in brain-storage.
- [ ] `Shard` owns `wal: Option<Wal>`; main loop handles `AppendWalRecord`.
- [ ] `spawn_shard` runs recovery, then opens/creates the WAL inside the executor.
- [ ] 5 new integration tests + 3-5 new unit tests pass in container.
- [ ] macOS host: brain-server still compiles (shard cfg-gated as before).
- [ ] Linux container: `cargo test -p brain-storage -p brain-server` green; clippy clean.
- [ ] Commit on `feature/brain-server`.
- [ ] Phase doc 9.6 marked `[x]`.
- [ ] Audit doc (`phase-09-glommio-port.md`) status row "WAL group commit" notes 9.6 as the consumer.

---

## 12. What 9.6 explicitly defers

- **Real metadata sink in recovery.** Uses `InMemoryMetadataSink`. 9.7 plumbs `MetadataDb`.
- **HNSW rebuild on recovery.** Lives in 9.7 via OpsContext.
- **PLAN/REASON tombstone filter carry-over.** 9.16.
- **Checkpoint worker integration.** 9.8 wires `write_checkpoint` into the snapshot worker.
- **Workers replay against WAL.** 9.7 starts the scheduler; no workers run in 9.6.
- **WAL retention worker.** Phase 8's seam; 9.8 wires it.

---

*Implement on approval, once 9.6a's verify + commit is in.*
