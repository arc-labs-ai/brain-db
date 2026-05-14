//! Prometheus metrics exposition.
//!
//! Extracted from the old hand-rolled `format_metrics` writeln-chain
//! in `admin/mod.rs` so it lives behind the standard brain-http
//! handler signature. Body bytes are identical to the previous
//! implementation; only the wire delivery changes.

use std::fmt::Write as _;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use brain_http::body::{full, ResponseBody};
use bytes::Bytes;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use tracing::warn;

use crate::admin::AdminState;

const HDR_PROMETHEUS: &str = "text/plain; version=0.0.4; charset=utf-8";

/// `GET /metrics` handler. Emits the Prometheus text-format exposition
/// with the existing metric set.
pub async fn handle(
    _req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let body = format_metrics(&state).await;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", HDR_PROMETHEUS)
        .body(full(Bytes::from(body)))
        .expect("static response always builds"))
}

async fn format_metrics(state: &AdminState) -> String {
    let mut s = String::with_capacity(2048);

    // Static + scalars.
    let uptime_secs = state.started_at.elapsed().as_secs();
    writeln!(&mut s, "# HELP brain_build_info Build information.").unwrap();
    writeln!(&mut s, "# TYPE brain_build_info gauge").unwrap();
    writeln!(
        &mut s,
        "brain_build_info{{version=\"{v}\",git_commit=\"{g}\"}} 1",
        v = state.build_info.version,
        g = state.build_info.git_commit,
    )
    .unwrap();

    writeln!(
        &mut s,
        "# HELP brain_up Server liveness; 1 if accepting requests."
    )
    .unwrap();
    writeln!(&mut s, "# TYPE brain_up gauge").unwrap();
    writeln!(&mut s, "brain_up 1").unwrap();

    writeln!(
        &mut s,
        "# HELP brain_shards_total Number of configured shards."
    )
    .unwrap();
    writeln!(&mut s, "# TYPE brain_shards_total gauge").unwrap();
    writeln!(&mut s, "brain_shards_total {}", state.shards.len()).unwrap();

    writeln!(
        &mut s,
        "# HELP brain_connections_active Currently in-flight client connections."
    )
    .unwrap();
    writeln!(&mut s, "# TYPE brain_connections_active gauge").unwrap();
    writeln!(
        &mut s,
        "brain_connections_active {}",
        state.connections.active.load(Ordering::Relaxed),
    )
    .unwrap();

    writeln!(
        &mut s,
        "# HELP brain_connections_total Total accepted client connections since startup."
    )
    .unwrap();
    writeln!(&mut s, "# TYPE brain_connections_total counter").unwrap();
    writeln!(
        &mut s,
        "brain_connections_total {}",
        state.connections.total.load(Ordering::Relaxed),
    )
    .unwrap();

    writeln!(
        &mut s,
        "# HELP process_uptime_seconds Process uptime since admin server start."
    )
    .unwrap();
    writeln!(&mut s, "# TYPE process_uptime_seconds counter").unwrap();
    writeln!(&mut s, "process_uptime_seconds {uptime_secs}").unwrap();

    writeln!(
        &mut s,
        "# HELP process_start_time_seconds Unix timestamp of process start (seconds)."
    )
    .unwrap();
    writeln!(&mut s, "# TYPE process_start_time_seconds gauge").unwrap();
    writeln!(
        &mut s,
        "process_start_time_seconds {}",
        state.started_at_unix_secs
    )
    .unwrap();

    // Per-worker counters from each shard's scheduler.
    writeln!(
        &mut s,
        "# HELP brain_worker_cycles_total Worker cycles completed."
    )
    .unwrap();
    writeln!(&mut s, "# TYPE brain_worker_cycles_total counter").unwrap();
    writeln!(
        &mut s,
        "# HELP brain_worker_processed_total Items processed by the worker."
    )
    .unwrap();
    writeln!(&mut s, "# TYPE brain_worker_processed_total counter").unwrap();
    writeln!(
        &mut s,
        "# HELP brain_worker_errors_total Worker cycle errors."
    )
    .unwrap();
    writeln!(&mut s, "# TYPE brain_worker_errors_total counter").unwrap();
    writeln!(
        &mut s,
        "# HELP brain_worker_last_run_unixtime Unix-time of the worker's last cycle."
    )
    .unwrap();
    writeln!(&mut s, "# TYPE brain_worker_last_run_unixtime gauge").unwrap();

    for shard in state.shards.iter() {
        let shard_id = shard.shard_id();
        match shard.scheduler_snapshot().await {
            Ok(snapshot) => {
                let mut workers = snapshot;
                workers.sort_by_key(|(name, _, _)| *name);
                for (name, _kind, snap) in workers {
                    writeln!(
                        &mut s,
                        "brain_worker_cycles_total{{shard=\"{shard_id}\",worker=\"{name}\"}} {}",
                        snap.cycles_total
                    )
                    .unwrap();
                    writeln!(
                        &mut s,
                        "brain_worker_processed_total{{shard=\"{shard_id}\",worker=\"{name}\"}} {}",
                        snap.processed_total
                    )
                    .unwrap();
                    writeln!(
                        &mut s,
                        "brain_worker_errors_total{{shard=\"{shard_id}\",worker=\"{name}\"}} {}",
                        snap.errors_total
                    )
                    .unwrap();
                    writeln!(
                        &mut s,
                        "brain_worker_last_run_unixtime{{shard=\"{shard_id}\",worker=\"{name}\"}} {}",
                        snap.last_run_unix_secs
                    )
                    .unwrap();
                }
            }
            Err(e) => {
                warn!(shard_id, error = %e, "scheduler_snapshot failed");
            }
        }
    }

    s
}
