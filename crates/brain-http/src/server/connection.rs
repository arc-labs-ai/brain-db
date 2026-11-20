//! Per-request handler used by the accept loop.
//!
//! Bridges the Brain [`Router`] dispatch to hyper's [`Service`]
//! shape. Lives in its own module so the accept loop stays focused
//! on listener / lifecycle concerns.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{Response, StatusCode};
use hyper::body::Incoming;
use tracing::Instrument;

use crate::body::{full, ResponseBody};
use crate::observability;
use crate::router::Router;

/// Run one request through the router with the per-request timeout
/// from the connection's `ServerLimits`. Returns `Infallible` so it
/// fits hyper's `service_fn` shape directly; errors inside the router
/// are converted to canned status responses by the router itself.
///
/// The body is wrapped in a per-request OTel span (`http.request`)
/// with `http.response.status_code` recorded after the handler
/// returns. Connection-level peer info comes from the parent
/// `http.connection` span set up in the accept loop.
pub(crate) async fn handle_request(
    router: Arc<Router<Incoming>>,
    request_timeout: Duration,
    req: http::Request<Incoming>,
) -> Result<Response<ResponseBody>, Infallible> {
    let span = observability::request_span(&req);
    async move {
        let dispatched = router.dispatch(req);
        let resp = match tokio::time::timeout(request_timeout, dispatched).await {
            Ok(r) => r,
            Err(_) => timeout_response(request_timeout),
        };
        observability::record_status(&tracing::Span::current(), resp.status().as_u16());
        Ok(resp)
    }
    .instrument(span)
    .await
}

fn timeout_response(after: Duration) -> Response<ResponseBody> {
    let body = format!("{{\"error\":\"request timed out after {after:?}\"}}\n");
    Response::builder()
        .status(StatusCode::GATEWAY_TIMEOUT)
        .header("content-type", "application/json; charset=utf-8")
        .body(full(Bytes::from(body)))
        .expect("static response always builds")
}
