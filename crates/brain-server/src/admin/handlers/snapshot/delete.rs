//! `DELETE /v1/snapshots/<id>[?shard=N]` — delete one snapshot.

use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Response, StatusCode};
use tracing::warn;

use crate::admin::query;
use crate::admin::util::text_response;
use crate::admin::AdminState;

pub async fn handle(
    id_str: &str,
    query_str: &str,
    state: &Arc<AdminState>,
) -> Response<ResponseBody> {
    let Ok(id) = id_str.parse::<u64>() else {
        return text_response(StatusCode::BAD_REQUEST, "snapshot id must be a u64\n");
    };
    let shard_id = match query::shard_required(query_str) {
        Ok(id) => id,
        Err(msg) => return text_response(StatusCode::BAD_REQUEST, &format!("{msg}\n")),
    };
    let Some(shard) = state.shards.get(shard_id) else {
        return text_response(StatusCode::NOT_FOUND, "shard out of range\n");
    };
    match shard.delete_snapshot(id).await {
        Ok(()) => Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(brain_http::body::empty())
            .expect("static response always builds"),
        Err(e) => {
            warn!(error = %e, "snapshot delete failed");
            text_response(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e}\n"))
        }
    }
}
