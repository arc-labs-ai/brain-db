//! `GET /v1/workers[?shard=N]` — list scheduler worker snapshots.

use std::fmt::Write as _;
use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use tracing::warn;

use crate::admin::query;
use crate::admin::util::{json_response, text_response};
use crate::admin::AdminState;

pub async fn list(
    req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let query_str = req.uri().query().unwrap_or("").to_owned();
    let shard_filter = match query::shard_optional(&query_str) {
        Ok(s) => s,
        Err(msg) => return Ok(text_response(StatusCode::BAD_REQUEST, &format!("{msg}\n"))),
    };
    let mut body = String::with_capacity(512);
    body.push_str("{\"workers\":[");
    let mut first = true;
    for (idx, shard) in state.shards.iter().enumerate() {
        if let Some(want) = shard_filter {
            if idx != want {
                continue;
            }
        }
        match shard.scheduler_snapshot().await {
            Ok(mut snaps) => {
                snaps.sort_by_key(|(name, _, _)| *name);
                for (name, _kind, snap) in snaps {
                    if !first {
                        body.push(',');
                    }
                    first = false;
                    write!(
                        &mut body,
                        "{{\"shard\":{idx},\"name\":\"{name}\",\"cycles\":{c},\"processed\":{p},\"errors\":{e},\"last_run_unix\":{lr}}}",
                        c = snap.cycles_total,
                        p = snap.processed_total,
                        e = snap.errors_total,
                        lr = snap.last_run_unix_secs,
                    )
                    .expect("string write");
                }
            }
            Err(e) => {
                warn!(shard = idx, error = %e, "scheduler_snapshot failed");
            }
        }
    }
    body.push_str("]}\n");
    Ok(json_response(StatusCode::OK, body))
}
