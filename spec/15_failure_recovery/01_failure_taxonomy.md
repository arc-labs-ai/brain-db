# 15.01 Failure Taxonomy

A categorization of Brain's failure modes.

## 1. Hardware failures

### Disk failure (write errors)

A write to disk returns an error.

- Detection: I/O error code.
- Response: Log critical, fail the operation, may fail-stop the shard.
- Recovery: Replace disk; restore from backup.

### Disk failure (corruption)

A disk silently corrupts data (rare with modern hardware + ECC).

- Detection: CRC mismatches in WAL or arena.
- Response: Refuse to read corrupted data, log critical.
- Recovery: Restore from backup.

### Memory failure (bit flip)

A memory error corrupts in-memory state.

- Detection: ECC RAM detects single-bit; double-bit corrupts. Some reads may detect via invariants.
- Response: Crash via SIGBUS or invariant violation.
- Recovery: Restart; recovery from WAL.

### Network failure

The network is partitioned, slow, or lossy.

- Detection: Connection timeouts; missing keep-alives.
- Response: Operations fail or retry; degraded performance.
- Recovery: Network heals; substrate continues.

### Power loss

The machine loses power.

- Detection: After restart, recovery procedure runs.
- Response: WAL replay restores state.
- Recovery: 1-5 minutes per shard.

### CPU / system failure

The machine crashes or hangs.

- Detection: External monitoring (process supervisor, etc.).
- Response: Auto-restart by supervisor.
- Recovery: WAL replay.

## 2. Software failures

### Substrate panic

A bug causes the substrate to panic (crash).

- Detection: Process exits non-zero; supervisor sees.
- Response: Auto-restart.
- Recovery: WAL replay; investigate cause.

### Substrate hang

A bug causes a deadlock or infinite loop.

- Detection: Health endpoint times out.
- Response: External monitor restarts; in some cases, internal watchdog.
- Recovery: WAL replay.

### Storage layer corruption (logical)

A bug writes inconsistent state (passing internal checks but logically wrong).

- Detection: Invariants firing later, possibly during recovery.
- Response: Logs critical; refuse to proceed.
- Recovery: Restore from backup.

### HNSW corruption

The HNSW index is inconsistent (e.g., dangling references).

- Detection: Search returns implausible results; integrity checker.
- Response: Trigger rebuild.
- Recovery: Rebuild from arena (the source of truth).

### Metadata corruption

The redb file is corrupt (very rare given redb's design).

- Detection: redb's checksums fail.
- Response: Refuse to open; alert.
- Recovery: Restore from backup; rebuild from WAL if possible.

## 3. Operational failures

### Configuration error

A misconfigured value causes errors.

- Detection: Errors at startup or at runtime.
- Response: Log; for fatal misconfigs, refuse to start.
- Recovery: Fix config; restart.

### Disk full

The disk fills.

- Detection: Write returns NoSpace.
- Response: Reject new writes; reads continue.
- Recovery: Free space; possibly migrate.

### CPU saturation

The CPU is at 100% sustained.

- Detection: Load metrics.
- Response: Backpressure (Overloaded errors); degrade gracefully.
- Recovery: Reduce load or scale up.

### Memory pressure

RAM usage exceeds capacity.

- Detection: Process metrics; OOM killer in extremis.
- Response: Shed load; fail allocations gracefully.
- Recovery: Scale up RAM; investigate leak.

### Operator mistake (data deletion)

An operator runs `agent delete` on the wrong agent.

- Detection: Audit logs (after the fact).
- Response: Operation succeeds (it's authorized).
- Recovery: Restore from snapshot.

## 4. Capacity failures

### Shard at capacity

A shard exceeds its memory count limit.

- Detection: Capacity metrics.
- Response: New writes succeed but performance degrades.
- Recovery: Split the shard; rebalance.

### HNSW degraded

Tombstone ratio too high; recall dropping.

- Detection: Recall estimate metric.
- Response: Maintenance worker triggers rebuild.
- Recovery: Rebuild restores recall.

### Embedder overload

Too many embed requests; queue grows.

- Detection: Embedder queue metric.
- Response: Backpressure on requests; rejections.
- Recovery: Add embedder workers; scale up.

## 5. Distributed-mode failures (v2)

### Node failure

A cluster node fails.

- Detection: Heartbeat / membership.
- Response: Failover to replica (with replication enabled).
- Recovery: Replace node; rejoin cluster.

### Network partition

Cluster is split.

- Detection: Membership disagreement.
- Response: Majority side continues; minority side gives up writes.
- Recovery: Network heals; reconciliation.

### Replication lag

A replica falls too far behind.

- Detection: Lag metric.
- Response: Mark out-of-sync; resync via snapshot.
- Recovery: Snapshot transfer; catch up.

## 6. Data-integrity failures

### Idempotency mismatch

A duplicate request with different parameters.

- Detection: Idempotency table check.
- Response: Return Conflict error.
- Recovery: Application uses new RequestId.

### Read-after-write violation

A read doesn't see a recent committed write (when ReadAfterWrite was requested).

- Detection: Application-side check (rare; would be a substrate bug).
- Response: Substrate's design prevents this.
- Recovery: N/A (would be a bug).

### Slot version mismatch

A stale MemoryId references a reused slot.

- Detection: Slot version check.
- Response: Operation returns NotFound.
- Recovery: Application uses fresh references.

## 7. Adversarial failures

### Authentication bypass attempt

A request without valid credentials.

- Detection: Auth fails.
- Response: Reject; log security event.
- Recovery: N/A; the substrate behaves correctly.

### Resource exhaustion attack

A client floods the substrate.

- Detection: Rate metrics.
- Response: Per-tenant quotas; rate limiting.
- Recovery: Block bad client; investigate.

### Malformed input

A request with invalid bytes (fuzzing or attack).

- Detection: Wire-protocol parsing fails.
- Response: Reject; log.
- Recovery: N/A.

## 8. Time-related failures

### Clock skew

Server clocks drift.

- Detection: Time-based features may behave oddly.
- Response: Brain uses monotonic time where possible; absolute time only for timestamps.
- Recovery: NTP / chrony keeps clocks aligned.

### Time-warp

A clock jumps backward (e.g., NTP correction).

- Detection: Internal monotonic check.
- Response: Brain uses monotonic time for ordering; immune to time-warp.
- Recovery: N/A.

## 9. The "interaction" failures

Some failures only manifest when combined:

- Slow disk + many concurrent writes → backpressure cascade.
- Memory pressure + ongoing rebuild → OOM.
- Network blip + retry storm → load spike.

Brain's design handles single failures; combined failures stress it more.

## 10. The "unknown unknowns"

Some failures haven't been seen yet:

- Novel bug paths.
- New hardware quirks.
- Adversarial techniques.

The substrate's defensive design (invariants, CRCs, audit) helps catch unknowns. Post-mortems then improve the design.

---

*Continue to [`02_crash_recovery.md`](02_crash_recovery.md) for crash recovery.*
