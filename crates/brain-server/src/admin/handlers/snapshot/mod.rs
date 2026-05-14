//! Admin HTTP handlers for the snapshot family (spec §14/06 §5;
//! sub-task 10.9).
//!
//! Routes:
//! - `POST /v1/snapshots[?shard=N]`        → take snapshot
//! - `GET  /v1/snapshots`                  → list across all shards
//! - `DELETE /v1/snapshots/<id>[?shard=N]` → delete
//!
//! One prefix-registered entry-point dispatches on `(method, path)`
//! because all three routes share the `/v1/snapshots*` family.

mod create;
mod delete;
mod list;

use std::sync::Arc;

use brain_http::body::ResponseBody;
use http::{Method, Request, Response, StatusCode};
use hyper::body::Incoming;

use crate::admin::util::text_response;
use crate::admin::AdminState;

pub async fn handle(
    req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let query = req.uri().query().unwrap_or("").to_owned();

    if method == Method::POST && path == "/v1/snapshots" {
        return Ok(create::handle(&query, &state).await);
    }
    if method == Method::GET && path == "/v1/snapshots" {
        return Ok(list::handle(&state).await);
    }
    if method == Method::DELETE {
        if let Some(id_str) = path.strip_prefix("/v1/snapshots/") {
            return Ok(delete::handle(id_str, &query, &state).await);
        }
    }
    Ok(text_response(
        StatusCode::NOT_FOUND,
        "snapshot route not found\n",
    ))
}
