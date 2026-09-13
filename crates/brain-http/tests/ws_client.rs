//! WebSocket client tests.
//!
//! Uses brain-http's own `ws::connect` against an `ws::accept`-based
//! server, closing the client/server loop end-to-end.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use brain_http::router::Router;
use brain_http::ws::{self, Message};
use common::TestServer;
use futures_util::{SinkExt, StreamExt};
use http::{HeaderName, HeaderValue, Request};
use hyper::body::Incoming;

async fn echo_handler(
    req: Request<Incoming>,
) -> brain_http::Result<http::Response<brain_http::body::ResponseBody>> {
    let (response, on_upgrade) = ws::accept(req)?;
    tokio::spawn(async move {
        if let Ok(mut ws) = on_upgrade.await_upgrade().await {
            while let Some(item) = ws.next().await {
                match item {
                    Ok(Message::Text(t)) => {
                        if ws.send(Message::Text(t)).await.is_err() {
                            break;
                        }
                    }
                    Ok(Message::Binary(b)) => {
                        if ws.send(Message::Binary(b)).await.is_err() {
                            break;
                        }
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        }
    });
    Ok(response)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_server_echo_round_trip() {
    let router = Router::new().get("/ws", echo_handler);
    let server = TestServer::start(router).await;
    let url = format!("ws://{}/ws", server.addr());

    let mut conn = ws::connect(&url).await.expect("connect");
    assert_eq!(conn.response.status(), 101);

    // Text echo.
    conn.stream
        .send(Message::Text("hello".into()))
        .await
        .expect("send text");
    let reply = conn.stream.next().await.expect("recv").expect("ok");
    assert_eq!(reply, Message::Text("hello".into()));

    // Binary echo.
    let payload: Vec<u8> = (0u8..=63u8).collect();
    conn.stream
        .send(Message::Binary(payload.clone()))
        .await
        .expect("send binary");
    let reply = conn.stream.next().await.expect("recv").expect("ok");
    match reply {
        Message::Binary(b) => assert_eq!(b, payload),
        other => panic!("expected binary, got {other:?}"),
    }

    conn.stream.close(None).await.expect("close");
    server.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_timeout_fires_on_unreachable() {
    // 192.0.2.0/24 is TEST-NET-1 — guaranteed unrouted per RFC 5737.
    // Any connect attempt will hang until the TCP timeout. Our
    // builder-level timeout should fire first.
    let url = "ws://192.0.2.1:9/ws";
    let started = std::time::Instant::now();
    let res = ws::ConnectBuilder::new(url)
        .connect_timeout(Duration::from_millis(200))
        .connect()
        .await;
    let elapsed = started.elapsed();

    let err = match res {
        Ok(_) => panic!("expected timeout, got Ok"),
        Err(e) => e,
    };
    assert!(
        matches!(err, brain_http::Error::Timeout(_)),
        "expected Timeout, got {err:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "timeout fired too late: {elapsed:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn custom_header_propagates() {
    // Handler asserts the custom header is present, then upgrades.
    let saw_header = Arc::new(AtomicBool::new(false));
    let saw_header_handler = saw_header.clone();

    let handler = move |req: Request<Incoming>| {
        let saw = saw_header_handler.clone();
        async move {
            if req
                .headers()
                .get("x-brain-test")
                .and_then(|v| v.to_str().ok())
                == Some("token-42")
            {
                saw.store(true, Ordering::SeqCst);
            }
            let (response, on_upgrade) = ws::accept(req)?;
            tokio::spawn(async move {
                if let Ok(mut ws) = on_upgrade.await_upgrade().await {
                    // Receive one message then close.
                    while let Some(item) = ws.next().await {
                        if matches!(item, Ok(Message::Close(_)) | Err(_)) {
                            break;
                        }
                        if let Ok(Message::Text(t)) = item {
                            let _ = ws.send(Message::Text(t)).await;
                        }
                    }
                }
            });
            Ok::<_, brain_http::Error>(response)
        }
    };

    let router = Router::new().route(http::Method::GET, "/ws", handler);
    let server = TestServer::start(router).await;
    let url = format!("ws://{}/ws", server.addr());

    let mut conn = ws::ConnectBuilder::new(&url)
        .header(
            HeaderName::from_static("x-brain-test"),
            HeaderValue::from_static("token-42"),
        )
        .connect()
        .await
        .expect("connect");
    conn.stream
        .send(Message::Text("ping".into()))
        .await
        .expect("send");
    let _ = conn.stream.next().await;
    conn.stream.close(None).await.expect("close");

    // Give the handler a moment to run.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        saw_header.load(Ordering::SeqCst),
        "server handler did not see the custom header"
    );

    server.shutdown().await.expect("shutdown");
}
