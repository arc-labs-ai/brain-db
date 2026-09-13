//! Smoke: prove brain-http types compose with hyper's `Service`
//! infrastructure.
//!
//! This is NOT a server test. The point is to verify
//! that:
//!
//! 1. `service_fn` accepts a Brain handler shape.
//! 2. The handler can be invoked end-to-end through the `Service`
//!    trait, producing a `Response<ResponseBody>`.
//! 3. `body::full` round-trips through the body collection helper.
//!
//! The handler is generic over `B: Body` so
//! we can drive the smoke test with a synthetic `Full<Bytes>` body
//! rather than the production `hyper::body::Incoming`.

use brain_http::body::{empty, full, read_to_bytes, ResponseBody, MAX_BODY_BYTES};
use brain_http::{service_fn, Error, StatusCode};
use bytes::Bytes;
use http::{Request, Response};
use http_body_util::Full;
use hyper::service::Service;

#[tokio::test]
async fn handler_round_trips_via_service_fn() {
    let svc = service_fn(echo_handler);
    let req = Request::builder()
        .method("POST")
        .uri("/echo")
        .body(Full::new(Bytes::from_static(b"ping")))
        .unwrap();

    let resp = svc.call(req).await.expect("handler ok");
    assert_eq!(resp.status(), StatusCode::OK);

    let collected = read_to_bytes(resp.into_body(), MAX_BODY_BYTES)
        .await
        .expect("collect");
    assert_eq!(collected.as_ref(), b"echo: ping");
}

#[tokio::test]
async fn no_content_handler_returns_empty_body() {
    let svc = service_fn(no_content_handler);
    let req = Request::builder()
        .uri("/healthz")
        .body(Full::new(Bytes::new()))
        .unwrap();

    let resp = svc.call(req).await.expect("handler ok");
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let collected = read_to_bytes(resp.into_body(), MAX_BODY_BYTES)
        .await
        .expect("collect");
    assert!(collected.is_empty());
}

#[tokio::test]
async fn body_too_large_path_short_circuits() {
    let body = Full::new(Bytes::from(vec![0u8; 2048]));
    let err = read_to_bytes(body, 1024).await.expect_err("over limit");
    assert!(matches!(
        err,
        Error::BodyTooLarge {
            actual: 2048,
            limit: 1024
        }
    ));
}

// ─── handlers used by the tests above ──────────────────────────────────

async fn echo_handler(req: Request<Full<Bytes>>) -> brain_http::Result<Response<ResponseBody>> {
    let body = read_to_bytes(req.into_body(), MAX_BODY_BYTES).await?;
    let mut prefixed = Vec::with_capacity(body.len() + 6);
    prefixed.extend_from_slice(b"echo: ");
    prefixed.extend_from_slice(&body);
    Ok(Response::builder()
        .status(StatusCode::OK)
        .body(full(Bytes::from(prefixed)))
        .unwrap())
}

async fn no_content_handler(
    _req: Request<Full<Bytes>>,
) -> brain_http::Result<Response<ResponseBody>> {
    Ok(Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(empty())
        .unwrap())
}
