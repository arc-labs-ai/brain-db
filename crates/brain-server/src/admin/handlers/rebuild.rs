//! Admin HTTP handlers for derived-index rebuild.
//!
//! Routes:
//! - `POST /v1/rebuild-ann[?shard=N]` → 201 +
//!   `{"entries":N,"elapsed_ms":N,"shard":N}` — memory-HNSW alias,
//!   kept for back-compat.
//! - `POST /v1/rebuild?index=<target>[&shard=N]` → 201 +
//!   `{"index":"<target>","entries":N,"elapsed_ms":N,"shard":N}` —
//!   rebuild any derived index from authoritative redb state. Unknown
//!   `index` → 400. Targets: `memory_hnsw`, `entity_hnsw`, `hype_hnsw`,
//!   `statement_question_hnsw`, `all`.

use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use tracing::warn;

use crate::admin::query;
use crate::admin::util::{json_response, text_response};
use crate::admin::AdminState;
use crate::shard::rebuild::RebuildTarget;

/// `POST /v1/rebuild-ann` — memory-HNSW alias (back-compat).
pub async fn handle(
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
    match shard.rebuild_hnsw().await {
        Ok(report) => {
            let body = format!(
                "{{\"entries\":{e},\"elapsed_ms\":{ms},\"shard\":{shard_id}}}\n",
                e = report.entries,
                ms = report.elapsed_ms
            );
            Ok(json_response(StatusCode::CREATED, body))
        }
        Err(e) => {
            // Log the internal detail; return a generic client-safe message.
            warn!(error = %e, "rebuild-ann failed");
            Ok(text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "index rebuild failed\n",
            ))
        }
    }
}

/// Parse `?index=` from a URI query string. Returns `None` when absent
/// or empty so the route can answer `400`.
fn index_param(query: &str) -> Option<&str> {
    for kv in query.split('&') {
        if let Some(rest) = kv.strip_prefix("index=") {
            if rest.is_empty() {
                return None;
            }
            return Some(rest);
        }
    }
    None
}

/// `POST /v1/rebuild?index=<target>[&shard=N]` — rebuild any derived
/// index from authoritative state.
pub async fn handle_index(
    req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let query_str = req.uri().query().unwrap_or("").to_owned();
    let Some(index_raw) = index_param(&query_str) else {
        return Ok(text_response(
            StatusCode::BAD_REQUEST,
            "missing required ?index= parameter\n",
        ));
    };
    let Some(target) = RebuildTarget::from_query(index_raw) else {
        return Ok(text_response(
            StatusCode::BAD_REQUEST,
            "unknown index: expected one of memory_hnsw, entity_hnsw, \
             hype_hnsw, statement_question_hnsw, all\n",
        ));
    };
    let shard_id = match query::shard_required(&query_str) {
        Ok(id) => id,
        Err(msg) => return Ok(text_response(StatusCode::BAD_REQUEST, &format!("{msg}\n"))),
    };
    let Some(shard) = state.shards.get(shard_id) else {
        return Ok(text_response(StatusCode::NOT_FOUND, "shard out of range\n"));
    };
    match shard.rebuild_index(target).await {
        Ok(report) => {
            let body = format!(
                "{{\"index\":\"{idx}\",\"entries\":{e},\"elapsed_ms\":{ms},\"shard\":{shard_id}}}\n",
                idx = index_raw,
                e = report.entries,
                ms = report.elapsed_ms
            );
            Ok(json_response(StatusCode::CREATED, body))
        }
        Err(e) => {
            warn!(error = %e, index = index_raw, "rebuild failed");
            Ok(text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "index rebuild failed\n",
            ))
        }
    }
}
