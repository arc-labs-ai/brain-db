//! Reconnect test: `Last-Event-ID` carries through.
//!
//! Two sequential TCP connections from this test:
//!
//! 1. First: GET with no `Last-Event-ID`. The handler emits events
//!    1..3 then closes.
//! 2. Second: GET with `Last-Event-ID: 3`. The handler reads the
//!    header, parses 3, emits events 4..6.
//!
//! This proves the server surfaces the header (it's just standard
//! `http::HeaderMap` — no brain-http special-case) and a handler can
//! react.

mod common;

use std::io::Read;
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::TestServer;
use http::Request;
use hyper::body::Incoming;
use tokio::sync::mpsc;

use brain_http::router::Router;
use brain_http::sse::{self, SseEvent};

fn read_headers(stream: &mut TcpStream) -> String {
    let mut raw = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).expect("read header byte");
        if n == 0 {
            break;
        }
        raw.push(byte[0]);
        if raw.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8_lossy(&raw).to_string()
}

fn read_chunk(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut size_line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte).expect("read size byte");
        if byte[0] == b'\n' && size_line.last() == Some(&b'\r') {
            size_line.pop();
            break;
        }
        size_line.push(byte[0]);
    }
    let hex = std::str::from_utf8(&size_line).unwrap().trim();
    let size = usize::from_str_radix(hex, 16).expect("hex size");
    if size == 0 {
        let mut tail = [0u8; 2];
        let _ = stream.read(&mut tail);
        return None;
    }
    let mut data = vec![0u8; size];
    stream.read_exact(&mut data).expect("read chunk");
    let mut crlf = [0u8; 2];
    stream.read_exact(&mut crlf).expect("read crlf");
    Some(data)
}

fn fire_and_collect(addr: std::net::SocketAddr, last_event_id: Option<u64>) -> Vec<String> {
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

    let mut req = String::from("GET /events HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    if let Some(id) = last_event_id {
        req.push_str(&format!("Last-Event-ID: {id}\r\n"));
    }
    req.push_str("\r\n");
    std::io::Write::write_all(&mut s, req.as_bytes()).expect("write");

    let head = read_headers(&mut s);
    assert!(
        head.to_ascii_lowercase()
            .contains("content-type: text/event-stream"),
        "non-SSE response: {head}"
    );

    let mut events = Vec::new();
    while let Some(chunk) = read_chunk(&mut s) {
        events.push(String::from_utf8(chunk).expect("utf8"));
    }
    events
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn last_event_id_carries_through() {
    // Counter shared across handler invocations — proves the second
    // connection's resume point comes from the request header, not
    // global state. After connection 1, counter = 3. Connection 2
    // sends `Last-Event-ID: 3`, handler reads it, emits 4..6.
    let counter = Arc::new(AtomicU64::new(0));
    let counter_handler = counter.clone();

    let handler = move |req: Request<Incoming>| {
        let counter = counter_handler.clone();
        async move {
            let resume_from: u64 = req
                .headers()
                .get("last-event-id")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);

            let (tx, rx) = mpsc::channel::<SseEvent>(8);
            tokio::spawn(async move {
                for i in (resume_from + 1)..=(resume_from + 3) {
                    counter.store(i, Ordering::SeqCst);
                    let _ = tx
                        .send(
                            SseEvent::new()
                                .with_id(i.to_string())
                                .with_data(format!("payload-{i}")),
                        )
                        .await;
                }
                // tx drops here -> body stream completes -> hyper
                // closes the response.
            });
            let s = tokio_stream::wrappers::ReceiverStream::new(rx);
            Ok::<_, brain_http::Error>(sse::response(s))
        }
    };

    let router = Router::new().route(http::Method::GET, "/events", handler);
    let server = TestServer::start(router).await;
    let addr = server.addr();

    // First connection: no Last-Event-ID. Should see ids 1, 2, 3.
    let first = tokio::task::spawn_blocking(move || fire_and_collect(addr, None))
        .await
        .expect("first connection task");
    assert_eq!(first.len(), 3, "first batch should have 3 events");
    assert!(first[0].contains("id: 1\n"));
    assert!(first[1].contains("id: 2\n"));
    assert!(first[2].contains("id: 3\n"));

    // Second connection: Last-Event-ID: 3. Should see ids 4, 5, 6.
    let second = tokio::task::spawn_blocking(move || fire_and_collect(addr, Some(3)))
        .await
        .expect("second connection task");
    assert_eq!(second.len(), 3, "second batch should have 3 events");
    assert!(
        second[0].contains("id: 4\n"),
        "resume from 3 should yield id 4 first; got: {:?}",
        second[0]
    );
    assert!(second[1].contains("id: 5\n"));
    assert!(second[2].contains("id: 6\n"));

    assert_eq!(counter.load(Ordering::SeqCst), 6);

    server.shutdown().await.expect("shutdown");
}
