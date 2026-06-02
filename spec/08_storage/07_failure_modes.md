# 08.07 Storage Layer Failure Modes

What can go wrong in the storage layer and Brain's response. Failures that span multiple layers are documented in [18. Failure Recovery](../18_failure_recovery/00_purpose.md); this file covers the storage-specific cases.

## 1. WAL write failure (transient)

**Failure mode.** A `pwritev2` returns an error (e.g., transient I/O issue, queue full).

**Detection.** The completion entry from io_uring carries a non-zero error code.

**Response.**

- The current group of pending records is failed with `WalUnavailable`.
- Brain retries the next attempt; if it succeeds, normal operation resumes.
- If multiple consecutive failures occur within a window, the WAL is marked broken.

**Implication.** Clients receiving `WalUnavailable` should back off and retry. Clients should do this with exponential backoff.

## 2. WAL write failure (persistent)

**Failure mode.** Multiple consecutive WAL writes fail. The disk is unavailable, has hardware issues, or is full.

**Detection.** A circuit breaker tracks consecutive failures over a sliding window.

**Response.**

- Brain marks the WAL as "broken".
- All write operations on the shard fail with `ShardReadOnly`.
- Reads continue (the existing arena and metadata are still readable).
- The shard does not auto-recover — operator intervention is required.

**Operator action.** Investigate disk health, free disk space, or restore from backup.

## 3. Arena page cache OOM

**Failure mode.** The system runs out of memory; the kernel evicts arena pages aggressively.

**Detection.** Brain doesn't directly detect this; it manifests as elevated read latency (page faults to disk on every search).

**Response.** Continue operating; performance degrades but correctness holds.

**Operator action.** Add memory, reduce concurrent shards on the node, or split the workload across more nodes.

## 4. Arena partial write before fsync (Brain does not fsync arena)

**Failure mode.** The arena is being written by `memcpy` into the mmap region. Brain crashes before the kernel writes the dirty pages back.

**Detection.** Recovery doesn't directly check this — but the WAL replay re-writes the affected slots, repairing any partial write.

**Response.** Recovery is correct because the WAL is the source of truth. The arena is "fixed" by replay.

**Implication.** None for the operator. This is expected behavior.

## 5. Arena bit flip

**Failure mode.** A vector in the arena has a bit flipped (cosmic ray, RAM error, disk error).

**Detection.**

- **On read** — norm check during search may flag the vector if the bit flip changed the norm meaningfully. (For small bit flips in the mantissa, the norm change is negligible.)
- **Background scrub** — periodic recomputation of the slot's metadata CRC catches arbitrary bit flips.

**Response.**

- If the slot's metadata CRC fails, the slot is flagged corrupted.
- Brain may attempt repair: re-embed the text from the metadata store. If the model is still available and the text is intact, repair succeeds.
- Otherwise, the memory is unrecoverable; the slot is tombstoned and a corrupted-memory event is logged.

## 6. Slot version overflow

**Failure mode.** A slot's `slot_version` reaches `u32::MAX` after many reclamation cycles.

**Detection.** At reclaim time, Brain checks if the version would overflow.

**Response.** The slot is permanently retired; it's not added back to the free list. The arena has one fewer usable slot. A counter tracks total retired slots.

**Operator implication.** In practice, this never happens — 4 billion reclamations of a single slot would take centuries at typical write rates. The defensive check is for correctness, not capacity.

## 7. Segment file deleted externally

**Failure mode.** A WAL segment file is deleted by an external actor (operator mistake, runaway script).

**Detection.** At the next WAL append after the deletion, Brain notices the file is gone.

**Response.** Brain logs the error and treats the WAL as broken. Recovery on next restart will detect the gap (segments aren't consecutive).

**Operator action.** Restore from backup. Don't delete WAL segments by hand.

## 8. Segment file truncated externally

**Failure mode.** A WAL segment file is truncated by an external actor.

**Detection.** At the next WAL append (if it falls in the truncated region), Brain notices the offset doesn't match expected.

**Response.** Same as deletion — WAL marked broken.

**Implication.** The truncation may have happened to the active segment. Records that Brain believed were durable may now be lost. This is a serious incident.

## 9. Metadata store corruption

**Failure mode.** redb's internal checks detect corruption in `metadata.redb`.

**Detection.** redb's `Database::open` returns an error.

**Response.** Brain refuses to start. Recovery cannot proceed without the metadata store.

**Operator action.** Restore from backup. The most recent snapshot's metadata.redb is the recovery point; replay any newer WAL records against it.

## 10. Disk full during normal operation

**Failure mode.** No disk space for new WAL records or arena growth.

**Detection.** `pwritev2` returns ENOSPC; `fallocate` returns ENOSPC.

**Response.**

- WAL writes fail with `OutOfStorage`. The shard transitions to read-only.
- Arena growth fails; subsequent encodes fail with `OutOfStorage` until space is available.
- Existing operations (reads, in-progress writes already past the WAL fsync) complete normally.

**Operator action.** Free disk space (delete old snapshots, expand storage). Once space is available, the shard auto-resumes — write operations succeed.

## 11. Filesystem corruption

**Failure mode.** The underlying filesystem is corrupted (extreme case, e.g., disk hardware failure).

**Detection.** Various errors from kernel I/O calls (EIO, EUCLEAN).

**Response.** Brain logs the error and treats the affected files as inaccessible. The shard may transition to read-only or refuse to operate at all, depending on which file is affected.

**Operator action.** This is a serious event. Run `fsck`, restore from backup, or replace storage.

## 12. Power loss without battery-backed cache

**Failure mode.** The host loses power. Data in the storage device's volatile cache is lost.

**Detection.** On restart, recovery may find that the last few WAL records have CRC mismatches — they were buffered but never made it to non-volatile storage.

**Response.** Recovery treats the CRC failure as a truncation point; everything after is lost. This is the same as a normal crash, except more records may be lost.

**Implication.** Use storage with non-volatile write caches (battery-backed, supercap-backed) for production. Without such storage, Brain's durability is "best effort" — it promises what the kernel promises, which depends on the device.

## 13. Concurrent open of arena from multiple processes

**Failure mode.** Two Brain processes (or other processes) attempt to open the same arena.

**Detection.** Brain uses a per-shard lock file (`shard.lock`) to enforce single-writer.

**Response.** The second process refuses to start, logging the conflict.

**Operator action.** Ensure only one Brain process has access to a given shard's data directory. This is typical for any embedded database.

## 14. Mmap pointer becomes invalid (mremap relocation)

**Failure mode.** During arena growth, `mremap` relocates the mapping to a new address. Code that captured the old base pointer would now see invalid memory.

**Detection.** Implementation-level: Brain uses `arc-swap` to publish the current mmap, and readers always reload it for each access.

**Response.** Readers using the old pointer continue with valid data until they release it (the old mapping is kept alive via Arc). New reads use the new pointer. The old mapping is unmapped after no readers reference it.

**Implication.** None operationally; this is internal correctness handled by Brain.

## 15. Arena read past end of file

**Failure mode.** A bug somewhere in Brain causes a read at an offset beyond the arena file's allocated size.

**Detection.** The mmap region is bounded by the file size at mmap time. Reads beyond it are SIGBUS errors (the kernel sees no backing for the page).

**Response.** Brain's panic handler catches the SIGBUS and logs the offending address. The process may abort.

**Operator action.** This is a bug; report it. Brain should not produce out-of-bounds reads in practice.

## 16. Snapshot file corruption

**Failure mode.** Snapshot files become corrupted (e.g., bit flip on archival storage).

**Detection.** When the snapshot is restored, the BLAKE3 verification fails.

**Response.** Brain refuses to restore the snapshot. Operator must use a different snapshot.

**Implication.** Verify snapshots periodically; keep multiple copies in different locations.

## 17. Operating system upgrade changes RWF_DSYNC behavior

**Failure mode.** A kernel upgrade changes the behavior of `RWF_DSYNC` (e.g., a regression where dsync doesn't wait for completion).

**Detection.** None automatic.

**Response.** None automatic. Brain trusts the kernel's documented semantics.

**Implication.** Test kernel upgrades on staging before production. The Linux kernel has historically been very stable in this area, but defensive testing is wise.

---

*Continue to [`../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md) for unresolved questions.*
