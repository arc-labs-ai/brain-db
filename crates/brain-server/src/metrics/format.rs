//! Top-level Prometheus body assembler.
//!
//! [`format()`] is the single entry point. Walks the supplied
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
/// handed to [`format()`]. Borrows everything to avoid clones on the
/// scrape path.
pub struct Snapshot<'a> {
    pub build_info: BuildInfo,
    pub started_at: Instant,
    pub started_at_unix_secs: u64,
    pub shards: &'a [ShardHandle],
    pub connections: &'a ConnectionMetrics,
    pub request_metrics: &'a RequestMetrics,
    /// Read-only borrow of the loaded config, surfaces as
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
    emit_derivation_admission(&mut s);
    emit_connection_basic(&mut s, snap.connections);
    emit_process_uptime(&mut s, snap.started_at, snap.started_at_unix_secs);
    emit_process_resource(&mut s);
    emit_worker_counters(&mut s, snap.shards).await;
    emit_hnsw_counts(&mut s, snap.shards).await;
    emit_storage_gauges(&mut s, snap.shards).await;
    emit_extraction_queue_gauges(&mut s, snap.shards).await;
    emit_request_metrics(&mut s, snap.request_metrics);
    emit_auto_edge_metrics(&mut s, snap.shards);
    emit_extractor_metrics(&mut s, snap.shards);
    emit_temporal_edge_metrics(&mut s, snap.shards);
    emit_causal_edge_metrics(&mut s, snap.shards);
    emit_statement_embed_metrics(&mut s, snap.shards);
    emit_retriever_metrics(&mut s, snap.shards);
    emit_query_metrics(&mut s, snap.shards);
    emit_read_degrade_metrics(&mut s);
    emit_writer_metrics(&mut s, snap.shards);
    emit_write_stage_metrics(&mut s, snap.shards);
    emit_aggregate_metrics(&mut s, snap.shards);
    emit_tracing_metrics(&mut s);

    s
}

/// OpenTelemetry trace-pipeline self-metrics. Process-global (the OTel
/// error handler is a singleton), so this reads the shared counters
/// directly rather than from the per-server `Snapshot`.
fn emit_tracing_metrics(out: &mut String) {
    let m = super::otel::global();
    emit_header(
        out,
        "brain_tracing_spans_dropped_total",
        "Spans dropped because the OTLP export buffer was full.",
        "counter",
    );
    let _ = writeln!(
        out,
        "brain_tracing_spans_dropped_total {}",
        m.spans_dropped.get()
    );

    emit_header(
        out,
        "brain_tracing_export_errors_total",
        "OTLP export attempts that failed or timed out.",
        "counter",
    );
    let _ = writeln!(
        out,
        "brain_tracing_export_errors_total {}",
        m.export_errors.get()
    );
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

/// `brain_config_info`. Value is always 1; the
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
        sc = cfg.storage.shards(),
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

/// What provider calls are holding right now, and the most they ever held.
///
/// Process-global, with no shard label: shards are share-nothing for data but
/// face one provider over one connection pool, so this pressure belongs to the
/// process.
///
/// `peak_bytes` is the operative number. Derivation admission is bounded by a
/// hand-chosen count today, which cannot use a large machine and says nothing
/// about what a call costs on a small one — a long text with a big response
/// schema and a one-liner are not the same load at the same count. Sizing a
/// memory budget to replace that bound needs the footprint measured first,
/// and this is that measurement.
fn emit_derivation_admission(out: &mut String) {
    let meter = brain_llm::inflight::global();
    emit_header(
        out,
        "brain_llm_calls_in_flight",
        "Provider calls currently outstanding.",
        "gauge",
    );
    let _ = writeln!(out, "brain_llm_calls_in_flight {}", meter.llm().holds());
    emit_header(
        out,
        "brain_llm_bytes_in_flight",
        "Request payload bytes currently outstanding across provider calls.",
        "gauge",
    );
    let _ = writeln!(out, "brain_llm_bytes_in_flight {}", meter.llm().bytes());
    emit_header(
        out,
        "brain_llm_peak_calls_in_flight",
        "Most provider calls ever outstanding at once, since start.",
        "gauge",
    );
    let _ = writeln!(
        out,
        "brain_llm_peak_calls_in_flight {}",
        meter.llm().peak_holds()
    );
    let admission = brain_llm::admission::global();
    emit_header(
        out,
        "brain_derivation_admission_budget_bytes",
        "Bytes derivation may hold at once, across BOTH in-flight provider payloads and prepared context.",
        "gauge",
    );
    let _ = writeln!(
        out,
        "brain_derivation_admission_budget_bytes {}",
        admission.budget_bytes()
    );
    emit_header(
        out,
        "brain_derivation_admission_available_bytes",
        "Budget still admittable right now; sustained zero means derivation is waiting on memory.",
        "gauge",
    );
    let _ = writeln!(
        out,
        "brain_derivation_admission_available_bytes {}",
        admission.available_bytes()
    );
    emit_header(
        out,
        "brain_llm_peak_bytes_in_flight",
        "Most request payload bytes ever outstanding at once, since start.",
        "gauge",
    );
    let _ = writeln!(
        out,
        "brain_llm_peak_bytes_in_flight {}",
        meter.llm().peak_bytes()
    );

    // Prep occupancy, metered apart from provider payloads because the
    // remedies differ: payload pressure is relieved by narrowing LLM
    // fan-out, prep pressure by narrowing the batch or the neighbour
    // fan-out that feeds it. They are admitted against ONE budget, so
    // the combined gauge below is the figure that budget actually
    // bounds — neither pool alone tells an operator how close
    // derivation is to parking.
    emit_header(
        out,
        "brain_derivation_prep_holds_in_flight",
        "Prepared-context reservations currently outstanding.",
        "gauge",
    );
    let _ = writeln!(
        out,
        "brain_derivation_prep_holds_in_flight {}",
        meter.prep().holds()
    );
    emit_header(
        out,
        "brain_derivation_prep_bytes_in_flight",
        "Bytes of prepared context (neighbour texts, candidate embeddings) currently held.",
        "gauge",
    );
    let _ = writeln!(
        out,
        "brain_derivation_prep_bytes_in_flight {}",
        meter.prep().bytes()
    );
    emit_header(
        out,
        "brain_derivation_prep_peak_bytes_in_flight",
        "Most prepared-context bytes ever held at once, since start.",
        "gauge",
    );
    let _ = writeln!(
        out,
        "brain_derivation_prep_peak_bytes_in_flight {}",
        meter.prep().peak_bytes()
    );
    emit_header(
        out,
        "brain_derivation_bytes_in_flight",
        "Payload plus prepared-context bytes held together; this is what the admission budget bounds.",
        "gauge",
    );
    let _ = writeln!(out, "brain_derivation_bytes_in_flight {}", meter.bytes());
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

    emit_header(
        out,
        "brain_connections_rejected_total",
        "Connections shed at accept time by the admission gate (global or per-IP cap).",
        "counter",
    );
    let _ = writeln!(
        out,
        "brain_connections_rejected_total {}",
        connections.rejected.load(Ordering::Relaxed),
    );

    // Connection-extended families: `brain_frame_size_bytes` histogram is
    // still deferred.
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

    // Frame size histograms (raw-mode; `_sum` is the true byte
    // total).
    emit_header(
        out,
        "brain_frame_size_bytes",
        "Per-frame wire size in bytes (header + payload), by direction.",
        "histogram",
    );
    emit_histogram(
        out,
        "brain_frame_size_bytes",
        "direction=\"send\"",
        &connections.frame_send_bytes,
    );
    emit_histogram(
        out,
        "brain_frame_size_bytes",
        "direction=\"recv\"",
        &connections.frame_recv_bytes,
    );
}

/// `/proc/self`-derived resource metrics.
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
        "Unix-time of the worker's last attempted cycle (success, error, or caught panic).",
        "gauge",
    );
    emit_header(
        out,
        "brain_worker_panics_total",
        "Worker cycles that panicked (a subset of errors_total). Nonzero is worth alerting on.",
        "counter",
    );
    emit_header(
        out,
        "brain_worker_pending_work",
        "Worker's last-observed estimate of outstanding work items.",
        "gauge",
    );
    emit_header(
        out,
        "brain_worker_cycle_duration_ms",
        "Duration of the worker's most recent successful cycle, in milliseconds.",
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
                    let _ = writeln!(
                        out,
                        "brain_worker_panics_total{{shard=\"{shard_id}\",worker=\"{name}\"}} {}",
                        snap.panics_total
                    );
                    let _ = writeln!(
                        out,
                        "brain_worker_pending_work{{shard=\"{shard_id}\",worker=\"{name}\"}} {}",
                        snap.pending_work_estimate
                    );
                    let _ = writeln!(
                        out,
                        "brain_worker_cycle_duration_ms{{shard=\"{shard_id}\",worker=\"{name}\"}} {}",
                        snap.last_cycle_duration_ms
                    );
                }
            }
            Err(e) => {
                warn!(shard_id, error = %e, "scheduler_snapshot failed");
            }
        }
    }
}

/// HNSW basic counters (node_count, tombstone_count,
/// tombstone_ratio). Sampled per-shard via
/// `ShardHandle::hnsw_snapshot`. The richer families
/// (search_visits, recall_estimate, rebuild_*) stay deferred.
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

/// Per-shard storage-footprint gauges. Sampled per-shard via
/// [`ShardHandle::storage_stats`], which stats the WAL directory and
/// `metadata.redb` and reads the live arena occupancy. Operators watch
/// these for "approaching capacity".
async fn emit_storage_gauges(out: &mut String, shards: &[ShardHandle]) {
    emit_header(
        out,
        "brain_wal_size_bytes",
        "Total bytes across the shard's WAL segment files.",
        "gauge",
    );
    emit_header(
        out,
        "brain_wal_segments",
        "Number of WAL segment files on disk.",
        "gauge",
    );
    emit_header(
        out,
        "brain_metadata_size_bytes",
        "Byte size of the shard's metadata.redb file.",
        "gauge",
    );
    emit_header(
        out,
        "brain_arena_capacity_bytes",
        "Addressable arena capacity in bytes.",
        "gauge",
    );
    emit_header(
        out,
        "brain_arena_used_bytes",
        "Bytes backing allocated arena slots (occupied + tombstoned).",
        "gauge",
    );
    emit_header(
        out,
        "brain_arena_slots_used",
        "Allocated arena slots (occupied + tombstoned).",
        "gauge",
    );
    emit_header(
        out,
        "brain_arena_slots_free",
        "Reclaimed arena slots on the free list, ready to reuse.",
        "gauge",
    );
    for shard in shards.iter() {
        let shard_id = shard.shard_id();
        match shard.storage_stats().await {
            Ok(st) => {
                let _ = writeln!(
                    out,
                    "brain_wal_size_bytes{{shard=\"{shard_id}\"}} {}",
                    st.wal_size_bytes
                );
                let _ = writeln!(
                    out,
                    "brain_wal_segments{{shard=\"{shard_id}\"}} {}",
                    st.wal_segments
                );
                let _ = writeln!(
                    out,
                    "brain_metadata_size_bytes{{shard=\"{shard_id}\"}} {}",
                    st.metadata_size_bytes
                );
                let _ = writeln!(
                    out,
                    "brain_arena_capacity_bytes{{shard=\"{shard_id}\"}} {}",
                    st.arena_capacity_bytes
                );
                let _ = writeln!(
                    out,
                    "brain_arena_used_bytes{{shard=\"{shard_id}\"}} {}",
                    st.arena_used_bytes
                );
                let _ = writeln!(
                    out,
                    "brain_arena_slots_used{{shard=\"{shard_id}\"}} {}",
                    st.arena_slots_used
                );
                let _ = writeln!(
                    out,
                    "brain_arena_slots_free{{shard=\"{shard_id}\"}} {}",
                    st.arena_slots_free
                );
            }
            Err(e) => {
                warn!(shard_id, error = %e, "storage_stats failed");
            }
        }
    }
}

/// Per-shard derivation backlog — the durable extraction queue, which
/// is where writes wait to become statements, entities and relations.
///
/// Separate family from `brain_extractor_*`: those are worker counters
/// that vanish when the worker is off, while the queue is a table on
/// disk that keeps growing exactly then. Depth is the leading indicator
/// (writes arrive far faster than derivation drains); age says how far
/// a workspace has already drifted from its own data.
///
/// A shard whose metadata read failed emits no rows rather than zeros —
/// `absent()` is a legible gap, a fabricated `0` is a silent all-clear.
async fn emit_extraction_queue_gauges(out: &mut String, shards: &[ShardHandle]) {
    emit_header(
        out,
        "brain_extraction_queue_depth",
        "Memories durably queued for derivation, awaiting the extractor worker.",
        "gauge",
    );
    emit_header(
        out,
        "brain_extraction_queue_oldest_age_seconds",
        "Seconds the oldest memory in the derivation queue has been waiting.",
        "gauge",
    );
    for shard in shards.iter() {
        let shard_id = shard.shard_id();
        match shard.extraction_backlog().await {
            Ok(Some(backlog)) => {
                let _ = writeln!(
                    out,
                    "brain_extraction_queue_depth{{shard=\"{shard_id}\"}} {}",
                    backlog.depth
                );
                let _ = writeln!(
                    out,
                    "brain_extraction_queue_oldest_age_seconds{{shard=\"{shard_id}\"}} {}",
                    backlog.oldest_age_secs
                );
            }
            Ok(None) => {
                warn!(
                    shard_id,
                    "extraction backlog unavailable; emitting no series"
                );
            }
            Err(e) => {
                warn!(shard_id, error = %e, "extraction_backlog failed");
            }
        }
    }
}

/// Per-shard AutoEdgeWorker metric family. Reads through
/// the metric handle attached to each `ShardHandle`. Shards with the
/// worker disabled emit no rows for that shard (no `0` placeholder —
/// PromQL distinguishes `absent()` from `0`).
fn emit_auto_edge_metrics(out: &mut String, shards: &[ShardHandle]) {
    emit_header(
        out,
        "brain_auto_edge_drops_total",
        "Encode-side enqueues dropped because the auto-edge channel was full.",
        "counter",
    );
    for shard in shards {
        if let Some(m) = shard.auto_edge_metrics() {
            let labels = format!("{{shard=\"{}\"}}", shard.shard_id());
            let _ = writeln!(
                out,
                "brain_auto_edge_drops_total{labels} {}",
                m.snapshot().drops_total
            );
        }
    }

    emit_header(
        out,
        "brain_auto_edge_edges_written_total",
        "Logical SimilarTo edges persisted by the AutoEdgeWorker.",
        "counter",
    );
    for shard in shards {
        if let Some(m) = shard.auto_edge_metrics() {
            let labels = format!("{{shard=\"{}\"}}", shard.shard_id());
            let _ = writeln!(
                out,
                "brain_auto_edge_edges_written_total{labels} {}",
                m.snapshot().edges_written_total
            );
        }
    }

    emit_header(
        out,
        "brain_auto_edge_cycle_duration_seconds",
        "Wall-clock duration of one AutoEdgeWorker cycle.",
        "histogram",
    );
    for shard in shards {
        if let Some(m) = shard.auto_edge_metrics() {
            let inner = format!("shard=\"{}\"", shard.shard_id());
            emit_worker_histogram(
                out,
                "brain_auto_edge_cycle_duration_seconds",
                &inner,
                &m.snapshot().cycle_duration_seconds,
            );
        }
    }

    emit_header(
        out,
        "brain_auto_edge_neighbours_found_per_cycle",
        "Above-threshold neighbours collected per AutoEdgeWorker cycle.",
        "histogram",
    );
    for shard in shards {
        if let Some(m) = shard.auto_edge_metrics() {
            let inner = format!("shard=\"{}\"", shard.shard_id());
            emit_worker_histogram(
                out,
                "brain_auto_edge_neighbours_found_per_cycle",
                &inner,
                &m.snapshot().neighbours_found_per_cycle,
            );
        }
    }
}

/// Per-shard TemporalEdgeWorker metric family. Mirrors the
/// AutoEdge emitter's shape.
fn emit_temporal_edge_metrics(out: &mut String, shards: &[ShardHandle]) {
    emit_header(
        out,
        "brain_temporal_edge_drops_total",
        "Encode-side enqueues dropped because the temporal-edge channel was full.",
        "counter",
    );
    for shard in shards {
        if let Some(m) = shard.temporal_edge_metrics() {
            let labels = format!("{{shard=\"{}\"}}", shard.shard_id());
            let _ = writeln!(
                out,
                "brain_temporal_edge_drops_total{labels} {}",
                m.snapshot().drops_total
            );
        }
    }

    emit_header(
        out,
        "brain_temporal_edge_edges_written_total",
        "Logical FollowedBy edges persisted by the TemporalEdgeWorker.",
        "counter",
    );
    for shard in shards {
        if let Some(m) = shard.temporal_edge_metrics() {
            let labels = format!("{{shard=\"{}\"}}", shard.shard_id());
            let _ = writeln!(
                out,
                "brain_temporal_edge_edges_written_total{labels} {}",
                m.snapshot().edges_written_total
            );
        }
    }

    emit_header(
        out,
        "brain_temporal_edge_skipped_total",
        "TemporalEdgeWorker enqueues that produced no edge, broken out by reason.",
        "counter",
    );
    for shard in shards {
        if let Some(m) = shard.temporal_edge_metrics() {
            let snap = m.snapshot();
            for (reason, value) in [
                ("no_prev", snap.skipped_no_prev),
                ("out_of_order", snap.skipped_out_of_order),
                ("tombstoned", snap.skipped_tombstoned),
                ("cross_session", snap.skipped_cross_session),
                ("window_exceeded", snap.skipped_window_exceeded),
            ] {
                let labels = format!("{{shard=\"{}\",reason=\"{reason}\"}}", shard.shard_id());
                let _ = writeln!(out, "brain_temporal_edge_skipped_total{labels} {value}");
            }
        }
    }

    emit_header(
        out,
        "brain_temporal_edge_cycle_duration_seconds",
        "Wall-clock duration of one TemporalEdgeWorker cycle.",
        "histogram",
    );
    for shard in shards {
        if let Some(m) = shard.temporal_edge_metrics() {
            let inner = format!("shard=\"{}\"", shard.shard_id());
            emit_worker_histogram(
                out,
                "brain_temporal_edge_cycle_duration_seconds",
                &inner,
                &m.snapshot().cycle_duration_seconds,
            );
        }
    }

    emit_header(
        out,
        "brain_temporal_edge_gap_seconds",
        "Observed predecessor→memory gap distribution for FollowedBy edges.",
        "histogram",
    );
    for shard in shards {
        if let Some(m) = shard.temporal_edge_metrics() {
            let inner = format!("shard=\"{}\"", shard.shard_id());
            emit_worker_histogram(
                out,
                "brain_temporal_edge_gap_seconds",
                &inner,
                &m.snapshot().gap_seconds,
            );
        }
    }
}

/// Per-shard CausalEdgeWorker metric family. Mirrors the
/// temporal-edge emitter; adds a `predicate_whitelist_resolved` gauge
/// for operator triage on no-schema deployments where the worker
/// runs but never finds a causal predicate.
fn emit_causal_edge_metrics(out: &mut String, shards: &[ShardHandle]) {
    emit_header(
        out,
        "brain_causal_edge_drops_total",
        "Extractor-side enqueues dropped because the causal-edge channel was full.",
        "counter",
    );
    for shard in shards {
        if let Some(m) = shard.causal_edge_metrics() {
            let labels = format!("{{shard=\"{}\"}}", shard.shard_id());
            let _ = writeln!(
                out,
                "brain_causal_edge_drops_total{labels} {}",
                m.snapshot().drops_total
            );
        }
    }

    emit_header(
        out,
        "brain_causal_edge_edges_written_total",
        "Logical Caused edges persisted by the CausalEdgeWorker.",
        "counter",
    );
    for shard in shards {
        if let Some(m) = shard.causal_edge_metrics() {
            let labels = format!("{{shard=\"{}\"}}", shard.shard_id());
            let _ = writeln!(
                out,
                "brain_causal_edge_edges_written_total{labels} {}",
                m.snapshot().edges_written_total
            );
        }
    }

    emit_header(
        out,
        "brain_causal_edge_skipped_total",
        "CausalEdgeWorker enqueues that produced no edge, broken out by reason.",
        "counter",
    );
    for shard in shards {
        if let Some(m) = shard.causal_edge_metrics() {
            let snap = m.snapshot();
            for (reason, value) in [
                ("non_causal_predicate", snap.skipped_non_causal_predicate),
                ("low_confidence", snap.skipped_low_confidence),
                ("no_evidence", snap.skipped_no_evidence),
                ("object_not_entity", snap.skipped_object_not_entity),
                ("no_related_statement", snap.skipped_no_related_statement),
                ("statement_missing", snap.skipped_statement_missing),
            ] {
                let labels = format!("{{shard=\"{}\",reason=\"{reason}\"}}", shard.shard_id());
                let _ = writeln!(out, "brain_causal_edge_skipped_total{labels} {value}");
            }
        }
    }

    emit_header(
        out,
        "brain_causal_edge_predicate_whitelist_resolved",
        "Count of causal predicates actually resolved against the active schema on this shard. \
         0 means the worker has no causal vocabulary and every drained enqueue no-ops.",
        "gauge",
    );
    for shard in shards {
        if let Some(m) = shard.causal_edge_metrics() {
            let labels = format!("{{shard=\"{}\"}}", shard.shard_id());
            let _ = writeln!(
                out,
                "brain_causal_edge_predicate_whitelist_resolved{labels} {}",
                m.snapshot().predicate_whitelist_resolved
            );
        }
    }

    emit_header(
        out,
        "brain_causal_edge_cycle_duration_seconds",
        "Wall-clock duration of one CausalEdgeWorker cycle.",
        "histogram",
    );
    for shard in shards {
        if let Some(m) = shard.causal_edge_metrics() {
            let inner = format!("shard=\"{}\"", shard.shard_id());
            emit_worker_histogram(
                out,
                "brain_causal_edge_cycle_duration_seconds",
                &inner,
                &m.snapshot().cycle_duration_seconds,
            );
        }
    }
}

/// Per-shard StatementEmbedWorker metric family. The worker is
/// unconditional, so every shard contributes a row (no `Option`
/// dispatch like the channel-driven workers).
fn emit_statement_embed_metrics(out: &mut String, shards: &[ShardHandle]) {
    emit_header(
        out,
        "brain_statement_embed_cycles_total",
        "StatementEmbedWorker cycles executed (one per tick).",
        "counter",
    );
    for shard in shards {
        let snap = shard.statement_embed_metrics().snapshot();
        let labels = format!("{{shard=\"{}\"}}", shard.shard_id());
        let _ = writeln!(
            out,
            "brain_statement_embed_cycles_total{labels} {}",
            snap.cycles_total
        );
    }

    emit_header(
        out,
        "brain_statement_embed_rows_embedded_total",
        "Statements successfully inserted into the Statement HNSW.",
        "counter",
    );
    for shard in shards {
        let snap = shard.statement_embed_metrics().snapshot();
        let labels = format!("{{shard=\"{}\"}}", shard.shard_id());
        let _ = writeln!(
            out,
            "brain_statement_embed_rows_embedded_total{labels} {}",
            snap.rows_embedded_total
        );
    }

    emit_header(
        out,
        "brain_statement_embed_rows_skipped_total",
        "Queue rows the worker dropped without embedding (tombstoned, superseded, already in HNSW).",
        "counter",
    );
    for shard in shards {
        let snap = shard.statement_embed_metrics().snapshot();
        let labels = format!("{{shard=\"{}\"}}", shard.shard_id());
        let _ = writeln!(
            out,
            "brain_statement_embed_rows_skipped_total{labels} {}",
            snap.rows_skipped_total
        );
    }

    emit_header(
        out,
        "brain_statement_embed_errors_total",
        "Embedder batch failures during StatementEmbedWorker cycles.",
        "counter",
    );
    for shard in shards {
        let snap = shard.statement_embed_metrics().snapshot();
        let labels = format!("{{shard=\"{}\"}}", shard.shard_id());
        let _ = writeln!(
            out,
            "brain_statement_embed_errors_total{labels} {}",
            snap.embed_errors_total
        );
    }

    emit_header(
        out,
        "brain_statement_embed_batch_duration_seconds",
        "Wall-clock duration of one StatementEmbedWorker tick.",
        "histogram",
    );
    for shard in shards {
        let snap = shard.statement_embed_metrics().snapshot();
        let inner = format!("shard=\"{}\"", shard.shard_id());
        emit_worker_histogram(
            out,
            "brain_statement_embed_batch_duration_seconds",
            &inner,
            &snap.batch_duration_seconds,
        );
    }
}

/// Per-shard read-path retriever metric family. Labeled by
/// `retriever = semantic | lexical | graph` (a bounded enum — no
/// unbounded cardinality). Same dispatch shape as
/// [`emit_statement_embed_metrics`]; recorded by the RECALL handler
/// after each `execute`.
fn emit_retriever_metrics(out: &mut String, shards: &[ShardHandle]) {
    emit_header(
        out,
        "brain_retriever_invocations_total",
        "Times each retriever lane was invoked while serving a recall.",
        "counter",
    );
    for shard in shards {
        let snap = shard.retriever_metrics().snapshot();
        for (idx, retriever) in brain_ops::RETRIEVER_LABELS.iter().enumerate() {
            let labels = format!(
                "{{shard=\"{}\",retriever=\"{retriever}\"}}",
                shard.shard_id()
            );
            let _ = writeln!(
                out,
                "brain_retriever_invocations_total{labels} {}",
                snap.invocations_total[idx]
            );
        }
    }

    emit_header(
        out,
        "brain_retriever_candidates_total",
        "Candidates each retriever lane returned across served recalls.",
        "counter",
    );
    for shard in shards {
        let snap = shard.retriever_metrics().snapshot();
        for (idx, retriever) in brain_ops::RETRIEVER_LABELS.iter().enumerate() {
            let labels = format!(
                "{{shard=\"{}\",retriever=\"{retriever}\"}}",
                shard.shard_id()
            );
            let _ = writeln!(
                out,
                "brain_retriever_candidates_total{labels} {}",
                snap.candidates_total[idx]
            );
        }
    }

    emit_header(
        out,
        "brain_retriever_latency_ms",
        "Per-retriever lane wall-clock latency histogram (milliseconds).",
        "histogram",
    );
    for shard in shards {
        let snap = shard.retriever_metrics().snapshot();
        for (idx, retriever) in brain_ops::RETRIEVER_LABELS.iter().enumerate() {
            let inner = format!("shard=\"{}\",retriever=\"{retriever}\"", shard.shard_id());
            emit_worker_histogram(
                out,
                "brain_retriever_latency_ms",
                &inner,
                &snap.latency_ms[idx],
            );
        }
    }
}

/// Per-shard write-path metric family, labeled by `phase` — the
/// [`brain_ops::Phase`] tag the writer already carries, so the label
/// set is bounded by the phase enum rather than by anything a client
/// sends. A phase only appears once the shard has submitted one: the
/// writer's counters are keyed by a map that grows on first sight.
fn emit_writer_metrics(out: &mut String, shards: &[ShardHandle]) {
    // One snapshot per shard for the whole family: the writer's
    // counters sit behind tag-keyed maps, so re-snapshotting per
    // section would re-lock and re-allocate on every scrape, and the
    // sections would disagree about which write they saw last.
    let snaps: Vec<(brain_core::ShardId, brain_ops::WriterMetricsSnapshot)> = shards
        .iter()
        .map(|s| (s.shard_id(), s.writer_metrics().snapshot()))
        .collect();

    emit_header(
        out,
        "brain_writer_submit_total",
        "Phases submitted through the unified write path, by outcome.",
        "counter",
    );
    for (shard_id, snap) in &snaps {
        for phase in &snap.by_phase {
            for (outcome, count) in [
                ("ok", phase.submit_ok),
                ("err", phase.submit_err),
                ("conflict", phase.submit_conflict),
            ] {
                let _ = writeln!(
                    out,
                    "brain_writer_submit_total{{shard=\"{}\",phase=\"{}\",outcome=\"{outcome}\"}} {count}",
                    *shard_id,
                    phase.phase,
                );
            }
        }
    }

    emit_header(
        out,
        "brain_writer_wal_skip_total",
        "Phases elided from the WAL append because they map to no WAL payload (durability via redb only).",
        "counter",
    );
    for (shard_id, snap) in &snaps {
        for phase in &snap.by_phase {
            let _ = writeln!(
                out,
                "brain_writer_wal_skip_total{{shard=\"{}\",phase=\"{}\"}} {}",
                *shard_id, phase.phase, phase.wal_skip,
            );
        }
    }

    emit_header(
        out,
        "brain_writer_apply_errors_total",
        "Phase apply failures, by phase and error kind.",
        "counter",
    );
    for (shard_id, snap) in &snaps {
        for err in &snap.apply_errors {
            let _ = writeln!(
                out,
                "brain_writer_apply_errors_total{{shard=\"{}\",phase=\"{}\",error=\"{}\"}} {}",
                *shard_id, err.phase, err.error, err.count,
            );
        }
    }

    emit_header(
        out,
        "brain_writer_idempotency_total",
        "Idempotency-cache lookups on the write path, by outcome.",
        "counter",
    );
    for (shard_id, snap) in &snaps {
        for (outcome, count) in [
            ("hit", snap.idempotency_hit),
            ("miss", snap.idempotency_miss),
            ("conflict", snap.idempotency_conflict),
        ] {
            let _ = writeln!(
                out,
                "brain_writer_idempotency_total{{shard=\"{}\",outcome=\"{outcome}\"}} {count}",
                *shard_id,
            );
        }
    }

    emit_header(
        out,
        "brain_writer_submit_duration_seconds",
        "End-to-end submit duration observed once per phase in the write.",
        "histogram",
    );
    for (shard_id, snap) in &snaps {
        for phase in &snap.by_phase {
            let inner = format!("shard=\"{shard_id}\",phase=\"{}\"", phase.phase);
            emit_worker_histogram(
                out,
                "brain_writer_submit_duration_seconds",
                &inner,
                &phase.submit_duration_seconds,
            );
        }
    }
}

/// Per-shard ack-path stage latency, labeled by
/// `stage = wal_append | redb_commit | hnsw_insert | tantivy_index`.
/// This is the family that attributes synchronous write latency to a
/// stage; the end-to-end `brain_request_duration_ms` only says a write
/// was slow, not where. Every stage is emitted on every scrape even at
/// zero observations, so a stage that stops running is visible as a
/// flat count rather than as a vanished series.
fn emit_write_stage_metrics(out: &mut String, shards: &[ShardHandle]) {
    emit_header(
        out,
        "brain_writer_stage_duration_ms",
        "Per-stage wall-clock latency of the synchronous write (ack) path, in milliseconds.",
        "histogram",
    );
    for shard in shards {
        let snap = shard.writer_metrics().snapshot();
        for (idx, stage) in brain_ops::WRITE_STAGE_LABELS.iter().enumerate() {
            let inner = format!("shard=\"{}\",stage=\"{stage}\"", shard.shard_id());
            emit_worker_histogram(
                out,
                "brain_writer_stage_duration_ms",
                &inner,
                &snap.stages.latency_ms[idx],
            );
        }
    }
}

/// Per-shard end-to-end RECALL (query) metric family. `outcome_total`
/// is labeled by `outcome = single | many | none` (a bounded enum).
/// Same dispatch shape as [`emit_statement_embed_metrics`].
fn emit_query_metrics(out: &mut String, shards: &[ShardHandle]) {
    emit_header(out, "brain_query_total", "Recalls served.", "counter");
    for shard in shards {
        let snap = shard.query_metrics().snapshot();
        let labels = format!("{{shard=\"{}\"}}", shard.shard_id());
        let _ = writeln!(out, "brain_query_total{labels} {}", snap.total);
    }

    emit_header(
        out,
        "brain_query_rerank_invoked_total",
        "Recalls where the cross-encoder rerank stage reordered the fused list.",
        "counter",
    );
    for shard in shards {
        let snap = shard.query_metrics().snapshot();
        let labels = format!("{{shard=\"{}\"}}", shard.shard_id());
        let _ = writeln!(
            out,
            "brain_query_rerank_invoked_total{labels} {}",
            snap.rerank_invoked_total
        );
    }

    emit_header(
        out,
        "brain_query_outcome_total",
        "Served recalls by answer shape (single / many / none).",
        "counter",
    );
    for shard in shards {
        let snap = shard.query_metrics().snapshot();
        for (idx, outcome) in brain_ops::QUERY_OUTCOME_LABELS.iter().enumerate() {
            let labels = format!("{{shard=\"{}\",outcome=\"{outcome}\"}}", shard.shard_id());
            let _ = writeln!(
                out,
                "brain_query_outcome_total{labels} {}",
                snap.outcome_total[idx]
            );
        }
    }

    emit_header(
        out,
        "brain_query_latency_ms",
        "End-to-end recall latency histogram (milliseconds).",
        "histogram",
    );
    for shard in shards {
        let snap = shard.query_metrics().snapshot();
        let inner = format!("shard=\"{}\"", shard.shard_id());
        emit_worker_histogram(out, "brain_query_latency_ms", &inner, &snap.latency_ms);
    }

    emit_header(
        out,
        "brain_query_fusion_k",
        "Effective adaptive fusion-k the engine fused at, per served recall (histogram).",
        "histogram",
    );
    for shard in shards {
        let snap = shard.query_metrics().snapshot();
        let inner = format!("shard=\"{}\"", shard.shard_id());
        emit_worker_histogram(out, "brain_query_fusion_k", &inner, &snap.fusion_k);
    }
}

/// Rows the read paths served in degraded form, labeled by
/// `reason`. Process-global rather than per-shard: the projections that
/// record it hold no shard handle, so there is no `shard` label to carry.
/// Same read-a-global shape as [`emit_derivation_admission`].
fn emit_read_degrade_metrics(out: &mut String) {
    let snap = brain_ops::metrics::read_degrade::global().snapshot();
    emit_header(
        out,
        "brain_read_degraded_rows_total",
        "Rows a read path returned with a part it could not resolve, by reason.",
        "counter",
    );
    for (idx, reason) in brain_ops::metrics::read_degrade::READ_DEGRADE_REASON_LABELS
        .iter()
        .enumerate()
    {
        let _ = writeln!(
            out,
            "brain_read_degraded_rows_total{{reason=\"{reason}\"}} {}",
            snap.rows_total[idx]
        );
    }
}

/// Per-shard ExtractorWorker metric family. Same dispatch
/// shape as [`emit_auto_edge_metrics`].
fn emit_extractor_metrics(out: &mut String, shards: &[ShardHandle]) {
    emit_header(
        out,
        "brain_extractor_drops_total",
        "Encode-side enqueues dropped because the extractor channel was full.",
        "counter",
    );
    for shard in shards {
        if let Some(m) = shard.extractor_metrics() {
            let labels = format!("{{shard=\"{}\"}}", shard.shard_id());
            let _ = writeln!(
                out,
                "brain_extractor_drops_total{labels} {}",
                m.snapshot().drops_total
            );
        }
    }

    emit_header(
        out,
        "brain_extractor_schema_filtered_total",
        "Items dropped because their predicate / relation_type isn't in the active schema.",
        "counter",
    );
    for shard in shards {
        if let Some(m) = shard.extractor_metrics() {
            let snap = m.snapshot();
            for (predicate, count) in snap.schema_filtered_total {
                let labels = format!(
                    "{{shard=\"{}\",predicate=\"{}\"}}",
                    shard.shard_id(),
                    escape_label(&predicate)
                );
                let _ = writeln!(out, "brain_extractor_schema_filtered_total{labels} {count}",);
            }
        }
    }

    emit_header(
        out,
        "brain_extractor_apply_dropped_total",
        "Real extracted items the apply pass could not persist, by reason. Any nonzero value is signal loss.",
        "counter",
    );
    for shard in shards {
        if let Some(m) = shard.extractor_metrics() {
            let snap = m.snapshot();
            for (reason, count) in snap.apply_dropped_total {
                let labels = format!(
                    "{{shard=\"{}\",reason=\"{}\"}}",
                    shard.shard_id(),
                    escape_label(&reason)
                );
                let _ = writeln!(out, "brain_extractor_apply_dropped_total{labels} {count}",);
            }
        }
    }

    emit_header(
        out,
        "brain_extractor_predicate_consolidated_total",
        "Open-vocab predicate surface forms folded onto an existing near-synonym predicate id by embedding consolidation.",
        "counter",
    );
    for shard in shards {
        if let Some(m) = shard.extractor_metrics() {
            let snap = m.snapshot();
            let labels = format!("{{shard=\"{}\"}}", shard.shard_id());
            let _ = writeln!(
                out,
                "brain_extractor_predicate_consolidated_total{labels} {}",
                snap.predicate_consolidated_total
            );
        }
    }

    emit_header(
        out,
        "brain_extractor_items_written_total",
        "Typed-graph rows persisted by the extractor worker, by item kind.",
        "counter",
    );
    for shard in shards {
        if let Some(m) = shard.extractor_metrics() {
            let snap = m.snapshot();
            for (i, label) in brain_ops::ITEM_KIND_LABELS.iter().enumerate() {
                let labels = format!("{{shard=\"{}\",item_kind=\"{label}\"}}", shard.shard_id());
                let _ = writeln!(
                    out,
                    "brain_extractor_items_written_total{labels} {}",
                    snap.items_written_total[i]
                );
            }
        }
    }

    emit_header(
        out,
        "brain_extractor_llm_micro_usd_spent_total",
        "LLM-tier spend reported by extractors, in dollar-micro-units (1e-6 USD).",
        "counter",
    );
    for shard in shards {
        if let Some(m) = shard.extractor_metrics() {
            let labels = format!("{{shard=\"{}\"}}", shard.shard_id());
            let _ = writeln!(
                out,
                "brain_extractor_llm_micro_usd_spent_total{labels} {}",
                m.snapshot().llm_micro_usd_spent_total
            );
        }
    }

    emit_header(
        out,
        "brain_extractor_cycle_duration_seconds",
        "Wall-clock duration of one ExtractorWorker cycle.",
        "histogram",
    );
    for shard in shards {
        if let Some(m) = shard.extractor_metrics() {
            let inner = format!("shard=\"{}\"", shard.shard_id());
            emit_worker_histogram(
                out,
                "brain_extractor_cycle_duration_seconds",
                &inner,
                &m.snapshot().cycle_duration_seconds,
            );
        }
    }

    emit_header(
        out,
        "brain_extractor_tier_runs_total",
        "Per-tier outcome for each memory the extractor processed.",
        "counter",
    );
    for shard in shards {
        if let Some(m) = shard.extractor_metrics() {
            let snap = m.snapshot();
            for (tier_idx, tier) in brain_ops::TIER_LABELS.iter().enumerate() {
                for (status_idx, status) in brain_ops::TIER_STATUS_LABELS.iter().enumerate() {
                    let labels = format!(
                        "{{shard=\"{}\",tier=\"{tier}\",status=\"{status}\"}}",
                        shard.shard_id()
                    );
                    let idx = tier_idx * brain_ops::TIER_STATUS_LABELS.len() + status_idx;
                    let _ = writeln!(
                        out,
                        "brain_extractor_tier_runs_total{labels} {}",
                        snap.tier_runs_total[idx]
                    );
                }
            }
        }
    }

    emit_header(
        out,
        "brain_extractor_resolver_outcome_total",
        "Resolver tier that satisfied each entity mention.",
        "counter",
    );
    for shard in shards {
        if let Some(m) = shard.extractor_metrics() {
            let snap = m.snapshot();
            for (i, tier) in brain_ops::RESOLVER_OUTCOME_LABELS.iter().enumerate() {
                let labels = format!("{{shard=\"{}\",tier=\"{tier}\"}}", shard.shard_id());
                let _ = writeln!(
                    out,
                    "brain_extractor_resolver_outcome_total{labels} {}",
                    snap.resolver_outcome_total[i]
                );
            }
        }
    }
}

/// Render a `WorkerHistogramSnapshot` in Prometheus text format with
/// the supplied label prefix. Mirrors `Histogram::expose` but reads
/// from the brain-ops snapshot type (the worker-side histogram lives
/// outside `brain-server` to keep the dependency edge correct).
fn emit_worker_histogram(
    out: &mut String,
    name: &str,
    label_prefix: &str,
    snap: &brain_ops::WorkerHistogramSnapshot,
) {
    for bucket in &snap.buckets {
        let le = match bucket.le {
            Some(v) => format!("{v}"),
            None => "+Inf".to_string(),
        };
        let labels = if label_prefix.is_empty() {
            format!("{{le=\"{le}\"}}")
        } else {
            format!("{{{label_prefix},le=\"{le}\"}}")
        };
        let _ = writeln!(out, "{name}_bucket{labels} {}", bucket.cumulative_count);
    }
    let bare_label = if label_prefix.is_empty() {
        String::new()
    } else {
        format!("{{{label_prefix}}}")
    };
    let _ = writeln!(out, "{name}_sum{bare_label} {}", snap.sum);
    let _ = writeln!(out, "{name}_count{bare_label} {}", snap.count);
}

/// Escape a Prometheus label value. Only `\\`, `"`, and `\n` need
/// escaping per the text-format spec; the predicate qnames the
/// extractor emits are colon-namespaced ASCII in practice but we
/// defend against the corner cases regardless.
fn escape_label(value: &str) -> String {
    let mut s = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => s.push_str("\\\\"),
            '"' => s.push_str("\\\""),
            '\n' => s.push_str("\\n"),
            other => s.push(other),
        }
    }
    s
}

/// Per-op request counters / in-flight gauge / duration
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

/// One per-shard metric family: header, then one line per shard that has a
/// value (`None` = this shard doesn't report it).
fn emit_per_shard(
    out: &mut String,
    shards: &[ShardHandle],
    name: &str,
    help: &str,
    kind: &str,
    value: impl Fn(&ShardHandle) -> Option<f64>,
) {
    emit_header(out, name, help, kind);
    for shard in shards {
        if let Some(v) = value(shard) {
            let _ = writeln!(out, "{name}{{shard=\"{}\"}} {v}", shard.shard_id());
        }
    }
}

/// Aggregate counters (build + self-check), `MEMORY_AGGREGATE` sources,
/// and the analytics snapshot exporter.
#[allow(clippy::cast_precision_loss)]
fn emit_aggregate_metrics(out: &mut String, shards: &[ShardHandle]) {
    let maint = |f: fn(&brain_metadata::tables::agg::MaintenanceStats) -> f64| {
        move |s: &ShardHandle| s.agg_maintenance_stats().map(|m| f(&m))
    };
    emit_per_shard(
        out,
        shards,
        "brain_agg_counters_ready",
        "1 when aggregate counters are complete; 0 while an online build after upgrade runs.",
        "gauge",
        maint(|m| f64::from(u8::from(m.counters_ready.load(Ordering::Relaxed)))),
    );
    emit_per_shard(
        out,
        shards,
        "brain_agg_counter_build_rows_total",
        "Memories counted by the online counter build.",
        "counter",
        maint(|m| m.build_rows.load(Ordering::Relaxed) as f64),
    );
    emit_per_shard(
        out,
        shards,
        "brain_agg_counter_verify_runs_total",
        "Full verify passes of the aggregate counters against memories.",
        "counter",
        maint(|m| m.verify_runs.load(Ordering::Relaxed) as f64),
    );
    emit_per_shard(
        out,
        shards,
        "brain_agg_counter_drifted_buckets_total",
        "Counter buckets found wrong and repaired. Should stay 0; anything else means a write path bypassed the counters.",
        "counter",
        maint(|m| m.drifted_buckets.load(Ordering::Relaxed) as f64),
    );
    for (name, help, pick) in [
        (
            "brain_memory_aggregate_counters_total",
            "MEMORY_AGGREGATE requests answered from live counters.",
            (|m: &brain_ops::AggregateMetrics| &m.counters_total)
                as fn(&_) -> &std::sync::atomic::AtomicU64,
        ),
        (
            "brain_memory_aggregate_snapshot_total",
            "MEMORY_AGGREGATE requests answered from the analytics snapshot.",
            |m: &brain_ops::AggregateMetrics| &m.snapshot_total,
        ),
        (
            "brain_memory_aggregate_errors_total",
            "MEMORY_AGGREGATE requests that failed.",
            |m: &brain_ops::AggregateMetrics| &m.errors_total,
        ),
    ] {
        emit_per_shard(out, shards, name, help, "counter", |s| {
            Some(pick(&s.aggregate_metrics()).load(Ordering::Relaxed) as f64)
        });
    }
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos()) as f64;
    emit_per_shard(
        out,
        shards,
        "brain_analytics_exports_total",
        "Analytics snapshot exports completed.",
        "counter",
        |s| s.analytics().map(|a| a.stats().exports_total as f64),
    );
    emit_per_shard(
        out,
        shards,
        "brain_analytics_export_failures_total",
        "Analytics snapshot exports that failed.",
        "counter",
        |s| {
            s.analytics()
                .map(|a| a.stats().export_failures_total as f64)
        },
    );
    emit_per_shard(
        out,
        shards,
        "brain_analytics_last_export_seconds",
        "Duration of the most recent analytics snapshot export.",
        "gauge",
        |s| s.analytics().map(|a| a.stats().last_export_ms as f64 / 1e3),
    );
    emit_per_shard(
        out,
        shards,
        "brain_analytics_snapshot_rows",
        "Memories in the current analytics snapshot.",
        "gauge",
        |s| {
            s.analytics()
                .and_then(|a| a.current_snapshot())
                .map(|i| i.rows as f64)
        },
    );
    emit_per_shard(
        out,
        shards,
        "brain_analytics_snapshot_age_seconds",
        "Age of the current analytics snapshot (staleness of scan answers).",
        "gauge",
        |s| {
            s.analytics()
                .and_then(|a| a.current_snapshot())
                .map(|i| (now_ns - i.taken_at_unix_nanos as f64).max(0.0) / 1e9)
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shard::ShardRequest;
    use brain_ops::{SubmitOutcome, WriteStage};
    use std::time::Duration;

    /// Build a handle whose writer family already carries one of
    /// everything the ack path records, so the exposition has real
    /// numbers to render rather than an all-zero skeleton.
    fn shard_with_recorded_write() -> (ShardHandle, flume::Receiver<ShardRequest>) {
        let (tx, rx) = flume::unbounded::<ShardRequest>();
        let handle = ShardHandle::new_for_test(0, tx);
        let m = handle.writer_metrics();
        m.record_idempotency(brain_ops::IdempotencyOutcome::Miss);
        m.record_submit("upsert_memory", SubmitOutcome::Ok, Duration::from_millis(3));
        m.record_wal_skip("reclaim_slots");
        m.record_apply_error("link", "not_found");
        m.stages()
            .record(WriteStage::WalAppend, Duration::from_micros(400));
        m.stages()
            .record(WriteStage::RedbCommit, Duration::from_micros(1_500));
        m.stages()
            .record(WriteStage::HnswInsert, Duration::from_micros(120));
        m.stages()
            .record(WriteStage::TantivyIndex, Duration::from_micros(3));
        m.stages()
            .record(WriteStage::RedbApply, Duration::from_micros(3_400));
        // The receiver rides back out so the request channel stays
        // connected for as long as the handle under test lives.
        (handle, rx)
    }

    #[test]
    fn writer_family_exposes_every_recorded_axis() {
        let (handle, _rx) = shard_with_recorded_write();
        let shards = vec![handle];
        let mut out = String::new();
        emit_writer_metrics(&mut out, &shards);

        for header in [
            "# HELP brain_writer_submit_total",
            "# TYPE brain_writer_submit_total counter",
            "# TYPE brain_writer_wal_skip_total counter",
            "# TYPE brain_writer_apply_errors_total counter",
            "# TYPE brain_writer_idempotency_total counter",
            "# TYPE brain_writer_submit_duration_seconds histogram",
        ] {
            assert!(out.contains(header), "missing {header} in:\n{out}");
        }

        assert!(out.contains(
            "brain_writer_submit_total{shard=\"0\",phase=\"upsert_memory\",outcome=\"ok\"} 1"
        ));
        assert!(out.contains(
            "brain_writer_submit_total{shard=\"0\",phase=\"upsert_memory\",outcome=\"err\"} 0"
        ));
        assert!(out.contains("brain_writer_wal_skip_total{shard=\"0\",phase=\"reclaim_slots\"} 1"));
        assert!(out.contains(
            "brain_writer_apply_errors_total{shard=\"0\",phase=\"link\",error=\"not_found\"} 1"
        ));
        assert!(out.contains("brain_writer_idempotency_total{shard=\"0\",outcome=\"miss\"} 1"));
        assert!(out.contains(
            "brain_writer_submit_duration_seconds_count{shard=\"0\",phase=\"upsert_memory\"} 1"
        ));
    }

    /// Every degradation reason has to reach the scrape, or an operator
    /// cannot tell which kind of corruption they are looking at.
    #[test]
    fn read_degrade_family_exposes_every_reason() {
        use brain_ops::metrics::read_degrade::{global, ReadDegradeReason};

        global().inc(ReadDegradeReason::PredicateQnameMissing);
        global().inc(ReadDegradeReason::RelationTypeQnameMissing);
        global().inc(ReadDegradeReason::EvidenceOverflowMissing);

        let mut out = String::new();
        emit_read_degrade_metrics(&mut out);

        assert!(out.contains("# HELP brain_read_degraded_rows_total"));
        assert!(out.contains("# TYPE brain_read_degraded_rows_total counter"));
        for reason in brain_ops::metrics::read_degrade::READ_DEGRADE_REASON_LABELS {
            let needle = format!("brain_read_degraded_rows_total{{reason=\"{reason}\"}} ");
            assert!(out.contains(&needle), "missing {needle} in:\n{out}");
        }
    }

    #[test]
    fn write_stage_family_exposes_one_series_per_stage() {
        let (handle, _rx) = shard_with_recorded_write();
        let shards = vec![handle];
        let mut out = String::new();
        emit_write_stage_metrics(&mut out, &shards);

        assert!(out.contains(
            "# HELP brain_writer_stage_duration_ms Per-stage wall-clock latency of the synchronous write (ack) path, in milliseconds."
        ));
        assert!(out.contains("# TYPE brain_writer_stage_duration_ms histogram"));
        // Exactly one header pair for the whole family — Prometheus
        // rejects a repeated HELP line for the same metric name.
        assert_eq!(
            out.matches("# TYPE brain_writer_stage_duration_ms").count(),
            1
        );

        for stage in brain_ops::WRITE_STAGE_LABELS {
            assert!(
                out.contains(&format!(
                    "brain_writer_stage_duration_ms_count{{shard=\"0\",stage=\"{stage}\"}} 1"
                )),
                "stage {stage} missing a count line in:\n{out}"
            );
            assert!(
                out.contains(&format!(
                    "brain_writer_stage_duration_ms_bucket{{shard=\"0\",stage=\"{stage}\",le=\"+Inf\"}} 1"
                )),
                "stage {stage} missing its +Inf bucket in:\n{out}"
            );
        }

        // Observations are milliseconds: a 1.5 ms redb commit must
        // render as 1.5, not 0.0015 or 1500.
        assert!(
            out.contains(
                "brain_writer_stage_duration_ms_sum{shard=\"0\",stage=\"redb_commit\"} 1.5"
            ),
            "redb_commit sum not in milliseconds:\n{out}"
        );
    }

    #[test]
    fn write_stage_series_are_emitted_before_any_write_lands() {
        let (tx, _rx) = flume::unbounded::<ShardRequest>();
        let shards = vec![ShardHandle::new_for_test(0, tx)];
        let mut out = String::new();
        emit_write_stage_metrics(&mut out, &shards);
        for stage in brain_ops::WRITE_STAGE_LABELS {
            assert!(out.contains(&format!(
                "brain_writer_stage_duration_ms_count{{shard=\"0\",stage=\"{stage}\"}} 0"
            )));
        }
    }
}
