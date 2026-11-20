# 17.05 Runbooks

Step-by-step procedures for common operational situations.

---

## RB-1: Brain doesn't start

### Symptoms
Process exits at startup. Logs show errors.

### Common causes

- Configuration error.
- Missing or corrupted data files.
- Port in use.
- Insufficient permissions.

### Steps

1. Check logs:
   ```bash
   tail -100 /var/log/brain/brain.log
   ```

2. If "config error":
   - Validate the config file before launch: `brain-server --check-config /etc/brain/config.toml`
   - Fix syntax / missing required fields.

3. If "address already in use":
   - Find conflicting process: `lsof -i :9090`
   - Kill or reconfigure port.

4. If "data directory missing":
   - Verify `data_dir` in config exists and is writable.
   - For a fresh deployment, point `data_dir` at an empty writable directory and start; Brain initializes the per-shard files on first boot.

5. If "file corruption" or "WAL gap":
   - **Stop**. Don't bring up; risk data loss.
   - Once the admin listener is up, restore from snapshot:
     `curl -s -X POST http://127.0.0.1:9092/v1/snapshots/<last-known-good>/restore -d '{"confirm":true}'`
   - Investigate cause.

### Escalate if
Issue not resolved by above steps. Capture full logs and pre/post state for engineering.

---

## RB-2: High latency on a shard

### Symptoms
`BrainHighLatency` alert. p99 > 100 ms sustained.

### Steps

1. Identify which operations are slow:
   ```
   histogram_quantile(0.99, sum by (le, op) (rate(brain_request_duration_ms_bucket[5m])))
   ```
   The `by` clause belongs to the inner aggregation, not after
   `histogram_quantile`, and the buckets need `rate()` — a quantile taken over
   raw cumulative buckets answers "since process start", not "right now".
   `op` is the only label on this family besides `le`; there is no `shard`
   label on the request metrics, so a shard cannot be selected here. Pin the
   shard by scraping that instance, or use the per-shard families below.

2. Check for resource exhaustion:
   - CPU: `top`. (`brain_executor_latency_ms` is deferred, not emitted.)
   - Memory: `process_memory_resident_bytes`.
   - Disk I/O: `iostat -x 1`.

3. Check for HNSW degradation:
   ```
   brain_hnsw_tombstone_ratio{shard="<id>"}
   ```
   Above 0.30 the maintenance worker rebuilds on its own, so a ratio sitting
   above it means the worker is not running — check that before rebuilding by
   hand. Between 0.15 and 0.30 the worker logs and does nothing, and a manual
   rebuild is the only remedy:
   ```bash
   curl -s -X POST http://127.0.0.1:9092/v1/rebuild-ann -d '{"shard":"<shard-id>"}'
   ```
   `brain_hnsw_recall_estimate` is not emitted, and the internal value it would
   carry is hardcoded to `1.0`
   (`crates/brain-workers/src/workers/hnsw_maint.rs:211`), so the tombstone
   ratio is not just the best available signal — it is the only one. It is also
   the right proxy: tombstones are what leave neighbour lists pointing at
   deleted nodes.

4. Check for embedder slowness. The `brain_embedder_*` family is deferred and
   not emitted; an embedder stall surfaces as encode latency:
   ```
   histogram_quantile(0.99, sum by (le) (rate(brain_request_duration_ms_bucket{op="encode"}[5m])))
   ```
   If embedding is the bottleneck, raise `[embedder] cache_size` and check
   `[embedder] batch_size` / `batch_window_ms` against the write rate.

5. Check for a hot agent. There is no metric path to this: `space_id` is never
   a metric label and neither is `shard` on the request family, so no PromQL
   query can attribute traffic to a tenant. Per-space attribution comes from
   the request logs: every dispatched request opens a `client.request` span
   carrying `brain.space_id` — the effective identity, with the acting
   principal recorded alongside it under delegation
   (`crates/brain-server/src/network/dispatch.rs:873-885`). Run the log
   stream through `jq`, counting that field, to rank callers. The audit trail
   answers the same question over a longer window. The
   cardinality rule behind this is deliberate, not an omission — a `space_id`
   label on a counter multiplies every series by the tenant count.

6. If still unresolved: profile the shard thread from outside the process.
   `POST /v1/diagnostics/profile` answers `501` — the in-process Glommio
   profiler is not built. Shard threads are named `brain-shard-<id>`, so:
   ```bash
   perf record -g -t "$(pgrep -f brain-server | head -1)" -- sleep 30
   perf report --sort symbol
   ```

### Escalate if
Issue not resolved within 30 minutes. Capture profile, dashboards, recent changes.

---

## RB-3: Memory pressure / OOM

### Symptoms
`BrainMemoryPressure` alert. Process resident memory > 85%. Risk of OOM kill.

### Steps

1. Identify which shard is heavy. Resident memory is a whole-process figure —
   Brain is one process with per-core shards, so there is no per-shard RSS to
   rank. Rank the per-shard footprint gauges instead:
   ```
   topk(3, brain_arena_used_bytes)
   topk(3, brain_hnsw_node_count)
   ```
   Then read the process total alongside them:
   ```
   process_memory_resident_bytes
   ```

2. Check for an ongoing rebuild. `brain_hnsw_rebuild_in_progress` is deferred
   and not emitted; the emitted signal is the maintenance worker's cadence:
   ```
   brain_worker_last_run_unixtime{shard="<id>",worker="hnsw_maintenance"}
   brain_worker_cycles_total{shard="<id>",worker="hnsw_maintenance"}
   ```
   A rebuild in flight means the resident graph is doubled for its duration;
   wait for it and memory will drop.

3. Check arena and HNSW sizes:
   ```
   brain_arena_used_bytes
   brain_hnsw_node_count
   ```
   If grown unexpectedly: investigate write rate.

4. Check for memory leaks:
   - Compare current vs historical baseline.
   - If growing without proportional data growth: possible leak.

5. Mitigations:
   - **Short-term**: restart Brain. Memory drops; recovery from WAL.
   - **Medium-term**: scale up RAM.
   - **Long-term**: shard the data.

6. If imminent OOM:
   - Engage backup capacity.
   - Restart Brain ASAP.

### Escalate if
Memory growth is unexplained. Likely a Brain bug worth investigation.

---

## RB-4: Disk filling

### Symptoms
`BrainDiskFilling` alert. Projection: full within 24 hours.

### Steps

1. Identify largest consumers:
   ```bash
   du -sh /var/lib/brain/data/*
   ```

2. Check WAL size:
   ```
   brain_wal_size_bytes
   ```
   If large: WAL retention worker may be stuck. Force its cycle — this is how an operator deletes eligible WAL segments now rather than waiting for the scheduled cycle:
   ```bash
   curl -s -X POST http://127.0.0.1:9092/v1/workers/wal_retention/run-now -d '{"shard":"<id>"}'
   ```
   See [§17.04](04_admin_ops.md) for the generic `POST /v1/workers/<name>/run-now` control plane.

3. Check for large spaces:
   ```bash
   curl -s http://127.0.0.1:9092/v1/spaces | jq 'sort_by(-.memory_count)'
   ```

4. Mitigations:
   - Free WAL: run the `wal-retention` worker (above) or the `wal` GC action.
   - Free old slots: reclaim eligible slots now with `curl -s -X POST http://127.0.0.1:9092/v1/workers/slot_reclamation/run-now` — the generic worker run-now control plane; see [§17.04](04_admin_ops.md).
   - Delete old snapshots: `curl -s http://127.0.0.1:9092/v1/snapshots`, then `curl -s -X DELETE http://127.0.0.1:9092/v1/snapshots/<name>` for unneeded ones.

5. If can't free enough:
   - Add more disk (LVM extend if possible).
   - Migrate shards to other disks.

### Escalate if
Mitigations insufficient.

---

## RB-5: Worker stuck

### Symptoms
`BrainWorkerStuck` alert. Worker hasn't run in 2 hours.

### Steps

1. Identify which worker:
   ```bash
   curl -s 'http://127.0.0.1:9092/v1/workers?shard=<id>'
   ```

2. Check worker logs:
   ```
   logs --filter "logger ~ 'brain.worker.<name>'" --since 4h
   ```

3. Look for errors in the worker's last cycle:
   - Storage errors?
   - Lock contention?

4. Try restarting the worker:
   ```bash
   curl -s -X POST http://127.0.0.1:9092/v1/workers/<worker-name>/stop  -d '{"shard":"<id>"}'
   curl -s -X POST http://127.0.0.1:9092/v1/workers/<worker-name>/start -d '{"shard":"<id>"}'
   ```

5. If worker still won't run: restart the shard:
   ```bash
   # Coordinated restart
   ```

### Escalate if
Restart doesn't help.

---

## RB-6: HNSW index quality degraded

### Symptoms
`BrainHighTombstoneRatio`. Searches returning fewer or worse hits than the
corpus warrants.

**There is no recall-based trigger for this runbook**, and a rule that waits
for one will wait forever. The maintenance worker stamps
`recall_estimate = 1.0` unconditionally, because query-sample logging is not
wired (`crates/brain-workers/src/workers/hnsw_maint.rs:14-15`, `:211`). Both
recall branches of the decision function compare against that constant
(`:92`, `:95`), so neither can ever be taken. **Tombstone ratio is the only
live signal.**

### Steps

1. Check the tombstone ratio — the one metric that drives maintenance:
   ```
   brain_hnsw_tombstone_ratio
   ```
   Read it against the worker's own thresholds
   (`crates/brain-workers/src/workers/hnsw_maint.rs:68-74`):

   | Ratio | What the worker does |
   |---|---|
   | above 0.30 | rebuilds the index itself, automatically |
   | 0.15 to 0.30 | **nothing reclaimable** — it logs "schedule rebuild soon" and returns |
   | below 0.15 | nothing |

   The middle band is why this runbook exists. `Action::ScheduleRebuildSoon`
   only emits a trace line (`:220-225`); no rebuild is queued and no later pass
   picks it up. A shard sitting at 0.2 will stay there indefinitely, degrading,
   until either the ratio crosses 0.30 or **an operator rebuilds it by hand**.
   That manual rebuild is step 2.

2. Trigger rebuild:
   ```bash
   curl -s -X POST http://127.0.0.1:9092/v1/rebuild-ann -d '{"shard":"<shard-id>"}'
   ```

3. Monitor progress. `brain_hnsw_rebuild_progress_pct` is deferred and not
   emitted; watch the worker counters and the node count instead:
   ```
   brain_worker_cycles_total{shard="<id>",worker="hnsw_maintenance"}
   brain_hnsw_node_count{shard="<id>"}
   ```

4. After the rebuild, verify the tombstone ratio has dropped:
   ```
   brain_hnsw_tombstone_ratio{shard="<id>"}
   ```
   This is the only post-rebuild confirmation available from metrics.
   `brain_hnsw_recall_estimate` is not emitted, and the internal figure it
   would carry is the hardcoded `1.0` described above — so it would confirm
   success whether or not the rebuild helped. **Measuring recall means a
   recall@K run against a known-answer set, not a scrape.**

### Escalate if
The tombstone ratio does not drop after a rebuild, or retrieval quality is poor
with a low tombstone ratio — the latter is not a maintenance problem and this
runbook does not cover it.

---

## RB-7: Recovery from corruption

### Symptoms
- Brain refuses to start: "WAL gap detected" or "metadata corruption".
- Recovery fails partway.

### Steps

1. **Stop**. Don't try to force-restart; preserve evidence.

2. Backup current state:
   ```bash
   tar czf /tmp/corrupt-state.tar.gz /var/lib/brain/data
   ```

3. Identify last known good snapshot:
   ```bash
   curl -s http://127.0.0.1:9092/v1/snapshots
   ```

4. Restore:
   ```bash
   curl -s -X POST http://127.0.0.1:9092/v1/snapshots/<name>/restore -d '{"confirm":true}'
   ```

5. Bring up Brain.

6. Investigate the cause of corruption (after recovery):
   - Hardware issues (memory errors, disk problems)?
   - Brain bug?
   - Operational error?

### Escalate if
No good snapshot exists. Engineering must investigate possible recovery from WAL fragments.

---

## RB-8: Brain becoming unresponsive

### Symptoms
Requests timing out. Health endpoint slow or failing.

### Steps

1. Check CPU:
   - 100% on one shard's core: starvation; investigate workers / queries.
   - 100% across all: insufficient capacity.

2. Check executor latency. `brain_executor_latency_ms` is deferred and not
   emitted — Glommio reactor metrics need instrumentation that does not exist
   yet. Go straight to the profile in step 3; a task hogging the executor shows
   up there as a single frame holding the shard thread.

3. Profile the shard thread. **`POST /v1/diagnostics/profile` answers `501`**
   (`crates/brain-server/src/admin/handlers/diagnostics/profile.rs`): there is
   no in-process profiler, so use the host's. Shard threads are named
   `brain-shard-<id>` (`crates/brain-server/src/shard/mod.rs:3113`) and each is
   pinned to one core when a cpu is assigned, so the thread and the hot core
   are the same target:
   ```bash
   perf top -t "$(ps -L -o tid=,comm= -p "$(pgrep -f brain-server|head -1)" \
                  | awk '$2=="brain-shard-0"{print $1}')"
   ```
   The hot frame is the answer. A worker shows up as that worker's own
   functions; a query shows up in the retriever or the HNSW search.

4. If profile shows a worker as hot: stop the worker.

5. If the profile shows request handlers, the shard is simply saturated.
   **There is no rate limiter to configure and no load shedding**: at the
   dispatch permit bound the shard parks and back-pressures the connection
   layer, so saturation appears as latency rather than as refusals. Nothing on
   the request path raises `RateLimited`, and `OpError::QuotaExceeded` is
   constructed only in a test (`crates/brain-ops/src/lib.rs:113`). The levers
   are capacity — more cores and a higher `[storage] shard_count` — or a
   limiter in front of the data plane.

### Escalate if
Issue persists.

---

## RB-9: Mass FORGET tombstone aftermath

### Symptoms
After a large delete operation, performance degrades.

### Steps

1. Check tombstone ratio:
   ```
   brain_hnsw_tombstone_ratio
   ```

2. If > 30%: trigger rebuild (will take 10s of seconds for 1M memories):
   ```bash
   curl -s -X POST http://127.0.0.1:9092/v1/rebuild-ann -d '{"shard":"<shard-id>"}'
   ```

3. Monitor the rebuild:
   ```
   brain_worker_cycles_total{shard="<id>",worker="hnsw_maintenance"}
   brain_hnsw_node_count{shard="<id>"}
   ```
   (`brain_hnsw_rebuild_progress_pct` is deferred and not emitted.)

4. After the rebuild, confirm `brain_hnsw_tombstone_ratio` has dropped.

### Escalate if
Rebuild doesn't help.

---

## RB-10: A node is unreachable

### Symptoms
`BrainSubstrateDown`. Scrapes to a node fail while others succeed.

### Steps

1. Identify which nodes are unreachable:
   ```
   up{job="brain-server"} == 0
   ```
   `up` is Prometheus's own scrape-result series and carries `job` and
   `instance`, never `shard` — it tells you which **process** is unreachable.
   Shards are threads inside one process, so they are never individually
   unreachable: either the process answers or none of its shards do.

2. Distinguish unreachable from not-ready. A node that answers `/readyz` with
   `503` is up and refusing traffic because a shard thread died:
   ```bash
   curl -s -o /dev/null -w '%{http_code}\n' http://<node>:9091/readyz
   curl -s http://<node>:9091/readyz
   ```
   `{"ready":false,"shards":4,"alive":3}` is a dead shard thread, not a
   network problem — that is RB-8, not this runbook.

3. Investigate the network:
   - Is the node reachable by ping from the scraper's side?
   - Is DNS resolving?
   - Have firewall rules or security groups changed?
   - Is the metrics listener bound to an interface the scraper can reach?
     It binds `[server] metrics_addr`, separately from the data plane.

4. Check whether clients are affected or only the scraper. The data plane and
   the metrics endpoint are different ports, so a firewall change can blind
   monitoring while traffic still flows — or the reverse, which is worse.

### Escalate if
The node is reachable but not answering, or the network cannot be resolved.
Capture `/readyz`, the scrape error, and the node's own logs.

---

*Continue to [`06_capacity_planning.md`](06_capacity_planning.md) for capacity planning.*
