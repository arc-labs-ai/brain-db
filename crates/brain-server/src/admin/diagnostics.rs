//! Admin HTTP handlers for `profile` + `debug-snapshot`
//! (spec §14/06 §9; sub-task 10.12).
//!
//! Routes:
//! - `POST /v1/diagnostics/profile?shard=N[&duration_secs=D]` → 501.
//!   Real Glommio profiler is deferred to phase-11; operators today
//!   can run `perf record` against the server PID.
//! - `GET /v1/diagnostics/debug-snapshot?shard=N` → 200 + JSON.
//!   v1 populates worker statuses from `scheduler_snapshot()` and
//!   flags missing fields in `deferred[]` per the plan §1.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use brain_http::body::ResponseBody;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use tracing::warn;

use crate::admin::util::{json_response, not_implemented, text_response};
use crate::admin::AdminState;

/// v1 always reports these spec'd fields as not yet populated. As
/// primitives land (active task registry, dispatch queue depth,
/// recent-error ring buffer, arena/HNSW counters), entries drop
/// out of this array.
const DEFERRED_FIELDS: &[&str] = &[
    "active_tasks",
    "pending_requests",
    "recent_errors",
    "in_memory_state_summary",
];

/// `POST /v1/diagnostics/profile` handler — deferred.
pub async fn profile(
    _req: Request<Incoming>,
    _state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    Ok(not_implemented(
        "phase-11/glommio-profiler",
        "in-process CPU profiler for the shard's Glommio executor",
    ))
}

/// `GET /v1/diagnostics/debug-snapshot` handler.
pub async fn debug_snapshot(
    req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let query = req.uri().query().unwrap_or("").to_owned();
    let shard_id = match parse_shard(&query) {
        Ok(id) => id,
        Err(msg) => return Ok(text_response(StatusCode::BAD_REQUEST, &msg)),
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
    body.push_str("],\"workers\":[");

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

fn parse_shard(query: &str) -> Result<usize, String> {
    if query.is_empty() {
        return Ok(0);
    }
    for kv in query.split('&') {
        if let Some(rest) = kv.strip_prefix("shard=") {
            return rest
                .parse::<usize>()
                .map_err(|e| format!("invalid shard: {e}\n"));
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_shard_default() {
        assert_eq!(parse_shard("").unwrap(), 0);
    }

    #[test]
    fn parse_shard_explicit() {
        assert_eq!(parse_shard("shard=3").unwrap(), 3);
        assert_eq!(parse_shard("duration_secs=30&shard=5").unwrap(), 5);
    }

    #[test]
    fn parse_shard_rejects_garbage() {
        assert!(parse_shard("shard=abc").is_err());
    }

    #[test]
    fn deferred_fields_match_plan() {
        // Spec §14/06 §9 lists 5 fields; one (worker_statuses) is
        // populated, four remain deferred in v1.
        assert!(DEFERRED_FIELDS.contains(&"active_tasks"));
        assert!(DEFERRED_FIELDS.contains(&"pending_requests"));
        assert!(DEFERRED_FIELDS.contains(&"recent_errors"));
        assert!(DEFERRED_FIELDS.contains(&"in_memory_state_summary"));
    }
}
