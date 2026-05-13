# Sub-task 9.5 — Real arena hookup

**Reads:** `spec/05_storage_arena_wal/02_arena_layout.md` (slot layout, already implemented in `brain-storage`), `spec/12_sharding_clustering/01_shard_model.md` §1–§5 (shard directory + lifecycle).
**Phase doc:** `docs/phases/phase-09-server.md` §9.5 (was §9.5 / "Real arena hookup" in the orientation; the original numbered phase doc didn't carry this entry — it's net-new from the audit's renumbering).
**Done when:** Each shard owns a real `ArenaFile` + `SlotAllocator` on disk under `<data_dir>/<shard_id>/`, persistent across restarts; a stub `AllocSlot` request returns `(slot_idx, slot_version)` directly from the allocator.

---

## 1. Scope

The first sub-task that gives a shard real durable state. After 9.4 the shard executor exists but owns nothing; 9.5 hands it an `ArenaFile` and a `SlotAllocator`. The Ping is preserved (still validates the boundary); `AllocSlot` is new and validates the storage.

**In scope:**
- Per-shard data directory: `<data_dir>/<shard_id>/` with `arena.bin` + `shard.uuid`.
- UUID persistence: generate on first open, read back on subsequent opens. Mismatch → fail-stop per spec §12/01 §3.
- `Shard` struct lives inside the executor's main loop, owns `ArenaFile` + `SlotAllocator` directly (single-thread access, no `RefCell`/`Rc` needed).
- New `ShardRequest::AllocSlot { reply_tx }` that delegates to `SlotAllocator::alloc(&mut arena)`.
- `ShardSpawnConfig` gains `data_dir: PathBuf` and `arena_initial_capacity_slots: u64`.

**Out of scope (later sub-tasks):**
- WAL — 9.6 (and the io_uring port in 9.6a).
- Recovery from existing WAL on open — 9.6.
- Writing vectors / metadata into slots — 9.7 (via OpsContext + RealWriterHandle).
- HNSW — 9.7.
- Concurrent allocation across shards — already covered by single-shard discipline.
- `metadata.redb` open — 9.7.
- Migrating slot allocation through Phase-8's pluggable free-list — 9.8.
- Sharing the arena across crates (HNSW, metadata) — happens naturally in 9.7 because everything lives on the same executor.

---

## 2. The shard directory contract

```
<data_dir>/                 # from ShardSpawnConfig.data_dir
└── <shard_id>/             # one per shard (decimal logical id; UUID-named
    │                       # dirs deferred to v2, spec §12/01 OQ — `0` / `1` / ...)
    ├── arena.bin           # mmap'd by ArenaFile
    └── shard.uuid          # 16 raw bytes, written once on first open
```

Why `<shard_id>` (decimal) and not `<shard_uuid>`:
- Spec §12/01 §2 shows UUID-named dirs as the long-term aspiration.
- v1 / Phase 9: operator-discoverable layout (`ls data/` shows `0 1 2 3` for a 4-shard deployment) is more useful than UUID strings.
- UUID stays inside the dir (`shard.uuid` file) — usable for snapshot manifests and cluster reconfiguration later.
- Trivial to swap to UUID dirs in v2; no schema change.

The UUID file holds 16 raw bytes (no JSON / no TOML — smallest possible artefact). Mismatch on reopen → `ShardError::ShardUuidMismatch`. Same shard ID + different UUID = operator did something wrong; fail-stop.

---

## 3. Surface changes

```rust
// crates/brain-server/src/shard.rs   (Linux-only)

use std::path::PathBuf;
use brain_storage::arena::{
    AllocError, ArenaFile, ArenaOpenError, SlotAllocator, DEFAULT_INITIAL_CAPACITY_SLOTS,
};
use brain_core::SlotVersion;

#[derive(Clone, Debug)]
pub struct ShardSpawnConfig {
    pub channel_capacity: usize,
    pub pin_cpu: Option<usize>,
    /// Root data directory. Per-shard subdir is `<data_dir>/<shard_id>/`.
    pub data_dir: PathBuf,
    /// Initial arena capacity in slots. Default = `DEFAULT_INITIAL_CAPACITY_SLOTS`.
    pub arena_initial_capacity_slots: u64,
}

pub(crate) enum ShardRequest {
    Ping { reply_tx: Sender<()> },
    /// Allocate a fresh slot. Returns (slot_idx, slot_version).
    AllocSlot { reply_tx: Sender<Result<(u64, SlotVersion), ShardOpError>> },
}

#[derive(Debug, thiserror::Error)]
pub enum ShardError {
    #[error("shard has shut down or is unreachable")]
    ShardDisconnected,
    #[error("failed to launch Glommio executor: {0}")]
    Spawn(String),
    #[error("failed to join shard executor thread: {0}")]
    Join(String),
    #[error("failed to open arena: {0}")]
    ArenaOpen(#[from] ArenaOpenError),
    #[error("failed to create shard directory at {path:?}: {source}")]
    DirCreate { path: PathBuf, #[source] source: std::io::Error },
    #[error("failed to read/write shard.uuid at {path:?}: {source}")]
    UuidFile { path: PathBuf, #[source] source: std::io::Error },
    #[error(
        "shard.uuid mismatch at {path:?}: arena has {arena_uuid:?}, file has {file_uuid:?}"
    )]
    ShardUuidMismatch { path: PathBuf, arena_uuid: [u8; 16], file_uuid: [u8; 16] },
}

/// In-shard error type for op-time failures (vs. ShardError which is
/// spawn-time). Sent back through `reply_tx`.
#[derive(Debug, thiserror::Error)]
pub enum ShardOpError {
    #[error("arena full: {0}")]
    ArenaFull(AllocError),
    // future: WalAppend, MetadataConflict, ...
}
```

`ShardHandle` gains an alloc convenience:
```rust
impl ShardHandle {
    pub async fn alloc_slot(&self) -> Result<(u64, SlotVersion), ShardError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.tx.send_async(ShardRequest::AllocSlot { reply_tx }).await
            .map_err(|_| ShardError::ShardDisconnected)?;
        reply_rx.recv_async().await
            .map_err(|_| ShardError::ShardDisconnected)?
            .map_err(|e| /* wrap or pass through */)
    }
}
```

`spawn_shard` does the directory + uuid + arena open before launching the executor:
```rust
pub fn spawn_shard(
    shard_id: ShardId,
    cfg: ShardSpawnConfig,
) -> Result<(ShardHandle, ShardJoiner), ShardError> {
    let dir = cfg.data_dir.join(shard_id.to_string());
    std::fs::create_dir_all(&dir).map_err(|source| ShardError::DirCreate {
        path: dir.clone(), source
    })?;

    let uuid_path = dir.join("shard.uuid");
    let shard_uuid = read_or_generate_uuid(&uuid_path)?;

    let arena_path = dir.join("arena.bin");
    let arena = ArenaFile::open(&arena_path, shard_uuid, cfg.arena_initial_capacity_slots)?;
    let allocator = SlotAllocator::rebuild_from_arena(&arena);
    // (rebuild_from_arena walks the slots reading flags — fast on an empty
    // arena, slow on a full one. For v1 / 9.5 this is fine; 9.6's WAL replay
    // will reseed the allocator from WAL records instead, but the rebuild
    // path is the recovery fallback.)

    // ... spawn Glommio executor with `Shard { arena, allocator, shard_id }`.
}
```

The shard's main loop owns the arena directly:
```rust
struct Shard {
    shard_id: ShardId,
    arena: ArenaFile,
    allocator: SlotAllocator,
}

async fn shard_main_loop(mut shard: Shard, rx: Receiver<ShardRequest>) {
    info!(shard_id = shard.shard_id, "shard executor entering main loop");
    while let Ok(req) = rx.recv_async().await {
        match req {
            ShardRequest::Ping { reply_tx } => {
                let _ = reply_tx.send_async(()).await;
            }
            ShardRequest::AllocSlot { reply_tx } => {
                let out = shard.allocator.alloc(&mut shard.arena)
                    .map_err(ShardOpError::ArenaFull);
                let _ = reply_tx.send_async(out).await;
            }
        }
    }
    // Optional: msync the arena before exit. ArenaFile already does the
    // right thing via mmap drop, but explicit is clearer:
    if let Err(e) = shard.arena.msync_all() {
        warn!(shard_id = shard.shard_id, error = %e, "msync at shutdown failed");
    }
    info!(shard_id = shard.shard_id, "shard main loop exiting");
}
```

---

## 4. UUID handling

```rust
fn read_or_generate_uuid(path: &Path) -> Result<[u8; 16], ShardError> {
    match std::fs::read(path) {
        Ok(bytes) if bytes.len() == 16 => {
            let mut out = [0u8; 16];
            out.copy_from_slice(&bytes);
            Ok(out)
        }
        Ok(other) => Err(ShardError::UuidFile {
            path: path.to_owned(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("shard.uuid expected 16 bytes, got {}", other.len()),
            ),
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let uuid = uuid::Uuid::now_v7();
            let bytes = *uuid.as_bytes();
            std::fs::write(path, bytes).map_err(|source| ShardError::UuidFile {
                path: path.to_owned(), source,
            })?;
            Ok(bytes)
        }
        Err(source) => Err(ShardError::UuidFile { path: path.to_owned(), source }),
    }
}
```

uuid is already in dev-deps from 9.3; promote to a real dep for this sub-task.

`ArenaFile::open` itself validates the stored uuid against what we pass; if they disagree (because the arena was written with a different uuid than what we just read from `shard.uuid`), it errors with `ArenaOpenError::ShardUuidMismatch`, which our `#[from]` impl wraps. That covers the "operator copied an arena file into the wrong dir" failure mode.

---

## 5. Tests

All Linux-gated (consistent with 9.4 — brain-storage refuses to compile on macOS).

### Integration (`tests/shard.rs`)

Reuse the existing test harness; add:

1. `arena_first_spawn_creates_files`
   - tempdir → spawn → assert `<dir>/0/arena.bin` and `<dir>/0/shard.uuid` exist → drop/join.

2. `arena_alloc_returns_sequential_indices`
   - spawn → alloc_slot 3× → expect (0, 1), (1, 1), (2, 1).
   - (Versions are 1 because `rebuild_from_arena` returns `next_fresh = 0` on empty arena; the allocator hands out 0 then bumps. Verify against the actual `SlotAllocator` semantics during impl.)

3. `arena_persists_across_restarts`
   - tempdir → spawn → alloc 2× → drop/join.
   - re-spawn same tempdir → uuid file matches → arena.bin exists, capacity unchanged → drop/join.

4. `shard_uuid_mismatch_errors`
   - tempdir → spawn (writes uuid A) → drop/join.
   - corrupt `shard.uuid` with all-zero bytes (different value than A).
   - re-spawn → ArenaOpen error (because the arena.bin still has uuid A but we ask for [0u8;16]).

5. `data_dir_under_relative_path` (smoke for path handling)
   - tempdir with a deep subpath → spawn → succeeds.

### Unit (`src/shard.rs`)

6. `shard_spawn_config_default_has_sensible_arena_capacity`
   - Default uses `DEFAULT_INITIAL_CAPACITY_SLOTS` (brain-storage's choice).
7. `read_or_generate_uuid_creates_file_when_absent`
   - tempdir → call helper → file exists, contents are 16 bytes.
8. `read_or_generate_uuid_returns_existing`
   - write known 16 bytes → call helper → returns same bytes.

---

## 6. Cargo

Workspace already has uuid in `[workspace.dependencies]`.

`crates/brain-server/Cargo.toml`:
```toml
[dependencies]
uuid.workspace = true   # promoted from dev-deps

[target.'cfg(target_os = "linux")'.dependencies]
glommio.workspace = true
brain-storage = { path = "../brain-storage" }   # new
```

brain-storage is Linux-only — same target gate as glommio. macOS builds skip both.

---

## 7. Risks

| Risk | Mitigation |
| ---- | ---------- |
| `SlotAllocator::rebuild_from_arena` is O(capacity) — could be slow on large arenas | `DEFAULT_INITIAL_CAPACITY_SLOTS` is small (probably 1024–16384, confirm in impl). 9.6 will reseed from WAL on warm start; rebuild stays as the cold-start fallback. |
| Two shards point at the same `<data_dir>/<id>/` | Out of scope: that's a config error. Spec §12/01 §3 (UUID permanence) gives us the mismatch fail-stop. |
| `ArenaFile` open from a path the executor thread can't actually `mmap` (e.g. permissions) | Surfaced as `ArenaOpenError` at spawn time, before any Ping ever arrives. |
| `msync_all()` at shutdown blocks the executor | It's a syscall, sync. For 9.5 the arena is small; later sub-tasks will move this to background. Audit §8.3 noted same concern for WAL — same answer here: acceptable for v1 scaffold. |
| 9.4's `spawn_unbound_and_join` test relies on `ShardSpawnConfig::default()` which no longer compiles (data_dir mandatory) | Update the unit test to construct a tempdir-backed config. Acceptable churn. |

---

## 8. File-by-file

| File | Action | LOC |
| ---- | ------ | --- |
| `crates/brain-server/Cargo.toml` | Edit | +3 (uuid promotion + brain-storage dep) |
| `crates/brain-server/src/shard.rs` | Edit | +~180 net (shard struct, AllocSlot, uuid helper, error variants) |
| `crates/brain-server/tests/shard.rs` | Edit | +~150 (5 new integration tests; existing tests updated for new ShardSpawnConfig) |

Single commit. Subject: `feat(brain-server): arena hookup (sub-task 9.5)`.

---

## 9. Verification plan

1. macOS: `cargo check / test / clippy / fmt -p brain-server` — 39 host tests still pass (shard module gated out).
2. Linux container (with `--ulimit memlock=-1 --security-opt seccomp=unconfined`): full test suite — 21 config/routing + 13 shard (10 from 9.4 + 3 new units + 5 new integration, with 1 unit replaced) = ~39 shard-side tests. All green.
3. Linux container clippy: clean.

If macOS check breaks (e.g. an unguarded `use brain_storage::…`), fix the cfg gate before committing. The shard module is already `#![cfg(target_os = "linux")]`; brain-storage import must stay inside it.

---

## 10. Done criteria

- [ ] Per-shard dir + arena.bin + shard.uuid persisted under `<data_dir>/<shard_id>/`.
- [ ] AllocSlot returns sequential `(slot_idx, slot_version)` from the executor.
- [ ] UUID survives restart; mismatch fail-stops with a clear error.
- [ ] macOS host: build + tests still green (gated).
- [ ] Linux container: build + tests + clippy green.
- [ ] Commit on `feature/brain-server`.
- [ ] Phase doc 9.5 marked `[x]`.

---

## 11. What 9.5 explicitly defers

- **WAL.** All allocation is in-memory + mmap'd; nothing is fsynced beyond the arena's own `msync_all` at shutdown. Crash-mid-alloc loses the bump pointer; on restart `rebuild_from_arena` re-derives it from slot flags. That's the v1 recovery story before WAL exists.
- **Freeing slots.** `SlotAllocator::free` exists in brain-storage but we don't expose it through ShardRequest. 9.7 (slot reclamation worker integration) does.
- **Growth.** `ArenaFile::grow_to` exists; we don't call it. 9.7 / 9.8 can plumb growth through the arena-capacity worker.
- **Vector / metadata writes.** AllocSlot returns the index; nothing writes to the slot yet. The slot stays all-zero (its `flags & OCCUPIED == 0`). 9.7's ENCODE wire-up fills it in.

---

*Implement on approval.*
