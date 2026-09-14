//! `GET /v1/diagnostics/debug-snapshot?shard=N` — partial snapshot of
//! per-shard runtime state.
//!
//! v1 populates:
//! - `workers` from `scheduler_snapshot()`,
//! - `pending_requests` from the shard's dispatch-queue depth
//!   (`ShardHandle::queue_depth`), and
//! - `in_memory_state_summary` from `hnsw_snapshot()` + `storage_stats()`.
//!
//! The remaining spec'd fields (`active_tasks`, `recent_errors`) are
//! flagged in `deferred[]` — see the module doc for why.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use brain_http::body::ResponseBody;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use tracing::warn;

use crate::admin::handlers::diagnostics::DEFERRED_FIELDS;
use crate::admin::query;
use crate::admin::util::{json_response, text_response};
use crate::admin::AdminState;

pub async fn debug_snapshot(
    req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let query_str = req.uri().query().unwrap_or("").to_owned();
    let shard_id = match query::shard_required(&query_str) {
        Ok(id) => id,
        Err(msg) => return Ok(text_response(StatusCode::BAD_REQUEST, &format!("{msg}\n"))),
    };
    let Some(shard) = state.shards.get(shard_id) else {
        return Ok(text_response(StatusCode::NOT_FOUND, "shard out of range\n"));
    };
    let captured_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut body = String::with_capacity(512);
    write!(
        &mut body,
        "{{\"shard\":{shard_id},\"captured_at_unix\":{captured_at},\"partial\":true,\"deferred\":["
    )
    .expect("string write");
    for (i, field) in DEFERRED_FIELDS.iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        write!(&mut body, "\"{field}\"").expect("string write");
    }
    body.push(']');

    // pending_requests: dispatch-queue depth (requests queued on the
    // shard's request channel, not yet drained by the executor).
    write!(&mut body, ",\"pending_requests\":{}", shard.queue_depth()).expect("string write");

    // in_memory_state_summary: HNSW node/tombstone counts + arena / WAL /
    // metadata footprint. Both reads round-trip through the executor; on
    // failure (shard disconnected mid-scrape) we emit `null` and warn,
    // rather than fail the whole snapshot.
    body.push_str(",\"in_memory_state_summary\":");
    let hnsw = shard.hnsw_snapshot().await;
    let storage = shard.storage_stats().await;
    match (hnsw, storage) {
        (Ok(h), Ok(s)) => {
            write!(
                &mut body,
                "{{\"hnsw\":{{\"node_count\":{hn},\"tombstone_count\":{ht}}},\
                 \"arena\":{{\"capacity_bytes\":{ac},\"used_bytes\":{au},\
                 \"slots_used\":{asu},\"slots_free\":{asf}}},\
                 \"wal\":{{\"size_bytes\":{ws},\"segments\":{wg}}},\
                 \"metadata\":{{\"size_bytes\":{ms}}}}}",
                hn = h.node_count,
                ht = h.tombstone_count,
                ac = s.arena_capacity_bytes,
                au = s.arena_used_bytes,
                asu = s.arena_slots_used,
                asf = s.arena_slots_free,
                ws = s.wal_size_bytes,
                wg = s.wal_segments,
                ms = s.metadata_size_bytes,
            )
            .expect("string write");
        }
        (hnsw_res, storage_res) => {
            if let Err(e) = hnsw_res {
                warn!(shard = shard_id, error = %e, "hnsw_snapshot failed");
            }
            if let Err(e) = storage_res {
                warn!(shard = shard_id, error = %e, "storage_stats failed");
            }
            body.push_str("null");
        }
    }

    body.push_str(",\"workers\":[");

    match shard.scheduler_snapshot().await {
        Ok(mut snaps) => {
            snaps.sort_by_key(|(name, _, _)| *name);
            for (i, (name, _kind, snap)) in snaps.iter().enumerate() {
                if i > 0 {
                    body.push(',');
                }
                write!(
                    &mut body,
                    "{{\"name\":\"{name}\",\"cycles\":{c},\"processed\":{p},\"errors\":{e},\"last_run_unix\":{lr}}}",
                    c = snap.cycles_total,
                    p = snap.processed_total,
                    e = snap.errors_total,
                    lr = snap.last_run_unix_secs,
                )
                .expect("string write");
            }
        }
        Err(e) => {
            warn!(shard = shard_id, error = %e, "scheduler_snapshot failed");
        }
    }
    body.push_str("]}\n");
    Ok(json_response(StatusCode::OK, body))
}
