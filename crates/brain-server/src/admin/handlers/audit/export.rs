//! `GET /v1/audit/export` — bulk convenience over the same selection as
//! `GET /v1/audit`. Walks pages internally and returns a single JSON
//! array. When the response reaches [`EXPORT_MAX_ROWS`] it stops and hands
//! back a `next_cursor` so the operator can resume; otherwise the whole
//! selection is returned in one shot with `next_cursor: null`.

use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use tracing::warn;

use crate::admin::handlers::audit::{
    encode_cursor, page_to_json, parse_params, EXPORT_MAX_ROWS, EXPORT_PAGE_LIMIT,
};
use crate::admin::util::{json_response, text_response};
use crate::admin::AdminState;
use crate::shard::{AuditCursor, AuditPage};

pub async fn export(
    req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let query_str = req.uri().query().unwrap_or("").to_owned();
    // Export fixes the page size internally; the caller's `limit=` is
    // irrelevant to a bulk pull.
    let params = match parse_params(&query_str, EXPORT_PAGE_LIMIT, EXPORT_PAGE_LIMIT) {
        Ok(p) => p,
        Err(msg) => return Ok(text_response(StatusCode::BAD_REQUEST, &format!("{msg}\n"))),
    };
    let Some(shard) = state.shards.get(params.shard_id) else {
        return Ok(text_response(StatusCode::NOT_FOUND, "shard out of range\n"));
    };

    let mut ext_rows = Vec::new();
    let mut res_rows = Vec::new();
    let mut is_resolution = false;
    let mut cursor = params.cursor;

    let final_next: Option<AuditCursor> = loop {
        let page = match shard
            .audit_query(params.selector, EXPORT_PAGE_LIMIT, cursor)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "audit export failed");
                return Ok(text_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "audit export failed\n",
                ));
            }
        };
        let next = match page {
            AuditPage::Extraction { mut rows, next } => {
                ext_rows.append(&mut rows);
                next
            }
            AuditPage::Resolution { mut rows, next } => {
                is_resolution = true;
                res_rows.append(&mut rows);
                next
            }
        };
        let total = ext_rows.len() + res_rows.len();
        match next {
            Some(c) if total < EXPORT_MAX_ROWS => cursor = Some(c),
            // Exhausted (next=None) or hit the export cap (next=Some,
            // returned so the operator resumes).
            other => break other,
        }
    };

    let page = if is_resolution {
        AuditPage::Resolution {
            rows: res_rows,
            next: final_next,
        }
    } else {
        AuditPage::Extraction {
            rows: ext_rows,
            next: final_next,
        }
    };
    let next = final_next.map(encode_cursor);
    Ok(json_response(
        StatusCode::OK,
        page_to_json(&page, next.as_deref()),
    ))
}
