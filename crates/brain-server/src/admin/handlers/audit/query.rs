//! `GET /v1/audit` — one paginated page of the historical audit tables.
//!
//! With `?shard=N` omitted the query is deployment-wide: it fans out to
//! every shard and merges their pages into the global order the selected
//! index implies (compound cursor). `?shard=N` slices a single shard, and
//! `by=memory` always routes to the one shard the `MemoryId` owns.

use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use tracing::warn;

use crate::admin::handlers::audit::{
    decode_compound_cursor, decode_cursor, deployment_wide_page, encode_compound_cursor,
    encode_cursor, owning_shard, page_to_json, parse_params, AuditParams, ShardPos,
    DEFAULT_QUERY_LIMIT, MAX_QUERY_LIMIT,
};
use crate::admin::util::{json_response, text_response};
use crate::admin::AdminState;
use crate::shard::{AuditPage, AuditSelector};

pub async fn query(
    req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let query_str = req.uri().query().unwrap_or("").to_owned();
    let params = match parse_params(&query_str, DEFAULT_QUERY_LIMIT, MAX_QUERY_LIMIT) {
        Ok(p) => p,
        Err(msg) => return Ok(text_response(StatusCode::BAD_REQUEST, &format!("{msg}\n"))),
    };

    // `by=memory` is inherently single-shard (a MemoryId encodes its owner);
    // an explicit `?shard=N` slices one shard; otherwise fan out.
    match params.selector {
        AuditSelector::Memory(bytes) => single_shard(state, owning_shard(bytes), params).await,
        _ => match params.shard {
            Some(shard_id) => single_shard(state, shard_id, params).await,
            None => deployment_wide(state, params).await,
        },
    }
}

/// One page from a single shard, using the simple 24-byte cursor. An
/// owning shard that routed out of range (a `by=memory` id foreign to this
/// deployment) has no local rows: return an empty page, not an error.
async fn single_shard(
    state: Arc<AdminState>,
    shard_id: usize,
    params: AuditParams,
) -> brain_http::Result<Response<ResponseBody>> {
    let cursor = match params.cursor.as_deref().map(decode_cursor).transpose() {
        Ok(c) => c,
        Err(msg) => return Ok(text_response(StatusCode::BAD_REQUEST, &format!("{msg}\n"))),
    };
    let Some(shard) = state.shards.get(shard_id) else {
        // A `by=memory` id that routes off this deployment simply has no
        // audit rows here; an explicit out-of-range `?shard=N` is a 404.
        if matches!(params.selector, AuditSelector::Memory(_)) {
            let empty = AuditPage::Extraction {
                rows: Vec::new(),
                next: None,
            };
            return Ok(json_response(StatusCode::OK, page_to_json(&empty, None)));
        }
        return Ok(text_response(StatusCode::NOT_FOUND, "shard out of range\n"));
    };

    match shard
        .audit_query(params.selector, params.limit, cursor)
        .await
    {
        Ok(page) => {
            let next = next_cursor_str(&page);
            Ok(json_response(
                StatusCode::OK,
                page_to_json(&page, next.as_deref()),
            ))
        }
        Err(e) => {
            warn!(error = %e, "audit query failed");
            Ok(text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "audit query failed\n",
            ))
        }
    }
}

/// One deployment-wide page: merge every shard's slice into global order,
/// carrying a compound per-shard cursor.
async fn deployment_wide(
    state: Arc<AdminState>,
    params: AuditParams,
) -> brain_http::Result<Response<ResponseBody>> {
    let n = state.shards.len();
    let positions = match params.cursor.as_deref() {
        Some(raw) => match decode_compound_cursor(raw, n) {
            Ok(p) => p,
            Err(msg) => return Ok(text_response(StatusCode::BAD_REQUEST, &format!("{msg}\n"))),
        },
        None => vec![ShardPos::Start; n],
    };

    match deployment_wide_page(&state.shards, params.selector, params.limit, &positions).await {
        Ok((page, next_positions)) => {
            let next = compound_next(&next_positions);
            Ok(json_response(
                StatusCode::OK,
                page_to_json(&page, next.as_deref()),
            ))
        }
        Err(e) => {
            warn!(error = %e, "audit query failed");
            Ok(text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "audit query failed\n",
            ))
        }
    }
}

/// Encode the compound `next_cursor`, or `None` once every shard is
/// exhausted (the deployment-wide walk has terminated).
fn compound_next(positions: &[ShardPos]) -> Option<String> {
    if positions.iter().all(|p| matches!(p, ShardPos::Exhausted)) {
        None
    } else {
        Some(encode_compound_cursor(positions))
    }
}

/// The opaque single-shard next-page token for a page, or `None` when
/// exhausted.
fn next_cursor_str(page: &AuditPage) -> Option<String> {
    match page {
        AuditPage::Extraction { next, .. } | AuditPage::Resolution { next, .. } => {
            next.map(encode_cursor)
        }
    }
}
