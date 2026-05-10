# 08.07 Cost Estimation

The planner uses cost estimates to pick parameters (ef_search, over_factor) and to detect overly-expensive requests. This file specifies the cost model.

## 1. The cost units

The substrate's costs come in a few flavors:

- **Time** — milliseconds of latency.
- **CPU** — actual core time used (latency × parallelism).
- **Memory** — bytes allocated during the operation.
- **Disk I/O** — bytes read/written.

For planning, time is the primary unit. Other units inform secondary decisions (e.g., memory limits).

## 2. The cost model components

For each operation:

| Operation | Cost |
|---|---|
| Embed (cache hit) | 5 µs |
| Embed (cache miss, CPU) | 5-10 ms |
| HNSW search (1M, ef=64) | 1-2 ms |
| HNSW search (10M, ef=64) | 3-5 ms |
| Metadata point lookup | 1-10 µs |
| Metadata range scan (100 rows) | 30-50 µs |
| WAL append + fsync (group) | 0.3 ms |
| Arena read | 0.001 ms |
| Arena write | 0.001 ms |
| HNSW insert | 0.5-2 ms |
| Network round-trip (intra-shard) | 0.1 ms |

These come from measurements in our test rig. They're approximate but close enough for planning.

## 3. Per-step cost formulas

```rust
fn cost_recall(req: &RecallRequest, shard_stats: &ShardStats) -> f32 {
    let mut cost_ms = 0.0;

    // Embed
    cost_ms += if cache_likely_hit(req.cue_text) {
        0.005
    } else {
        7.5  // average of 5-10 ms
    };

    // ANN search
    let n = shard_stats.memory_count;
    let ef = pick_ef(req, shard_stats);
    cost_ms += ann_search_cost(n, ef);

    // Metadata lookups
    cost_ms += req.k as f32 * 0.005;

    // Filtering
    let candidates = req.k * over_factor(req);
    if has_metadata_filter(req) {
        cost_ms += candidates as f32 * 0.005;
    }

    cost_ms
}

fn ann_search_cost(n: usize, ef: usize) -> f32 {
    // Empirical: ~0.05 ms baseline + 0.001 ms per (ef * log(n))
    let log_n = (n as f32).log2();
    0.05 + (ef as f32) * log_n * 0.001
}
```

These formulas are approximate. They serve to choose parameters, not to predict latency precisely.

## 4. Cost-based ef_search

The planner picks ef_search to balance quality and cost:

```rust
fn pick_ef(req: &RecallRequest, shard_stats: &ShardStats) -> usize {
    let target_recall = 0.95;
    let baseline_ef = 64;

    // Baseline + adjustments
    let mut ef = baseline_ef;
    if shard_stats.memory_count > 1_000_000 {
        ef = ef.max(100);
    }
    if shard_stats.tombstone_ratio > 0.1 {
        ef = (ef as f32 * (1.0 + shard_stats.tombstone_ratio * 5.0)) as usize;
    }
    if filter_selectivity(req) < 0.5 {
        ef = (ef as f32 / filter_selectivity(req)) as usize;
    }

    ef.min(config.max_ef_search)
}
```

The substrate doesn't run cost minimization; it has fixed rules.

## 5. Over-budget detection

If the estimated cost exceeds a threshold, the planner takes precautions:

```rust
fn check_budget(estimated_cost_ms: f32, req: &Request) -> Result<()> {
    if estimated_cost_ms > 1000.0 {
        return Err(PlanError::QueryTooExpensive);
    }
    if estimated_cost_ms > 100.0 {
        log_warning("Slow query plan", req);
    }
    Ok(())
}
```

The 1-second cap protects against pathological queries (e.g., REASON across many shards with high depth). Operators can configure the cap.

## 6. Cost vs request configuration

Some request parameters can be lowered to make queries faster:

- Lower K → less work.
- Lower max_depth (PLAN/REASON) → less traversal.
- Tighter confidence_min → fewer candidates considered.

The planner doesn't lower client-supplied parameters automatically. If a query is too expensive, the planner errors out rather than silently degrading.

## 7. Per-shard cost variation

Different shards have different costs:

- Shard with 1M memories: HNSW search ~1 ms.
- Shard with 10M memories: HNSW search ~5 ms.
- Heavily-tombstoned shard: search needs higher ef.

The planner consults per-shard stats when estimating.

## 8. Cross-shard cost

Cross-shard queries:

```rust
fn cost_cross_shard(per_shard_cost: f32, n_shards: usize) -> f32 {
    let parallel_cost = per_shard_cost;          // Run in parallel
    let merge_cost = 0.05;                        // Merge top results
    let serialization_cost = 0.1 * (n_shards as f32);  // Network and serialization
    parallel_cost + merge_cost + serialization_cost
}
```

For 2 shards: roughly per-shard cost + 0.25 ms.

## 9. The "cheap fast path" detection

Some requests are cheap enough that the planner skips estimation:

```rust
fn is_simple(req: &RecallRequest) -> bool {
    req.k <= 20
        && req.filter.is_minimal()
        && req.consistency == Consistency::Eventual
}

if is_simple(req) {
    return RecallPlan::default();    // Fast path; no cost estimation
}
```

The fast path skips ~20 µs of estimation. Worthwhile for high-throughput simple queries.

## 10. Statistics maintenance

Per-shard statistics:

```rust
struct ShardStats {
    memory_count: usize,
    tombstone_count: usize,
    tombstone_ratio: f32,
    last_rebuild_at: Timestamp,
    avg_search_latency_ms: f32,    // Last 5 minutes
    avg_encode_latency_ms: f32,
}
```

These are maintained by the substrate's monitoring layer ([14. Observability + Operations](../14_observability_ops/)). Updated periodically (every few seconds).

## 11. Cost estimation accuracy

How accurate is the cost estimate?

- For simple queries (RECALL with K=10, no complex filter): within 20% of actual.
- For complex queries (PLAN with depth 5, multiple shards): within 50% of actual.

The accuracy is enough for parameter picking and budget enforcement. Not enough for fine-grained capacity planning.

## 12. The cost is not exposed to clients

Clients don't see cost estimates. The substrate's response is just the answer. (For debugging, `ADMIN_EXPLAIN_PLAN` returns the plan including cost estimate.)

## 13. Logging cost vs actual

After execution, the substrate logs:

- The estimated cost.
- The actual latency.
- The discrepancy.

Operators monitor the discrepancy to detect:
- Stale statistics (estimates consistently too low or too high).
- Pathological queries (large discrepancies indicate the model is missing something).

## 14. The cost adjustment loop

Periodically (daily or so), the substrate could analyze logged data and adjust the cost coefficients (the magic numbers in the formulas).

In v1, the coefficients are hardcoded. Adjustment is manual: an operator looks at logs and updates configuration.

A future enhancement: automatic coefficient tuning based on observed-vs-estimated logs.

## 15. The simplicity priority

The cost model is intentionally simple. We could:

- Run mini-benchmarks at startup to calibrate.
- Track per-query patterns and predict from history.
- Use ML to predict cost.

We don't, because:

- Simple rules are predictable; their failure modes are understandable.
- Mini-benchmarks add startup time.
- ML predictions can fail unexpectedly.

The simple rules are good enough.

---

*Continue to [`08_executor.md`](08_executor.md) for the executor.*
