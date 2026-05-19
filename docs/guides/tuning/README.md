# Tuning

**Audience:** operators with a measured workload and a specific
problem (recall too low, latency too high, ingest too slow, RAM
too high).

**Goal:** *informed parameter changes*. Not magic numbers. Each
page tells you what the parameter controls, what direction to move
it, and what to measure afterwards.

## The tuning loop

1. **Measure first.** If you don't have p50/p95/p99 of the
   operation you're trying to improve, stop and instrument
   ([`../observability.md`](../observability.md)).
2. **Change one thing.** Tune one parameter at a time.
3. **Re-measure.** Same workload, same host, same tool.
4. **Compare.** If the change didn't move the metric, revert it.

Brain ships with sensible defaults. **Most operators never need
to touch these knobs.** Read this section when you have a specific
problem; don't pre-tune.

## Pages

| Page | The lever | Symptom that justifies pulling it |
|---|---|---|
| [`hnsw-parameters.md`](hnsw-parameters.md) | `[hnsw] M`, `ef_construction`, `ef_search` | Recall too low; latency too high |
| [`shard-sizing.md`](shard-sizing.md) | `[storage] shard_count` | Per-shard write contention, hot-shard imbalance |
| [`wal-tuning.md`](wal-tuning.md) | `[shard] wal_segment_size_bytes`, `wal_retention_segments` | Disk pressure, recovery time, fsync latency |
| [`embedding-throughput.md`](embedding-throughput.md) | `[embedder] cache_size`, `batch_size`, `batch_window_ms` | ENCODE latency, GPU/CPU utilisation |

## See also

- [`../../benchmarks/latency-targets.md`](../../benchmarks/latency-targets.md)
  — the numbers Brain commits to. Tune to meet these, not to beat
  them.
- [`../../reference/performance.md`](../../reference/performance.md)
  — quick-reference target table.
- [`../../architecture/04-hnsw-index.md`](../../architecture/04-hnsw-index.md)
  — why HNSW parameters trade off the way they do.
