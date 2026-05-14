//! Admin HTTP handlers for the snapshot family (spec §14/06 §5;
//! sub-task 10.9).
//!
//! Routes:
//! - `POST /v1/snapshots[?shard=N]`       → take snapshot
//! - `GET  /v1/snapshots`                 → list across all shards
//! - `DELETE /v1/snapshots/<id>[?shard=N]` → delete
//!
//! Migrated to brain-http in M3. One prefix-handler dispatches
//! internally on `(method, path)` because all three routes share the
//! `/v1/snapshots*` family.

use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Method, Request, Response, StatusCode};
use hyper::body::Incoming;
use tracing::warn;

use crate::admin::util::{json_response, text_response};
use crate::admin::AdminState;

pub async fn handle(
    req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let query = req.uri().query().unwrap_or("").to_owned();

    if method == Method::POST && path == "/v1/snapshots" {
        return Ok(handle_create(&query, &state).await);
    }
    if method == Method::GET && path == "/v1/snapshots" {
        return Ok(handle_list(&state).await);
    }
    if method == Method::DELETE {
        if let Some(id_str) = path.strip_prefix("/v1/snapshots/") {
            return Ok(handle_delete(id_str, &query, &state).await);
        }
    }
    // Route table registered the prefix; if we get here the
    // request didn't match any sub-shape.
    Ok(text_response(
        StatusCode::NOT_FOUND,
        "snapshot route not found\n",
    ))
}

async fn handle_create(query: &str, state: &Arc<AdminState>) -> Response<ResponseBody> {
    let shard_id = match parse_shard(query) {
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

async fn handle_list(state: &Arc<AdminState>) -> Response<ResponseBody> {
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

async fn handle_delete(
    id_str: &str,
    query: &str,
    state: &Arc<AdminState>,
) -> Response<ResponseBody> {
    let Ok(id) = id_str.parse::<u64>() else {
        return text_response(StatusCode::BAD_REQUEST, "snapshot id must be a u64\n");
    };
    let shard_id = match parse_shard(query) {
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

/// Parse `?shard=N` from a URI query string. Defaults to `0`.
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
        assert_eq!(parse_shard("other=1&shard=7").unwrap(), 7);
    }

    #[test]
    fn parse_shard_rejects_garbage() {
        assert!(parse_shard("shard=abc").is_err());
    }
}
