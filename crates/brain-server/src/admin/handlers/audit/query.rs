//! `GET /v1/audit` — one paginated page of the historical audit tables.

use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use tracing::warn;

use crate::admin::handlers::audit::{
    encode_cursor, page_to_json, parse_params, DEFAULT_QUERY_LIMIT, MAX_QUERY_LIMIT,
};
use crate::admin::util::{json_response, text_response};
use crate::admin::AdminState;
use crate::shard::AuditPage;

pub async fn query(
    req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let query_str = req.uri().query().unwrap_or("").to_owned();
    let params = match parse_params(&query_str, DEFAULT_QUERY_LIMIT, MAX_QUERY_LIMIT) {
        Ok(p) => p,
        Err(msg) => return Ok(text_response(StatusCode::BAD_REQUEST, &format!("{msg}\n"))),
    };
    let Some(shard) = state.shards.get(params.shard_id) else {
        return Ok(text_response(StatusCode::NOT_FOUND, "shard out of range\n"));
    };

    match shard
        .audit_query(params.selector, params.limit, params.cursor)
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
            // Log the internal detail; return a generic client-safe message.
            warn!(error = %e, "audit query failed");
            Ok(text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "audit query failed\n",
            ))
        }
    }
}

/// The opaque next-page token for a page, or `None` when exhausted.
fn next_cursor_str(page: &AuditPage) -> Option<String> {
    match page {
        AuditPage::Extraction { next, .. } | AuditPage::Resolution { next, .. } => {
            next.map(encode_cursor)
        }
    }
}
