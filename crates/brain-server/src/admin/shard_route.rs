//! Admin HTTP handlers for `shard` (spec §14/06 §11; sub-task 10.11).
//!
//! Routes:
//! - `GET /v1/shards` → 200 + `{"shards":[{"index":N,"shard_id":N}]}`
//! - `POST /v1/shards` / `DELETE /v1/shards/{idx}` → 501 (cluster
//!   expansion / decommission is Phase-12 territory).
//!
//! Note: file name is `shard_route.rs`, not `shard.rs`, because
//! `crate::shard` already exists at the workspace level.

use std::fmt::Write as _;
use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;

use crate::admin::util::{json_response, not_implemented};
use crate::admin::AdminState;

/// `GET /v1/shards` handler.
pub async fn list(
    _req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let mut body = String::with_capacity(64);
    body.push_str("{\"shards\":[");
    for (i, shard) in state.shards.iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        write!(
            &mut body,
            "{{\"index\":{i},\"shard_id\":{id}}}",
            id = shard.shard_id(),
        )
        .expect("string write");
    }
    body.push_str("]}\n");
    Ok(json_response(StatusCode::OK, body))
}

/// `POST /v1/shards` handler — deferred.
pub async fn create(
    _req: Request<Incoming>,
    _state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    Ok(not_implemented(
        "phase-12/shard-create",
        "cluster expansion via online shard creation",
    ))
}

/// `DELETE /v1/shards/{idx}` handler — deferred.
pub async fn delete(
    _req: Request<Incoming>,
    _state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    Ok(not_implemented(
        "phase-12/shard-delete",
        "cluster decommission via online shard delete",
    ))
}
