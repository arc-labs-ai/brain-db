//! `GET /v1/diagnostics/debug-snapshot?shard=N` — partial snapshot of
//! per-shard runtime state.
//!
//! v1 populates worker statuses from `scheduler_snapshot()` and flags
//! the remaining spec'd fields in `deferred[]`.

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
