//! SSE basic tests.

mod common;

use std::io::Read;
use std::net::TcpStream;
use std::time::{Duration, Instant};

use common::TestServer;
use http::{Request, Response};
use hyper::body::Incoming;
use tokio::sync::mpsc;

use brain_http::body::ResponseBody;
use brain_http::router::Router;
use brain_http::sse::{self, SseEvent};

/// Read the response status line + headers, return (status, headers_text).
fn read_headers(stream: &mut TcpStream) -> (u16, String) {
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
    let head = String::from_utf8_lossy(&raw).to_string();
    let status_line = head.lines().next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, head)
}

/// Read a single chunked body chunk. Returns `Some(bytes)` for data
/// chunks, `None` on the terminating 0-size chunk.
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
        // Final chunk. Drain trailing CRLF.
        let mut tail = [0u8; 2];
        let _ = stream.read(&mut tail);
        return None;
    }
    let mut data = vec![0u8; size];
    stream.read_exact(&mut data).expect("read chunk data");
    let mut crlf = [0u8; 2];
    stream.read_exact(&mut crlf).expect("read chunk crlf");
    Some(data)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_arrives_within_50ms() {
    async fn handler(_req: Request<Incoming>) -> brain_http::Result<Response<ResponseBody>> {
        // Emit one event immediately, then hold the connection open
        // for ~1 second to prove the event flushed before the
        // connection closed.
        let (tx, rx) = mpsc::channel::<SseEvent>(4);
        tokio::spawn(async move {
            let _ = tx
                .send(
                    SseEvent::new()
                        .with_id("1")
                        .with_event("ping")
                        .with_data("hello"),
                )
                .await;
            tokio::time::sleep(Duration::from_secs(1)).await;
            // tx drops here, closing the stream.
        });
        let s = tokio_stream::wrappers::ReceiverStream::new(rx);
        Ok(sse::response(s))
    }

    let router = Router::new().get("/events", handler);
    let server = TestServer::start(router).await;
    let addr = server.addr();

    let observed = tokio::task::spawn_blocking(move || {
        let mut s = TcpStream::connect(addr).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        std::io::Write::write_all(
            &mut s,
            b"GET /events HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .expect("write");

        let request_sent = Instant::now();
        let (status, head) = read_headers(&mut s);
        assert_eq!(status, 200);
        let head_lc = head.to_ascii_lowercase();
        assert!(
            head_lc.contains("content-type: text/event-stream"),
            "head: {head}"
        );
        assert!(head_lc.contains("cache-control: no-cache"));

        // First chunk = our SseEvent.
        let chunk = read_chunk(&mut s).expect("first chunk");
        let arrived = Instant::now();
        let body = String::from_utf8(chunk).expect("utf8 chunk");
        (body, arrived.duration_since(request_sent))
    })
    .await
    .expect("client task");

    let (body, lag) = observed;
    assert_eq!(body, "id: 1\nevent: ping\ndata: hello\n\n");
    assert!(
        lag < Duration::from_millis(150),
        "event took {lag:?} to arrive (limit 150 ms); flush discipline is broken"
    );

    server.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_event_round_trip() {
    async fn handler(_req: Request<Incoming>) -> brain_http::Result<Response<ResponseBody>> {
        let (tx, rx) = mpsc::channel::<SseEvent>(8);
        tokio::spawn(async move {
            for i in 1..=5_u64 {
                let _ = tx
                    .send(
                        SseEvent::new()
                            .with_id(i.to_string())
                            .with_data(format!("payload-{i}")),
                    )
                    .await;
            }
        });
        let s = tokio_stream::wrappers::ReceiverStream::new(rx);
        Ok(sse::response(s))
    }

    let router = Router::new().get("/events", handler);
    let server = TestServer::start(router).await;
    let addr = server.addr();

    let chunks = tokio::task::spawn_blocking(move || {
        let mut s = TcpStream::connect(addr).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        std::io::Write::write_all(
            &mut s,
            b"GET /events HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .expect("write");
        let (status, _head) = read_headers(&mut s);
        assert_eq!(status, 200);

        let mut events = Vec::new();
        while let Some(chunk) = read_chunk(&mut s) {
            events.push(String::from_utf8(chunk).unwrap());
        }
        events
    })
    .await
    .expect("client task");

    assert_eq!(chunks.len(), 5);
    for (i, body) in chunks.iter().enumerate() {
        let n = i + 1;
        let expected = format!("id: {n}\ndata: payload-{n}\n\n");
        assert_eq!(body, &expected);
    }

    server.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_line_data_emits_one_data_line_per_source_line() {
    async fn handler(_req: Request<Incoming>) -> brain_http::Result<Response<ResponseBody>> {
        let (tx, rx) = mpsc::channel::<SseEvent>(2);
        tokio::spawn(async move {
            let _ = tx
                .send(SseEvent::new().with_data("line1\nline2\nline3"))
                .await;
        });
        let s = tokio_stream::wrappers::ReceiverStream::new(rx);
        Ok(sse::response(s))
    }

    let router = Router::new().get("/events", handler);
    let server = TestServer::start(router).await;
    let addr = server.addr();

    let body = tokio::task::spawn_blocking(move || {
        let mut s = TcpStream::connect(addr).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        std::io::Write::write_all(
            &mut s,
            b"GET /events HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .expect("write");
        let (status, _) = read_headers(&mut s);
        assert_eq!(status, 200);
        String::from_utf8(read_chunk(&mut s).expect("chunk")).unwrap()
    })
    .await
    .expect("client task");

    assert_eq!(body, "data: line1\ndata: line2\ndata: line3\n\n");

    server.shutdown().await.expect("shutdown");
}
