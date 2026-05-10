# 16.03 Throughput Targets

The throughput targets Brain v1 must meet.

## 1. Per-shard targets

| Operation | Target (ops/sec/shard) |
|---|---|
| ENCODE | ≥ 5,000 |
| RECALL | ≥ 20,000 |
| PLAN (depth 3) | ≥ 8,000 |
| REASON (depth 3) | ≥ 5,000 |
| FORGET | ≥ 10,000 |
| LINK | ≥ 30,000 |
| UNLINK | ≥ 30,000 |

These are MUST targets at 1M memories per shard, on reference hardware.

## 2. The "mixed workload" target

A realistic mix (70% recall, 25% encode, 5% other):

- Combined throughput: ≥ 10,000 ops/sec/shard.
- Latency: stays within p99 targets.

This is the primary target — it reflects actual deployments.

## 3. The multi-shard target

For a 16-shard substrate:

- Aggregate throughput: ≥ 100,000 ops/sec.
- Per-shard: ~6,000-8,000 ops/sec average.

(Per-shard reduces because not all operations target one shard; some shards may be idle while others are busy.)

## 4. The "burst" tolerance

The substrate handles bursts above sustained:

- 2× sustained for 10 seconds: tolerated, latency may spike.
- 5× sustained for 1 second: shed via Overloaded.

Bursts are common; tolerance prevents transient overloads from cascading.

## 5. The "max throughput" exploration

Beyond targets:

- Max sustained: ~20-50K ops/sec/shard for RECALL (bottleneck: HNSW search).
- Max sustained: ~10-20K ops/sec/shard for ENCODE (bottleneck: WAL fsync).

These define the hard ceilings; targets are conservative below.

## 6. The bottleneck identification

For each operation, the limiting factor:

| Operation | Bottleneck |
|---|---|
| ENCODE | WAL fsync (NVMe ~50K IOPS) |
| RECALL | HNSW search + embedder |
| PLAN | Edge traversal (memory-bound) |
| REASON | Edge traversal (memory-bound) |
| FORGET | WAL fsync |
| LINK | WAL fsync |

WAL fsync is the dominant bottleneck for writes. Reads are CPU-bound.

## 7. The "WAL group commit" effect

Group commit batches multiple ENCODEs into a single fsync:

- 1 ENCODE / sec: 1 fsync.
- 1000 ENCODE / sec: ~50-100 fsyncs (batched into groups of 10-20).
- 10000 ENCODE / sec: ~500 fsyncs (at limit of NVMe).

Group commit lets ENCODE throughput exceed the raw fsync rate.

## 8. The connection-pool effect

For a single client connection:
- Limited by request-response sequencing.
- ~1000 ops/sec from one connection (if synchronous).

For 100 connections:
- ~10,000 ops/sec aggregate (if each does ~100/sec).

Many parallel connections are needed for high throughput. SDKs handle this.

## 9. The "concurrent" target

The substrate supports:

- 10K concurrent connections per substrate.
- 100K concurrent in-flight requests.
- 1M streams per second (open + close).

These are MUST. Beyond these, behavior is "best effort".

## 10. The pipelining throughput

With pipelining (multiple requests in flight per connection):

- Per-connection: ~10K ops/sec (vs ~1K without).
- Aggregate: scales with the number of active connections.

Brain's protocol supports pipelining; SDKs use it.

## 11. The "load step" test

Throughput tests:

- Start at 1K ops/sec.
- Increase by 1K/sec every 10 seconds.
- Stop when latency p99 exceeds target.

The crossover point is the substrate's "knee" — sustainable throughput before latency degrades.

## 12. The "sustain" requirement

Throughput must be sustainable, not just peak:

- Run at target for 10 minutes.
- Verify no latency degradation.
- Verify no resource exhaustion.

A substrate that hits target for 1 minute then collapses doesn't pass.

## 13. The data-freshness effect

When the data is fresh (recent encodes, no rebuild yet):

- HNSW may have many tombstones (slow).
- Throughput drops.

Acceptance tests don't run this scenario (it's a special case). Operators understand and trigger rebuilds proactively.

## 14. The "saturation" indicators

When throughput approaches max:

- p99 latency rises (request queue grows).
- CPU utilization climbs.
- Occasional Overloaded errors.

Monitor and back off before saturation. Operators scale before this point.

## 15. The "single core" reality

Each Glommio shard runs on one core. So:

- One shard's max throughput is bounded by what one core can do.
- More shards = more throughput, but each shard is limited.

Reasonable per-core throughput: ~10-50K ops/sec depending on operation.

For very high throughput, scale shards (more cores).

## 16. The cross-shard hit

For multi-shard fan-out operations:

- The fan-out coordinator does extra work.
- Each shard does its work in parallel.
- Aggregate doesn't quite scale linearly.

For 16 shards: ~10-12× speedup vs 1 shard, not 16×.

Most operations are single-shard, so this is rarely the main path.

## 17. The throughput vs latency trade-off

Higher throughput typically means higher latency:

- More requests in flight.
- More queueing.
- Larger group-commit batches.

Brain's design balances:
- Group commits batch up to 20 ms.
- Latency p99 stays within targets.

For deployments wanting lower latency at lower throughput: configurable batch sizes.

## 18. The reporting

Throughput benchmarks report:

- Sustained ops/sec at each load level.
- Latency at each load level.
- Resource utilization (CPU, memory, disk, network).
- Errors / second (should be 0 unless overloaded).

Combined picture shows where the substrate's "sweet spot" is.

## 19. The historical comparison

Each release compares to previous:

```
Release 1.0:
  ENCODE throughput: 5,200 ops/sec (target: 5,000) ✓

Release 1.1:
  ENCODE throughput: 5,500 ops/sec (target: 5,000) ✓ (+5.8%)
```

Improvements are tracked; regressions are flagged.

## 20. The "v2 horizon"

V1 is single-node; throughput per machine is the limit.

V2 (clustered) will scale across machines:

- Linear scaling for read-heavy workloads.
- Sub-linear for write-heavy (due to replication overhead).

The targets in this spec are v1 single-node. V2 will have its own.

---

*Continue to [`04_resource_targets.md`](04_resource_targets.md) for resource targets.*
