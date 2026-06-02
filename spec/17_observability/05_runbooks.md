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
   histogram_quantile(0.99, brain_request_duration_ms_bucket{shard="<id>"}) by (op)
   ```

2. Check for resource exhaustion:
   - CPU: `top` or `brain_executor_latency_ms`.
   - Memory: `process_resident_memory_bytes`.
   - Disk I/O: `iostat -x 1`.

3. Check for HNSW degradation:
   ```
   brain_hnsw_tombstone_ratio{shard="<id>"}
   brain_hnsw_recall_estimate{shard="<id>"}
   ```
   If ratio > 30% or recall < 90%, trigger rebuild:
   ```bash
   curl -s -X POST http://127.0.0.1:9092/v1/rebuild-ann -d '{"shard":"<shard-id>"}'
   ```

4. Check for embedder slowness:
   ```
   brain_embedder_duration_ms{quantile="0.99"}
   brain_embedder_queue_depth
   ```
   If embedder is the bottleneck:
   - Cache hit rate low? Increase cache size.
   - Queue deep? Add workers.

5. Check for hot agent:
   ```
   topk(5, sum(rate(brain_request_total{shard="<id>"}[5m])) by (agent_id))
   ```
   (requires per-agent metrics, which Brain doesn't emit by default — use audit logs instead)

6. If still unresolved: capture profile:
   ```bash
   curl -s -X POST http://127.0.0.1:9092/v1/diagnostics/profile \
     -d '{"shard":"<id>","duration":"30s","output":"/tmp/profile.pb"}'
   ```

### Escalate if
Issue not resolved within 30 minutes. Capture profile, dashboards, recent changes.

---

## RB-3: Memory pressure / OOM

### Symptoms
`BrainMemoryPressure` alert. Process resident memory > 85%. Risk of OOM kill.

### Steps

1. Identify which shard is heavy:
   ```
   topk(3, process_resident_memory_bytes by (shard))
   ```

2. Check for ongoing rebuilds:
   ```
   brain_hnsw_rebuild_in_progress
   ```
   If rebuild is in progress: wait for completion. Memory will drop.

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
   If large: WAL retention worker may be stuck. Force its cycle:
   ```bash
   curl -s -X POST http://127.0.0.1:9092/v1/workers/wal-retention/run-now -d '{"shard":"<id>"}'
   ```
   For an immediate `wal`-type garbage collection, administer via the admin HTTP API (`/v1/*` on the admin listener); see [§17.04](04_admin_ops.md). (Operator action: delete eligible WAL segments now. Route name TBD.)

3. Check for large agents:
   ```bash
   curl -s http://127.0.0.1:9092/v1/agents | jq 'sort_by(-.memory_count)'
   ```

4. Mitigations:
   - Free WAL: run the `wal-retention` worker (above) or the `wal` GC action.
   - Free old slots: run the slot-reclamation worker, or the `slots` GC action — administer via the admin HTTP API (`/v1/*`); see [§17.04](04_admin_ops.md). (Operator action: reclaim eligible slots now. Route name TBD.)
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

## RB-6: HNSW recall degraded

### Symptoms
`BrainRecallQualityDegraded` alert. Recall estimate < 85%.

### Steps

1. Check tombstone ratio:
   ```
   brain_hnsw_tombstone_ratio
   ```
   If high: rebuild is needed.

2. Trigger rebuild:
   ```bash
   curl -s -X POST http://127.0.0.1:9092/v1/rebuild-ann -d '{"shard":"<shard-id>"}'
   ```

3. Monitor progress:
   ```
   brain_hnsw_rebuild_progress_pct
   ```

4. After rebuild, verify recall:
   ```
   brain_hnsw_recall_estimate
   ```
   Should be > 95%.

### Escalate if
Rebuild doesn't restore recall. Possible deeper issue.

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

2. Check executor latency:
   ```
   brain_executor_latency_ms{quantile="0.99"}
   ```
   If > 10 ms: a task is hogging the executor.

3. Capture profile:
   ```bash
   curl -s -X POST http://127.0.0.1:9092/v1/diagnostics/profile \
     -d '{"shard":"<id>","duration":"30s","output":"/tmp/profile.pb"}'
   pprof -top /tmp/profile.pb
   ```
   Identifies the hot function.

4. If profile shows a worker as hot: stop the worker.

5. If profile shows request handlers: shed load (configure rate limit, scale up).

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

3. Monitor rebuild:
   ```
   brain_hnsw_rebuild_progress_pct
   ```

4. After rebuild, verify recall is restored.

### Escalate if
Rebuild doesn't help.

---

## RB-10: Network partition (future clustering)

### Symptoms
Some shards reachable, others not.

### Steps

1. Identify which shards are partitioned:
   ```
   up{job="brain"} == 0
   ```

2. Investigate network:
   - Ping reachable from this side?
   - DNS working?
   - Firewall changes?

3. The cluster may be in degraded state but operating with the majority side.

4. Resolve network. Cluster auto-reconciles when partition heals.

### Escalate if
Network can't be resolved. Manual intervention to reduce cluster size to majority side.

---

*Continue to [`06_capacity_planning.md`](06_capacity_planning.md) for capacity planning.*
