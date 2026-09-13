//! `GET /metrics` — Prometheus text-format exposition.
//!
//! The exposition body is built by [`crate::metrics::format`] and the
//! typed primitives in `crate::metrics::{counter,gauge,histogram}`.
//! This handler is a thin shim: build the body, set the content-type,
//! return.

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
    let snap = state.metrics_snapshot();
    let body = format::format(&snap).await;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", HDR_PROMETHEUS)
        .body(full(Bytes::from(body)))
        .expect("static response always builds"))
}
