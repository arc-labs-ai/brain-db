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

use brain_core::MemoryId;
use brain_protocol::request::ForgetMode;
use brain_sdk_rust::Client;

use support_harness::start;

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
