# Changelog

All notable changes to Brain. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the
project adheres to [SemVer](https://semver.org/spec/v2.0.0.html)
from `v1.0.0` onward.

## [Unreleased]

Pending the operator-run 48 h soak result and the
`phase-13-complete` + `phase-14-complete` + `v1.0.0` tags.

## v1.0.0 — TBD

The first stable release. Spec §16/08's 10 acceptance gates pass on
reference hardware; operator runbooks + Grafana dashboards +
Alertmanager rules ship in-tree. Wire protocol, on-disk format,
and SDK surface are stable under SemVer.

### Cognitive substrate

The complete v1 surface. Each cognitive operation has its own
opcode + handler + tests:

- **ENCODE** — store a memory with text, metadata, and optional
  pre-computed vector (spec §09/02).
- **RECALL** — k-NN search with optional filters and text fetch
  (§09/03).
- **PLAN** / **REASON** — multi-step query planning + execution
  over the graph (§09/04, §09/05).
- **FORGET** — soft / hard delete with tombstone grace (§09/06).
- **LINK** / **UNLINK** — edge management between memories
  (§09/07, §09/08).
- **TXN_BEGIN** / **TXN_COMMIT** / **TXN_ABORT** — transactional
  batches (§09/09).
- **SUBSCRIBE** / **UNSUBSCRIBE** / **CANCEL_STREAM** — server-
  pushed events on memory changes (§09/10).
- **ADMIN_\*** — snapshot, rebuild-ann, worker management,
  config read, agent listing, shard inventory (§14/06).

### Storage

- Memory-mapped arena with 1600-byte slots (1536-byte vector
  capacity + 64-byte metadata) — forward-compatible with larger
  embedding models. Spec §05/02.
- WAL-before-acknowledge with `O_DIRECT` + `pwritev2(RWF_DSYNC)`
  group commit. Spec §05/03.
- CRC32C on every WAL record and every arena slot.
- Recovery on startup; idempotent replay; spec §05/08.
- Snapshot / restore via the admin CLI.

### Index + embedder

- HNSW via `hnsw_rs` (M=16, ef_construction=200, ef_search=64).
  Spec §06.
- Persistent index format with shard-UUID coupling. Spec §06/04.
- Tombstone-aware search; rebuild trigger at ratio > 0.3
  (configurable via the `hnsw_maintenance` worker).
- BGE-small (384-dim) via `candle` for the default embedder; LRU
  cache; per-call fingerprinting. Spec §05.

### Server runtime

- Single Tokio multi-thread runtime accepts TCP/TLS; per-connection
  task handles HELLO/AUTH and dispatches frames.
- Per-shard Glommio `LocalExecutor` (thread-per-core, io_uring)
  owns the shard's arena + WAL + metadata + HNSW. Spec §10/02.
- Boundary primitive: `flume::Sender<ShardRequest>` from Tokio →
  Glommio. Spec §10/03.
- 12 background workers (decay, access_boost, consolidation,
  hnsw_maintenance, idempotency_cleanup, slot_reclamation,
  wal_retention, edge_scrub, counter_reconciliation,
  statistics_update, embedder_cache_eviction, snapshot). Spec §11.
- Graceful shutdown: drains connections, then shards. Spec §10/06.

### Network + protocol

- Wire protocol over TCP: framed binary, rkyv-encoded payloads,
  CRC32C on every frame. Spec §03.
- TLS termination via rustls (optional `tls_cert_file` / `_key_file`
  in config). Spec §03/02 §2.4.
- `brain-http` (Phase 11) — hyper-based HTTP/1.1 + WebSocket + SSE
  layer used by the admin server. Supports streaming bodies and
  Last-Event-ID reconnect.

### Observability

- Prometheus metrics on `/metrics`. Phase 12.1:
  - Build / config info, `up`, `shards_total`.
  - Request counters / in-flight gauge / duration histogram
    per op.
  - Connection lifecycle: active / total / closed-by-reason /
    frame send/recv counters.
  - Per-shard worker counters: cycles, processed, errors,
    last-run-unixtime.
  - HNSW basics: node_count, tombstone_count, tombstone_ratio.
  - Process resource: CPU, RSS, VMS, open FDs.
- JSON-structured logs via `tracing-subscriber` (Phase 12.2);
  `BRAIN_LOG` env filter; per-target level overrides.
- OpenTelemetry tracing via OTLP/HTTP (Phase 12.3); per-request
  `brain.request` span attached at the dispatch boundary;
  always_on / always_off / ratio / parent_based sampling.
- 8 reference Grafana dashboards (overview, per-shard, storage,
  HNSW, workers, network, errors, capacity). Phase 12.4.
- Prometheus alert rules covering spec §14/05 RB-1..RB-10.
  Phase 12.5.
- Operator guide at `docs/observability.md`. Phase 12.6.

### Tooling

- `brain` admin CLI: stats, health, snapshot create/list/delete,
  rebuild-ann, worker list/control, config get/reload/set, audit
  query/export, agent list/by-id, shard list/create/delete,
  diagnostics profile/debug-snapshot. Phase 10.
- `brain-sdk-rust`: typed client with connection pool, retry with
  exponential backoff + jitter, per-op builders, streaming async
  iterators, OTel-friendly observability hooks. Phase 10.
- Criterion benches per crate (router, SSE encoder, end-to-end,
  HNSW recall + insert, frame codec, crc32c, embedder throughput).
  Phase 13.1.
- `load_generator` example: sustained-rate SDK driver with
  configurable mix + CSV per-op output. Phase 13.2.
- `soak` example: long-duration drift detection (memory / latency /
  error-rate). Phase 13.4.

### Failure recovery

- WAL recovery is idempotent and CRC-validated; truncated writes
  recover to the last clean record (Phase 2 random_kill, 1000
  iterations).
- Bit-flip corruption: detected via CRC; recovery either errors
  out or stops at the bad record. Phase 13.3.
- I/O-fault during recovery: propagates the sink error; no
  half-applied state. Phase 13.3.

### Acceptance

- `acceptance/run.sh` runs spec §16/08's 10 release gates. Phase 14.1.
- 10 operator runbooks in `docs/runbooks/`. Phase 14.2.
- Install / configure / operate / upgrade guides in
  `docs/guides/`. Phase 14.3.

### Known limitations

- **Auth.** `[auth] mode = "none"` only; token / mTLS are deferred
  to a follow-up minor.
- **Wire-protocol traceparent.** Client→server OTel trace
  propagation requires a spec §03 amendment (tracker
  `phase-13/wire-traceparent`); v1 emits server-side spans only.
- **HNSW sampling metrics.** `search_visits`, `recall_estimate`,
  `rebuild_*` quantiles are documented in spec §14/01 §6 but
  require sampling primitives that aren't yet wired (tracker
  `phase-12/hnsw-sampling`).
- **Storage stat API.** `brain_arena_used_bytes`,
  `brain_wal_size_bytes`, `brain_metadata_size_bytes` need a
  brain-storage / brain-metadata getter (tracker
  `phase-12/storage-stat-api`).
- **Embedder hooks.** `brain_embedder_calls_total`, cache hits /
  misses, duration histogram await the production dispatcher
  landing on top of `NopDispatcher` (tracker
  `phase-12/embedder-instrumentation`).
- **Glommio executor latency.** `brain_executor_latency_ms` /
  `_tasks_active` need reactor instrumentation (tracker
  `phase-12/glommio-reactor-metrics`).
- **Audit log.** The 501 placeholder routes are wired in the admin
  CLI surface; the underlying primitive lands in a follow-up
  (tracker `phase-11/audit-log`).
- **Frame-size histogram.** `brain_frame_size_bytes` requires the
  `Histogram` primitive to become unit-agnostic (tracker
  `phase-12/histogram-unit-agnostic`).
- **Clustering.** v1 is single-host per shard. v2 will add
  quorum-based multi-host clustering; RB-10 scaffolds the v2
  procedure.

All trackers are documented inline in the source where the
deferred surface lives. No silent "TODO"s; every deferred family
carries a `phase-NN/<slug>` marker.

### Compatibility

- **Linux-only.** Kernel ≥ 5.15. macOS / Windows are dev-container
  only.
- **MSRV.** Rust stable, latest minus one (1.95 at release).
- **Wire-protocol version.** v1; future minor versions are
  backwards-compatible via the HELLO/WELCOME negotiation.

---

## Phase history

| Phase | Status | Tag |
|---|---|---|
| 0  Workspace skeleton | shipped | `phase-0-complete` |
| 1  Wire protocol + core types | shipped | `phase-1-complete` |
| 2  Storage: arena + WAL + recovery | shipped | `phase-2-complete` |
| 3  Metadata + redb | shipped | `phase-3-complete` |
| 4  ANN index (HNSW) | shipped | `phase-4-complete` |
| 5  Embedder | shipped | `phase-5-complete` |
| 6  Query planner | shipped | `phase-6-complete` |
| 7  Cognitive operations | shipped | `phase-7-complete` |
| 8  Background workers | shipped | `phase-8-complete` |
| 9  Server binary | shipped | `phase-9-complete` |
| 10 Rust SDK + admin CLI | shipped | `phase-10-complete` |
| 11 brain-http (HTTP / WS / SSE) | shipped | `phase-11-complete` |
| 12 Observability | shipped | `phase-12-complete` |
| 13 Benchmarks + chaos | scaffolded | pending `phase-13-complete` after 48 h soak |
| 14 Acceptance + release | scaffolded | pending `phase-14-complete` + `v1.0.0` |
