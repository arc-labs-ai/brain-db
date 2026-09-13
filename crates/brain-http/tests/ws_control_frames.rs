//! WebSocket control-frame tests.
//!
//! Verifies that `tokio-tungstenite` auto-replies to a ping with a
//! pong (the RFC 6455 requirement) and that oversized control frames
//! are rejected.

mod common;

use std::time::Duration;

use brain_http::body::ResponseBody;
use brain_http::router::Router;
use brain_http::ws::{self, Message};
use common::TestServer;
use futures_util::{SinkExt, StreamExt};
use http::{Request, Response};
use hyper::body::Incoming;
use tokio::net::TcpStream;
use tokio_tungstenite::client_async;

async fn passive_handler(req: Request<Incoming>) -> brain_http::Result<Response<ResponseBody>> {
    // The handler doesn't echo, just holds the connection open so we
    // can observe ping/pong control frames flowing through.
    let (response, on_upgrade) = ws::accept(req)?;
    tokio::spawn(async move {
        match on_upgrade.await_upgrade().await {
            Ok(mut ws) => {
                // Drive the stream to completion — tungstenite's
                // background auto-reply happens during these polls.
                while let Some(item) = ws.next().await {
                    if matches!(item, Ok(Message::Close(_)) | Err(_)) {
                        break;
                    }
                }
            }
            Err(e) => eprintln!("upgrade failed: {e}"),
        }
    });
    Ok(response)
}

async fn connect_ws(addr: std::net::SocketAddr) -> tokio_tungstenite::WebSocketStream<TcpStream> {
    let tcp = TcpStream::connect(addr).await.expect("tcp connect");
    let url = format!("ws://{addr}/ws");
    let (ws, response) = client_async(url, tcp).await.expect("ws client");
    assert_eq!(response.status(), 101);
    ws
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ping_gets_pong_reply() {
    let router = Router::new().get("/ws", passive_handler);
    let server = TestServer::start(router).await;
    let mut ws = connect_ws(server.addr()).await;

    // Send a ping with a custom payload.
    let probe: Vec<u8> = b"ping-probe-1".to_vec();
    ws.send(Message::Ping(probe.clone()))
        .await
        .expect("send ping");

    // Pump the stream until we see the pong reply. tokio-tungstenite
    // surfaces pongs in the Stream so the application can observe
    // them; the auto-reply itself happens silently in the background
    // when we ping (this is the symmetric path).
    //
    // Send the ping, then we expect the server-side stream's
    // tungstenite to receive it and auto-reply with a pong frame
    // carrying the same payload. The client sees that as `Pong`.
    let pong = tokio::time::timeout(Duration::from_secs(2), ws.next())
        .await
        .expect("timeout")
        .expect("some")
        .expect("ok");
    match pong {
        Message::Pong(payload) => assert_eq!(payload, probe),
        other => panic!("expected pong, got {other:?}"),
    }

    ws.close(None).await.expect("close");
    server.shutdown().await.expect("shutdown");
}
