# RB-09: Mass FORGET aftermath

**Severity:** **P2** typically.
**Alert:** `BrainHighTombstoneRatio`.
**SLO impact:** recall quality degrades. Disk usage
grows transiently. Latency may rise on affected
shards.
**Estimated duration:** 30 minutes to a few hours
(rebuild + reclamation).
**Skill level:** comfortable with the HNSW rebuild
flow and the tombstone-reclamation lifecycle.

A batch of FORGET operations (compliance cleanup,
data retention sweep, agent decommission) has driven
the tombstone ratio above a threshold. The
substrate is otherwise healthy but the HNSW is bloated
with dead nodes.

---

## Am I in the right runbook?

You should see:

- `brain_hnsw_tombstone_ratio` above 0.20 on at least
  one shard.
- A recent spike in FORGET rate
  (`rate(brain_forget_total[1h])` showed a peak in
  the last 24 hours).
- Recall on affected shards starting to slow or
  return lower-quality results.

This runbook is a **specific case** of
[RB-06](rb-06-recall-degraded.md) where the cause is
known to be mass FORGETs. If the tombstone rise is
gradual (slow accumulation over weeks), RB-06's
broader scope is more appropriate.

If you're *worried about* a mass FORGET that
hasn't happened yet — that's not an incident; it's
planning. See the Prevention section.

---

## Stop the bleeding

There's no immediate bleeding; mass FORGET aftermath
is a slow degradation. Take these steps:

1. Acknowledge the page; open the incident channel.
2. Capture a diagnostic bundle ([DR-01](dr-01-diagnostic-bundle.md)).
3. **Confirm the scope.** Was this a planned FORGET
   batch (compliance sweep) or unexpected?

   ```bash
   # Recent FORGET rate by shard.
   ```

   ```promql
   sum by (shard_id) (rate(brain_forget_total[24h]))
   ```

   - **Planned:** continue with the runbook. The
     remediation is to clean up after the planned
     operation.
   - **Unexpected:** before remediating, identify
     *who* did the FORGETs. A rogue script or an
     attacker exfiltrating data could be the cause;
     remediation without root cause = next-day
     repeat.

---

## Diagnose

### 1. How big is the tombstone backlog?

```promql
brain_hnsw_tombstone_count{shard_id="<n>"}
brain_hnsw_node_count{shard_id="<n>"}
brain_hnsw_tombstone_ratio{shard_id="<n>"}
```

The ratio tells you the magnitude. Different
ranges call for different responses:

- **0.20-0.30:** borderline. Auto-rebuild may handle
  it within the next cycle; verify worker is
  healthy.
- **0.30-0.50:** rebuild recommended. Recall is
  measurably degraded.
- **>0.50:** rebuild urgent. Recall quality is
  significantly impaired.

### 2. Which shards are affected?

Mass FORGETs from a single agent affect one shard
(per-agent isolation, chapter 23 of concepts). A
broad sweep affects all shards. Identify scope:

```promql
sort_desc(brain_hnsw_tombstone_ratio)
```

- **One shard hot:** easy — rebuild that shard.
- **All shards hot:** roll a rebuild across them.
  Schedule for low-traffic windows if possible.

### 3. How much disk is "stuck" in tombstones?

Tombstoned slots in the arena take up space until
the slot is reclaimed. After the FORGET's grace
period (default 7 days), the `slot_reclamation`
worker frees them for reuse — but the arena file
itself doesn't shrink.

```bash
brain-cli admin shard <n> | jq '{
    live_memories,
    tombstoned_memories,
    arena_bytes,
    arena_capacity_bytes
}'
```

If `arena_bytes` is near the capacity and most of
it is tombstoned, the slots will be reused once
reclamation completes — no immediate disk action
needed. If the arena is also close to full from
*live* memories, then growth + reclamation race;
might need to extend disk.

### 4. Is the rebuild worker healthy?

The `hnsw_maintenance` worker normally catches
tombstone-ratio spikes automatically. If you're in
this runbook, it didn't act fast enough or it's
broken.

```bash
brain-cli admin worker show hnsw_maintenance
```

If paused, broken, or behind schedule, see
[RB-05](rb-05-worker-stuck.md). Fix the worker
*and* trigger a manual rebuild.

### 5. Is the reclamation worker healthy?

The `slot_reclamation` worker reclaims tombstoned
slots after the grace period. If it's broken, the
arena keeps growing (new encodes get fresh slots
instead of reusing reclaimed ones).

```bash
brain-cli admin worker show slot_reclamation
```

Same diagnosis as above; fix if broken.

### 6. Was the FORGET intentional?

Cross-check with whoever might have done the FORGET:

- Compliance / privacy team running a deletion
  request batch.
- Data retention policy automatically forgetting
  memories beyond an age.
- A migration / re-extraction process.

If no one owns the FORGET burst and you can't
identify a legitimate cause, treat as security
event:

- Escalate to security.
- Audit the source of the FORGETs (admin RPC logs,
  client IP, agent token).
- Don't reclaim the slots yet — hard-forget hasn't
  zeroed them, and forensic recovery may be needed.

---

## Remediate

### Trigger HNSW rebuild

The fast path. Rebuilds the index on the affected
shard, dropping tombstoned nodes:

```bash
brain-cli rebuild-ann --shard <n>
```

For multiple shards:

```bash
for SHARD in 0 1 2 3; do
    brain-cli rebuild-ann --shard "$SHARD"
done
```

Rebuilds take ~30 seconds per 1M nodes. During the
rebuild, latency on the affected shard rises (the
worker uses CPU). Plan accordingly — low-traffic
window if possible.

After rebuild:

- `tombstone_ratio` resets to 0.
- `node_count` drops to live-memory count.
- Recall quality recovers within a few minutes.

### Wait for natural reclamation

If the tombstones aren't yet eligible for
reclamation (grace period not elapsed), they stay
in the arena. The HNSW rebuild *handles the recall
quality issue*; the disk-space side resolves when
the grace period elapses and the reclamation worker
runs.

For most deployments, this resolves itself over
the following 7 days (default grace period). No
operator action needed beyond monitoring.

### Force reclamation (if appropriate)

If you need slots back faster than the grace
period:

```bash
brain-cli admin shrink-grace-period --shard <n> --days 0
brain-cli admin worker run-now slot_reclamation
```

**Warning:** the grace period exists because
clients may still hold `MemoryId`s for the
forgotten memories. Shrinking it to 0 risks those
clients getting wrong-content reads
(see chapter 19 of concepts — the slot-version
check protects against this, but the experience is
"my MemoryId stopped working"). Only do this for:

- A test environment.
- A cleanup where you know no clients hold stale
  `MemoryId`s.
- A privacy-driven forget where immediate slot
  reuse is acceptable.

Restore the grace period afterwards.

### Hard-forget if not done already

If the FORGETs were *soft* (default) and you need
the data genuinely gone (not just hidden):

```bash
# For specific memories:
brain-cli forget --hard <memory_id>

# Or batch-hard-forget by filter:
brain-cli forget --hard --filter "agent_id=<id>"
```

Hard-forget zeros the slot's vector and text bytes
immediately, *before* the grace period. The slot
still exists; reclamation reuses it later.

This is the path for privacy-compliance forgets.

### Pace the rebuild

If rebuilding all shards at once would cause too
much load:

```bash
for SHARD in $(brain-cli admin shards | jq -r '.[].id'); do
    brain-cli rebuild-ann --shard "$SHARD"
    sleep 600    # 10 min between shards
done
```

Each shard rebuild affects only that shard's
latency; staggering them avoids saturating the
host.

---

## Verify

```promql
# Tombstone ratio dropped.
brain_hnsw_tombstone_ratio < 0.10

# Recall quality recovered.
brain_hnsw_recall_at_k_estimate > 0.92
```

Smoke test:

```bash
brain-cli recall "known cue with expected results"
```

The result quality should be back to normal.

Disk-side, monitor:

```bash
brain-cli admin shard <n> | jq '.tombstoned_memories'
```

If this drops over the following 7 days (grace
period), the natural reclamation is working.

---

## Post-incident

```
:white_check_mark: Resolved at HH:MM UTC.
Trigger: <e.g., compliance team ran 50K FORGET requests at HH:MM>.
Tombstone ratio at peak: <e.g., 0.41>.
Remediation: rebuilt HNSW on shards 0, 1, 2.
Recall quality recovery: complete.
Disk reclamation: in progress (slot grace period).
Follow-up: TICKET-NNNN (auto-rebuild threshold; coordinate with
compliance ops).
Postmortem: <yes/no>.
```

Postmortem rule for RB-09:

- **Yes** if the FORGET wasn't intentional (security
  case).
- **Sometimes** if the operations team didn't know
  the compliance batch was coming (coordination gap
  worth documenting).
- **Skip** if it was an expected operation that
  triggered the documented runbook cleanly.

---

## Prevention

The pattern: a FORGET burst → tombstone accumulation
→ recall degradation. Best preventions are:

- **Pre-rebuild after planned FORGET batches.**
  If you're going to do a large compliance sweep,
  schedule a `rebuild-ann` immediately after. No
  alert needs to fire.
- **Lower the auto-rebuild threshold.** Default may
  be 0.40; for workloads that see frequent
  FORGETs, 0.20 or 0.25 is safer.
- **Run the FORGET sweep gradually.** A FORGET rate
  of 1/sec is much easier to handle than 1000/sec
  in a single batch. Pace your compliance scripts.
- **Coordinate FORGET batches with the operations
  team.** Compliance shouldn't be silently
  triggering quality alerts.
- **Monitor FORGET-then-tombstone-rise** as a
  composite alert. The lead indicator catches
  problems earlier than the lagging tombstone-ratio
  alert.

---

## Related runbooks

- [RB-04 — Disk filling](rb-04-disk-filling.md) (if
  arena growth is also a problem)
- [RB-05 — Worker stuck](rb-05-worker-stuck.md) (if
  `hnsw_maintenance` or `slot_reclamation` is
  broken)
- [RB-06 — Recall degraded](rb-06-recall-degraded.md)
  (more general version of this runbook)

---

## Last validated

*Update on first use.*
