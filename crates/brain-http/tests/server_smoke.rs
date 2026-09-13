//! Server smoke: real TCP, real hyper, real router.
//!
//! Three scenarios:
//! 1. Round-trip GET with a small response body.
//! 2. Round-trip POST with an echoed body.
//! 3. Five requests on a single keep-alive TCP connection.

mod common;

use bytes::Bytes;
use common::{http_request, keep_alive_round_trip, TestServer};
use http::{Method, Request, Response, StatusCode};
use hyper::body::Incoming;

use brain_http::body::{full, read_to_bytes, ResponseBody, MAX_BODY_BYTES};
use brain_http::router::Router;

async fn healthz(_req: Request<Incoming>) -> brain_http::Result<Response<ResponseBody>> {
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/plain; charset=utf-8")
        .body(full(Bytes::from_static(b"ok")))
        .unwrap())
}

async fn echo(req: Request<Incoming>) -> brain_http::Result<Response<ResponseBody>> {
    let body = read_to_bytes(req.into_body(), MAX_BODY_BYTES).await?;
    let mut prefixed = Vec::with_capacity(body.len() + 6);
    prefixed.extend_from_slice(b"echo: ");
    prefixed.extend_from_slice(&body);
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/plain; charset=utf-8")
        .body(full(Bytes::from(prefixed)))
        .unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_round_trip() {
    let router = Router::new().get("/healthz", healthz);
    let server = TestServer::start(router).await;
    let (status, body) = http_request(
        server.addr(),
        "GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status, 200);
    assert_eq!(body, "ok");
    server.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_round_trip() {
    let router = Router::new().post("/echo", echo);
    let server = TestServer::start(router).await;
    let payload = "hello-brain";
    let request = format!(
        "POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        payload.len(),
        payload
    );
    let (status, body) = http_request(server.addr(), &request);
    assert_eq!(status, 200);
    assert_eq!(body, "echo: hello-brain");
    server.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keep_alive_serves_multiple_requests() {
    let router = Router::new().get("/healthz", healthz);
    let server = TestServer::start(router).await;
    let bodies = tokio::task::spawn_blocking({
        let addr = server.addr();
        move || keep_alive_round_trip(addr, "/healthz", 5)
    })
    .await
    .expect("join");
    assert_eq!(bodies.len(), 5);
    for b in bodies {
        assert_eq!(b, "ok");
    }
    server.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_route_returns_404() {
    let router = Router::new().get("/healthz", healthz);
    let server = TestServer::start(router).await;
    let (status, _body) = http_request(
        server.addr(),
        "GET /totally-fake HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status, 404);
    server.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn method_mismatch_returns_405() {
    let router = Router::new().post("/v1/snap", echo);
    let server = TestServer::start(router).await;
    let (status, _body) = http_request(
        server.addr(),
        "GET /v1/snap HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status, 405);
    server.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prefix_route_matches_subpath() {
    async fn echo_path(req: Request<Incoming>) -> brain_http::Result<Response<ResponseBody>> {
        let path = req.uri().path().to_owned();
        Ok(Response::builder()
            .status(StatusCode::OK)
            .body(full(Bytes::from(path)))
            .unwrap())
    }
    let router = Router::new().route_prefix(Method::GET, "/v1/snapshots/", echo_path);
    let server = TestServer::start(router).await;
    let (status, body) = http_request(
        server.addr(),
        "GET /v1/snapshots/abc HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status, 200);
    assert_eq!(body, "/v1/snapshots/abc");
    server.shutdown().await.expect("shutdown");
}
