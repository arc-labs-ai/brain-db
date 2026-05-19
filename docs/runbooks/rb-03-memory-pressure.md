# RB-03: Memory pressure / OOM

**Severity:** **P2** (P1 if the OOM-killer has already
fired or is imminent).
**Alert:** `BrainHighMemoryPressure`.
**SLO impact:** latency degraded under pressure;
imminent crash if the OOM killer fires.
**Estimated duration:** 30 minutes to 2 hours; longer
if the fix is "add more RAM" and you have to wait
for a cloud resize.
**Skill level:** comfortable reading `top` / `free`,
familiar with the per-shard memory model.

The brain-server process is using too much RAM,
threatening either degraded performance (swap, OOM
killer) or an outright crash.

---

## Am I in the right runbook?

You should see one or more of these:

- `process_resident_memory_bytes{job="brain"}` close
  to the host's total RAM.
- `mem_available` (host-level) trending toward zero.
- Swap usage rising (if swap is configured; ideally
  it isn't for Brain).
- `dmesg` shows recent OOM events
  (`Out of memory: Killed process ...`).
- Brain processes have been restarted by the OOM
  killer (visible as restart count climbing without
  matching deploys).

If the substrate **was killed by OOM and is now
down**, you're in [RB-01](rb-01-substrate-down.md) for
restart (with this runbook for prevention). If you're
seeing memory issues plus latency, both this and
[RB-02](rb-02-high-latency.md) are relevant.

---

## Stop the bleeding

Memory pressure can escalate to a crash quickly.
Take immediate defensive action:

1. Acknowledge the page; open the incident channel.
2. Capture a diagnostic bundle
   ([DR-01](dr-01-diagnostic-bundle.md)) **now**,
   before any action that might lose the in-process
   state.
3. **Check the trend.** Is RSS climbing, or stable
   at a high level?

   ```promql
   rate(process_resident_memory_bytes{job="brain"}[5m])
   ```

   - **Climbing:** active leak or growing working set.
     Limited time before OOM. Continue to step 4.
   - **Stable but high:** the substrate is using the
     RAM it allocated, not leaking. More breathing
     room; jump to Diagnose.

4. **If RSS is within 5 % of host RAM**, consider
   **proactive restart** to avoid the OOM killer. A
   controlled restart loses less state than an
   OOM-kill (which is `SIGKILL` — no clean WAL flush,
   no draining):

   ```bash
   sudo systemctl restart brain-server
   ```

   Yes, this is briefly worse for users (recovery
   takes 30 s - 2 min depending on shard size); but
   it's better than the OOM-killer's `SIGKILL`. Post
   in the channel before you do it.

   **Don't** restart if RSS has plenty of headroom
   (> 20 % free); diagnose first.

---

## Diagnose

### 1. What's using the memory?

A brain-server process's memory breaks down roughly:

- **HNSW indexes** — usually the biggest single
  consumer. Scales with `node_count × bytes_per_node`
  (~1-2 KB per node).
- **Arena memory-map** — `arena.bin` is mmap'd, but
  only the actively-touched pages count toward RSS.
  Working-set sized.
- **Embedder model** — ~130 MB for BGE-small.
  Constant.
- **Embedder cache** — `cache_size × ~1.5 KB`.
  Typically 10-30 MB.
- **redb metadata** — the redb file is mmap'd too;
  active pages count.
- **Per-shard Glommio runtime + buffers** — ~10-50 MB
  per shard.
- **Tantivy indexes** (knowledge-active mode) —
  variable; can be substantial.

The Prometheus breakdown:

```promql
process_resident_memory_bytes{job="brain"}
sum by (shard_id) (brain_hnsw_node_count) * 1500   # rough estimate
brain_arena_bytes_resident                          # what's hot in arena
brain_metadata_db_bytes_resident                    # redb resident
brain_embedder_cache_bytes                          # cache size
```

Sum the breakdown. If it's close to RSS, no leak —
the substrate is just *big*. If RSS exceeds the
breakdown significantly, look for a leak.

### 2. Per-shard breakdown

```promql
sum by (shard_id) (brain_hnsw_node_count) * 1500
```

If one shard is much bigger than others, that's
your hot shard. Three reasons:

- **Skewed shard count** — an agent (or set of
  agents) has accumulated disproportionate
  memories. Usually fine; just means that shard's
  RSS is high.
- **Tombstones** — high tombstone ratio means HNSW
  is bigger than its live-node count would suggest.
  Cross-check with `brain_hnsw_tombstone_ratio`.
- **Recent backfill** — a recent extractor backfill
  may have spiked memory; might be transient.

### 3. Leak detection

If RSS climbs steadily while load is constant:

```promql
# RSS slope.
deriv(process_resident_memory_bytes{job="brain"}[1h])

# If slope is steadily positive (e.g. +10 MB/min)
# and not driven by data growth, you have a leak.
```

Common leak shapes:

- **Embedder cache growing without bound** —
  shouldn't happen (`cache_size` caps it), but a
  configuration bug or recent change might disable
  the cap.
- **Idempotency table not pruning** — every
  state-mutating request adds a row; if the cleanup
  worker is paused, the table grows. Check
  `brain_idempotency_rows`.
- **Tantivy index growing without merge** —
  knowledge-layer-specific. Should be caught by the
  text-indexer worker.
- **HNSW rebuild churn** — old indexes not being
  freed after rebuild. Substrate bug; escalate.

For deeper analysis, run heaptrack
([DR-04](dr-04-profiles-and-heap-dumps.md)) on a
staging copy of the workload. Not on production hot
shards.

### 4. Host-level pressure

```bash
free -h
cat /proc/meminfo | head -20
vmstat 1 5
```

Look for:

- **`MemAvailable` close to zero** → host is out of
  memory; OOM imminent.
- **`SwapTotal > 0` and `SwapUsed > 0`** → the
  system is swapping. Bad for Brain — random latency
  spikes. Either turn off swap (preferred) or accept
  the latency.
- **`si`/`so` columns in vmstat non-zero** → active
  swap-in/out. Confirms swap is in use.

### 5. Worker memory

A few workers can balloon memory during their
cycles:

- **`hnsw_maintenance`** holds two HNSWes during
  rebuild (old + new) until the swap. Briefly
  doubles index RAM.
- **`snapshot`** copies arena and metadata files; can
  bump page-cache pressure briefly.
- **`backfill`** (knowledge-layer) iterates all
  memories; could pull cold pages into cache.

Check:

```bash
brain-cli admin workers --running
```

If a heavy worker is in flight, the memory peak may
be transient. Wait it out before assuming a leak.

---

## Remediate

### Quick mitigations

Buy yourself headroom while you diagnose:

```bash
# Drop the embedder cache (will repopulate gradually).
brain-cli admin embedder-cache clear

# Pause heavy workers.
brain-cli admin worker pause hnsw_maintenance
brain-cli admin worker pause consolidation

# Drop OS-level page cache (last resort; reverts on its own).
sync
sudo sysctl -w vm.drop_caches=3
```

These don't fix the root cause; they buy 15-30
minutes. Use them only if RSS is dangerously high.
Remember to unpause workers once the incident is
contained.

### Shrink memory footprint

Configuration knobs that reduce RAM:

```toml
[embedder]
cache_size = 1000              # was 10000; smaller cache
batch_size = 8                 # was 32; smaller embed batches

[shard]
arena_initial_capacity_slots = 65536    # smaller initial mmap

[knowledge]
llm_cache.cap_bytes = "2GiB"   # was 10GiB

[server]
max_payload_bytes = 4194304    # smaller request buffer
```

Roll the config out ([OP-08](op-08-config-change-rollout.md)).
Test in staging first; verify the shrunk setup
actually fits and that performance doesn't degrade
unacceptably.

### Reduce shard count if too many

If you have `shard_count = 32` on an 8-core host with
16 GB RAM, you're sized wrong. Each shard's Glommio
runtime + cold caches consumes RAM whether the
shard is active or not.

Right-size by either:

- **Reducing shard count** — requires data migration
  (export → reimport into the new layout). Not in-
  incident.
- **Adding RAM** — vertical scale. Cloud resize or
  hardware swap.

### Restart to clear transient growth

If the substrate just needs a clean state (e.g.,
fragmentation from long uptime):

```bash
sudo systemctl restart brain-server
```

Resets all RAM-only structures. Comes back at a clean
baseline. Lose 30 s - 2 min of availability.

If a single shard is growing and others are fine,
some deployments support per-shard restart (kill the
shard's thread and let the supervisor respawn). If
yours doesn't, full-process restart is the only
option.

### Scale up the host

Cloud resize, hardware swap, etc. Out of scope for
this runbook; refer to your deployment guide.

While waiting on the resize, keep the runbook's
quick-mitigations applied.

---

## Verify

After remediation:

```promql
# RSS settled at a sustainable level.
process_resident_memory_bytes{job="brain"}

# Trend over the next 30 min is flat or declining.
deriv(process_resident_memory_bytes{job="brain"}[15m])
```

Wait 30 minutes after the fix to declare
victory — slow leaks need that long to show whether
they're still leaking.

Also verify the substrate is functional, not just
small:

```bash
brain-cli encode "memory smoke test $(date +%s)"
brain-cli recall "smoke test"
```

The `BrainHighMemoryPressure` alert clears once
process RSS is back under threshold for the alert's
window.

---

## Post-incident

```
:white_check_mark: Resolved at HH:MM UTC.
Root cause: <e.g., embedder cache config bug; cache_size=0 disabled cap>.
Remediation: <e.g., corrected config, restart>.
Follow-up: TICKET-NNNN (cache bound regression test).
Postmortem: <yes/no>.
```

Postmortem rule for RB-03:

- **Always** if the OOM killer fired (P1).
- **Usually** for sustained pressure events (P2).
- **Sometimes** for transient spikes that resolved
  on their own — worth writing if you learned
  something.

---

## Prevention

- **Monitor RSS trend, not just peak.** A 30-day RSS
  trend that's monotonically rising is the early
  warning a leak gives you.
- **Set conservative `cache_size`** at first; raise
  only if you have cache-miss data justifying it.
- **Don't enable swap.** Brain expects RAM access
  latencies; swapping to disk produces unpredictable
  latency tail. Better to OOM clean than to swap
  silently.
- **Right-size per-shard memory at deployment.** As
  a rough rule:
  - `~1.5 KB per memory` for HNSW.
  - `~100-500 MB` per shard for runtime overhead.
  - 1-2 GB headroom per shard for caches and
    workers.
  - So a 5M-memory shard wants ~9-10 GB.
- **Regular soak tests.** A 24-hour soak with a
  representative workload exposes memory leaks that
  short tests don't.

---

## Related runbooks

- [RB-01 — Substrate doesn't start](rb-01-substrate-down.md) (if OOM killed it)
- [RB-02 — High latency](rb-02-high-latency.md) (often co-occurs)
- [RB-04 — Disk filling](rb-04-disk-filling.md) (also a resource exhaustion)
- [RB-12 — Restart loop](rb-12-restart-loop.md) (if it OOMs on start)
- [DR-04 — Profiles and heap dumps](dr-04-profiles-and-heap-dumps.md)
- [OP-08 — Configuration change rollout](op-08-config-change-rollout.md)

---

## Last validated

*Update on first use.*
