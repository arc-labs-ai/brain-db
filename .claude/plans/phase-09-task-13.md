# Sub-task 9.13 — Health + metrics endpoints

**Reads:**
- `spec/14_observability_ops/01_metrics.md` (full): OpenMetrics
  format, naming conventions, labels (`shard`, `op`, low-cardinality
  rule), histograms vs summaries, `up{}`, `brain_build_info`,
  `/metrics` endpoint security.
- `spec/14_observability_ops/00_purpose.md` (for context on which
  signals are P1 vs nice-to-have).
- `crates/brain-server/src/config.rs` — already exposes
  `cfg.server.metrics_addr` (default `127.0.0.1:9091`).
- `crates/brain-workers/src/metrics.rs` — `WorkerMetrics` already
  carries the spec-prescribed atomics; we read them out.

**Phase doc:** orientation §11 sub-task **9.13**.

**Done when:** the server binds a separate HTTP listener on
`metrics_addr`; `/healthz` returns `200 OK` + a tiny body once the
core stack is ready; `/metrics` returns Prometheus-format exposition
covering the metrics that are *actually wired today*. Per-IP /
per-agent connection limits — listed as a 9.13 nice-to-have in the
9.10 plan — are deferred to a follow-up.

---

## 1. Scope vs. defer

Spec §14/01 lists ~50 metric families across request, memory,
storage, HNSW, embedder, worker, connection, resource. Most of these
require instrumentation that's not in place yet (per-op latency
histograms need 9.x request-tracing pass, storage gauges need
`ArenaFile::usage()` accessors, HNSW search-visit sampling needs
hnsw_rs internals, etc.).

9.13 emits only what's **already counted somewhere first-party**:

| Family | 9.13 status |
| ------ | ----------- |
| `brain_build_info` | ✓ ship — version/git from `env!` |
| `brain_up{shard}` | ✓ ship — always 1 once shard is alive |
| `brain_shards_total` | ✓ ship — `cfg.storage.shard_count` |
| `brain_connections_active` | ✓ ship — new atomic in `ConnectionListener` |
| `brain_connections_total` | ✓ ship — counter increments on accept |
| `process_uptime_seconds` | ✓ ship — `std::time::Instant` at startup |
| `brain_worker_*` (cycles/processed/errors) | ✓ ship — read `WorkerMetrics::snapshot()` from each shard's scheduler |
| `brain_request_*`, `brain_memory_*`, `brain_arena_*`, `brain_hnsw_*`, `brain_embedder_*` | defer — not yet instrumented end-to-end |
| Histograms (`*_duration_ms`, `*_size_bytes`) | defer — bucketing helpers not wired yet |
| Per-IP / per-agent connection limits | defer (was nice-to-have on 9.10's deferred list) |

This is realistic for the actual instrumentation state. Each
deferred family becomes a 1–2 LOC add-on once the underlying
counter exists. The 9.13 commit message will list them so a future
contributor doesn't repeat the survey.

---

## 2. The HTTP server choice

Spec §14/01 §1 calls out OpenMetrics over HTTP. Two options:

| Option | Verdict |
| ------ | ------- |
| Hand-roll minimal HTTP/1.1 over `tokio::net::TcpListener` | ✓ chosen — only two endpoints (`GET /healthz`, `GET /metrics`), spec is text-based and we already vendor Tokio. ~200 LOC. No new dependency. |
| Pull in `hyper` / `axum` | ✗ rejected for 9.13 — drags in ~50 deps for two endpoints; the wire format is trivial. If 9.x later needs richer admin endpoints, we revisit. |
| Pull in a Prometheus client crate (`prometheus`, `metrics-exporter-prometheus`) | ✗ rejected — same reasoning. Spec exposition format is a few `format!`s away; a registry crate adds complexity without saving lines for the v1 metric set. |

The hand-rolled server only accepts `GET <path>` requests on the
metrics port. Anything else returns 400. This is fine for Prometheus
scrapers and `curl /healthz` smoke checks.

---

## 3. Module layout

| File | Action | Approx LOC |
| ---- | ------ | ---------- |
| `crates/brain-server/src/admin.rs` | new — `AdminServer` + `serve_admin` + `/healthz` + `/metrics` handlers + Prometheus exposition format builder | ~400 |
| `crates/brain-server/src/connection.rs` | extend — `ConnectionMetrics { active: Arc<AtomicU64>, total: Arc<AtomicU64> }`; increment on accept; decrement on connection task exit | ~30 delta |
| `crates/brain-server/src/main.rs` | extend — build `AdminServer` alongside `ConnectionListener`; spawn both inside the runtime; collect `Vec<ShardHandle>` + `Vec<Arc<WorkerScheduler>>` into the admin server's read-only state | ~50 delta |
| `crates/brain-server/src/shard.rs` | extend — expose `ShardHandle::scheduler_snapshot()` returning per-worker `WorkerMetrics::Snapshot`s (one shard request that the main loop fills synchronously, like `Ping`) | ~80 delta |
| `crates/brain-server/tests/admin.rs` | new — 4 integration tests: healthz, metrics root, build_info, worker counts after a forced cycle | ~250 |

Total: ~810 LOC. Larger than 9.12 but smaller than 9.10. Single
commit.

---

## 4. AdminServer shape

```rust
pub struct AdminServer {
    listen_addr: SocketAddr,
    state: Arc<AdminState>,
    shutdown: ShutdownSignal,
}

pub struct AdminState {
    started_at: Instant,
    build_info: BuildInfo,           // version + git commit
    shard_count: usize,
    shards: Arc<Vec<ShardHandle>>,   // for scheduler snapshots
    connections: Arc<ConnectionMetrics>,
}

pub struct ConnectionMetrics {
    pub active: AtomicU64,
    pub total:  AtomicU64,
}

pub struct BuildInfo {
    pub version: &'static str,        // env!("CARGO_PKG_VERSION")
    pub git_commit: &'static str,     // env!("VERGEN_GIT_SHA").unwrap_or("unknown")
    pub build_unix_secs: u64,
}

impl AdminServer {
    pub fn new(addr: SocketAddr, state: Arc<AdminState>, shutdown: ShutdownSignal) -> Self;
    pub async fn serve(self) -> io::Result<()>;     // bind + accept loop
}
```

`serve` runs the same shape as `ConnectionListener::serve`:
`tokio::select! { shutdown | accept }`. Per-request task reads one
HTTP/1.1 request, dispatches to the right handler, writes the
response, closes.

### 4.1 `/healthz`

```
HTTP/1.1 200 OK\r\n
content-type: text/plain; charset=utf-8\r\n
content-length: N\r\n
\r\n
ok\n
```

For 9.13 we always return `ok` once `AdminServer` is serving — the
process is reachable. A v2 enhancement could check shard
heartbeat, WAL writer state, etc., and return 503 on failures.

### 4.2 `/metrics`

Body assembled by `format_metrics(state: &AdminState) -> String`.
Format follows spec §14/01:

```text
# HELP brain_build_info Build information.
# TYPE brain_build_info gauge
brain_build_info{version="0.1.0",git_commit="<sha>"} 1

# HELP brain_up Server liveness; 1 if accepting requests.
# TYPE brain_up gauge
brain_up 1

# HELP brain_shards_total Number of configured shards.
# TYPE brain_shards_total gauge
brain_shards_total 4

# HELP brain_connections_active Currently in-flight client connections.
# TYPE brain_connections_active gauge
brain_connections_active 2

# HELP brain_connections_total Total accepted client connections since startup.
# TYPE brain_connections_total counter
brain_connections_total 17

# HELP process_uptime_seconds Process uptime since admin server start.
# TYPE process_uptime_seconds counter
process_uptime_seconds 42

# HELP brain_worker_cycles_total Worker cycles completed.
# TYPE brain_worker_cycles_total counter
brain_worker_cycles_total{shard="0",worker="decay"} 7
brain_worker_processed_total{shard="0",worker="decay"} 0
brain_worker_errors_total{shard="0",worker="decay"} 0
...
```

Worker counts come from `ShardHandle::scheduler_snapshot()` — a new
shard op that returns `Vec<(WorkerKind, WorkerMetrics::Snapshot)>`.
The shard side reads `scheduler.metrics_snapshot()` (new method on
`WorkerScheduler`).

`scheduler.metrics_snapshot()` iterates registered workers and
returns each one's `WorkerMetrics::snapshot()`. Already feasible
from the field state — no new instrumentation needed.

---

## 5. Shard-side: SchedulerSnapshot

`ShardRequest` gains:

```rust
SchedulerSnapshot {
    reply_tx: Sender<Vec<(WorkerKind, brain_workers::MetricsSnapshot)>>,
},
```

And `ShardHandle`:

```rust
pub async fn scheduler_snapshot(&self)
    -> Result<Vec<(WorkerKind, brain_workers::MetricsSnapshot)>, ShardError>;
```

The handler in `shard_main_loop` calls
`shard.scheduler.as_ref().unwrap().metrics_snapshot()`.

`WorkerScheduler::metrics_snapshot` is new — currently no method
exposes the metrics. The implementation:

```rust
pub fn metrics_snapshot(&self) -> Vec<(WorkerKind, MetricsSnapshot)> {
    self.handles.iter()
        .map(|h| (h.kind, h.metrics.snapshot()))
        .collect()
}
```

(Assumes `WorkerHandle` carries `metrics: Arc<WorkerMetrics>` and
`kind: WorkerKind` — confirm during impl.)

---

## 6. Connection-side: counter

`ConnectionListener` gains:

```rust
struct BoundConnectionListener {
    // ... existing fields ...
    connections: Arc<ConnectionMetrics>,
}
```

In `serve`'s accept loop:

```rust
let connections = self.connections.clone();
connections.total.fetch_add(1, Ordering::Relaxed);
connections.active.fetch_add(1, Ordering::Relaxed);
tokio::spawn(async move {
    let _guard = ConnectionGuard(connections);  // decrement on drop
    serve_connection(stream, topology, event_hub, limits, shutdown).await
});
```

`ConnectionGuard` is a tiny RAII struct that decrements `active` on
drop. Works whether the connection task panics, errors, or returns
normally.

`AdminServer` shares the `Arc<ConnectionMetrics>`; the metrics
endpoint reads the live atomics every scrape.

---

## 7. main.rs wire-up

```rust
let connections = Arc::new(ConnectionMetrics::default());
let admin_state = Arc::new(AdminState {
    started_at: Instant::now(),
    build_info: BuildInfo::from_env(),
    shard_count: cfg.storage.shard_count,
    shards: shards.clone(),
    connections: connections.clone(),
});

let listener = ConnectionListener::new(/* …, connections.clone() */);
let admin = AdminServer::new(cfg.server.metrics_addr, admin_state, shutdown.clone());

let admin_handle = tokio::spawn(admin.serve());
let bound = listener.bind()?;
let serve_result = bound.serve().await;

// On shutdown, both observe the watch; admin exits then bound exits.
let _ = admin_handle.await;
```

Both servers share the same `ShutdownSignal` so a single ctrl-c
brings both down.

---

## 8. Tests (`tests/admin.rs`, 4 cases)

The scaffold spawns an `AdminServer` with a synthetic `AdminState`
(no shards required for the basic tests; the worker-counts test
runs against real shards).

1. **`healthz_returns_ok`** — `GET /healthz` → `200 OK` + body `ok`.
2. **`metrics_emits_build_info_and_up`** — `GET /metrics` → 200 OK
   + body contains `brain_build_info{...} 1` and `brain_up 1`.
3. **`metrics_increments_connections_total_on_accept`** —
   spawn a real `ConnectionListener` + `AdminServer`; connect to
   the connection listener twice; observe
   `brain_connections_total = 2` in the metrics body.
4. **`metrics_emits_worker_counters`** — spawn a real shard,
   observe one row per worker (12 from 9.7b) in the metrics body.
   Asserts `brain_worker_cycles_total{shard="0",worker="decay"}`
   is *present* (counter starts at 0 for sleeping workers — that's
   fine; the presence proves the wire works).

A 5th test for `bad_path_returns_400` is a nice-to-have if the
hand-rolled HTTP parser stays compact enough.

---

## 9. Risks

| Risk | Mitigation |
| ---- | ---------- |
| Hand-rolled HTTP parser is buggy | Bound it tightly: only `GET /healthz` and `GET /metrics`; reject everything else with 400; cap header bytes at 8 KiB. Tests cover the happy path + bad path. |
| Connection counter doesn't drop on panic | `ConnectionGuard` Drop impl handles all exit paths (Ok, Err, panic). |
| `WorkerScheduler::metrics_snapshot` requires field access we don't have | Confirm `WorkerHandle.kind` + `WorkerHandle.metrics` during impl. If those aren't exposed, add a tiny accessor method. |
| Histogram families are absent → ops dashboards miss latency | Acceptable for 9.13 — the spec's bucketing scheme isn't yet plumbed end-to-end. Tracked as a follow-up. 9.13 emits enough to monitor liveness + worker progress; latency dashboards come with the wire-tracing pass. |
| Admin port collision with data plane | Default config separates them (`9091` vs `7474`); test scaffolds use `:0` ephemeral binding. |
| Per-IP / per-agent connection limits (originally 9.13 nice-to-have) | Deferred — needs IP→agent mapping that the AUTH pass doesn't cleanly hand off yet. File for a follow-up after 9.14. |
| HTTP/1.1 keepalive / chunked encoding edge cases | Reject `Transfer-Encoding: chunked`; respond `Connection: close` and close after one request. Prometheus scrapers are happy with this. |

---

## 10. Done criteria

- [ ] `crates/brain-server/src/admin.rs` ships `AdminServer` + `AdminState` + Prometheus exposition format builder.
- [ ] `ConnectionListener` carries `Arc<ConnectionMetrics>`; accept loop increments; `ConnectionGuard` decrements on connection-task drop.
- [ ] `ShardRequest::SchedulerSnapshot` + `ShardHandle::scheduler_snapshot()` work.
- [ ] `WorkerScheduler::metrics_snapshot()` returns per-worker `(WorkerKind, MetricsSnapshot)` tuples.
- [ ] `main.rs::linux_main::run` spawns admin + connection servers under the same `ShutdownSignal`.
- [ ] 4 integration tests in `tests/admin.rs` pass.
- [ ] All prior wire tests still pass.
- [ ] `just docker-verify` green workspace-wide.
- [ ] Phase doc 9.13 marked `[x]`.

---

## 11. What 9.13 explicitly defers

- **Per-op latency histograms** (`brain_request_duration_ms{op=…}`) — needs request-tracing pass.
- **Storage gauges** (`brain_arena_*`, `brain_wal_*`, `brain_metadata_size_bytes`) — need accessors on `ArenaFile` / `Wal` / `MetadataDb`.
- **HNSW health gauges** (`brain_hnsw_node_count`, `tombstone_ratio`) — `SharedHnsw` has `.len()` + `.tombstone_count()`; deferred only because they need a `ShardHandle` accessor (small follow-up).
- **Embedder gauges** (`brain_embedder_*`) — no `Dispatcher` is real yet (NopDispatcher stub).
- **Per-IP / per-agent connection limits** — was on 9.10's nice-to-have list; needs IP→agent mapping.
- **`/metrics` auth** — spec §14/01 §19 marks the endpoint unauthenticated by default; production deployments firewall the port. v1 ships unauthenticated.
- **OpenMetrics richer features** — exemplars, metadata, etc.; we emit the Prometheus dialect subset.
- **Cardinality safeguards** (rejecting agent_id labels) — irrelevant in v1 since we never emit those.

---

*Implement on approval.*
