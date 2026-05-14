//! Metrics primitives + Prometheus exposition.
//!
//! Phase 12 — sub-task 12.1a. Spec §14/01.
//!
//! ## Layout
//!
//! - [`counter`] / [`gauge`] / [`histogram`] — atomic primitives, the
//!   on-heap data that runtime code mutates.
//! - [`exposition`] — wire-format helpers shared by every emit site
//!   (HELP / TYPE headers, labelled / labelless lines).
//! - [`format`] — the entry point: walks the [`crate::admin::AdminState`]
//!   and produces the full Prometheus body for `/metrics`.
//!
//! ## Scope of 12.1a
//!
//! This module replaces the writeln-chain that lived in
//! `admin/handlers/metrics.rs` with typed primitives. The metric
//! families emitted are **identical** to the pre-12.1a body:
//!
//! - `brain_build_info` (info gauge)
//! - `brain_up` (gauge)
//! - `brain_shards_total` (gauge)
//! - `brain_connections_active` / `brain_connections_total` (gauge / counter)
//! - `process_uptime_seconds` / `process_start_time_seconds`
//! - `brain_worker_cycles_total` / `_processed_total` / `_errors_total` /
//!   `_last_run_unixtime` (per-shard, per-worker counters / gauge)
//!
//! Sub-tasks 12.1b and 12.1c add the request / connection-extended /
//! HNSW / embedder / memory / process families and the deferred set
//! documented below.
//!
//! ## Deferred metric families
//!
//! These are listed in the 12.1 plan; they emerge as the
//! corresponding primitives land:
//!
//! - `brain_wal_size_bytes`, `brain_metadata_size_bytes` — needs a
//!   storage-stat API. Tracker: `phase-12/storage-stat-api`.
//! - `brain_hnsw_search_visits`, `brain_hnsw_recall_estimate`,
//!   `brain_hnsw_rebuild_*` quantiles — sampling infrastructure.
//!   Tracker: `phase-12/hnsw-sampling`.
//! - `brain_embedder_duration_ms`, `_queue_depth`, `_workers_active`
//!   — embedder needs internal instrumentation hooks. Tracker:
//!   `phase-12/embedder-instrumentation`.
//! - `brain_executor_latency_ms`, `_tasks_active` — Glommio reactor
//!   metrics; paired with task 12.3 (OTel). Tracker:
//!   `phase-12/glommio-reactor-metrics`.

#![cfg(target_os = "linux")]
// Gauge accessors and a few exposition helpers are scaffolding that
// 12.1c (connection-extended / process metrics) lights up. The
// blanket allow retires when that lands.
#![allow(dead_code)]

pub mod counter;
pub mod exposition;
pub mod format;
pub mod gauge;
pub mod histogram;
pub mod request;
