//! Admin HTTP handlers for the snapshot family.
//!
//! Routes:
//! - `POST /v1/snapshots[?shard=N]`                → take snapshot
//! - `POST /v1/snapshots/<id>/restore[?shard=N]`   → restore (destructive)
//! - `GET  /v1/snapshots`                          → list across all shards
//! - `DELETE /v1/snapshots/<id>[?shard=N]`         → delete
//!
//! One prefix-registered entry-point dispatches on `(method, path)`
//! because all routes share the `/v1/snapshots*` family.

mod create;
mod delete;
mod list;
mod restore;

use std::sync::Arc;

use brain_http::body::{read_to_bytes, ResponseBody, MAX_BODY_BYTES};
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

    if method == Method::POST {
        // `/v1/snapshots/<id>/restore` — destructive, body-carrying.
        if let Some(rest) = path.strip_prefix("/v1/snapshots/") {
            if let Some(id_str) = rest.strip_suffix("/restore") {
                let body = read_to_bytes(req.into_body(), MAX_BODY_BYTES).await?;
                return Ok(restore::handle(id_str, &query, body, &state).await);
            }
        }
        if path == "/v1/snapshots" {
            return Ok(create::handle(&query, &state).await);
        }
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
