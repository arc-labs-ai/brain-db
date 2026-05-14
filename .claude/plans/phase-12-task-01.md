# Phase 11 — Sub-task 11.1 plan

**Task:** Full metrics taxonomy.

**Phase doc target:**
> Every spec'd metric is emitted; `/metrics` endpoint returns the
> full set in Prometheus format.

**Spec:** `spec/14_observability_ops/01_metrics.md`.

---

## 1. Scope decision (read this first)

Spec §3–§10 lists ~50 distinct metric families across 8 categories.
Today's `/metrics` body emits **8** (build_info, up, shards_total,
connections_active, connections_total, process_uptime_seconds,
process_start_time_seconds, four worker counters).

A blunt audit of which families have backing counters in the
runtime right now:

| Category | Spec families | Backed today? | Action |
|---|---|---|---|
| Build / info (§17) | `brain_build_info`, `brain_config_info` | ✅ build_info; ❌ config_info | Add config_info (labels only, value=1) |
| `up` (§16) | `brain_up` | ✅ | Promote to per-shard form `up{shard=N}` |
| Request (§3) | `brain_request_total`, `brain_request_duration_ms` (histogram), `brain_request_active` | ❌ none | Add per-op atomic counters in `brain-server/src/network/dispatch.rs`; histogram via simple bucket-array |
| Memory (§4) | `brain_memory_count`, `brain_memory_count_tombstoned`, `brain_memory_kind{kind=}` | ❌ no aggregated counters | Sample on `/metrics` request by asking each shard for a counter snapshot |
| Storage (§5) | `brain_arena_used_bytes`, `_capacity_bytes`, `_slots_used`, `_slots_free`, `brain_wal_size_bytes`, `brain_wal_segments`, `brain_metadata_size_bytes` | ❌ no surface | Add `ShardHandle::storage_snapshot()` request variant |
| HNSW (§6) | `node_count`, `tombstone_count`, `tombstone_ratio`, `rebuild_in_progress`, `rebuild_count_total`, `rebuild_duration_sec` quantile | ❌ no exposed counters; tombstone_count exists in `brain-index` but isn't queryable from server | Add a getter on `SharedHnsw` → wire through ShardHandle |
| Embedder (§7) | `calls_total`, `cache_hits_total`, `cache_misses_total`, `duration_ms`, `queue_depth`, `workers_active` | ❌ none | Add `EmbedderMetrics` struct in `brain-embed`, wire counters at entry points |
| Worker (§8) | `cycles_total`, `processed_total`, `errors_total`, `cycle_duration_ms`, `last_run_unixtime`, `pending_work` | ✅ first 3 + last_run; ⚠️ duration histogram + pending_work missing from `/metrics` exposition | Extend the exposition loop to emit them |
| Connection (§9) | `connections_active`, `_total`, `_closed_total{reason=}`, `streams_active`, `frame_send_total`, `frame_recv_total`, `frame_size_bytes` | ✅ first 2; ❌ rest | Extend `ConnectionMetrics` with closed_total, streams_active, frame counters |
| Resource (§10) | `process_cpu_seconds_total`, `process_memory_resident_bytes`, `process_memory_virtual_bytes`, `process_open_fds`, `brain_executor_latency_ms`, `brain_executor_tasks_active` | ❌ none | Read `/proc/self/{stat,status,fd}` on Linux; executor latency deferred to Phase 11.3 |

**Decision (scope of 11.1):**

Land **the foundation + the high-value bulk**, defer the surfaces
that need primitives that don't exist yet. Specifically:

**In scope (this sub-task):**
1. New `brain-server/src/metrics/` module with three primitive types
   — `Counter`, `Gauge`, `Histogram` (16-bucket fixed-bucket array,
   spec §12 default buckets).
2. **Request metrics (§3)** — full set, including the histogram. The
   highest-value category; dashboards and alerts depend on it.
3. **Connection metrics (§9)** — extend `ConnectionMetrics` with
   `closed_total{reason}`, `streams_active`, `frame_send_total`,
   `frame_recv_total`, `frame_size_bytes` histogram.
4. **Worker metrics (§8)** — wire the existing `last_cycle_duration_ms`
   + `pending_work_estimate` counters through the exposition path.
5. **HNSW basic metrics (§6)** — `node_count`, `tombstone_count`,
   `tombstone_ratio`. Add a getter on `SharedHnsw` that's already
   trivially computable; rebuild metrics need Phase 11.3 wiring.
6. **Memory metrics (§4)** — basic aggregated counters via
   `ShardHandle::memory_snapshot()` returning
   `{ active, tombstoned, by_kind }`. Backed by existing redb scans.
7. **Storage metrics (§5)** — partial: `arena_capacity_bytes` and
   `arena_slots_used` (counters that already exist in the arena
   allocator). Defer `wal_size_bytes`, `metadata_size_bytes` to a
   follow-up — they require statfs-style introspection that isn't
   in the storage layer's API yet.
8. **Embedder metrics (§7)** — `EmbedderMetrics` struct in
   `brain-embed` with `calls_total`, `cache_hits_total`,
   `cache_misses_total`. Defer `duration_ms` histogram +
   `queue_depth` + `workers_active` to a follow-up; the embedder's
   batching surface needs more instrumentation hooks.
9. **Resource metrics (§10)** — `process_cpu_seconds_total`,
   `process_memory_resident_bytes`, `process_open_fds` via
   `/proc/self`. Defer `brain_executor_*` to Phase 11.3 (tracing
   pulls Glommio reactor metrics in that sub-task).
10. **`brain_config_info` (§17)** — labels-only gauge that exposes
    the loaded config's key knobs (shard_count, arena_capacity,
    hnsw_m, embedder_model).

**Deferred (with marker comments + follow-up tickets):**
- `brain_wal_size_bytes`, `brain_metadata_size_bytes` — storage
  introspection API doesn't exist yet (`phase-11/storage-stat-api`).
- `brain_hnsw_search_visits`, `brain_hnsw_recall_estimate`,
  `brain_hnsw_rebuild_*` — sampling infrastructure deferred
  (`phase-11/hnsw-sampling`).
- `brain_embedder_duration_ms`, `_queue_depth`, `_workers_active`
  — embedder needs internal instrumentation
  (`phase-11/embedder-instrumentation`).
- `brain_executor_latency_ms`, `_tasks_active` — Glommio executor
  metrics (`phase-11/glommio-reactor-metrics`); paired with 11.3.

The deferred set isn't a 501-marker scenario like the CLI's 501
pattern — these are metrics that simply don't appear in `/metrics`
yet. A comment block in `metrics/mod.rs` lists them with the same
`phase-11/<slug>` form, and Phase 11 follow-up sub-tasks pop them
off as the underlying primitives land.

**Result:** ~30 of ~50 spec'd metric families emitted end-to-end,
plus the full primitive infrastructure ready to absorb the rest.

---

## 2. New files

```
crates/brain-server/src/metrics/
├── mod.rs              # Registry + Prometheus exposition
├── counter.rs          # Counter<L> labeled atomic
├── gauge.rs            # Gauge<L> labeled atomic
├── histogram.rs        # 16-bucket fixed histogram
└── process.rs          # /proc/self resource counters

crates/brain-server/src/network/request_metrics.rs
                        # Per-op request counters + histogram

crates/brain-embed/src/metrics.rs
                        # EmbedderMetrics struct
```

**Why `brain-server/src/metrics/` and not a new crate?** The
exposition lives in `brain-server::admin`; the counters are owned by
the runtime crates. Pulling them into a separate crate creates a
trait abstraction with no second implementation. Plain modules are
the right call until v2 needs pluggable backends (statsd, opentelemetry
metrics, etc.).

**Why fixed 16-bucket histograms (not HDRHistogram)?** Spec §12
mandates exactly 14 bucket boundaries + 2 sentinels (`-Inf`/`+Inf`).
A fixed array is ~80 bytes per histogram and adds no dependencies.
HDRHistogram would let us derive any quantile but is overkill for
the bucket-only Prometheus exposition format.

---

## 3. Touchpoints (modifications)

| File | Change |
|---|---|
| `crates/brain-server/src/admin/mod.rs` | Replace inline `format_metrics` body with `MetricsRegistry::expose(state)`; remove hand-rolled writeln chain. |
| `crates/brain-server/src/network/connection.rs` | `ConnectionMetrics` gains `closed_total: AtomicU64`, `streams_active: AtomicU64`, `frame_send_total: AtomicU64`, `frame_recv_total: AtomicU64`. |
| `crates/brain-server/src/network/dispatch.rs` | Wrap each request path with `RequestTimer::start("encode")` → records to per-op counter + histogram. |
| `crates/brain-server/src/shard/mod.rs` | New `ShardRequest::MemorySnapshot { reply_tx }` and `::HnswSnapshot { reply_tx }`. New `ShardHandle::memory_snapshot()` / `::hnsw_snapshot()`. |
| `crates/brain-index/src/shared.rs` | `SharedHnsw` getter: `node_count`, `tombstone_count`. |
| `crates/brain-embed/src/service.rs` | Embedder gains `Arc<EmbedderMetrics>` field; bumps `calls_total` on every embed, `cache_hits_total` / `_misses_total` on cache lookup. |
| `crates/brain-server/src/admin/mod.rs::AdminState` | New `request_metrics: Arc<RequestMetrics>` and `embedder_metrics: Arc<EmbedderMetrics>` fields. |
| `crates/brain-server/src/main.rs` | Wire the new metrics structs at construction. |

---

## 4. Histogram primitive design

```rust
pub const DEFAULT_BUCKETS_MS: &[f64] = &[
    1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0,
    250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0,
    // +Inf sentinel is implicit (the overflow slot)
];

pub struct Histogram {
    buckets: [AtomicU64; 14],   // 13 buckets + 1 +Inf
    sum_ms_x1000: AtomicU64,    // sum scaled to micros to avoid float atomics
    count: AtomicU64,
}

impl Histogram {
    pub fn observe_ms(&self, value_ms: f64) { ... }
    pub fn expose(&self, name: &str, labels: &str, out: &mut String) { ... }
}
```

Sum tracked in micros (×1000) so we can use `AtomicU64::fetch_add`
without float compare-and-swap. Exposition divides by 1000 to print
ms-decimal.

---

## 5. Tests

**Per-primitive unit tests** (colocated):
- `Counter::inc` race-free under 100 threads.
- `Gauge::set`, `::dec` correctness.
- `Histogram::observe_ms` lands in correct bucket; sum/count
  accurate after 1000 observations.
- `Histogram::expose` emits valid Prometheus text format.

**Integration tests** (`crates/brain-server/tests/metrics.rs`):
- `/metrics` body contains every spec'd in-scope family.
- After 10 encode requests, `brain_request_total{op="encode",status="success"} == 10`.
- After embed cache miss + hit, `cache_hits` and `cache_misses` increment.
- HNSW node_count matches the encoded count.

---

## 6. Done when

- [ ] `brain-server/src/metrics/` module with Counter / Gauge /
      Histogram primitives, tested.
- [ ] `/metrics` body contains all in-scope families from §2.
- [ ] `brain-server/tests/metrics.rs` integration tests pass.
- [ ] Deferred set documented in `metrics/mod.rs` with
      `phase-11/<slug>` markers.
- [ ] Phase doc 11.1 ticked, deferred list noted.
- [ ] `just docker-verify` green.

---

## 7. Risks / open questions

- **Risk:** request-path instrumentation adds atomic operations to
  the hot loop. AtomicU64::fetch_add is ~2ns on modern hardware,
  but we should sanity-check the encode latency before/after with
  a simple `cargo bench` invocation if criterion benches exist.
- **Risk:** memory snapshot via redb scan is potentially slow on
  large shards. Mitigation: cache the snapshot in `ShardHandle` with
  a 30s TTL (spec §4: "sampled periodically (every 30s by default)").
- **Open Q:** label `shard` uses UUID in spec §15 but numeric index
  everywhere else in the 10.x routes. Plan continues with numeric
  index for consistency with the existing CLI; UUID labelling can
  be added by a metric_relabel_config in Prometheus.
- **Open Q:** the spec's §13 cardinality rule excludes agent-id
  labels. Plan honors this — no per-agent labels anywhere.
