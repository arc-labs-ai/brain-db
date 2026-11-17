# 17.06 Capacity Planning

How to size a Brain deployment and plan for growth.

## 1. The dimensions

Plan along these axes:

- **Memory count** — total memories stored.
- **Read rate** — RECALLs per second.
- **Write rate** — ENCODEs per second.
- **Edge density** — edges per memory.
- **Average text size** — bytes per memory.

These determine resource needs.

## 2. Per-shard targets

A v1 shard's recommended ranges:

| Metric | Recommended | Hard limit |
|---|---|---|
| Memory count | 10K-10M | 50M |
| Read rate | up to 20K/s | 50K/s |
| Write rate | up to 5K/s | 20K/s |
| Edge density | 1-100 per memory | 1000 per memory |
| Text size | < 8 KB avg | 64 KB |

Beyond hard limits, shard or split.

## 3. Resource sizing

For a typical shard at recommended load:

| Resource | Estimate |
|---|---|
| CPU | 1 core (Glommio thread-per-core) |
| RAM | 4-32 GB (data-size dependent) |
| Disk | ~3-10× data size |
| Network | 100 Mbps - 1 Gbps |

## 4. Storage sizing

Per memory:

```
Vector (384 floats × 4 bytes): 1.5 KB
Slot metadata + padding: ~64 bytes
Metadata in redb: ~200-500 bytes
Edges (avg 5 per memory × ~50 bytes): ~250 bytes
Text (variable; avg 2 KB): ~2 KB
WAL (over time): equivalent to writes' worth, retained briefly

Total per memory: ~4-5 KB
```

The embedding is 384 dimensions (`crates/brain-embed/src/model/forward.rs:24`),
which is 1536 **bytes** at `f32`. That figure is easy to mistake for a
dimension count — the widely-quoted 1536 belongs to other providers' models,
and assuming it here overstates the vector's footprint fourfold.

For 1M memories: ~4-5 GB. For 10M: ~40-50 GB.

Plan disk accordingly, with headroom for:
- WAL (~512 MiB - 1 GiB).
- Tombstones (memories before arena GC).
- Snapshots (if kept).

A useful heuristic: provision 3× the data size for working capacity.

## 5. Memory (RAM) sizing

In-memory components:

```
HNSW index: ~150 MB per million memories (384-dimension vectors plus neighbour lists).
Embedder model: ~150 MB (loaded once).
Caches: 100 MB - 1 GB (configurable).
Per-connection state: ~100 KB.
Tasks and overhead: ~500 MB.
```

For 1M memories: ~1-2 GB RAM. For 10M: ~3-5 GB.

Plus headroom for:
- HNSW rebuild (peak 2× the index size).
- Burst caches.
- OS file cache.

A useful heuristic: 4× the in-memory data size.

## 6. CPU sizing

Each shard wants a dedicated core. Plus:

- 1-2 cores for the connection layer.
- 1-2 cores for the embedder (or shared with shards).
- 1-2 cores for OS / monitoring.

For a 16-shard deployment: 16 + 4 = 20 cores recommended.

## 7. Network sizing

Per request:
- ENCODE: ~10 KB request, ~100 bytes response.
- RECALL: ~100 bytes request, ~5 KB response (K=10, no text).
- RECALL with text: ~100 bytes request, ~50 KB response (K=10, text).

For 10K requests/s with mixed workload: ~100-500 Mbps. 1 Gbps NIC is plenty.

## 8. The "watch the trend" approach

Static sizing is one snapshot. Capacity planning is also tracking trends:

- Memory count: linear growth? Exponential?
- Request rate: stable? Growing?
- Storage: matches expectations?

Brain's metrics include enough history (Prometheus retention) for trend analysis.

## 9. Forecast tools

```promql
# Allocated slots in 30 days, linear extrapolation
predict_linear(brain_arena_slots_used[30d], 30 * 86400)

# Disk usage in 30 days
predict_linear(brain_arena_used_bytes[30d], 30 * 86400)
```

There is no live-memory-count gauge. `brain_arena_slots_used` counts allocated
slots, **tombstoned ones included**, which is the right basis for a capacity
forecast anyway: a tombstoned slot occupies arena space until reclamation runs.
A live count — for a space, a session or a shard — comes from `ADMIN_STATS` or
`GET /v1/spaces`, where it can be scoped without putting a tenant id on a
metric series.

Both are gauges that only rise until reclamation runs, which is what makes
`predict_linear` meaningful on them — it fits a straight line and extrapolates,
so it is the right function here and `rate()` is not. A reclamation pass bends
the line down and the forecast with it, so read a projection taken across one
as conservative rather than wrong.

Validate by comparing the actual rate against the forecast.

## 10. Headroom

Don't run at 100% capacity. Headroom is needed for:

- Spikes (handles unexpected load).
- Background work (rebuilds, consolidation).
- Maintenance.

Recommended headroom:

- CPU: stay below 60% average. Spikes to 80% are OK.
- RAM: stay below 70%. Spikes to 90% are OK (RAM doesn't get throttled like CPU).
- Disk: stay below 70%. Spikes to 80% trigger cleanup.

## 11. Scaling triggers

When to scale:

- CPU sustained > 60%: consider adding capacity.
- p99 latency > 50 ms (target): tune or scale.
- Memory growth tracks data growth: normal. If memory grows faster: leak suspected.
- Disk projection: when forecast says "30 days to full", start sourcing.

These are indicators; specifics depend on the SLO.

## 12. Vertical vs horizontal

For Brain v1:

- **Vertical** is easier — bigger machine, more resources.
- **Horizontal** means more shards or (in a future major version) clustering.

For v1 single-node:

- Up to ~64 cores per machine: feasible.
- 1-2 TB of memory: feasible.
- 10-50 TB of disk: feasible.

Up to those limits, vertical scaling is the path.

## 13. The "embarrassment of capacity"

Some workloads:

- High write, low read.
- Or low write, high read.
- Or burst-y.

For burst-y workloads, provision for the peak. Don't average.

For asymmetric workloads (e.g., write-only ingest + occasional read), the bottleneck differs. Profile to find it.

## 14. Multi-tenant capacity

In multi-tenant scenarios tenants share a shard, or have dedicated ones.

Two things an operator will reach for do not exist, and planning has to account
for their absence:

- **There are no per-tenant quotas.** No memory cap, context cap or
  request-rate cap is enforced per space or per namespace. The limits that do
  refuse a write are physical or per-field — arena capacity, disk, and the byte
  caps in `crates/brain-ops/src/handlers/limits.rs`. One tenant can therefore
  consume a shard's whole arena, and nothing in Brain will stop it.
- **There are no per-tenant metrics.** `space_id` and `namespace` are never
  metric labels, by design: a tenant label multiplies every series by the
  tenant count, and on a shard serving a million spaces that is a
  metrics-system outage rather than a slow dashboard.

So multi-tenant capacity is planned at the shard, enforced outside Brain, and
attributed from the logs and the audit trail rather than from metrics. A
front-door gateway is the natural place to put a rate limit, since it already
holds the per-tenant identity. Per-space footprint for planning comes from
`ADMIN_STATS` or `GET /v1/spaces`, which can be scoped to a tenant without
putting a tenant id on a metric series.

## 15. The "warm-up" planning

A new deployment doesn't have caches warmed up:

- Embedder cache empty.
- File system cache empty.
- HNSW initial state.

First-hour performance is worse than steady state. Plan for ramp-up time when launching a new deployment.

## 16. The "data growth" model

For a typical chatbot agent:

- ~100-1000 memories per active user per day.
- Half are small (single sentence): ~2 KB each.
- Half are larger (paragraphs): ~10 KB each.
- Average: ~6 KB per memory.

For 10K daily active users:
- 1M-10M memories per day.
- ~6-60 GB of data per day.

Capacity to plan: a single shard handles ~1 day of this scale before getting full. Multiple shards needed.

## 17. The "growth budget"

Buy capacity for growth:

- Plan for 2× current load.
- Keep 50% headroom.
- Plan additional capacity 30-90 days ahead of need.

This avoids "all-hands-on-deck" emergency scaling.

## 18. Cost analysis

For a 16-shard deployment:

- Hardware: 1 machine with ~20 cores, ~128 GB RAM, ~4 TB NVMe. ~$500-1500/month cloud (depending on provider).
- For HA: 2× machines with replication. ~$1000-3000/month.

Plus operational cost (engineering time).

Compare to: managed vector databases (~$1-10/M vectors/month). Brain is competitive at scale.

## 19. The "what if doubled" exercise

For each metric:

- What if memory count doubles? (More disk, more RAM for HNSW, longer rebuilds.)
- What if request rate doubles? (More CPU, possibly more shards.)
- What if average text size doubles? (More disk, more network.)

Knowing the answer ahead of time prevents surprises.

## 20. The "load test" rhythm

Before major launches:

- Synthetic load test against staging.
- Identify the actual bottleneck.
- Provision accordingly.

After major launches:

- Monitor closely.
- Tune as data emerges.

Capacity planning is iterative; the first plan is rarely the last.
