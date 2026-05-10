# 16.04 Resource Targets

The resource utilization targets — CPU, memory, disk, network — Brain v1 must meet.

## 1. The setup

Reference workload at 1M memories per shard, sustained 10K ops/sec/shard.

## 2. CPU targets

| Target | Per shard core |
|---|---|
| Sustained at target throughput | ≤ 70% utilization |
| Idle baseline (workers running, no requests) | ≤ 5% |
| Peak burst | ≤ 95% (transient) |

The 70% target leaves headroom for spikes. Sustained > 80% indicates need for scaling.

## 3. Memory targets

For 1M memories per shard:

| Component | RAM |
|---|---|
| HNSW index | ~150-200 MB |
| Embedder model (loaded once for substrate) | ~150 MB |
| Caches (file system, embedder) | 200-500 MB |
| Connections, tasks | 50-200 MB |
| Total per-shard | ~500-1000 MB |

For a 16-shard substrate: ~8-16 GB. Comfortable on 32 GB+ machines.

## 4. Memory growth

Memory should grow proportionally to data:

- 100K memories: ~150 MB per shard.
- 1M memories: ~700 MB per shard.
- 10M memories: ~6 GB per shard.

For 10M memories per shard, RAM is significant — operators should size accordingly.

## 5. Disk targets

For 1M memories:

| Component | Disk |
|---|---|
| Arena | ~6 GB (1.6 KB per slot × 1M) |
| Metadata | ~1 GB |
| WAL (active + retained) | ~500 MB - 1 GB |
| HNSW snapshot (if kept) | ~200 MB |
| Total per shard | ~8-10 GB |

For 16 shards: ~150 GB. Manageable on 1 TB+ NVMe.

## 6. Disk I/O bandwidth

Sustained:

- Reads: ~50-100 MB/s/shard (cold reads + WAL replay during recovery).
- Writes: ~20-50 MB/s/shard at target write rate.

Modern NVMe: ~3,000+ MB/s sequential, ~50K IOPS random. Plenty of headroom.

## 7. Disk IOPS

Sustained:

- WAL: ~500-2000 IOPS per shard at peak (with group commit).
- Arena: ~10K IOPS reads, ~5K writes (cached for hot data).
- Metadata: ~5K IOPS.

Total per shard: up to ~30K IOPS. NVMe handles easily; SATA SSDs may limit.

## 8. Network targets

For 10K ops/sec:

- Inbound: ~50-200 Mbps (small requests).
- Outbound: ~100-500 Mbps (responses with text).

For 16 shards / 100K ops/sec aggregate: ~1-5 Gbps.

10 Gbps NIC is plenty.

## 9. The "memory leak" check

Run for 48 hours at target load. Compare memory usage:

- Should stabilize within first hour.
- Should not grow beyond ~10% over the run.

Growing memory indicates a leak; investigate.

## 10. The "disk growth" check

At target write rate for 24 hours:

- Disk usage grows by expected amount (number of memories × ~10 KB).
- WAL stays bounded (retention worker keeps it within configured limit).
- No stale snapshots accumulating.

## 11. The "open files" budget

Per substrate:

- Arena files (one per shard): 16.
- WAL files (per shard, multiple segments): ~20 per shard = 320.
- redb files: 16.
- Snapshots: 16.
- Connections (file descriptors): up to 10K.
- Internal: ~50.

Total: ~10K-12K file descriptors.

Set ulimit accordingly: 65536 fd is comfortable.

## 12. The "thread count"

Brain uses minimal threads:

- One Glommio thread per shard (executor).
- A few helper threads (embedder, syscall offload).
- Tokio runtime for connection layer (handful of threads).

Total: ~20-30 threads for a 16-shard substrate. Light.

## 13. The "context switch" rate

With Glommio:

- No thread context switches under normal operation (each shard is one thread).
- Async task switches are user-space (no kernel involvement).

Context switch rate stays low: ~1000-10000/sec total. Vs traditional thread-per-request: ~100K-1M/sec.

This contributes to predictable latency.

## 14. The "io_uring queue" depth

Per shard: ~256 in-flight I/O operations.

This is enough for parallel I/O without exhausting kernel resources.

## 15. The cache-hit-rate targets

Embedder cache:
- Hit rate ≥ 70% in steady state (typical workloads have repeating cues).

File system cache (kernel-managed):
- Hit rate ≥ 90% for active data.

Cache misses are normal but should be the minority.

## 16. The "background work" budget

Workers should consume:
- < 5% CPU on average.
- < 50 MB/s disk I/O on average.
- Should yield generously to operations.

If a worker exceeds: tune interval or batch size.

## 17. The OOM-protection budget

The substrate should:
- Not exceed configured memory limit.
- Shed load before approaching the limit.
- Refuse new operations rather than OOM.

Tested via cgroup memory limits + load testing.

## 18. The "fairness" across shards

Resource usage should be roughly even across shards:
- ~Equal CPU.
- ~Equal memory.
- ~Equal disk.

Hot shards (uneven distribution) indicate hashing or workload issues.

## 19. The reporting

Resource benchmarks report:
- CPU utilization over time.
- Memory usage over time.
- Disk I/O rates.
- Network rates.
- Cache hit rates.
- Per-component breakdowns where useful.

Operators use these to validate behavior in their environment.

## 20. The trade-offs

Brain prioritizes predictability over absolute minimum:

- Larger memory budget for caches (faster).
- More background work to maintain index quality (less rebuild surprise).
- Periodic snapshots (more disk for faster recovery).

These costs are intentional. Operators wanting different trade-offs adjust configuration.

---

*Continue to [`05_recall_quality.md`](05_recall_quality.md) for recall quality criteria.*
