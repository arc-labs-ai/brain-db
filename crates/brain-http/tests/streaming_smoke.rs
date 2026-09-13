//! Streaming smoke: server emits 5 chunks at 10 ms intervals;
//! verify each chunk arrives within 50 ms of its emit time.
//!
//! The bug-pattern this guards against is buffering — a framework
//! that holds chunks until its internal buffer fills would have
//! them all arrive together at the end.

mod common;

use std::io::Read;
use std::net::TcpStream;
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::TestServer;
use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use tokio::sync::mpsc;

use brain_http::body::{stream, ResponseBody};
use brain_http::router::Router;

async fn streaming_handler(_req: Request<Incoming>) -> brain_http::Result<Response<ResponseBody>> {
    // Producer task: emit 5 chunks at 10 ms intervals.
    let (tx, rx) = mpsc::channel::<Result<Bytes, brain_http::Error>>(8);
    tokio::spawn(async move {
        for i in 1..=5_u8 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            // Each chunk is exactly "chunk-N\n" for easy parsing.
            let chunk = Bytes::from(format!("chunk-{i}\n"));
            if tx.send(Ok(chunk)).await.is_err() {
                return; // consumer disconnected
            }
        }
    });
    let s = tokio_stream::wrappers::ReceiverStream::new(rx);
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/plain; charset=utf-8")
        .body(stream(s))
        .expect("static response always builds"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn five_chunks_each_within_50ms() {
    let router = Router::new().get("/stream", streaming_handler);
    let server = TestServer::start(router).await;
    let addr = server.addr();

    // Blocking reader: parse chunked transfer-encoding by hand.
    let observation = tokio::task::spawn_blocking(move || {
        let mut s = TcpStream::connect(addr).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        std::io::Write::write_all(
            &mut s,
            b"GET /stream HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .expect("write");

        // Find end of headers.
        let mut raw = Vec::with_capacity(4096);
        let mut byte = [0u8; 1];
        loop {
            let n = s.read(&mut byte).expect("read header byte");
            if n == 0 {
                break;
            }
            raw.push(byte[0]);
            if raw.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let head = String::from_utf8_lossy(&raw);
        assert!(
            head.contains("Transfer-Encoding: chunked")
                || head.contains("transfer-encoding: chunked"),
            "expected chunked encoding, got headers: {head}"
        );

        // Body: parse chunked transfer. Each chunk is
        // `<hex-size>\r\n<data>\r\n`; terminated by a `0\r\n\r\n`.
        let mut chunks: Vec<(Bytes, Instant)> = Vec::new();
        let started = Instant::now();
        loop {
            // Read chunk-size line.
            let mut size_line = Vec::new();
            loop {
                s.read_exact(&mut byte).expect("read size byte");
                if byte[0] == b'\n' && size_line.last() == Some(&b'\r') {
                    size_line.pop(); // drop the \r
                    break;
                }
                size_line.push(byte[0]);
            }
            let hex = std::str::from_utf8(&size_line)
                .expect("utf8 chunk size")
                .trim();
            let size = usize::from_str_radix(hex, 16).expect("hex size");
            if size == 0 {
                // Final chunk. Drain trailing CRLF.
                let mut tail = [0u8; 2];
                let _ = s.read(&mut tail);
                break;
            }
            let mut data = vec![0u8; size];
            s.read_exact(&mut data).expect("read chunk body");
            let now = Instant::now();
            chunks.push((Bytes::from(data), now));
            // Trailing CRLF after chunk body.
            let mut crlf = [0u8; 2];
            s.read_exact(&mut crlf).expect("read chunk trailer crlf");
        }
        (chunks, started)
    })
    .await
    .expect("client task");

    let (chunks, started) = observation;
    assert_eq!(chunks.len(), 5, "expected 5 chunks, got {}", chunks.len());

    // Each chunk should arrive at roughly (10 * i) ms after the
    // start of the response. Allow 50 ms slack for scheduling.
    for (i, (data, ts)) in chunks.iter().enumerate() {
        let expected = format!("chunk-{}\n", i + 1);
        assert_eq!(
            data.as_ref(),
            expected.as_bytes(),
            "chunk {} payload mismatch",
            i + 1
        );

        let elapsed_since_start = ts.duration_since(started);
        let expected_emit = Duration::from_millis(10 * (i as u64 + 1));
        let observed_lag = elapsed_since_start.saturating_sub(expected_emit);
        assert!(
            observed_lag < Duration::from_millis(150),
            "chunk {} arrived {observed_lag:?} after its emit time (limit 150 ms; \
            elapsed since start={elapsed_since_start:?}, expected={expected_emit:?}); \
            framework is probably buffering",
            i + 1
        );
    }

    server.shutdown().await.expect("shutdown");
}
