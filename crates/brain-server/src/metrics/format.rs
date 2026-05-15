//! Top-level Prometheus body assembler.
//!
//! [`format`] is the single entry point. Walks the supplied
//! [`Snapshot`] and produces the full `/metrics` body in a stable
//! order so dashboards / regex-based smoke tests stay deterministic.
//!
//! `Snapshot` is constructed by `admin::AdminState::metrics_snapshot`.
//! The indirection keeps `metrics::*` from depending on `admin::*`,
//! which matters for integration tests that mount metrics but not
//! admin (e.g. dispatch / connection / subscribe).

use std::fmt::Write as _;
use std::sync::atomic::Ordering;
use std::time::Instant;

use tracing::warn;

use super::exposition::{
    emit_counter_labeled, emit_gauge_labeled, emit_header, emit_histogram, emit_info, emit_scalar,
};
use super::process::ProcessSnapshot;
use super::request::{RequestMetrics, OP_LABELS, STATUS_LABELS};
use crate::config::Config;
use crate::connection::ConnectionMetrics;
use crate::shard::ShardHandle;

/// Compile-time build identifiers exposed via `brain_build_info`.
#[derive(Clone, Copy, Debug)]
pub struct BuildInfo {
    pub version: &'static str,
    pub git_commit: &'static str,
}

/// Loose-reference snapshot of everything the exposition reads.
/// Built by callers (e.g. `admin::AdminState::metrics_snapshot`) and
/// handed to [`format`]. Borrows everything to avoid clones on the
/// scrape path.
pub struct Snapshot<'a> {
    pub build_info: BuildInfo,
    pub started_at: Instant,
    pub started_at_unix_secs: u64,
    pub shards: &'a [ShardHandle],
    pub connections: &'a ConnectionMetrics,
    pub request_metrics: &'a RequestMetrics,
    /// 12.1c — read-only borrow of the loaded config, surfaces as
    /// `brain_config_info` labels.
    pub config: &'a Config,
}

/// Render the full `/metrics` body. Async because per-shard
/// scheduler snapshots are awaited via the same flume request-channel
/// the rest of the admin layer uses.
pub async fn format(snap: &Snapshot<'_>) -> String {
    let mut s = String::with_capacity(4096);

    emit_build_info(&mut s, snap.build_info);
    emit_config_info(&mut s, snap.config);
    emit_up(&mut s);
    emit_shards_total(&mut s, snap.shards);
    emit_connection_basic(&mut s, snap.connections);
    emit_process_uptime(&mut s, snap.started_at, snap.started_at_unix_secs);
    emit_process_resource(&mut s);
    emit_worker_counters(&mut s, snap.shards).await;
    emit_hnsw_counts(&mut s, snap.shards).await;
    emit_request_metrics(&mut s, snap.request_metrics);

    s
}

fn emit_build_info(out: &mut String, info: BuildInfo) {
    emit_header(out, "brain_build_info", "Build information.", "gauge");
    let labels = format!(
        "{{version=\"{v}\",git_commit=\"{g}\"}}",
        v = info.version,
        g = info.git_commit,
    );
    emit_info(out, "brain_build_info", &labels);
}

/// `brain_config_info` per spec §14/01 §17. Value is always 1; the
/// information rides in labels. Cardinality stays bounded because
/// every label value is a config knob, not user input.
fn emit_config_info(out: &mut String, cfg: &Config) {
    emit_header(
        out,
        "brain_config_info",
        "Loaded config — knobs that affect runtime behaviour.",
        "gauge",
    );
    let labels = format!(
        "{{shard_count=\"{sc}\",arena_capacity_bytes=\"{arena}\",hnsw_m=\"{m}\",embedder_model=\"{em}\"}}",
        sc = cfg.storage.shard_count,
        arena = cfg.shard.arena_capacity_bytes,
        m = cfg.hnsw.m,
        em = cfg.embedder.model,
    );
    emit_info(out, "brain_config_info", &labels);
}

fn emit_up(out: &mut String) {
    emit_header(
        out,
        "brain_up",
        "Server liveness; 1 if accepting requests.",
        "gauge",
    );
    let _ = writeln!(out, "brain_up 1");
}

fn emit_shards_total(out: &mut String, shards: &[ShardHandle]) {
    emit_header(
        out,
        "brain_shards_total",
        "Number of configured shards.",
        "gauge",
    );
    let _ = writeln!(out, "brain_shards_total {}", shards.len());
}

fn emit_connection_basic(out: &mut String, connections: &ConnectionMetrics) {
    emit_header(
        out,
        "brain_connections_active",
        "Currently in-flight client connections.",
        "gauge",
    );
    let _ = writeln!(
        out,
        "brain_connections_active {}",
        connections.active.load(Ordering::Relaxed),
    );

    emit_header(
        out,
        "brain_connections_total",
        "Total accepted client connections since startup.",
        "counter",
    );
    let _ = writeln!(
        out,
        "brain_connections_total {}",
        connections.total.load(Ordering::Relaxed),
    );

    // 12.7 (deferred-set burn-down): connection-extended families
    // per spec §14/01 §9. `brain_frame_size_bytes` histogram is
    // still deferred — tracker `phase-12/histogram-unit-agnostic`.
    emit_header(
        out,
        "brain_connections_closed_total",
        "Connections closed by reason.",
        "counter",
    );
    for (i, reason) in crate::connection::CLOSE_REASONS.iter().enumerate() {
        let labels = format!("{{reason=\"{reason}\"}}");
        let v = connections.closed_by_reason[i].load(Ordering::Relaxed);
        let _ = writeln!(out, "brain_connections_closed_total{labels} {v}");
    }

    emit_header(
        out,
        "brain_frame_send_total",
        "Total outbound frames since startup.",
        "counter",
    );
    let _ = writeln!(
        out,
        "brain_frame_send_total {}",
        connections.frame_send_total.load(Ordering::Relaxed),
    );

    emit_header(
        out,
        "brain_frame_recv_total",
        "Total inbound frames since startup.",
        "counter",
    );
    let _ = writeln!(
        out,
        "brain_frame_recv_total {}",
        connections.frame_recv_total.load(Ordering::Relaxed),
    );
}

/// 12.1c — `/proc/self`-derived resource metrics per spec §14/01 §10.
/// Sampled fresh on every scrape; missing fields are skipped so a
/// `/proc` access failure doesn't pollute dashboards with zeros.
fn emit_process_resource(out: &mut String) {
    let snap = ProcessSnapshot::capture();

    if let Some(secs) = snap.cpu_seconds {
        emit_header(
            out,
            "process_cpu_seconds_total",
            "Total process CPU time (user + system).",
            "counter",
        );
        // Sub-second precision: write as decimal.
        let _ = writeln!(out, "process_cpu_seconds_total {secs}");
    }
    if let Some(bytes) = snap.memory_resident_bytes {
        emit_header(
            out,
            "process_memory_resident_bytes",
            "Resident set size (RSS) of the process.",
            "gauge",
        );
        emit_scalar(out, "process_memory_resident_bytes", bytes);
    }
    if let Some(bytes) = snap.memory_virtual_bytes {
        emit_header(
            out,
            "process_memory_virtual_bytes",
            "Virtual memory size of the process.",
            "gauge",
        );
        emit_scalar(out, "process_memory_virtual_bytes", bytes);
    }
    if let Some(count) = snap.open_fds {
        emit_header(out, "process_open_fds", "Open file descriptors.", "gauge");
        emit_scalar(out, "process_open_fds", count);
    }
}

fn emit_process_uptime(out: &mut String, started_at: Instant, started_at_unix_secs: u64) {
    let uptime_secs = started_at.elapsed().as_secs();
    emit_header(
        out,
        "process_uptime_seconds",
        "Process uptime since admin server start.",
        "counter",
    );
    emit_scalar(out, "process_uptime_seconds", uptime_secs);

    emit_header(
        out,
        "process_start_time_seconds",
        "Unix timestamp of process start (seconds).",
        "gauge",
    );
    emit_scalar(out, "process_start_time_seconds", started_at_unix_secs);
}

async fn emit_worker_counters(out: &mut String, shards: &[ShardHandle]) {
    emit_header(
        out,
        "brain_worker_cycles_total",
        "Worker cycles completed.",
        "counter",
    );
    emit_header(
        out,
        "brain_worker_processed_total",
        "Items processed by the worker.",
        "counter",
    );
    emit_header(
        out,
        "brain_worker_errors_total",
        "Worker cycle errors.",
        "counter",
    );
    emit_header(
        out,
        "brain_worker_last_run_unixtime",
        "Unix-time of the worker's last cycle.",
        "gauge",
    );

    for shard in shards.iter() {
        let shard_id = shard.shard_id();
        match shard.scheduler_snapshot().await {
            Ok(snapshot) => {
                let mut workers = snapshot;
                workers.sort_by_key(|(name, _, _)| *name);
                for (name, _kind, snap) in workers {
                    let _ = writeln!(
                        out,
                        "brain_worker_cycles_total{{shard=\"{shard_id}\",worker=\"{name}\"}} {}",
                        snap.cycles_total
                    );
                    let _ = writeln!(
                        out,
                        "brain_worker_processed_total{{shard=\"{shard_id}\",worker=\"{name}\"}} {}",
                        snap.processed_total
                    );
                    let _ = writeln!(
                        out,
                        "brain_worker_errors_total{{shard=\"{shard_id}\",worker=\"{name}\"}} {}",
                        snap.errors_total
                    );
                    let _ = writeln!(
                        out,
                        "brain_worker_last_run_unixtime{{shard=\"{shard_id}\",worker=\"{name}\"}} {}",
                        snap.last_run_unix_secs
                    );
                }
            }
            Err(e) => {
                warn!(shard_id, error = %e, "scheduler_snapshot failed");
            }
        }
    }
}

/// 12.8: HNSW basic counters (node_count, tombstone_count,
/// tombstone_ratio) per spec §14/01 §6. Sampled per-shard via
/// `ShardHandle::hnsw_snapshot`. The richer §6 families
/// (search_visits, recall_estimate, rebuild_*) stay deferred —
/// tracker `phase-12/hnsw-sampling`.
async fn emit_hnsw_counts(out: &mut String, shards: &[ShardHandle]) {
    emit_header(
        out,
        "brain_hnsw_node_count",
        "Active HNSW node count.",
        "gauge",
    );
    emit_header(
        out,
        "brain_hnsw_tombstone_count",
        "Tombstoned HNSW node count.",
        "gauge",
    );
    emit_header(
        out,
        "brain_hnsw_tombstone_ratio",
        "Tombstoned / active+tombstoned ratio (0..1).",
        "gauge",
    );
    for shard in shards.iter() {
        let shard_id = shard.shard_id();
        match shard.hnsw_snapshot().await {
            Ok(c) => {
                let _ = writeln!(
                    out,
                    "brain_hnsw_node_count{{shard=\"{shard_id}\"}} {}",
                    c.node_count
                );
                let _ = writeln!(
                    out,
                    "brain_hnsw_tombstone_count{{shard=\"{shard_id}\"}} {}",
                    c.tombstone_count
                );
                let _ = writeln!(
                    out,
                    "brain_hnsw_tombstone_ratio{{shard=\"{shard_id}\"}} {}",
                    c.tombstone_ratio()
                );
            }
            Err(e) => {
                warn!(shard_id, error = %e, "hnsw_snapshot failed");
            }
        }
    }
}

/// 12.1b: per-op request counters / in-flight gauge / duration
/// histogram. Cross-references `crate::metrics::request`.
fn emit_request_metrics(out: &mut String, m: &RequestMetrics) {
    emit_header(
        out,
        "brain_request_total",
        "Total requests by operation and terminal status.",
        "counter",
    );
    for (op_idx, op) in OP_LABELS.iter().enumerate() {
        for (status_idx, status) in STATUS_LABELS.iter().enumerate() {
            let labels = format!("{{op=\"{op}\",status=\"{status}\"}}");
            emit_counter_labeled(
                out,
                "brain_request_total",
                &labels,
                m.total(op_idx, status_idx),
            );
        }
    }

    emit_header(
        out,
        "brain_request_active",
        "Requests currently in flight by operation.",
        "gauge",
    );
    for (op_idx, op) in OP_LABELS.iter().enumerate() {
        let labels = format!("{{op=\"{op}\"}}");
        emit_gauge_labeled(out, "brain_request_active", &labels, m.active_gauge(op_idx));
    }

    emit_header(
        out,
        "brain_request_duration_ms",
        "Request duration histogram (milliseconds) by operation.",
        "histogram",
    );
    for (op_idx, op) in OP_LABELS.iter().enumerate() {
        let inner = format!("op=\"{op}\"");
        emit_histogram(out, "brain_request_duration_ms", &inner, m.duration(op_idx));
    }
}
