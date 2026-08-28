//! `GET /v1/audit/export` — bulk convenience over the same selection as
//! `GET /v1/audit`. Walks pages internally and returns a single JSON
//! array. When the response reaches [`EXPORT_MAX_ROWS`] it stops and hands
//! back a `next_cursor` so the operator can resume; otherwise the whole
//! selection is returned in one shot with `next_cursor: null`.
//!
//! Scope mirrors the query route: deployment-wide by default (compound
//! cursor), a single shard under `?shard=N`, and the owning shard for
//! `by=memory`.

use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use tracing::warn;

use crate::admin::handlers::audit::{
    decode_compound_cursor, decode_cursor, deployment_wide_page, encode_compound_cursor,
    encode_cursor, owning_shard, page_to_json, parse_params, AuditParams, ShardPos,
    EXPORT_MAX_ROWS, EXPORT_PAGE_LIMIT,
};
use crate::admin::util::{json_response, text_response};
use crate::admin::AdminState;
use crate::shard::{AuditCursor, AuditPage, AuditSelector};

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

    match params.selector {
        AuditSelector::Memory(bytes) => single_shard(state, owning_shard(bytes), params).await,
        _ => match params.shard {
            Some(shard_id) => single_shard(state, shard_id, params).await,
            None => deployment_wide(state, params).await,
        },
    }
}

/// Bulk pull from a single shard, walking the simple cursor internally.
async fn single_shard(
    state: Arc<AdminState>,
    shard_id: usize,
    params: AuditParams,
) -> brain_http::Result<Response<ResponseBody>> {
    let start = match params.cursor.as_deref().map(decode_cursor).transpose() {
        Ok(c) => c,
        Err(msg) => return Ok(text_response(StatusCode::BAD_REQUEST, &format!("{msg}\n"))),
    };
    let Some(shard) = state.shards.get(shard_id) else {
        if matches!(params.selector, AuditSelector::Memory(_)) {
            let empty = AuditPage::Extraction {
                rows: Vec::new(),
                next: None,
            };
            return Ok(json_response(StatusCode::OK, page_to_json(&empty, None)));
        }
        return Ok(text_response(StatusCode::NOT_FOUND, "shard out of range\n"));
    };

    let mut ext_rows = Vec::new();
    let mut res_rows = Vec::new();
    let mut is_resolution = false;
    let mut cursor = start;

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
            other => break other,
        }
    };

    let page = assemble(is_resolution, ext_rows, res_rows);
    let next = final_next.map(encode_cursor);
    Ok(json_response(
        StatusCode::OK,
        page_to_json(&page, next.as_deref()),
    ))
}

/// Deployment-wide bulk pull: repeatedly merge one page across all shards,
/// advancing the compound cursor, until every shard is exhausted or the
/// row ceiling is hit.
async fn deployment_wide(
    state: Arc<AdminState>,
    params: AuditParams,
) -> brain_http::Result<Response<ResponseBody>> {
    let n = state.shards.len();
    let mut positions = match params.cursor.as_deref() {
        Some(raw) => match decode_compound_cursor(raw, n) {
            Ok(p) => p,
            Err(msg) => return Ok(text_response(StatusCode::BAD_REQUEST, &format!("{msg}\n"))),
        },
        None => vec![ShardPos::Start; n],
    };

    let mut ext_rows = Vec::new();
    let mut res_rows = Vec::new();
    let mut is_resolution = false;

    let final_positions: Vec<ShardPos> = loop {
        let (page, next_positions) = match deployment_wide_page(
            &state.shards,
            params.selector,
            EXPORT_PAGE_LIMIT,
            &positions,
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "audit export failed");
                return Ok(text_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "audit export failed\n",
                ));
            }
        };
        match page {
            AuditPage::Extraction { mut rows, .. } => ext_rows.append(&mut rows),
            AuditPage::Resolution { mut rows, .. } => {
                is_resolution = true;
                res_rows.append(&mut rows);
            }
        }
        positions = next_positions;
        let exhausted = positions.iter().all(|p| matches!(p, ShardPos::Exhausted));
        let total = ext_rows.len() + res_rows.len();
        if exhausted || total >= EXPORT_MAX_ROWS {
            break positions;
        }
    };

    let page = assemble(is_resolution, ext_rows, res_rows);
    let next = if final_positions
        .iter()
        .all(|p| matches!(p, ShardPos::Exhausted))
    {
        None
    } else {
        Some(encode_compound_cursor(&final_positions))
    };
    Ok(json_response(
        StatusCode::OK,
        page_to_json(&page, next.as_deref()),
    ))
}

/// Rebuild the accumulated rows into the appropriate [`AuditPage`] variant.
/// The `next` field is unused by `page_to_json` (the caller passes the
/// resume token separately) so a placeholder `None` is fine.
fn assemble(
    is_resolution: bool,
    ext_rows: Vec<brain_metadata::tables::audit::ExtractionAudit>,
    res_rows: Vec<brain_metadata::tables::audit::ResolutionAudit>,
) -> AuditPage {
    if is_resolution {
        AuditPage::Resolution {
            rows: res_rows,
            next: None,
        }
    } else {
        AuditPage::Extraction {
            rows: ext_rows,
            next: None,
        }
    }
}
