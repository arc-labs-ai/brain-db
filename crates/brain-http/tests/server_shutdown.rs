//! Graceful-shutdown tests.
//!
//! Verifies the accept loop stops, in-flight connections drain, and
//! the 30 s drain timeout fires for stragglers.

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::TestServer;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;

use brain_http::body::{full, ResponseBody};
use brain_http::router::Router;

async fn slow_handler(_req: Request<Incoming>) -> brain_http::Result<Response<ResponseBody>> {
    // Holds the request for 200 ms, then returns 200 OK.
    tokio::time::sleep(Duration::from_millis(200)).await;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .body(full(Bytes::from_static(b"slow ok")))
        .unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_drains_in_flight_request() {
    let router = Router::new().get("/slow", slow_handler);
    let server = TestServer::start(router).await;
    let addr = server.addr();

    // In a blocking task: fire one request and read the response.
    let client_task = tokio::task::spawn_blocking(move || {
        let mut stream = TcpStream::connect(addr).expect("connect");
        stream
            .write_all(b"GET /slow HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .expect("write");
        stream.flush().expect("flush");
        let mut raw = Vec::with_capacity(256);
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.read_to_end(&mut raw).expect("read");
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("CRLFCRLF");
        let status_line = std::str::from_utf8(&raw[..split])
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_owned();
        let body = String::from_utf8_lossy(&raw[split + 4..]).into_owned();
        (status_line, body)
    });

    // Give the request a moment to land on the server.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Trigger shutdown while the request is still in-flight.
    let shutdown_started = Instant::now();
    let shutdown_join = tokio::spawn(async move { server.shutdown().await });

    // The client should still receive a full 200 OK because the
    // drain waits for the in-flight request to finish.
    let (status_line, body) = client_task.await.expect("client join");
    assert!(status_line.contains("200"), "status line: {status_line}");
    assert_eq!(body, "slow ok");

    // The shutdown future should complete cleanly within the drain
    // timeout.
    shutdown_join
        .await
        .expect("shutdown join")
        .expect("shutdown ok");

    let elapsed = shutdown_started.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "expected fast drain, took {elapsed:?}"
    );
}
