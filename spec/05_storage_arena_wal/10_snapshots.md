# 05.10 Snapshots

A **snapshot** is a point-in-time, self-consistent backup of a shard. Snapshots can be used for:

- Disaster recovery.
- Cloning a shard for analysis.
- Migration between nodes.
- Long-term archival.

This file specifies the snapshot mechanism.

## 1. The mechanism: filesystem reflinks

Brain's snapshots are built on filesystem-level **reflinks** — a copy operation that shares underlying disk blocks via copy-on-write semantics.

Reflinks let us snapshot a multi-gigabyte arena in milliseconds, without consuming additional disk space until the original or the snapshot is modified.

Supported filesystems:

- **btrfs** — reflinks are intrinsic and always available.
- **xfs** — reflinks are available with `mkfs.xfs -m reflink=1`. (xfs without `-m reflink=1` does not support reflinks.)
- **ext4** — does **not** support reflinks. Operators on ext4 must use full-copy snapshots, which are slower.
- **zfs** — has its own snapshot mechanism (zfs snapshot); the substrate doesn't drive it directly.

The reflink ioctl is `FICLONE` (or `FICLONERANGE` for partial files), defined in [`include/uapi/linux/fs.h`](https://github.com/torvalds/linux/blob/master/include/uapi/linux/fs.h).

## 2. The snapshot procedure

A snapshot is created by `ADMIN_SNAPSHOT_CREATE`. The operation:

1. **Drain.** Stop accepting writes briefly (a fraction of a second).
2. **Force checkpoint.** Run a checkpoint to ensure the arena and metadata store are up to date with WAL.
3. **Reflink-copy.** Issue `FICLONE` on `arena.bin` and `metadata.redb`, plus the WAL segments since the checkpoint.
4. **Resume writes.** The active arena and metadata files continue normally; the snapshot files are independent (block-level CoW).
5. **Verify.** Compute checksums on the snapshot files and store them with the snapshot metadata.
6. **Mark complete.** Write a snapshot manifest containing all the snapshot's files, the LSN at which the snapshot was taken, and the checksums.

The total drain time is dominated by step 2 (checkpoint). On NVMe this is typically 10–50 ms.

## 3. Snapshot files

A snapshot is a directory:

```
snapshots/
└── 0000000007-2024-01-15T12-30-00Z/
    ├── manifest.json
    ├── arena.bin             # reflink of arena.bin
    ├── metadata.redb         # reflink of metadata.redb
    └── wal/
        ├── 0000000005.wal    # All segments containing records since the snapshot's checkpoint
        ├── 0000000006.wal
        └── ...
```

The directory name encodes the snapshot ID (sequential) and timestamp (ISO 8601). The manifest:

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

- The drain ensures no writes were in flight at snapshot start.
- The checkpoint ensures arena and metadata are consistent with WAL up to a known LSN.
- Reflinks capture all files atomically (the FICLONE is point-in-time).

A snapshot's contents represent the shard's state at exactly the snapshot LSN.

## 5. Restore procedure

To restore a snapshot:

1. Stop the substrate (or at least the affected shard).
2. Move the existing files (`arena.bin`, `metadata.redb`, `wal/`) to a backup location.
3. Reflink-copy (or full-copy) the snapshot files into the shard's data directory.
4. Restart the substrate (or shard).
5. The substrate runs recovery on the restored data. Recovery will replay the WAL records present in the snapshot to bring everything to the snapshot LSN.

If the snapshot is several minutes (or hours) old, the operator may also want to apply newer WAL records that were captured separately (point-in-time recovery, see § 9).

## 6. Snapshot retention

Operators decide how many snapshots to keep. The substrate offers:

- `ADMIN_SNAPSHOT_LIST` — list all snapshots with their metadata.
- `ADMIN_SNAPSHOT_DELETE` — remove a snapshot.
- A configurable retention policy: "keep last N", "keep all newer than M days", etc.

Old snapshots can be moved to colder storage (S3, tape) for archival. The substrate doesn't manage this; it's an operational concern.

## 7. Cross-node restore

The snapshot files are self-contained. They can be moved to a different node:

1. Take the snapshot on node A.
2. Copy the snapshot directory to node B.
3. On node B, point the data directory at the snapshot files.
4. Start the substrate on node B; recovery completes the restore.

The shard UUID in the snapshot's manifest must match the shard's UUID at the destination — i.e., this is a restore of the same shard, not a new shard. For new-shard creation from a snapshot, see [12. Sharding + Clustering](../12_sharding_clustering/) §Snapshot-Based Provisioning.

## 8. Snapshot performance

For a 10 GiB arena on a btrfs filesystem with reflinks:

- Drain + checkpoint: 50 ms.
- Reflink ioctls: ~10 ms (most are O(1) per file).
- Manifest write: ~5 ms.
- Verification (checksum): ~3 seconds (depends on disk read speed).

Total: 3–5 seconds wall-clock. The user-visible interruption is the drain (50 ms); the rest happens in the background.

For ext4 (no reflink), full-copy of a 10 GiB arena takes minutes. We recommend btrfs or xfs (with reflinks enabled) for production deployments.

## 9. Point-in-time recovery

A snapshot at LSN X plus WAL records LSN X+1 to Y can recover to any LSN in the range [X, Y].

The substrate doesn't directly implement point-in-time recovery, but provides the building blocks:

1. Operator restores a snapshot at LSN X.
2. Operator applies additional WAL records (from a backed-up WAL stream) up to the desired LSN Y.
3. The shard is now at LSN Y.

Step 2 requires the WAL records to be available somewhere — either retained in the substrate (subject to retention policy) or backed up externally.

For external WAL backup, the SUBSCRIBE feature is the natural mechanism: a long-running consumer copies WAL records to external storage as they're produced.

## 10. Snapshot atomicity guarantee

The reflink ioctls are per-file atomic. A snapshot of multiple files (arena, metadata, WAL segments) is therefore *not* a single atomic operation — there's a brief window during the snapshot where, e.g., the arena has been reflinked but the metadata hasn't.

The substrate handles this by:

- Pausing writes during the snapshot (the drain).
- Reflinking files in a specific order (metadata first, then arena, then WAL).
- Verifying consistency after.

The drain ensures no writes are happening during the multi-file ioctl sequence. The order ensures that if the snapshot is interrupted, partial state is recoverable.

If the substrate crashes mid-snapshot, the partial snapshot is detected on next startup and discarded. The next snapshot succeeds normally.

## 11. Snapshot integrity

Each snapshot's manifest carries BLAKE3 checksums of every file. After taking a snapshot, the substrate verifies the checksums match the actual file contents. This catches:

- Bit-level corruption from broken hardware.
- Filesystem bugs in the reflink implementation.
- Concurrent modifications (which shouldn't happen, but defensive checks help).

If verification fails, the snapshot is marked invalid and the operator is alerted. Subsequent snapshots are still taken on schedule.

## 12. Encrypted snapshots

For deployments with encryption requirements, the substrate doesn't directly implement snapshot encryption — that's a filesystem or volume-level concern. Operators use:

- LUKS encryption on the underlying block device.
- Filesystem-level encryption (ext4 with fscrypt, btrfs with encryption, ZFS native).
- Per-snapshot post-processing with `gpg` or similar.

The substrate's snapshots are byte-for-byte the same regardless of encryption, because encryption happens below the filesystem.

## 13. Failure modes

### 13.1 Reflink not supported

If the underlying filesystem doesn't support reflinks (e.g., ext4), the snapshot operation falls back to full-copy. The operation logs a warning. Operators should consider btrfs or xfs.

### 13.2 Disk full during reflink

A reflink itself doesn't consume disk space. But if the snapshot manifest write fails (out of space), the snapshot is incomplete and discarded.

### 13.3 Concurrent admin operations

Two `ADMIN_SNAPSHOT_CREATE` calls cannot run concurrently on the same shard. The substrate serializes them.

A snapshot can run concurrently with normal user operations (the drain is brief).

A snapshot cannot run concurrently with arena growth or rebalancing; the substrate serializes these.

## 14. Live snapshots (no drain)

A future enhancement: snapshots without any drain. The technique:

1. Snapshot the metadata store at its current state.
2. Snapshot the WAL segments at their current state (including the active segment, with its current write offset).
3. The arena is captured at its current state (including any in-flight writes).
4. The snapshot's "as-of" LSN is the last record in the captured WAL.

This requires careful handling of partial writes (especially in the active WAL segment), but is feasible. v1 uses the simpler drain-based approach; future versions may switch.

---

*Continue to [`11_failure_modes.md`](11_failure_modes.md) for storage failure modes.*
