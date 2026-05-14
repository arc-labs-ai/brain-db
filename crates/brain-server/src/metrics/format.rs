//! Top-level Prometheus body assembler.
//!
//! `format(&AdminState) -> String` is the single entry point. Walks
//! every metric family the admin layer is responsible for emitting in
//! a stable order so dashboards / regex-based smoke tests stay
//! deterministic.

use std::fmt::Write as _;
use std::sync::atomic::Ordering;

use tracing::warn;

use super::exposition::{emit_header, emit_info, emit_scalar};
use crate::admin::AdminState;

/// Render the full `/metrics` body for the supplied state. Async
/// because per-shard scheduler snapshots are awaited via the same
/// flume request-channel the rest of the admin layer uses.
pub async fn format(state: &AdminState) -> String {
    let mut s = String::with_capacity(4096);

    emit_build_info(&mut s, state);
    emit_up(&mut s);
    emit_shards_total(&mut s, state);
    emit_connection_basic(&mut s, state);
    emit_process_uptime(&mut s, state);
    emit_worker_counters(&mut s, state).await;

    s
}

fn emit_build_info(out: &mut String, state: &AdminState) {
    emit_header(out, "brain_build_info", "Build information.", "gauge");
    let labels = format!(
        "{{version=\"{v}\",git_commit=\"{g}\"}}",
        v = state.build_info.version,
        g = state.build_info.git_commit,
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

fn emit_shards_total(out: &mut String, state: &AdminState) {
    emit_header(
        out,
        "brain_shards_total",
        "Number of configured shards.",
        "gauge",
    );
    let _ = writeln!(out, "brain_shards_total {}", state.shards.len());
}

fn emit_connection_basic(out: &mut String, state: &AdminState) {
    emit_header(
        out,
        "brain_connections_active",
        "Currently in-flight client connections.",
        "gauge",
    );
    let _ = writeln!(
        out,
        "brain_connections_active {}",
        state.connections.active.load(Ordering::Relaxed),
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
        state.connections.total.load(Ordering::Relaxed),
    );
}

fn emit_process_uptime(out: &mut String, state: &AdminState) {
    let uptime_secs = state.started_at.elapsed().as_secs();
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
    emit_scalar(
        out,
        "process_start_time_seconds",
        state.started_at_unix_secs,
    );
}

async fn emit_worker_counters(out: &mut String, state: &AdminState) {
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

    for shard in state.shards.iter() {
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
