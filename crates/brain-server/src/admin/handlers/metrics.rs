//! `GET /metrics` — Prometheus text-format exposition.
//!
//! As of Phase 12 sub-task 12.1a the writeln-chain that lived here is
//! moved into [`crate::metrics::format`] and the typed primitives in
//! `crate::metrics::{counter,gauge,histogram}`. This handler is now a
//! thin shim: build the body, set the content-type, return.

use std::sync::Arc;

use brain_http::body::{full, ResponseBody};
use bytes::Bytes;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;

use crate::admin::AdminState;
use crate::metrics::format;

const HDR_PROMETHEUS: &str = "text/plain; version=0.0.4; charset=utf-8";

/// `GET /metrics` handler.
pub async fn handle(
    _req: Request<Incoming>,
    state: Arc<AdminState>,
) -> brain_http::Result<Response<ResponseBody>> {
    let body = format::format(&state).await;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", HDR_PROMETHEUS)
        .body(full(Bytes::from(body)))
        .expect("static response always builds"))
}
