//! WebSocket close-handshake tests.
//!
//! Both directions: peer-initiated close (client closes, server's
//! stream ends cleanly) and server-initiated close (server closes,
//! client's stream sees the Close frame).

mod common;

use std::time::Duration;

use brain_http::router::Router;
use brain_http::ws::{self, Message};
use common::TestServer;
use futures_util::StreamExt;
use http::Request;
use hyper::body::Incoming;
use tokio::net::TcpStream;
use tokio_tungstenite::client_async;

async fn connect_ws(addr: std::net::SocketAddr) -> tokio_tungstenite::WebSocketStream<TcpStream> {
    let tcp = TcpStream::connect(addr).await.expect("tcp connect");
    let url = format!("ws://{addr}/ws");
    let (ws, response) = client_async(url, tcp).await.expect("ws client");
    assert_eq!(response.status(), 101);
    ws
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peer_initiated_close_completes_handshake() {
    // Server-side: drive the stream until None, then signal the test
    // via an unbounded mpsc so we can assert the server saw a clean
    // close (no error).
    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<&'static str>();
    let handler = move |req: Request<Incoming>| {
        let done_tx = done_tx.clone();
        async move {
            let (response, on_upgrade) = ws::accept(req)?;
            tokio::spawn(async move {
                if let Ok(mut ws) = on_upgrade.await_upgrade().await {
                    loop {
                        match ws.next().await {
                            None => {
                                let _ = done_tx.send("none");
                                return;
                            }
                            Some(Ok(_)) => continue,
                            Some(Err(_)) => {
                                let _ = done_tx.send("err");
                                return;
                            }
                        }
                    }
                }
            });
            Ok::<_, brain_http::Error>(response)
        }
    };
    let router = Router::new().route(http::Method::GET, "/ws", handler);
    let server = TestServer::start(router).await;
    let mut ws = connect_ws(server.addr()).await;

    // Client initiates close.
    ws.close(None).await.expect("close");

    // Client should see the close finish — `next()` returns None
    // after the close exchange.
    let next = tokio::time::timeout(Duration::from_secs(2), ws.next())
        .await
        .expect("client next timeout");
    match next {
        Some(Ok(Message::Close(_))) => {}
        Some(Ok(other)) => panic!("expected Close, got {other:?}"),
        None => {} // already drained
        Some(Err(e)) => panic!("client recv error: {e}"),
    }
    let final_next = tokio::time::timeout(Duration::from_secs(2), ws.next()).await;
    assert!(matches!(final_next, Ok(None)));

    let signal = tokio::time::timeout(Duration::from_secs(2), done_rx.recv())
        .await
        .expect("signal timeout")
        .expect("signal");
    assert_eq!(signal, "none", "server saw clean stream end");

    server.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_initiated_close() {
    // Handler that closes the stream as soon as the WS connection
    // upgrades. Used to test the client-side observation of a
    // server-initiated close.
    let handler = |req: Request<Incoming>| async move {
        let (response, on_upgrade) = ws::accept(req)?;
        tokio::spawn(async move {
            if let Ok(mut ws) = on_upgrade.await_upgrade().await {
                let _ = ws.close(None).await;
                // Drive the stream so tungstenite can flush its
                // close frame and the peer's response can arrive.
                while ws.next().await.is_some() {}
            }
        });
        Ok::<_, brain_http::Error>(response)
    };

    let router = Router::new().route(http::Method::GET, "/ws", handler);
    let server = TestServer::start(router).await;
    let mut ws = connect_ws(server.addr()).await;

    // Client should see the close frame initiated by the server.
    let next = tokio::time::timeout(Duration::from_secs(2), ws.next())
        .await
        .expect("client next timeout");
    match next {
        Some(Ok(Message::Close(_))) => {}
        Some(Ok(other)) => panic!("expected Close, got {other:?}"),
        None => panic!("stream ended before yielding Close"),
        Some(Err(e)) => panic!("client recv error: {e}"),
    }

    // Acknowledge by initiating our own close (tungstenite already
    // queued it internally; this drains the sink).
    let _ = ws.close(None).await;
    let trailing = tokio::time::timeout(Duration::from_secs(2), ws.next()).await;
    assert!(matches!(trailing, Ok(None)));

    server.shutdown().await.expect("shutdown");
}
