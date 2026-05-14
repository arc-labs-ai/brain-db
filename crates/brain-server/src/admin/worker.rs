//! Admin HTTP handlers for `worker` (spec §14/06 §6; sub-task 10.11).
//!
//! Routes:
//! - `GET /v1/workers[?shard=N]` → 200 +
//!   `{"workers":[{shard,name,cycles,processed,errors,last_run_unix}]}`
//! - `POST /v1/workers/{name}/{stop|start|run-now}` → 501
//!
//! Worker control plane is deferred; spec §14/06 §6 calls for
//! pause/resume/trigger but the Scheduler has no such hooks today.

use std::fmt::Write as _;
use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Method, Request, Response, StatusCode};
use hyper::body::Incoming;
use tracing::warn;

use crate::admin::util::{json_response, not_implemented, text_response};
use crate::admin::AdminState;

const KNOWN_WORKERS: &[&str] = &[
    "decay",
    "access_boost",
    "consolidation",
    "hnsw_maintenance",
    "idempotency_cleanup",
    "slot_reclamation",
    "wal_retention",
    "edge_scrub",
    "counter_reconcile",
    "statistics",
    "embedder_cache_evict",
    "snapshot",
];
const KNOWN_ACTIONS: &[&str] = &["stop", "start", "run-now"];

/// `GET /v1/workers` handler.
pub async fn list(
    req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let query = req.uri().query().unwrap_or("").to_owned();
    let shard_filter = match parse_shard(&query) {
        Ok(s) => s,
        Err(msg) => return Ok(text_response(StatusCode::BAD_REQUEST, &msg)),
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

/// `POST /v1/workers/{name}/{action}` handler — prefix-registered.
pub async fn control(
    req: Request<Incoming>,
    _state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    if req.method() != Method::POST {
        return Ok(text_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "method not allowed\n",
        ));
    }
    let path = req.uri().path();
    let Some(rest) = path.strip_prefix("/v1/workers/") else {
        return Ok(text_response(
            StatusCode::NOT_FOUND,
            "worker route not found\n",
        ));
    };
    let mut parts = rest.splitn(2, '/');
    let name = parts.next().unwrap_or("");
    let action = parts.next().unwrap_or("");

    if !KNOWN_WORKERS.contains(&name) {
        return Ok(text_response(
            StatusCode::BAD_REQUEST,
            &format!("unknown worker `{name}`\n"),
        ));
    }
    if !KNOWN_ACTIONS.contains(&action) {
        return Ok(text_response(
            StatusCode::BAD_REQUEST,
            &format!("unknown worker action `{action}`\n"),
        ));
    }
    Ok(not_implemented(
        "phase-11/scheduler-control",
        "live worker pause/resume/trigger",
    ))
}

fn parse_shard(query: &str) -> Result<Option<usize>, String> {
    if query.is_empty() {
        return Ok(None);
    }
    for kv in query.split('&') {
        if let Some(rest) = kv.strip_prefix("shard=") {
            return rest
                .parse::<usize>()
                .map(Some)
                .map_err(|e| format!("invalid shard: {e}\n"));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_shard_optional() {
        assert_eq!(parse_shard("").unwrap(), None);
        assert_eq!(parse_shard("shard=2").unwrap(), Some(2));
        assert!(parse_shard("shard=abc").is_err());
    }

    #[test]
    fn known_action_set() {
        assert!(KNOWN_ACTIONS.contains(&"stop"));
        assert!(KNOWN_ACTIONS.contains(&"start"));
        assert!(KNOWN_ACTIONS.contains(&"run-now"));
    }
}
