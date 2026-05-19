# RB-02: High latency on a shard

**Severity:** **P2** (P1 if user-visible across most shards).
**Alert:** `BrainHighLatency`.
**SLO impact:** elevated response times for a fraction of users; some requests may time out.
**Estimated duration:** 15 minutes to 90 minutes depending on root cause.
**Skill level:** comfortable reading Prometheus / Grafana, familiar with the substrate's per-shard model.

p99 latency is above SLO target on one or more shards
for >10 min. The substrate is serving, just slowly.

---

## Am I in the right runbook?

You should see:

- `brain_request_duration_ms` p99 above SLO (default
  100 ms for recall) for at least 10 minutes.
- The substrate is responding — clients are getting
  answers, just late.
- Not all operations are necessarily affected;
  often one specific op type is the offender.

If the substrate **isn't responding at all** to recent
requests, that's [RB-08](rb-08-unresponsive.md).
If the substrate **won't start**, that's [RB-01](rb-01-substrate-down.md).
If recall results are **wrong** (low quality), that's
[RB-06](rb-06-recall-degraded.md).

---

## Stop the bleeding

Latency is a user-visible problem but rarely a
data-integrity problem. You don't need to do anything
emergency before diagnosing. **Don't restart the
substrate at this stage** — restart drops the
embedder cache and the HNSW state, often making
latency worse for 5-15 minutes after.

Do these in parallel:

1. Acknowledge the page; open the incident channel.
2. Capture a diagnostic bundle ([DR-01](dr-01-diagnostic-bundle.md)).
3. **Identify the affected shard(s) immediately**, so
   you can scope the rest of the diagnosis:

   ```promql
   topk(3,
     histogram_quantile(0.99,
       sum by (shard_id, le) (rate(brain_request_duration_ms_bucket[5m]))))
   ```

   The top results are the shards in trouble.

---

## Diagnose

### 1. Which operation is slow?

Decompose by op:

```promql
histogram_quantile(0.99,
  sum by (op, le) (rate(brain_request_duration_ms_bucket[5m])))
```

Look at the per-op breakdown. Usually one operation
dominates:

- **`recall` slow:** vector search bottleneck. Continue
  to step 2 (HNSW health) and step 4 (embedder).
- **`encode` slow:** write path bottleneck. Continue
  to step 3 (WAL) and step 5 (HNSW insert).
- **`query` slow** (knowledge-active mode): hybrid
  retriever issue. Steps 2, 4, plus
  [RB-06](rb-06-recall-degraded.md).
- **All ops slow:** resource exhaustion (step 6) or
  worker contention (step 7).
- **Admin / health endpoints slow:** the substrate's
  control plane is busy. Likely background-worker
  contention (step 7).

### 2. HNSW health

If recall or query is slow:

```promql
brain_hnsw_tombstone_ratio{shard_id="<n>"}
brain_hnsw_node_count{shard_id="<n>"}
brain_hnsw_last_rebuild_unixtime{shard_id="<n>"}
```

Look at:

- **`tombstone_ratio > 0.30`** → too many forgotten
  memories cluttering the graph. Recall walks dead
  nodes, slowing down. This is a common cause.
  Proceed to *Remediate → Rebuild HNSW*.
- **`tombstone_ratio` normal but `node_count` very
  large** → shard is just big. Latency scales with
  log(N); a 50M-node shard is going to be slower
  than a 1M-node shard regardless of health.
  Long-term solution is more shards.
- **`last_rebuild` recent** but latency still bad →
  the rebuild didn't help; look elsewhere.

### 3. WAL health

If encode is slow:

```promql
histogram_quantile(0.99,
  sum by (le) (rate(brain_wal_fsync_duration_ms_bucket{shard_id="<n>"}[5m])))
brain_wal_bytes_pending{shard_id="<n>"}
rate(brain_wal_fsync_total{shard_id="<n>"}[5m])
```

Read:

- **fsync p99 > 200 ms** → underlying disk is slow.
  Step 6 (resource check). Common in cloud
  environments where disk performance is throttled.
- **`bytes_pending` rising without bound** → WAL is
  backing up; flushes can't keep up with writes.
  Disk or filesystem problem.
- **fsync rate dropping but writes continue** → the
  WAL writer is stuck. Look for an error in the logs;
  if recent ERRORs mention WAL, escalate.

### 4. Embedder bottleneck

If any operation that needs an embedding is slow:

```promql
histogram_quantile(0.99,
  sum by (le) (rate(brain_embed_duration_ms_bucket[5m])))
rate(brain_embed_cache_hits_total[5m])
rate(brain_embed_cache_misses_total[5m])
```

Look at:

- **embed p99 > 50 ms** → embedder is slow. Could be
  CPU contention (step 6), tokeniser issues, or just
  cold cache.
- **cache hit rate dropping** → the workload's text
  diversity is wider than the cache can hold.
  Increasing cache size helps; bigger workload may
  need a bigger cache or GPU.
- **cache hit rate near zero** → either the cache is
  disabled, or the workload genuinely has no
  repetition. Different remediation than the above.

### 5. HNSW insert

If encode is slow but WAL is healthy:

```promql
histogram_quantile(0.99,
  sum by (le) (rate(brain_hnsw_insert_duration_ms_bucket{shard_id="<n>"}[5m])))
```

p99 over 50 ms on inserts is unusual. The most
common cause is the HNSW being mid-rebuild (the
maintenance worker is consuming CPU). Check:

```bash
brain-cli admin worker show hnsw_maintenance
```

If `running` and at high progress %, wait it out
(rebuild finishing improves latency).

### 6. Resource exhaustion

A general check, regardless of which op:

```bash
# CPU
top -bn1 -d 1 | head -20

# Memory
free -h
ps aux | grep brain-server | awk '{print $4, $6, $11}'

# Disk I/O (specifically on the data device)
iostat -x 1 5 | grep -E 'Device|sd[a-z]|nvme|md'

# Disk free
df -h /var/lib/brain
```

Look for:

- **One core pegged at 100 %.** Common — Brain is
  thread-per-core per shard. Use `top -H -p $(pgrep
  brain-server)` to see per-thread CPU. The thread
  named `brain-shard-<n>` is your offender.
- **RSS near or above host RAM** → memory pressure,
  see [RB-03](rb-03-memory-pressure.md).
- **Disk `%util` > 80** → I/O saturation. The disk is
  the bottleneck; latency on encode (and recall, on
  cache miss) will track it.
- **Disk space low** (< 20 % free) → could be
  triggering filesystem-level slowness. See [RB-04](rb-04-disk-filling.md).

### 7. Worker contention

Brain workers (decay, consolidation, snapshot, …) run
on the same shard executors as request handlers. A
heavy worker can contend with serving:

```bash
brain-cli admin workers
```

Look for:

- A worker with `last_cycle_duration_ms` much higher
  than usual.
- A worker that's been running continuously (no idle
  gap between cycles).
- A worker that recently started a heavy operation
  (HNSW rebuild, consolidation pass).

Workers Brain ships:

- `decay`, `access_boost` — usually small.
- `consolidation` — can be heavy (full pass over
  recent memories).
- `hnsw_maintenance` — heavy (rebuild).
- `snapshot` — moderate (file copy).
- `wal_retention`, `idempotency_cleanup`,
  `slot_reclamation`, `edge_scrub` — usually small.

If a worker is the cause, *Remediate → Throttle or
pause worker*.

### 8. Cross-cutting checks

If the symptom doesn't match any single category:

- **Recent deploy?** Check deploy timestamps. The
  most common "everything got slow at 14:31" cause is
  a deploy at 14:28.
- **Config change?** Same logic.
- **Embedder API changes** (knowledge-active mode): if
  LLM extractors are slow, the upstream provider may
  have rate-limited you. Check `brain_llm_*` metrics.

### 9. Take a profile if stuck

If steps 1-8 haven't identified the cause, capture a
CPU profile ([DR-04](dr-04-profiles-and-heap-dumps.md)):

```bash
sudo perf record -F 99 -g -p $(pgrep brain-server) -- sleep 30
```

Convert to a flame graph and look for the dominant
frame. If the cause isn't obvious from the flame
graph, escalate ([IR-03](ir-03-escalation-policy.md)).

---

## Remediate

### Rebuild HNSW

If `tombstone_ratio > 0.30`:

```bash
brain-cli rebuild-ann --shard <n>
```

This triggers a synchronous rebuild on the specified
shard. During rebuild (up to a few minutes for 1M
nodes), latency on that shard will spike *higher*
than the current degraded state. Plan accordingly:

- If the affected shard is in a critical path,
  consider draining traffic from it first if your
  load balancer supports per-shard routing.
- For a smaller shard, rebuild during a low-traffic
  window if possible. For a P2 with active impact,
  do it now anyway — degraded recall is worse than a
  brief deeper dip.

After rebuild, tombstone ratio resets to 0; latency
should return to baseline within a couple of minutes.

### Disk-bound encode

If WAL fsync is slow due to disk:

- **Cloud:** check whether your IOPS / throughput
  budget is being throttled. Some cloud SSDs (e.g.,
  GP3 on EBS) have configurable IOPS that you can
  raise without instance restart.
- **Bare metal:** check disk health with SMART. Bad
  sectors can manifest as slow fsync long before
  outright errors.
- **Filesystem:** if the data volume is heavily
  fragmented, performance degrades. `e2fsck -fD` or
  filesystem-specific defrag tools can help (offline
  only).

If the disk is genuinely saturated, the substrate
can't make it faster. Add IOPS or split shards.

### Embedder cache bigger

If cache hit rate is too low and that's plausibly the
cause:

```toml
[embedder]
cache_size = 50000     # was 10000
```

Rolling restart to apply ([OP-01](op-01-rolling-restart.md)).
The cache will warm up over a few hours under typical
load.

### Throttle or pause worker

If a worker is contending with serving:

```bash
brain-cli admin worker pause hnsw_maintenance
```

This pauses the worker; it'll resume next time you
unpause. Don't leave a maintenance worker paused for
long (a paused `hnsw_maintenance` means tombstones
will accumulate). Resume once the incident is over:

```bash
brain-cli admin worker resume hnsw_maintenance
```

For long-term throttling, configure the worker's
cadence to be less aggressive in the TOML and roll
the change ([OP-08](op-08-config-change-rollout.md)).

### Split shards (long-term)

If the shard is just too big, no in-incident fix
will help; the root cause is sizing. Document a
follow-up to redeploy with more shards. Until then,
mitigate by:

- Raising `top_k` floors on hot queries (so HNSW
  walks aren't extreme).
- Pre-filtering aggressively at the client.
- Caching at the client side for repeat queries.

---

## Verify

After remediation:

```promql
# p99 has dropped back under SLO.
histogram_quantile(0.99,
  sum by (op, le) (rate(brain_request_duration_ms_bucket[5m])))
```

Wait at least 5 minutes for the metric to settle.
The `BrainHighLatency` alert should clear once p99
is back under threshold for 10 minutes.

Smoke test:

```bash
time brain-cli recall "smoke test query"
```

Should return in well under your SLO target.

---

## Post-incident

In the incident channel:

```
:white_check_mark: Resolved at HH:MM UTC.
Root cause: <e.g., HNSW tombstone ratio 0.41 on shard 3>.
Remediation: rebuild-ann.
User impact: ~Xm of degraded recall on shard 3.
Follow-up: TICKET-NNNN (auto-rebuild threshold).
Postmortem: <yes/no>.
```

Postmortem rule for RB-02:

- **Always** if it's a recurring symptom (third time
  this month).
- **Always** if the runbook didn't lead you to a
  resolution efficiently.
- **Often** otherwise; it's a P2.

---

## Prevention

- **Auto-rebuild HNSW** before tombstone ratio gets
  this high. The `hnsw_maintenance` worker's
  threshold is configurable; lower it if you find
  yourself rebuilding manually often.
- **Right-size your shards** at deployment. Shards
  larger than ~5M memories start showing this kind of
  latency-tail.
- **Watch the embedder cache hit rate** as a leading
  indicator. A drop usually precedes recall latency
  by hours.
- **Watch WAL fsync p99** as a leading indicator for
  encode-side latency.
- **Don't ignore the YELLOW state.** This runbook is
  for the RED alert; if you have a YELLOW alert at
  p99 > 75 ms, address that before it becomes RED.

---

## Related runbooks

- [RB-03 — Memory pressure / OOM](rb-03-memory-pressure.md)
- [RB-04 — Disk filling](rb-04-disk-filling.md)
- [RB-05 — Worker stuck](rb-05-worker-stuck.md)
- [RB-06 — HNSW recall degraded](rb-06-recall-degraded.md)
- [RB-08 — Substrate becoming unresponsive](rb-08-unresponsive.md)
- [RB-09 — Mass FORGET aftermath](rb-09-mass-forget.md)
- [DR-02 — Reading traces, metrics, and logs](dr-02-reading-traces-metrics-logs.md)
- [DR-04 — Profiles and heap dumps](dr-04-profiles-and-heap-dumps.md)

---

## Last validated

*Update on first use.*
