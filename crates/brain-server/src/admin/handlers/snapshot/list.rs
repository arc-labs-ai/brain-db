//! `GET /v1/snapshots` — list snapshots across every shard.

use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Response, StatusCode};

use crate::admin::util::{json_response, text_response};
use crate::admin::AdminState;

pub async fn handle(state: &Arc<AdminState>) -> Response<ResponseBody> {
    let mut all = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for (idx, shard) in state.shards.iter().enumerate() {
        match shard.list_snapshots().await {
            Ok(descs) => {
                for d in descs {
                    all.push((idx, d));
                }
            }
            Err(e) => errors.push(format!("shard {idx}: {e}")),
        }
    }
    if !errors.is_empty() {
        let msg = errors.join("; ");
        return text_response(StatusCode::INTERNAL_SERVER_ERROR, &format!("{msg}\n"));
    }
    let mut body = String::from("[");
    for (i, (shard_id, d)) in all.iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        body.push_str(&format!(
            "{{\"shard\":{shard_id},\"id\":{id},\"taken_at_unix_nanos\":{ts},\"size_bytes\":{sz}}}",
            shard_id = shard_id,
            id = d.id,
            ts = d.taken_at_unix_nanos,
            sz = d.size_bytes,
        ));
    }
    body.push_str("]\n");
    json_response(StatusCode::OK, body)
}
