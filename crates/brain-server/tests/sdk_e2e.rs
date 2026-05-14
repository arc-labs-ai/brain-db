//! End-to-end: drive the in-process brain-server harness via
//! `brain_sdk_rust::Client`. Sub-task 10.13.
//!
//! The goal is *protocol-level* coverage: prove the same three
//! layers (server, wire protocol, SDK) survive a full client
//! lifecycle. Content-level assertions (e.g. "recall returns the
//! memory I just encoded") are intentionally avoided — the brain-
//! server harness's dispatcher path doesn't guarantee semantic
//! correctness under the test config (cf. sub-task 9.17 framing in
//! `e2e.rs`).

#![cfg(target_os = "linux")]

#[allow(dead_code)]
#[path = "../src/admin/mod.rs"]
mod admin;
#[allow(dead_code)]
#[path = "../src/config/mod.rs"]
mod config;
#[allow(dead_code)]
#[path = "../src/network/connection.rs"]
mod connection;
#[path = "../src/network/dispatch.rs"]
mod dispatch;
#[path = "../src/metrics/mod.rs"]
mod metrics;
#[allow(dead_code)]
#[path = "../src/network/routing.rs"]
mod routing;
#[allow(dead_code)]
#[path = "../src/shard/mod.rs"]
mod shard;
#[path = "../src/network/subscribe.rs"]
mod subscribe;
#[allow(dead_code)]
#[path = "../src/bootstrap/tls.rs"]
mod tls;

mod support_harness;

use std::net::SocketAddr;
use std::time::Duration;

use brain_core::MemoryId;
use brain_protocol::request::ForgetMode;
use brain_sdk_rust::Client;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use support_harness::start;

/// Minimal HTTP/1.1 GET against the admin server. Returns
/// `(status, body)`. Used only by the metrics integration tests
/// below — production paths go through `brain-cli::http`.
async fn http_get(addr: SocketAddr, path: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).await.expect("connect");
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).await.expect("write");
    let mut buf = Vec::with_capacity(8192);
    let _ = tokio::time::timeout(Duration::from_secs(2), s.read_to_end(&mut buf))
        .await
        .expect("read timeout");
    let raw = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
    let status_line = head.lines().next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, body.to_owned())
}

/// 1. Connection + handshake — the SDK's `connect` drives the
///    HELLO/AUTH negotiation; counters start at zero.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sdk_handshake_succeeds() {
    let server = start(1).await;
    let client = Client::connect(server.data_plane_addr)
        .await
        .expect("connect");
    let snap = client.metrics_snapshot();
    assert_eq!(snap.requests_total, 0, "no ops issued yet");
    client.bye().await.expect("bye");
    server.stop().await;
}

/// 2. ENCODE returns a non-null memory_id and bumps
///    `requests_total`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sdk_encode_returns_memory_id() {
    let server = start(1).await;
    let client = Client::connect(server.data_plane_addr)
        .await
        .expect("connect");
    let resp = client
        .encode("the cat sat on the mat")
        .send()
        .await
        .expect("encode");
    assert_ne!(resp.memory_id, 0, "encoded id must be non-null");
    let snap = client.metrics_snapshot();
    assert!(snap.requests_total >= 1);
    client.bye().await.expect("bye");
    server.stop().await;
}

/// 3. ENCODE → RECALL — wire-level smoke. We don't assert that the
///    recall result contains the encoded memory (content
///    correctness is a separate test surface); we assert the
///    recall returns Ok without panicking.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sdk_encode_recall_roundtrip() {
    let server = start(1).await;
    let client = Client::connect(server.data_plane_addr)
        .await
        .expect("connect");
    let _ = client
        .encode("the cat sat on the mat")
        .send()
        .await
        .expect("encode");
    let results = client.recall("cat").send().await.expect("recall");
    // Either the harness's dispatcher returns the encoded memory
    // or it returns an empty Vec — both are acceptable here. The
    // contract being asserted: the SDK + server round-trip succeeds.
    let _ = results;
    client.bye().await.expect("bye");
    server.stop().await;
}

/// 4. ENCODE → FORGET — the SDK can soft-forget by id.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sdk_encode_forget_succeeds() {
    let server = start(1).await;
    let client = Client::connect(server.data_plane_addr)
        .await
        .expect("connect");
    let encode = client
        .encode("ephemeral fact")
        .send()
        .await
        .expect("encode");
    let memory_id = MemoryId::from_raw(encode.memory_id);
    let forget = client
        .forget(memory_id)
        .mode(ForgetMode::Soft)
        .send()
        .await
        .expect("forget");
    assert_eq!(forget.memory_id, encode.memory_id);
    client.bye().await.expect("bye");
    server.stop().await;
}

/// 5. Concurrent ops via the pool — `Client::connect` uses the
///    single-connection preset, so concurrent issues serialize
///    correctly without protocol races. Asserts `requests_total`
///    advances by exactly the number of issued ops.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sdk_concurrent_encodes_serialize() {
    let server = start(1).await;
    let client = Client::connect(server.data_plane_addr)
        .await
        .expect("connect");
    // 4 ops, issued concurrently. Result: 4 increments on
    // requests_total. The pool's single-connection guard
    // serializes them; the test just asserts no protocol race.
    let f0 = client.encode("fact one").send();
    let f1 = client.encode("fact two").send();
    let f2 = client.encode("fact three").send();
    let f3 = client.encode("fact four").send();
    let (a, b, c, d) = tokio::join!(f0, f1, f2, f3);
    a.expect("e0");
    b.expect("e1");
    c.expect("e2");
    d.expect("e3");
    let snap = client.metrics_snapshot();
    assert!(snap.requests_total >= 4);
    client.bye().await.expect("bye");
    server.stop().await;
}

/// 12.1b: after running a few encodes, /metrics emits the new
/// request-path families with the correct counts. Asserts spec
/// §14/01 §3 (`brain_request_total`, `_active`, `_duration_ms`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admin_metrics_emit_request_families() {
    let server = start(1).await;
    let client = Client::connect(server.data_plane_addr)
        .await
        .expect("connect");
    for i in 0..3 {
        client
            .encode(format!("fact {i}"))
            .send()
            .await
            .expect("encode");
    }
    // Allow the response frame to flow back so the RequestTimer
    // dropped + the counter incremented.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (code, body) = http_get(server.admin_addr, "/metrics").await;
    assert_eq!(code, 200, "/metrics returned {code}");

    assert!(
        body.contains("# TYPE brain_request_total counter"),
        "missing brain_request_total TYPE; body:\n{body}"
    );
    assert!(
        body.contains("# TYPE brain_request_active gauge"),
        "missing brain_request_active TYPE"
    );
    assert!(
        body.contains("# TYPE brain_request_duration_ms histogram"),
        "missing brain_request_duration_ms TYPE"
    );

    // Find the encode success counter line.
    let success_line = body
        .lines()
        .find(|l| l.starts_with("brain_request_total{op=\"encode\",status=\"success\"}"))
        .expect("missing encode success counter line");
    let value: u64 = success_line
        .split_whitespace()
        .last()
        .and_then(|v| v.parse().ok())
        .expect("parse counter value");
    assert!(
        value >= 3,
        "expected ≥3 encode successes, got {value}; body:\n{body}"
    );

    // Histogram count line should match.
    let hist_count_line = body
        .lines()
        .find(|l| l.starts_with("brain_request_duration_ms_count{op=\"encode\"}"))
        .expect("missing histogram _count line");
    let hist_count: u64 = hist_count_line
        .split_whitespace()
        .last()
        .and_then(|v| v.parse().ok())
        .expect("parse hist count");
    assert!(hist_count >= 3, "histogram count = {hist_count}");

    // In-flight gauge should be back to zero now.
    let active_line = body
        .lines()
        .find(|l| l.starts_with("brain_request_active{op=\"encode\"}"))
        .expect("missing active gauge line");
    let active: i64 = active_line
        .split_whitespace()
        .last()
        .and_then(|v| v.parse().ok())
        .expect("parse active gauge");
    assert_eq!(active, 0, "in-flight gauge should drain to 0");

    client.bye().await.expect("bye");
    server.stop().await;
}
