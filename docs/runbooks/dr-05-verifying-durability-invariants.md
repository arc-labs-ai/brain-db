# DR-05: Verifying durability invariants

**When to use:** any time you suspect data corruption,
after a hard crash, before declaring an incident
resolved, or as a periodic operational check.

The substrate makes seven durability invariants. This
runbook explains how to *verify* each one and what to
do if a check fails. The concepts behind the invariants
themselves live in
[`../concepts/24-invariants-and-trust.md`](../concepts/24-invariants-and-trust.md);
this runbook is the operator-facing how-to.

---

## The seven invariants, recap

| # | Invariant | How to verify |
|---|---|---|
| 1 | WAL-before-acknowledge | Inspect WAL tail; correlate with last acked LSN. |
| 2 | Single writer per shard | Confirm one substrate process per data dir. |
| 3 | CRC everywhere | Run CRC sweep on WAL and arena. |
| 4 | `MemoryId` slot version | Spot-check `MemoryId` → slot version mapping. |
| 5 | Idempotency by `request_id` | Verify the idempotency table is being pruned. |
| 6 | Tombstone grace | Confirm reclaim worker isn't running ahead of grace. |
| 7 | Fail-stop on corruption | Confirm the substrate refuses to spawn on bad state (don't override). |

Each section below walks through the verification.

---

## Invariant 1: WAL-before-acknowledge

**The promise.** A successful `encode` ack means the
WAL record is fsync'd to disk. Power loss can't lose
the memory.

### Quick check

```bash
brain-cli admin wal-tail --count 10
```

Output should show the last 10 WAL records with their
LSNs, types, and CRCs. The most recent records should
be valid (CRC matches the stored CRC).

```json
[
  {
    "lsn": 4271832,
    "record_type": "Encode",
    "timestamp_unix_nanos": 1726145482000000000,
    "crc_valid": true
  },
  ...
]
```

### What to look for

- **`crc_valid: false` anywhere in the tail.** This is
  a CRC mismatch — torn write or corruption. Open
  [RB-07](rb-07-corruption-recovery.md).
- **LSN gaps.** LSNs should be strictly monotonic.
  Gaps suggest the recovery driver applied something
  out of order or skipped records.
- **A record older than the last acked operation.**
  The most recent record's timestamp should be very
  recent (microseconds-to-seconds). A stale tail with
  a recent ack suggests the WAL writer is stuck.

### Detailed check: WAL fsync rate

```promql
rate(brain_wal_fsync_total[5m])
histogram_quantile(0.99, sum by (le) (rate(brain_wal_fsync_duration_ms_bucket[5m])))
```

Healthy: fsync rate roughly matches encode rate;
fsync p99 latency under ~200 ms on commodity NVMe.

Unhealthy:

- Fsync rate dropping to zero while writes continue
  → encode is queueing; ack happened but disk didn't
  catch up. Investigate.
- Fsync latency spiking → underlying disk or
  filesystem issue.

---

## Invariant 2: Single writer per shard

**The promise.** Within one shard, only one task is
writing to its data structures at a time. Enforced by
the Glommio single-threaded executor.

### Verify only one process per data dir

```bash
ps auxf | grep brain-server | grep -v grep
lsof +D /var/lib/brain/data 2>/dev/null | awk '{print $2}' | sort -u
```

Expected: one PID for the substrate process. Multiple
PIDs accessing the same data directory means **stop
immediately**; that combination corrupts. Two processes
sharing a `data_dir` is the most common cause of
silent corruption.

Common failure modes:

- A second `brain-server` started while the first
  was still running (config bug or operator error).
- A Docker container restarted but the old container
  didn't fully exit.
- A previous `brain-server` process is zombied —
  `kill -9`'d but file descriptors still held by a
  child.

If two processes are touching the data dir, stop both
and start one cleanly. If you have any reason to
believe the data is corrupted, open
[RB-07](rb-07-corruption-recovery.md).

### Verify shard-to-thread mapping

```bash
brain-cli admin shards
```

Each shard has a `thread_id`. They should all be
distinct. If two shards share a thread, that's a
configuration error (shouldn't be possible with
Brain's normal startup, but worth noting).

---

## Invariant 3: CRC everywhere

**The promise.** Every WAL record and every arena
slot carries a CRC32C. The substrate verifies on read.

### Sweep the WAL

```bash
brain-cli admin verify-wal --shard <id>
```

Output:

```
Verifying shard 0 WAL...
  segments: 4
  records:  1,247,832
  crc_errors: 0
  truncations: 0
  duration: 12.3s
OK
```

Run on each shard. Any non-zero `crc_errors` is a
P1 problem; open [RB-07](rb-07-corruption-recovery.md).

`truncations` at the very end of the most recent
segment is normal (the substrate appends and may
have been interrupted between flushes). Truncations
in the middle of segments are not normal.

### Sweep the arena

```bash
brain-cli admin verify-arena --shard <id>
```

Same shape. Verifies the arena header CRC, then every
non-tombstoned slot's `metadata_crc32c`. Tombstoned
slots are skipped (their bytes are intentionally not
guaranteed to be coherent after hard-forget).

This is slower than the WAL sweep — proportional to
shard size. For a 1M-memory shard, expect minutes.

Run during off-peak hours or on a snapshot copy if
production CPU matters. The admin CLI supports
`--read-only-snapshot <id>` to verify a snapshot copy
without touching the live data.

---

## Invariant 4: `MemoryId` slot version

**The promise.** Every `MemoryId` encodes the version
of its slot at issue time. A reclaimed slot has a
different version; stale `MemoryId`s safely return
`NotFound`.

### Spot-check

This is harder to verify in bulk; the easiest check
is a contrived test:

```bash
# Encode a memory.
MID=$(brain-cli encode "test memory for verification" \
    | jq -r '.memory_id')

# Confirm it's recallable.
brain-cli get "$MID"   # → success

# Forget it; wait past the grace period (default 7 days).
# In a test environment, you can shorten this via config.
brain-cli forget --hard "$MID"

# After grace, try the original MemoryId.
brain-cli get "$MID"   # → MemoryNotFound, NOT a stale read
```

The final `get` returning `NotFound` (not "wrong
memory") is the proof of correct slot-version
discipline.

For production: trust the unit tests. The substrate's
test suite exercises this invariant extensively;
operator-side verification isn't normally needed
unless you suspect a specific bug.

### Audit-log spot check

If your deployment runs the audit log:

```bash
brain-cli admin audit search --filter 'event=MemoryIdMismatch'
```

A non-zero result means somewhere a stale `MemoryId`
was caught by the version check — exactly what the
invariant promises. Good signal that the mechanism is
working, not a problem.

---

## Invariant 5: Idempotency by `request_id`

**The promise.** Retrying a state-mutating op with
the same `request_id` returns the cached response
(or `IdempotencyConflict` for different params), not
a duplicate effect.

### Verify the idempotency table exists and is pruning

```bash
brain-cli admin idempotency stats
```

Output:

```
shard 0:
  rows: 47,213
  oldest: 2024-09-10T14:33:01Z (49h ago)
  newest: 2024-09-12T15:31:22Z (0h ago)
  last_prune: 2024-09-12T13:00:00Z
```

Key checks:

- **`oldest`** should not be more than ~24h old. The
  TTL is 24h by default; anything older means the
  pruner is stuck.
- **`last_prune`** should be recent (within the last
  hour). If stale, the `idempotency_cleanup` worker is
  paused or broken.

### Test the idempotency contract

```bash
RID=$(uuidgen)

# First call.
brain-cli encode "test" --request-id "$RID"
# → MemoryId mem_001

# Same request_id, same params.
brain-cli encode "test" --request-id "$RID"
# → Returns mem_001 (replay).

# Same request_id, different params.
brain-cli encode "different" --request-id "$RID"
# → IdempotencyConflict.
```

The three outcomes (success, replay, conflict) confirm
the contract holds.

---

## Invariant 6: Tombstone grace before reclamation

**The promise.** Forgotten memories' slots aren't
reclaimed until a configurable grace period (default
7 days) has elapsed.

### Verify the reclaim worker isn't running ahead

```bash
brain-cli admin worker show slot_reclamation
```

Output:

```
worker: slot_reclamation
status: running
last_run: 2024-09-12T15:00:00Z
last_processed: 1,247 slots
grace_period_days: 7
oldest_tombstone_reclaimed_age_days: 7.3
```

Key check: `oldest_tombstone_reclaimed_age_days`
should be **at least** `grace_period_days`. If it's
less, the worker is reclaiming too early —
configuration or substrate bug.

A safer cross-check:

```bash
brain-cli admin slots --shard 0 --status tombstoned \
    | jq '.[] | .tombstoned_at' \
    | sort | head -1
```

The oldest tombstoned-but-not-reclaimed slot. Should
be less than `grace_period_days` ago.

### What can go wrong

- **Grace period misconfigured.** Operator set
  `grace_period_days = 0` thinking it disables the
  worker. It actually means "reclaim immediately,"
  which violates the invariant.
- **Worker is paused.** Tombstones accumulate, but
  none are reclaimed. Eventually disk fills. Open
  [RB-04](rb-04-disk-filling.md).
- **Worker is buggy.** Reclaiming the wrong slots.
  Very rare; if you suspect this, escalate.

---

## Invariant 7: Fail-stop on corruption

**The promise.** If the substrate detects internal
inconsistency, it refuses to operate. It does not
serve potentially-wrong data.

### What to look for

This invariant is mostly about *not overriding it*.
When the substrate refuses to start because of a
recovery error or a CRC mismatch, the operator's job
is to **respect the refusal** and restore from
snapshot — not force a restart.

### Verify the substrate stays down on bad state

A controlled test (do this in a dev/staging
environment only):

```bash
# Stop substrate.
sudo systemctl stop brain-server

# Corrupt the arena header.
sudo dd if=/dev/urandom of=/var/lib/brain/data/shard-0/arena.bin \
    bs=1 count=64 conv=notrunc

# Start. Should refuse.
sudo systemctl start brain-server

# Expected: process exits with `ArenaOpenError: header CRC mismatch`.
sudo journalctl -u brain-server --since "1 min ago"
```

If the substrate *did* start despite the corruption,
the invariant is violated — escalate immediately to
engineering.

### In production

You don't manufacture failures in production. But if
you encounter a substrate that won't start with a
"corruption detected" message:

- **Don't force-restart.** Follow
  [RB-01](rb-01-substrate-down.md).
- **Don't edit the data files to make it start.**
  That's converting a recoverable incident into an
  unrecoverable one.
- **Restore from snapshot.** That's the supported
  path.

---

## Putting it together: the full sweep

A periodic verification routine, ideally monthly:

```bash
#!/usr/bin/env bash
# brain-invariant-sweep.sh
set -euo pipefail

ADMIN_ADDR="${BRAIN_ADMIN_ADDR:-127.0.0.1:9092}"

echo "=== Invariant 1: WAL tail ==="
brain-cli --addr "$ADMIN_ADDR" admin wal-tail --count 10

echo "=== Invariant 2: process check ==="
ps auxf | grep brain-server | grep -v grep

echo "=== Invariant 3: WAL CRC sweep ==="
for SHARD in $(brain-cli --addr "$ADMIN_ADDR" admin shards | jq -r '.[].id'); do
    brain-cli --addr "$ADMIN_ADDR" admin verify-wal --shard "$SHARD"
done

echo "=== Invariant 3 (cont): arena CRC sweep ==="
for SHARD in $(brain-cli --addr "$ADMIN_ADDR" admin shards | jq -r '.[].id'); do
    brain-cli --addr "$ADMIN_ADDR" admin verify-arena --shard "$SHARD"
done

echo "=== Invariant 5: idempotency ==="
brain-cli --addr "$ADMIN_ADDR" admin idempotency stats

echo "=== Invariant 6: reclaim worker ==="
brain-cli --addr "$ADMIN_ADDR" admin worker show slot_reclamation

echo "=== Sweep complete. Review output for anomalies. ==="
```

Schedule via cron during off-peak hours. The arena
sweep can be slow; consider running it less
frequently than the others, or on a snapshot copy.

---

## When a verification fails

| Failed invariant | Action |
|---|---|
| 1 — WAL tail anomalies | [RB-07](rb-07-corruption-recovery.md) |
| 2 — Multiple processes per data dir | Stop both; investigate; restart one cleanly. |
| 3 — CRC mismatch in WAL or arena | [RB-07](rb-07-corruption-recovery.md); P1. |
| 4 — Stale `MemoryId` returns wrong data | Escalate; P1 substrate bug. |
| 5 — Idempotency table not pruning | [RB-05](rb-05-worker-stuck.md). |
| 6 — Reclaim running ahead of grace | Pause worker; check config; escalate if config is correct. |
| 7 — Substrate accepted known-bad state | Escalate; P1 substrate bug. |

Each row maps to a runbook or an escalation path. The
ones that say "escalate; P1 substrate bug" are the
*serious* ones — they indicate the substrate isn't
keeping its promises. Don't proceed without
engineering involvement.

---

## Related runbooks

- [DR-01 — Diagnostic bundle](dr-01-diagnostic-bundle.md)
- [DR-03 — Safe production access](dr-03-safe-production-access.md)
- [RB-04 — Disk filling](rb-04-disk-filling.md)
- [RB-05 — Worker stuck](rb-05-worker-stuck.md)
- [RB-07 — Recovery from corruption](rb-07-corruption-recovery.md)
- [Concepts: invariants and the trust model](../concepts/24-invariants-and-trust.md)

---

## Last validated

*Update on first use.*
