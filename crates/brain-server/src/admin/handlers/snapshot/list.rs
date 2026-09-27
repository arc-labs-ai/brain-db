//! `GET /v1/snapshots` — list snapshots across every shard.

use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Response, StatusCode};
use tracing::warn;

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
        return list_error_response(&errors);
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

/// Build the 500 response for a snapshot-listing failure. The per-shard
/// error `Display` carries host internals (filesystem paths, redb detail),
/// so it is logged server-side but never echoed to the client.
fn list_error_response(errors: &[String]) -> Response<ResponseBody> {
    warn!(detail = %errors.join("; "), "snapshot listing failed");
    text_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "snapshot listing failed\n",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt as _;

    async fn body_string(resp: Response<ResponseBody>) -> String {
        let bytes = resp
            .into_body()
            .collect()
            .await
            .expect("collect body")
            .to_bytes();
        String::from_utf8(bytes.to_vec()).expect("utf8")
    }

    #[tokio::test]
    async fn list_error_does_not_leak_internal_detail() {
        // A shard's list_snapshots error Display can carry host filesystem
        // paths and redb internals; those must be logged, not echoed.
        let errors = vec![
            "shard 0: io error opening /srv/brain/data/shard-0/snapshots: redb: table not found"
                .to_string(),
        ];
        let resp = list_error_response(&errors);
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_string(resp).await;
        assert!(
            !body.contains("/srv/brain/data"),
            "response leaked a filesystem path: {body}"
        );
        assert!(
            !body.contains("redb"),
            "response leaked redb detail: {body}"
        );
        assert!(
            body.contains("snapshot listing failed"),
            "unexpected body: {body}"
        );
    }
}
