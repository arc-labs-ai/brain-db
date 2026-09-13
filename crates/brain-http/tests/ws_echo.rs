//! WebSocket integration tests — echo round-trip for text and
//! binary, plus a multi-message sequence.
//!
//! Server uses `brain_http::ws::accept`; client uses
//! `tokio_tungstenite::client_async` against the same raw TCP stream
//! the server's accept loop produced. Driving the WS protocol from
//! both sides exercises framing + masking end-to-end.

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

/// Server-side echo handler. Spawns the upgrade future on the tokio
/// runtime so the 101 response is sent first.
async fn echo_handler(req: Request<Incoming>) -> brain_http::Result<Response<ResponseBody>> {
    let (response, on_upgrade) = ws::accept(req)?;
    tokio::spawn(async move {
        match on_upgrade.await_upgrade().await {
            Ok(mut ws) => {
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
                        Ok(_) => {} // ping/pong handled by tungstenite
                    }
                }
            }
            Err(e) => eprintln!("upgrade failed: {e}"),
        }
    });
    Ok(response)
}

/// Open a WS client connection to a bound brain-http server.
async fn connect_ws(
    addr: std::net::SocketAddr,
    path: &str,
) -> tokio_tungstenite::WebSocketStream<TcpStream> {
    let tcp = TcpStream::connect(addr).await.expect("tcp connect");
    let url = format!("ws://{addr}{path}");
    let (ws, response) = client_async(url, tcp).await.expect("ws client");
    assert_eq!(response.status(), 101);
    ws
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_round_trip() {
    let router = Router::new().get("/ws", echo_handler);
    let server = TestServer::start(router).await;
    let mut ws = connect_ws(server.addr(), "/ws").await;

    ws.send(Message::Text("hello-brain".into()))
        .await
        .expect("send");
    let echoed = ws.next().await.expect("recv").expect("ok");
    assert_eq!(echoed, Message::Text("hello-brain".into()));

    ws.close(None).await.expect("close");
    server.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_round_trip() {
    let router = Router::new().get("/ws", echo_handler);
    let server = TestServer::start(router).await;
    let mut ws = connect_ws(server.addr(), "/ws").await;

    let payload: Vec<u8> = (0u8..=255u8).collect();
    ws.send(Message::Binary(payload.clone()))
        .await
        .expect("send");
    let echoed = ws.next().await.expect("recv").expect("ok");
    match echoed {
        Message::Binary(b) => assert_eq!(b, payload),
        other => panic!("expected binary, got {other:?}"),
    }

    ws.close(None).await.expect("close");
    server.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ten_messages_in_sequence() {
    let router = Router::new().get("/ws", echo_handler);
    let server = TestServer::start(router).await;
    let mut ws = connect_ws(server.addr(), "/ws").await;

    for i in 0..10 {
        let msg = format!("msg-{i}");
        ws.send(Message::Text(msg.clone())).await.expect("send");
        let echoed = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .expect("recv timeout")
            .expect("some")
            .expect("ok");
        assert_eq!(echoed, Message::Text(msg));
    }

    ws.close(None).await.expect("close");
    server.shutdown().await.expect("shutdown");
}
