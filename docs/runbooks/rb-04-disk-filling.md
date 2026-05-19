# RB-04: Disk filling

**Severity:** **P2** (P1 if disk will be full in <1 h).
**Alert:** `BrainDiskFilling`.
**SLO impact:** writes (encode, link, etc.) will fail
when the disk is full. Reads continue working until
the substrate detects the failure and goes into
fail-stop.
**Estimated duration:** 30 minutes to several hours
depending on whether the fix is "free space" or
"extend disk."
**Skill level:** comfortable with `df` / `du` and
the substrate's data directory layout.

The data partition is projected to fill up within the
configured warning horizon (typically 24 hours).

---

## Am I in the right runbook?

You should see:

- `df` shows the data partition (where
  `storage.data_dir` lives) above ~80 % usage.
- `node_filesystem_free_bytes` decreasing
  consistently.
- `predict_linear(node_filesystem_free_bytes[1h], 24*3600) < 0`
  in Prometheus (i.e., projects to zero in <24 h).

If the disk is **already full**, the substrate is
probably in [RB-01](rb-01-substrate-down.md)
(refusing to start) or [RB-08](rb-08-unresponsive.md)
(stuck mid-write). Run *Stop the bleeding* below
either way.

---

## Stop the bleeding

If <1 hour until disk is full, this is a P1. The
substrate will start failing writes shortly.

In parallel:

1. Acknowledge the page; open the incident channel.
2. Capture a diagnostic bundle
   ([DR-01](dr-01-diagnostic-bundle.md)).
3. **Stop snapshots** if any are running — they
   temporarily double space usage:

   ```bash
   brain-cli admin worker pause snapshot
   ```

4. **Check whether the substrate is still writing.**
   If you have *any* meaningful space, the goal is
   to free space *while* it keeps serving. If you
   have <500 MB free, the substrate may be about to
   error out; consider stopping it cleanly first:

   ```bash
   # ONLY if substrate is about to crash anyway.
   sudo systemctl stop brain-server
   ```

   Then you have unlimited time to free space without
   the substrate clobbering anything. The cost is
   downtime.

---

## Diagnose

### 1. What's using the space?

```bash
du -sh /var/lib/brain/data/* 2>/dev/null | sort -h
du -sh /var/lib/brain/data/*/* 2>/dev/null | sort -h | tail -20
```

Typical layout per shard:

| Path | Usually big? |
|---|---|
| `arena.bin` | Yes — scales with memory count. |
| `wal/` | Variable — bounded by retention config. |
| `metadata.redb` | Moderate — scales with rows. |
| `snapshots/` | Yes — each snapshot is a near-full copy. |
| `entity.hnsw` | Small. |
| `statement.hnsw` | Moderate. |
| `*.tantivy/` | Moderate, can be large in heavy text workloads. |
| `llm_cache.redb` | Configurable cap (default 10 GB). |
| `shard.uuid` | Tiny. |

Identify the top consumers and target them.

### 2. WAL bigger than expected?

```bash
du -sh /var/lib/brain/data/*/wal/
```

The retention worker (`wal_retention`) prunes old
WAL segments after a configurable number is
retained (default 4 segments × 64 MiB = 256 MiB
per shard).

If WAL is much bigger:

```bash
brain-cli admin worker show wal_retention
```

- **`status: paused`** → unpause:
  `brain-cli admin worker resume wal_retention`.
- **`last_run` stale (hours ago)** → worker stuck.
  See [RB-05](rb-05-worker-stuck.md).
- **`last_error` set** → something specific is
  blocking pruning. Read the error.

### 3. Too many snapshots?

```bash
brain-cli admin snapshots
du -sh /var/lib/brain/data/*/snapshots/*
```

The substrate keeps snapshots indefinitely by
default — operator's responsibility to prune.

For a healthy retention:

- Keep at least the **last 7 days** of daily
  snapshots.
- Keep at least the **last 2 weeks** of weekly
  snapshots.
- Anything older is candidate for deletion (assuming
  you have off-server backups).

### 4. Idempotency table growing?

If `metadata.redb` is much bigger than expected:

```bash
brain-cli admin idempotency stats
```

If the idempotency table has millions of rows and
the oldest is days old, the cleanup worker isn't
running. Same diagnosis as WAL retention; check the
`idempotency_cleanup` worker.

### 5. LLM cache filling

If `llm_cache.redb` is big (>5 GB per shard):

```bash
ls -lh /var/lib/brain/data/*/llm_cache.redb
brain-cli admin llm-cache stats
```

The cache has a configurable cap and TTL. If above
cap, either the cap is set too high or the
`llm_cache_sweeper` worker isn't running.

### 6. Tombstones not being reclaimed

If `arena.bin` is much bigger than your live memory
count would suggest:

```bash
brain-cli admin shard 0 | jq '.live_memories, .tombstoned_memories'
```

Tombstones stay in the arena until the grace period
expires and the `slot_reclamation` worker runs. If
many tombstones are pending reclamation, that's
disk pressure with a fix: the grace period passing.

Note: reclamation doesn't shrink `arena.bin` (it
reuses slots), but it does mean **future encodes
reuse slots** rather than appending. So this matters
for *growth* more than for *current* usage.

### 7. Is it really Brain's data?

```bash
df -h /var/lib/brain
ls -la /var/lib/brain
du -sh /var/lib/brain/* 2>/dev/null | sort -h
```

If non-Brain files are on the partition (logs piling
up, core dumps from a previous crash, leftover
files from upgrades), those might be the issue.
Brain's data dir is `/var/lib/brain/data` by default;
anything else under `/var/lib/brain/` is fair game
to look at.

System logs:

```bash
sudo du -sh /var/log/* 2>/dev/null | sort -h | tail
```

Often the second-biggest source after Brain's own
data.

---

## Remediate

### Free space by pruning snapshots

```bash
# List snapshots, oldest first.
brain-cli admin snapshots --sort-by age

# Delete one.
brain-cli admin snapshot delete --id <snapshot_id>

# Or, batch-delete anything older than 30 days.
brain-cli admin snapshots --older-than 30d \
    | jq -r '.[].id' \
    | xargs -I{} brain-cli admin snapshot delete --id {}
```

Confirm you have off-server backups of anything
you delete. **Never** delete the last snapshot
unless one was just successfully completed.

### Resume the retention worker

If the WAL is bloated:

```bash
brain-cli admin worker resume wal_retention
brain-cli admin worker run-now wal_retention
```

The `run-now` triggers an immediate cycle outside
the regular schedule. The worker prunes WAL segments
beyond the configured retention. Within seconds (or
minutes for large WALs), space frees.

If the worker is stuck (not paused, just not
working), see [RB-05](rb-05-worker-stuck.md).

### Hard-forget specific memories

If you have specific large memories that should be
removed immediately (privacy or correctness):

```bash
brain-cli forget --hard <memory_id>
```

Hard-forget zeros the slot's bytes immediately
*before* the grace period, but the slot itself still
exists. The arena file doesn't shrink; reclamation
just reuses the slot later.

Hard-forget is for *content*, not *space*. To
recover space you need slot reclamation, which is
the next remediation.

### Reduce LLM cache cap

If the LLM cache is the offender:

```toml
[knowledge.llm_cache]
cap_bytes = "2GiB"      # was 10GiB
```

Roll the config out ([OP-08](op-08-config-change-rollout.md)).
The next sweeper cycle prunes entries above the new
cap. You won't recover the space *instantly* — the
sweeper runs hourly by default. You can speed it up:

```bash
brain-cli admin worker run-now llm_cache_sweeper
```

### Extend the disk

If you've done everything above and still don't have
enough space, you need more disk.

Common patterns:

**LVM extend** (preferred where possible):

```bash
# Identify the volume group and logical volume.
sudo vgs
sudo lvs

# Extend the LV.
sudo lvextend -L +50G /dev/vgname/lvname
sudo resize2fs /dev/vgname/lvname   # or xfs_growfs on XFS
```

LVM extend doesn't require unmounting. Verify with
`df -h`.

**Cloud disk resize:**

- AWS: increase EBS volume size in console / via API.
  Run `growpart` and `resize2fs` / `xfs_growfs`
  inside the instance.
- GCP: `gcloud compute disks resize`.
- Azure: similar in the portal / CLI.

Brain doesn't need to be restarted for an LVM /
cloud-disk extend. The substrate uses `fallocate` for
arena growth and the WAL writer reads
`statfs()` at each segment rotation; both pick up the
new capacity automatically.

**Migrate to a bigger disk** (last resort, requires
downtime):

1. Stop the substrate cleanly.
2. Copy the data directory to the new disk.
3. Update `[storage] data_dir` in the config.
4. Start.

This is a [RB-07](rb-07-corruption-recovery.md)-shape
operation; follow that runbook's "snapshot restore"
procedure but to a different mount point.

---

## Verify

```bash
df -h /var/lib/brain
```

Free space should be well above the alert threshold
(typically >20 % free).

Confirm writes are working:

```bash
brain-cli encode "smoke test $(date +%s)"
```

Check the alert. `BrainDiskFilling` clears once
projected-free is above the warning horizon (24 h
typically).

If you paused workers, unpause them now:

```bash
brain-cli admin worker resume snapshot
brain-cli admin worker resume wal_retention
brain-cli admin worker resume llm_cache_sweeper
```

---

## Post-incident

```
:white_check_mark: Resolved at HH:MM UTC.
Disk usage at <X>% from <Y>%.
Root cause: <e.g., snapshot worker hadn't pruned old snapshots>.
Remediation: <e.g., deleted snapshots >30 days; extended LVM by 50 GB>.
Follow-up: TICKET-NNNN (snapshot pruning policy).
Postmortem: <yes/no>.
```

Postmortem rule for RB-04:

- **Always** if the disk filled (alerts at 5 % free).
- **Usually** if the cause was a stuck worker (that's
  a recurring class of bug; worth documenting).
- **Sometimes** if it's a slow accumulation that's
  expected behaviour.

---

## Prevention

- **Track free-space trend, not just current.**
  Linear-projection alerts catch a slow disk-filling
  pattern early, before you're 24 hours from
  failure.
- **Establish a snapshot-retention policy** and
  *automate it.* The substrate's snapshot worker
  takes snapshots; it doesn't prune them. You need a
  pruning policy expressed as a cron / scheduled
  task.
- **Test snapshot deletion** as part of routine
  operational drills ([OP-03](op-03-backup-verification.md)).
- **Don't over-provision the LLM cache.** A 10 GB
  cache is rarely worth what it costs; tune to
  actual hit rates.
- **Size disks with headroom.** A 1M-memory shard
  needs ~3-5 GB of substrate data on disk. Plus
  ~10 GB for the LLM cache if you're using one.
  Plus 2× snapshot overhead. Plus growth budget.
- **Monitor `/var/log`** separately; system logs
  filling a shared partition has caused outages.

---

## Related runbooks

- [RB-01 — Substrate doesn't start](rb-01-substrate-down.md) (if disk is full)
- [RB-05 — Worker stuck](rb-05-worker-stuck.md) (if retention is broken)
- [RB-07 — Recovery from corruption](rb-07-corruption-recovery.md) (if disk full caused corruption)
- [RB-14 — LLM cost spike](rb-14-llm-cost-spike.md) (a runaway LLM cache may indicate cost spike too)
- [OP-03 — Backup verification](op-03-backup-verification.md)
- [OP-08 — Configuration change rollout](op-08-config-change-rollout.md)

---

## Last validated

*Update on first use.*
