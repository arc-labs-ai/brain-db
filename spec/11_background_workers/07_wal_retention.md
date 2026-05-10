# 11.07 WAL Retention Worker

The WAL retention worker deletes old WAL segments after a checkpoint has covered them.

## 1. The WAL segment lifecycle

From [05.04 WAL Layout](../05_storage_arena_wal/04_wal_layout.md):

- WAL is split into segments (256 MiB each by default).
- New writes append to the current segment.
- When a segment fills, it's closed; a new segment is started.
- Closed segments are kept until they're "covered" by a checkpoint.

A segment is **covered** when:
- Its highest LSN is less than the latest checkpoint's `durable_lsn`.
- Equivalently: the metadata reflects all changes from this segment.

A covered segment is no longer needed for recovery.

## 2. The cycle

Every 1 minute (configurable):

1. Read the latest checkpoint.
2. List segments older than the checkpoint's LSN coverage.
3. Delete those segments (and add to deletion log).

## 3. Implementation

```rust
async fn cycle(state: &ShardState) -> Result<usize> {
    let checkpoint = state.metadata.read_latest_checkpoint().await?;
    let cutoff_lsn = checkpoint.durable_lsn;
    let retention_extra = state.config.wal_retention_extra;  // For safety
    let safe_cutoff = cutoff_lsn.saturating_sub(retention_extra);

    let mut deleted = 0;
    let segments = state.wal.list_segments().await?;
    
    for segment in segments {
        if segment.last_lsn < safe_cutoff {
            state.wal.delete_segment(segment.id).await?;
            deleted += 1;
            audit_log("wal_segment_deleted", &segment);
        }
    }

    Ok(deleted)
}
```

Segments below the safe cutoff are deleted. The `retention_extra` (default: 1 segment's worth of LSNs) provides a safety buffer.

## 4. Why retain a buffer

Without the buffer:

- Checkpoint at LSN 1,000,000.
- Latest WAL segment ends at LSN 1,000,500.
- The worker deletes segments ending below 1,000,000.

But the checkpoint may not actually cover everything up to 1,000,000 — it's a snapshot of metadata at the time, and there may be in-flight writes still being applied to metadata when the checkpoint was taken.

The retention buffer protects against this: keep an extra segment's worth, just in case.

In practice, the buffer is rarely needed; checkpointing is conservative. But it's cheap and adds robustness.

## 5. The recovery dependency

WAL retention is critical for recovery: if a needed segment is deleted, recovery fails.

The worker is conservative about deletion:
- Only delete after a confirmed checkpoint covers the segment.
- Apply the retention buffer.
- Verify the segment's LSN range against the checkpoint.

## 6. The disk usage tradeoff

Retention period determines WAL disk usage:

- Default checkpoint cadence: every ~1 hour or 256 MiB worth of WAL.
- Retention: until checkpointed.
- Steady state: ~512 MiB - 1 GiB of WAL on disk per shard.

For deployments with longer checkpoint cadence, more WAL retained. For very short cadence, very little.

## 7. The configuration

```toml
[wal]
retention_extra = "256MiB"        # The buffer
segment_size = "256MiB"

[workers.wal_retention]
enabled = true
interval = "1m"
```

Per-segment cleanup happens promptly (within the next cycle after coverage).

## 8. The "audit"

Each segment deletion is logged:

```
{
  event: "wal_segment_deleted",
  segment_id: 12345,
  first_lsn: 800000,
  last_lsn: 850000,
  size_bytes: 268435456,
}
```

For audit trails, these logs document data lifecycle. If a segment is deleted in error, the log is the first place to look.

## 9. The safety check

Before deleting, the worker double-checks:

- The metadata's `next_lsn` is greater than the segment's last LSN (the metadata has progressed past this segment).
- The shard isn't currently recovering.
- The checkpoint is recent (not stale).

If any check fails, the deletion is skipped. The worker tries again next cycle.

## 10. The "rare failure" case

If a segment is somehow deleted prematurely (a bug), recovery fails:

- Recovery sees a gap in the WAL (LSN X exists in metadata but the WAL for LSN X-1 is missing).
- The substrate refuses to start.
- An operator must restore from backup.

Such failures are bugs and should be caught in testing. The retention buffer provides defense-in-depth.

## 11. The interaction with snapshots

When a snapshot is taken (`ADMIN_SNAPSHOT_CREATE`):

- The snapshot includes the current state up to a known LSN.
- The snapshot is not a substitute for the WAL.

WAL retention is independent of snapshots. A snapshot may exist that includes the deleted WAL data, but recovery uses snapshots only via explicit `ADMIN_RESTORE`.

## 12. The cleanup cost

Per segment deletion: ~10-50 ms (for Linux's unlink + filesystem metadata update).

For a 1-minute cycle deleting 1-2 segments: negligible.

## 13. The "no deletions" path

If no segments are eligible for deletion:

- The worker checks, finds none.
- Sleep until next cycle.

For low-write shards, most cycles have nothing to delete.

## 14. The disk full prevention

If the disk is filling, the substrate doesn't aggressively delete WAL — that would risk recoverability. Instead:

- The substrate sheds load (rejects new writes).
- The operator must address disk space.

WAL retention is conservative; it prefers data safety over disk reclamation.

## 15. The audit log itself

The deletion logs are in the substrate's structured log stream. They can be exported or stored separately for compliance.

For high-compliance deployments, the logs may include cryptographic hashes of segments before deletion (so an auditor can verify what was deleted).

In v1, simple structured logs are enough. Cryptographic logging is a future enhancement.

## 16. The "manual delete" override

`ADMIN_WAL_PRUNE <up_to_lsn>` triggers an immediate retention pass with the specified LSN cutoff. Useful for:

- Reclaiming space after operator confirmation.
- Cleaning up after a known recovery completion.

The operation respects the safety checks; if the cutoff is unsafe (would delete uncheckpointed data), it's rejected.

---

*Continue to [`08_misc_workers.md`](08_misc_workers.md) for the remaining workers.*
