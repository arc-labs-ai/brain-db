//! Admin HTTP handler for `rebuild-ann` (spec §14/06 §4;
//! sub-task 10.10).
//!
//! Route:
//! - `POST /v1/rebuild-ann[?shard=N]` → 201 +
//!   `{"entries":N,"elapsed_ms":N,"shard":N}`

use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use tracing::warn;

use crate::admin::util::{json_response, text_response};
use crate::admin::AdminState;

pub async fn handle(
    req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let query = req.uri().query().unwrap_or("").to_owned();
    let shard_id = match parse_shard(&query) {
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
            warn!(error = %e, "rebuild-ann failed");
            Ok(text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("{e}\n"),
            ))
        }
    }
}

fn parse_shard(query: &str) -> Result<usize, String> {
    if query.is_empty() {
        return Ok(0);
    }
    for kv in query.split('&') {
        if let Some(rest) = kv.strip_prefix("shard=") {
            return rest
                .parse::<usize>()
                .map_err(|e| format!("invalid shard: {e}"));
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_shard_default() {
        assert_eq!(parse_shard("").unwrap(), 0);
    }

    #[test]
    fn parse_shard_explicit() {
        assert_eq!(parse_shard("shard=3").unwrap(), 3);
    }

    #[test]
    fn parse_shard_rejects_garbage() {
        assert!(parse_shard("shard=abc").is_err());
    }
}
