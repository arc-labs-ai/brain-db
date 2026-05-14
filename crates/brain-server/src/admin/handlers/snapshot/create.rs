//! `POST /v1/snapshots[?shard=N]` — take a snapshot of one shard.

use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Response, StatusCode};
use tracing::warn;

use crate::admin::query;
use crate::admin::util::{json_response, text_response};
use crate::admin::AdminState;

pub async fn handle(query_str: &str, state: &Arc<AdminState>) -> Response<ResponseBody> {
    let shard_id = match query::shard_required(query_str) {
        Ok(id) => id,
        Err(msg) => return text_response(StatusCode::BAD_REQUEST, &format!("{msg}\n")),
    };
    let Some(shard) = state.shards.get(shard_id) else {
        return text_response(StatusCode::NOT_FOUND, "shard out of range\n");
    };
    match shard.take_snapshot().await {
        Ok(id) => {
            let body = format!("{{\"id\":{id},\"shard\":{shard_id}}}\n");
            json_response(StatusCode::CREATED, body)
        }
        Err(e) => {
            warn!(error = %e, "snapshot create failed");
            text_response(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e}\n"))
        }
    }
}
