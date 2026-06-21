//! Metrics primitives + Prometheus exposition.
//!
//! ## Layout
//!
//! - [`counter`] / [`gauge`] / [`histogram`] — atomic primitives, the
//!   on-heap data that runtime code mutates.
//! - [`exposition`] — wire-format helpers shared by every emit site
//!   (HELP / TYPE headers, labelled / labelless lines).
//! - [`mod@format`] — the entry point: walks the
//!   [`crate::admin::AdminState`] and produces the full Prometheus
//!   body for `/metrics`.
//!
//! ## Emitted families
//!
//! - `brain_build_info` (info gauge)
//! - `brain_up` (gauge)
//! - `brain_shards_total` (gauge)
//! - `brain_connections_active` / `brain_connections_total` (gauge / counter)
//! - `process_uptime_seconds` / `process_start_time_seconds`
//! - `brain_worker_cycles_total` / `_processed_total` / `_errors_total` /
//!   `_last_run_unixtime` (per-shard, per-worker counters / gauge)
//! - `brain_wal_size_bytes` / `brain_wal_segments` /
//!   `brain_metadata_size_bytes` / `brain_arena_*` (per-shard storage
//!   gauges).
//! - request / connection-extended / HNSW / embedder / memory / process
//!   families.
//!
//! ## Deferred metric families
//!
//! These emerge as the corresponding primitives land:
//!
//! - `brain_hnsw_search_visits`, `brain_hnsw_recall_estimate`,
//!   `brain_hnsw_rebuild_*` quantiles — sampling infrastructure.
//! - `brain_embedder_duration_ms`, `_queue_depth`, `_workers_active`
//!   — embedder needs internal instrumentation hooks.
//! - `brain_executor_latency_ms`, `_tasks_active` — Glommio reactor
//!   metrics.

#![cfg(target_os = "linux")]
// `gauge::Gauge::set` and the labelless `emit_gauge` helper remain
// unused by current emit sites (process gauges are scalars; request
// gauges use `inc`/`dec`). Connection-extended metrics
// (`brain_connections_closed_total{reason=...}` etc.) light them up.
#![allow(dead_code)]

pub mod counter;
pub mod exposition;
pub mod format;
pub mod gauge;
pub mod histogram;
pub mod otel;
pub mod process;
pub mod request;
