# RB-05: Worker stuck

**Severity:** **P3** typically; **P2** if the stuck
worker is causing visible degradation; **P1** if it's
blocking something critical like snapshots.
**Alert:** `BrainWorkerStuck`.
**SLO impact:** depends on which worker — most are
background and tolerant of short pauses. The
maintenance, retention, and snapshot workers have
larger impact when stuck.
**Estimated duration:** 15-60 minutes.
**Skill level:** comfortable with the worker
scheduler and the substrate's admin CLI.

A background worker has stopped making progress —
either paused inadvertently, errored repeatedly, or
hung. The substrate is otherwise healthy.

---

## Am I in the right runbook?

You should see:

- `brain_worker_last_run_unixtime{worker="<name>"}` is
  stale (more than 2× the worker's expected interval
  ago).
- The worker is not paused (you didn't pause it).
- Other workers and the substrate's serving path are
  healthy.

If multiple workers are stuck, the substrate itself
is probably degraded — see
[RB-08](rb-08-unresponsive.md). If the substrate is
down entirely, [RB-01](rb-01-substrate-down.md).

---

## Stop the bleeding

Most stuck workers don't need emergency action — they're
background. Take a moment to identify the impact:

| Stuck worker | What degrades |
|---|---|
| `decay` | Salience values stay frozen; ranking quality slowly drifts. Low urgency. |
| `access_boost` | Recently-accessed memories don't get boosted. Low urgency. |
| `consolidation` | Episodic memories don't get consolidated. Low urgency. |
| `hnsw_maintenance` | Tombstone ratio rises; recall quality degrades over hours-to-days. Medium urgency. |
| `idempotency_cleanup` | Idempotency table grows unbounded. Medium urgency (disk pressure). |
| `slot_reclamation` | Forgotten slots not reused; arena grows. Medium urgency (disk pressure). |
| `wal_retention` | WAL segments accumulate; disk fills fast. High urgency. |
| `edge_scrub` | Stale edges accumulate. Low urgency. |
| `counter_reconcile` | Counters drift from truth. Low urgency. |
| `statistics` | Cached stats go stale. Low urgency. |
| `embedder_cache_evict` | Embedder cache may grow above target. Low urgency. |
| `snapshot` | No new snapshots; recovery would be slow if needed. **High urgency.** |

Page-vs-ticket depends on which one. If
`wal_retention` is stuck and the disk is filling
fast, you're effectively in [RB-04](rb-04-disk-filling.md);
treat it as a P2.

Steps:

1. Acknowledge / take ownership.
2. Capture worker state and a diagnostic bundle
   ([DR-01](dr-01-diagnostic-bundle.md)):

   ```bash
   brain-cli admin workers > workers.json
   ```

3. **Don't restart the substrate yet.** A restart
   resets the worker scheduler, which may mask the
   actual cause.

---

## Diagnose

### 1. Which worker, and how stuck?

```bash
brain-cli admin worker show <worker_name>
```

Sample output:

```
worker: hnsw_maintenance
status: running                      ← or "paused", "failed", "idle"
interval_secs: 600
last_run: 2024-09-12T11:00:00Z
last_cycle_duration_ms: 1200
last_error: null                     ← or an error message
cycles_completed: 142
cycles_failed: 0
shard_id: 0                          ← per-shard worker
```

Read the fields:

- **`status`** — `running` means the loop is alive
  and ticking but may be stuck waiting on something.
  `paused` means the operator (or a previous
  incident) paused it. `failed` means the worker
  errored and the scheduler stopped re-running it.
- **`last_run`** — if it's hours stale, the worker
  isn't completing cycles. If it's seconds-to-minutes
  stale, the worker is healthy.
- **`last_error`** — set if the previous cycle
  errored. Read it carefully; it usually says exactly
  what went wrong.
- **`cycles_failed`** — counter; rising means
  recurring failure.

### 2. Is it paused?

If status is `paused`:

```bash
brain-cli admin worker resume <worker_name>
```

Done — most likely a previous incident paused it and
nobody unpaused it.

If you didn't pause it and the channel doesn't show
anyone else pausing it: it's possibly a regression in
the substrate (worker shouldn't auto-pause). Note in
the postmortem.

### 3. Is it failing repeatedly?

If `cycles_failed` is rising and `last_error` is set,
read the error.

Common errors:

- **`Permission denied`** — file ownership broke.
  Check `chown` on the data directory ([RB-01](rb-01-substrate-down.md)'s
  permissions branch).
- **`No space left on device`** — disk full; see
  [RB-04](rb-04-disk-filling.md). The worker can't
  write its progress.
- **`Snapshot worker: failed to create directory`** —
  the snapshots directory doesn't exist or is
  unwritable.
- **`Database locked`** — usually means redb
  contention with another writer. Should be
  impossible in a healthy substrate; investigate.
- **`Network error: failed to upload snapshot`** —
  if snapshots are uploaded off-host, the remote may
  be unreachable. Check connectivity.

Fix the underlying error first; the worker will
self-recover on its next cycle.

### 4. Is it stuck in a single long cycle?

If `last_cycle_duration_ms` is much larger than
typical (e.g., hours), the worker is in the middle
of a cycle that won't end.

This is most likely for:

- **`hnsw_maintenance`** doing a full rebuild on a
  very large shard.
- **`consolidation`** processing a huge backlog of
  recent memories.
- **`snapshot`** copying a multi-GB arena.
- **`backfill`** (knowledge layer) processing many
  memories × extractors.

Check the worker's progress:

```bash
brain-cli admin worker progress <worker_name>
```

If it reports progress (e.g., `processed: 80%`),
let it finish. If it's stuck at the same number for
extended time, it's hung.

### 5. Hung — take a stack trace

If the worker is genuinely hung (not progressing,
not erroring):

Take a stack dump of the substrate
([DR-04](dr-04-profiles-and-heap-dumps.md)). Look
for threads named after the worker (e.g.,
`worker-hnsw-maint`).

Common hang patterns:

- **Blocked on a futex** (`__lll_lock_wait`) —
  contention on a lock that should be uncontended.
  Substrate bug.
- **In `read` / `write` / `fsync`** on a file
  descriptor — disk I/O is hung. Storage problem.
- **In a network syscall** (snapshot uploads) — the
  upstream is unreachable; should time out
  eventually, but might not.

Hung workers are usually a substrate bug. Escalate
([IR-03](ir-03-escalation-policy.md)) after
capturing the dump.

### 6. Is the scheduler itself unhealthy?

If multiple workers across multiple shards are
stuck, the worker scheduler may be broken (not
individual workers):

```bash
brain-cli admin scheduler-status
```

Healthy: each shard's scheduler reports `active`
with recent activity.

Unhealthy: schedulers reporting `error`, `crashed`,
or stale state. This is substrate-bug territory;
escalate.

---

## Remediate

### Resume a paused worker

```bash
brain-cli admin worker resume <worker_name>
```

The worker runs its next cycle on its normal
schedule. If you want it to run immediately:

```bash
brain-cli admin worker run-now <worker_name>
```

### Force a cycle to verify

After a fix, force a cycle to check:

```bash
brain-cli admin worker run-now <worker_name>
brain-cli admin worker show <worker_name>
```

`last_run` should advance, and `last_error` should
be empty if the underlying problem is gone.

### Cancel a stuck long-running cycle

If a worker is stuck in a single cycle (step 4 above)
and you need to interrupt it:

```bash
brain-cli admin worker cancel <worker_name>
```

The worker's current cycle aborts; the next cycle
starts on schedule. Use sparingly — cancelling
mid-rebuild means the work is wasted and has to
restart.

### Restart the substrate

If the worker scheduler itself is broken (step 6),
or a worker is genuinely hung at the thread level:

```bash
sudo systemctl restart brain-server
```

Restart clears all worker state. The substrate
re-spawns workers with fresh scheduler state. Lose
30 s - 2 min of availability.

This is a last resort. Most stuck-worker incidents
shouldn't need a restart; they're recoverable via
admin RPCs.

### Disable a chronically-failing worker

If a worker fails repeatedly and you need to ship
forward without it:

```toml
[workers.<worker_name>]
enabled = false
```

Roll the config out ([OP-08](op-08-config-change-rollout.md)).
The worker doesn't run; its background effect is
absent.

**Caveat:** know what you're giving up. If you
disable `wal_retention`, the WAL grows; you must
manually clean it. If you disable `slot_reclamation`,
forgotten memories stay tombstoned forever (more
disk used).

This is a stopgap until the underlying bug is fixed.
File a ticket; don't leave a worker disabled
indefinitely.

---

## Verify

```bash
brain-cli admin worker show <worker_name>
```

Expect:

- `status: running` (or `idle` between cycles).
- `last_run` is recent.
- `cycles_failed` not increasing.

Also confirm the *symptom* the worker addresses has
gone away:

- `wal_retention` stuck → WAL segments now being
  pruned. Disk usage going down.
- `idempotency_cleanup` stuck → table size dropping.
- `slot_reclamation` stuck → tombstoned-slot count
  going down.
- `hnsw_maintenance` stuck → next maintenance cycle
  completes; tombstone ratio drops if applicable.

For `snapshot`, manually trigger a snapshot and
verify it succeeds:

```bash
brain-cli admin snapshot take
brain-cli admin snapshots --latest
```

The `BrainWorkerStuck` alert clears once
`last_run_unixtime` is fresh.

---

## Post-incident

```
:white_check_mark: Resolved at HH:MM UTC.
Worker: <name>
Root cause: <e.g., disk full at /var/lib/brain prevented WAL retention from writing its checkpoint>.
Remediation: <e.g., freed disk; resumed worker>.
Follow-up: TICKET-NNNN.
Postmortem: <yes/no>.
```

Postmortem rule for RB-05:

- **Usually no** for paused-then-resumed.
- **Yes** if a worker hung at the thread level
  (substrate bug).
- **Yes** if a worker failed repeatedly with the
  same error (extractor / config bug).
- **Yes** if the impact escalated to a higher
  severity before resolution.

---

## Prevention

- **Alert per-worker on stale `last_run`**, not just a
  generic stuck-worker alert. A more specific alert
  routes faster.
- **Don't pause workers casually.** Pausing for
  diagnostic isolation is fine; leaving them paused
  is how you wake up to a P1 a week later.
- **Auto-resume paused workers after N hours** via
  an external scheduler. The substrate doesn't
  auto-resume; that's deliberate (operator's call),
  but it means pauses get forgotten.
- **Track worker cycle duration over time.** If a
  worker's `last_cycle_duration_ms` is trending up,
  that's a leading indicator — eventually it'll be
  too slow to fit its interval and the worker will
  appear stuck.

---

## Related runbooks

- [RB-02 — High latency](rb-02-high-latency.md) (sometimes
  worker contention is the cause)
- [RB-04 — Disk filling](rb-04-disk-filling.md) (often
  caused by a stuck `wal_retention` / `snapshot` /
  `slot_reclamation`)
- [RB-06 — Recall degraded](rb-06-recall-degraded.md) (caused
  by stuck `hnsw_maintenance`)
- [RB-09 — Mass FORGET aftermath](rb-09-mass-forget.md)
  (often pairs with stuck `slot_reclamation`)
- [DR-04 — Profiles and heap dumps](dr-04-profiles-and-heap-dumps.md)
- [OP-08 — Configuration change rollout](op-08-config-change-rollout.md)

---

## Last validated

*Update on first use.*
