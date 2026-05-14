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
use super::request::{RequestMetrics, OP_LABELS, STATUS_LABELS};
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
}

/// Render the full `/metrics` body. Async because per-shard
/// scheduler snapshots are awaited via the same flume request-channel
/// the rest of the admin layer uses.
pub async fn format(snap: &Snapshot<'_>) -> String {
    let mut s = String::with_capacity(4096);

    emit_build_info(&mut s, snap.build_info);
    emit_up(&mut s);
    emit_shards_total(&mut s, snap.shards);
    emit_connection_basic(&mut s, snap.connections);
    emit_process_uptime(&mut s, snap.started_at, snap.started_at_unix_secs);
    emit_worker_counters(&mut s, snap.shards).await;
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
