# 08.06 Snapshots

A **snapshot** is a point-in-time, self-consistent backup of a shard. Snapshots can be used for:

- Disaster recovery.
- Cloning a shard for analysis.
- Migration between nodes.
- Long-term archival.

This file specifies the snapshot mechanism.

## 1. The mechanism: filesystem reflinks

Brain's snapshots are built on filesystem-level **reflinks** — a copy operation that shares underlying disk blocks via copy-on-write semantics.

Reflinks let Brain snapshot a multi-gigabyte arena in milliseconds, without consuming additional disk space until the original or the snapshot is modified.

Supported filesystems:

- **btrfs** — reflinks are intrinsic and always available.
- **xfs** — reflinks are available with `mkfs.xfs -m reflink=1`. (xfs without `-m reflink=1` does not support reflinks.)
- **ext4** — does **not** support reflinks. Operators on ext4 must use full-copy snapshots, which are slower.
- **zfs** — has its own snapshot mechanism (zfs snapshot); Brain doesn't drive it directly.

The reflink ioctl is `FICLONE` (or `FICLONERANGE` for partial files), defined in [`include/uapi/linux/fs.h`](https://github.com/torvalds/linux/blob/master/include/uapi/linux/fs.h).

## 2. The snapshot procedure

A snapshot is created by `POST /v1/snapshots[?shard=N]` on the admin plane. The operation:

1. **Checkpoint.** Run the checkpoint sequence inline: append `CHECKPOINT_BEGIN` and sample the WAL tail, `msync` the arena, then append `CHECKPOINT_END` carrying `min(durable watermark, sampled tail)`. Writes are neither drained nor paused.
2. **Reflink-copy.** Issue `FICLONE` on `arena.bin` and `metadata.redb`, plus the WAL segments since the checkpoint. The arena copy is taken straight after step 1's `msync_all`, so its on-disk image matches the checkpoint's `durable_lsn`; the metadata copy is taken with a read transaction held and released first.
3. **Verify.** Compute checksums on the snapshot files and store them with the snapshot metadata.
4. **Mark complete.** Write a snapshot manifest containing all the snapshot's files, the LSN at which the snapshot was taken, and the checksums.

Snapshot IDs are assigned from the highest numerically-named directory already under the snapshots root. A read error there is treated as "no snapshots", which can delay an id but never reuses one, so a transient `stat` failure cannot overwrite a prior bundle.

The shard's core is occupied for the duration — the arena `msync` and the copies are synchronous — but no write is rejected or made to wait on a drain. On NVMe the checkpoint portion is typically 10–50 ms.

## 3. Snapshot files

A snapshot is a directory:

```
snapshots/
└── 00000000000000000007/
    ├── manifest.json
    ├── arena.bin             # reflink of arena.bin
    ├── metadata.redb         # reflink of metadata.redb
    ├── shard.uuid            # so the bundle is self-describing
    ├── hnsw.*                # memory HNSW graph + data + wrapper
    └── wal/
        ├── 0000000005.wal    # Every segment whose starting_lsn <= the snapshot LSN
        ├── 0000000006.wal
        └── ...
```

`shard.uuid` is copied best-effort — the manifest's `shard_uuid` is the authoritative identity, so a missing uuid file does not fail the snapshot. It is there so a restore onto a freshly-laid-out data directory lands the identity file too.

A snapshot is **per shard**, and that is the whole of what it covers. Two
pieces of durable state sit at the data-directory root instead, shared by
every shard, and neither is reflinked here:

| File | In a snapshot? |
|---|---|
| `llm_credentials.redb` | **No.** Tenant LLM provider keys, sealed. A DR backup must pick it up explicitly — see [`../18_failure_recovery/06_disaster_recovery.md`](../18_failure_recovery/06_disaster_recovery.md) §3a. |
| `llm_credential_kek` | **No, deliberately.** Shipping the sealing key beside the rows it seals would make the copy plaintext-equivalent. |

The directory name is the snapshot ID, zero-padded to 20 digits. The timestamp lives in the manifest, not the name, so the lexical order of the directories is the numeric order of the IDs. The manifest:

```json
{
    "snapshot_id": 7,
    "shard_uuid": "...",
    "snapshot_lsn": 1234567,
    "checkpoint_lsn": 1234500,
    "taken_at": "2024-01-15T12:30:00Z",
    "files": {
        "arena.bin": { "size": ..., "blake3": "..." },
        "metadata.redb": { "size": ..., "blake3": "..." },
        "wal/0000000005.wal": { ... },
        ...
    }
}
```

## 4. Snapshot consistency

The snapshot is a consistent point-in-time view because:

- The checkpoint's `durable_lsn` is the shard's contiguous committed prefix, so every record at or below it is genuinely in metadata and every record above it is replayed on restore. In-flight writes are therefore safe to ignore rather than drain: whichever side of the line they fall on, the restore is correct.
- Reflinks capture each file atomically (the FICLONE is point-in-time).

A snapshot's contents represent the shard's state at exactly the snapshot LSN.

## 5. Restore procedure

To restore a snapshot:

1. Stop Brain (or at least the affected shard).
2. Move the existing files (`arena.bin`, `metadata.redb`, `wal/`) to a backup location.
3. Reflink-copy (or full-copy) the snapshot files into the shard's data directory.
4. Restart Brain (or shard).
5. Brain runs recovery on the restored data. Recovery will replay the WAL records present in the snapshot to bring everything to the snapshot LSN.

If the snapshot is several minutes (or hours) old, the operator may also want to apply newer WAL records that were captured separately (point-in-time recovery, see § 9).

## 6. Snapshot retention

Operators decide how many snapshots to keep. Brain offers:

- `GET /v1/snapshots` — list all snapshots with their metadata, fanned out across shards.
- `DELETE /v1/snapshots/{id}[?shard=N]` — remove a snapshot.
- A configurable retention policy: "keep last N", "keep all newer than M days", etc.

Old snapshots can be moved to colder storage (S3, tape) for archival. Brain doesn't manage this; it's an operational concern.

## 7. Cross-node restore

The snapshot files are self-contained. They can be moved to a different node:

1. Take the snapshot on node A.
2. Copy the snapshot directory to node B.
3. On node B, point the data directory at the snapshot files.
4. Start Brain on node B; recovery completes the restore.

The shard UUID in the snapshot's manifest must match the shard's UUID at the destination — i.e., this is a restore of the same shard, not a new shard. For new-shard creation from a snapshot, see [16. Sharding & Clustering](../16_sharding/00_purpose.md) §Snapshot-Based Provisioning.

## 8. Snapshot performance

For a 10 GiB arena on a btrfs filesystem with reflinks:

- Checkpoint (including the arena `msync`): 50 ms.
- Reflink ioctls: ~10 ms (most are O(1) per file).
- Manifest write: ~5 ms.
- Verification (checksum): ~3 seconds (depends on disk read speed).

Total: 3–5 seconds wall-clock. No write is rejected or drained in that window; the cost shows up as the shard's core being busy, not as a closed door.

For ext4 (no reflink), full-copy of a 10 GiB arena takes minutes. Brain recommends btrfs or xfs (with reflinks enabled) for production deployments.

## 9. Point-in-time recovery

A snapshot at LSN X plus WAL records LSN X+1 to Y can recover to any LSN in the range [X, Y].

Brain doesn't directly implement point-in-time recovery, but provides the building blocks:

1. Operator restores a snapshot at LSN X.
2. Operator applies additional WAL records (from a backed-up WAL stream) up to the desired LSN Y.
3. The shard is now at LSN Y.

Step 2 requires the WAL records to be available somewhere — either retained in Brain (subject to retention policy) or backed up externally.

For external WAL backup, the SUBSCRIBE feature is the natural mechanism: a long-running consumer copies WAL records to external storage as they're produced.

## 10. Snapshot atomicity guarantee

The reflink ioctls are per-file atomic. A snapshot of multiple files (arena, metadata, WAL segments) is therefore *not* a single atomic operation — there's a brief window during the snapshot where, e.g., the arena has been reflinked but the metadata hasn't.

Brain handles this by:

- Fixing the bundle's as-of LSN *before* any copy, at `CHECKPOINT_END`. Every file is then read against a line already drawn, so a write that lands mid-sequence cannot move it.
- Reflinking files in a specific order: `arena.bin` first (immediately after the checkpoint's `msync_all`, so its image matches `durable_lsn`), then `metadata.redb` under a held-then-released read transaction, then `shard.uuid`, then the WAL tail — every segment whose `starting_lsn <= ` the sampled tail, so the bundle always carries the segment containing the as-of LSN and all earlier ones.
- Verifying consistency after.

Writes continuing during the sequence are harmless. Anything they commit above the as-of LSN is replayed on restore from the copied WAL; anything at or below it was already committed when the watermark was read. The order ensures that if the snapshot is interrupted, partial state is recognizable as partial.

If Brain crashes mid-snapshot, the partial snapshot is detected on next startup and discarded. The next snapshot succeeds normally.

## 11. Snapshot integrity

Each snapshot's manifest carries BLAKE3 checksums of every file. After taking a snapshot, Brain verifies the checksums match the actual file contents. This catches:

- Bit-level corruption from broken hardware.
- Filesystem bugs in the reflink implementation.
- Concurrent modifications (which shouldn't happen, but defensive checks help).

If verification fails, the snapshot is marked invalid and the operator is alerted. Subsequent snapshots are still taken on schedule.

## 12. Encrypted snapshots

For deployments with encryption requirements, Brain doesn't directly implement snapshot encryption — that's a filesystem or volume-level concern. Operators use:

- LUKS encryption on the underlying block device.
- Filesystem-level encryption (ext4 with fscrypt, btrfs with encryption, ZFS native).
- Per-snapshot post-processing with `gpg` or similar.

Brain's snapshots are byte-for-byte the same regardless of encryption, because encryption happens below the filesystem.

## 13. Failure modes

### 13.1 Reflink not supported

If the underlying filesystem doesn't support reflinks (e.g. ext4), the snapshot operation falls back to full-copy. The copy behaviour is correct either way; only the cost differs. Operators should consider btrfs or xfs.

**The warning is latched per `errno`, not emitted per file.** On a filesystem with no reflink support the `FICLONE` ioctl can never succeed, so warning on each call emits the same unactionable line for every file of every snapshot, forever — an hourly snapshot of four files is ~96 `WARN` lines a day whose only effect is to teach operators that `WARN` on this service means nothing.

Latching on `errno` rather than on a plain "already warned" flag matters, because the two failures are different problems:

| `errno` | Meaning | Operator action |
|---|---|---|
| `EOPNOTSUPP` | the filesystem has no reflinks at all | expected on ext4; switch filesystem, or accept full copies |
| `EXDEV` | source and destination are on different mounts | a real misconfiguration — the snapshot target should be on the same filesystem |

A single flag would hide whichever arrived second, and `EXDEV` is the one worth seeing.

The response's `used_reflink` field reports what actually happened for that snapshot, so an operator does not have to infer it from log volume.

### 13.2 Disk full during reflink

A reflink itself doesn't consume disk space. But if the snapshot manifest write fails (out of space), the snapshot is incomplete and discarded.

### 13.3 Concurrent admin operations

Two `POST /v1/snapshots` calls cannot run concurrently on the same shard. The shard's receive loop handles the snapshot request inline rather than detaching it, so a second one waits in the channel until the first returns.

A snapshot runs concurrently with normal user operations. Writes already in flight when the snapshot starts keep going, and each one's inclusion is decided by the durable watermark the snapshot reads, not by timing.

## 14. Snapshots are live

Snapshots take no drain and impose no write stall:

1. The metadata store is reflinked after a read transaction has flushed in-memory state to disk — redb's read transactions are snapshot-isolated, so a copy taken while one is alive is a consistent point-in-time image.
2. The WAL segments are captured at their current state, including the active segment with its current write offset.
3. The arena is reflinked immediately after the checkpoint sequence's `msync_all`, so the on-disk image is consistent with the checkpoint's `durable_lsn`.
4. The snapshot's "as-of" LSN is `min(the shard's durable watermark, the WAL tail sampled at CHECKPOINT_BEGIN)`.

Step 4 is what removes the need for a drain. A write that is WAL-durable but not yet committed in redb sits strictly above the as-of LSN, so restoring this snapshot replays it rather than skipping it. Partial writes in the active WAL segment need no special handling for the same reason: they are above the as-of LSN, and recovery stops at the first CRC failure.

Where the filesystem supports it, the arena and metadata copies are reflinks (`FICLONE`), sharing blocks copy-on-write; otherwise they fall back to a full copy.

---

*Continue to [`07_failure_modes.md`](07_failure_modes.md) for storage failure modes.*
