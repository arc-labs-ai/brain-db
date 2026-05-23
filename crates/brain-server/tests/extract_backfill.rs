//! `POST /v1/extract/backfill` — end-to-end integration.
//!
//! Spawns a 1-shard server, encodes a few memories through the wire
//! protocol, then drives the admin endpoint with each of the three
//! selectors (`?memory=<id>`, `?since=<ts>`, `?all`) and asserts the
//! returned `(enqueued, skipped, shards)` triple.
//!
//! The test does *not* assert anything about the downstream pipeline
//! state — that's `tests/extractor_pipeline.rs`'s job. The contract
//! verified here is the admin route + the per-shard handler's read
//! over `MEMORIES_TABLE` / `TEXTS_TABLE` + the `WriterHandle`
//! enqueue. It exists so a regression in the admin wiring fails the
//! suite cleanly without requiring the live LLM tier.

#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::TcpStream as StdTcpStream;
use std::time::Duration;

use brain_core::MemoryId;
use brain_protocol::connection::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::codec::opcode::Opcode;
use brain_protocol::envelope::request::{EncodeRequest, MemoryKindWire, RequestBody};
use brain_protocol::envelope::response::ResponseBody;
use brain_protocol::Frame;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use uuid::Uuid;

#[allow(dead_code)]
#[path = "../src/admin/mod.rs"]
mod admin;
#[allow(dead_code)]
#[path = "../src/network/auth.rs"]
mod auth;
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

use support_harness::start_in;

const FLAG_EOS: u8 = 1 << 7;

// ---------------------------------------------------------------------------
// Wire helpers (lifted from extractor_pipeline.rs — same pattern).
// ---------------------------------------------------------------------------

async fn read_one_frame<S>(stream: &mut S) -> Frame
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut header = [0u8; brain_protocol::HEADER_SIZE];
    stream.read_exact(&mut header).await.expect("header");
    let payload_len = u32::from_be_bytes([0, header[16], header[17], header[18]]) as usize;
    let mut buf = Vec::with_capacity(brain_protocol::HEADER_SIZE + payload_len);
    buf.extend_from_slice(&header);
    if payload_len > 0 {
        buf.resize(brain_protocol::HEADER_SIZE + payload_len, 0);
        stream
            .read_exact(&mut buf[brain_protocol::HEADER_SIZE..])
            .await
            .expect("payload");
    }
    let (frame, _) =
        Frame::decode_with_max(&buf, brain_protocol::MAX_PAYLOAD_BYTES as u32).expect("decode");
    frame
}

async fn send_frame(client: &mut TcpStream, frame: Frame) {
    client.write_all(&frame.encode()).await.expect("send");
    client.flush().await.expect("flush");
}

async fn handshake(client: &mut TcpStream) {
    let hello = HelloPayload {
        client_id: "extract-backfill-tester".into(),
        supported_versions: vec![brain_protocol::VERSION],
        capabilities: HelloCapabilities {
            streaming: true,
            compression_zstd: false,
            server_push: false,
        },
        client_session_token: None,
    };
    send_frame(
        client,
        Frame::new(
            Opcode::Hello.as_u16(),
            FLAG_EOS,
            0,
            RequestBody::Hello(hello).encode(),
        ),
    )
    .await;
    let welcome = read_one_frame(client).await;
    assert_eq!(welcome.header.opcode_u16(), Opcode::Welcome.as_u16());

    let auth = AuthPayload {
        method: AuthMethod::None,
        agent_id: *Uuid::now_v7().as_bytes(),
        credentials: AuthCredentials::None,
    };
    send_frame(
        client,
        Frame::new(
            Opcode::Auth.as_u16(),
            FLAG_EOS,
            0,
            RequestBody::Auth(auth).encode(),
        ),
    )
    .await;
    let auth_ok = read_one_frame(client).await;
    assert_eq!(auth_ok.header.opcode_u16(), Opcode::AuthOk.as_u16());
}

async fn round_trip(client: &mut TcpStream, stream_id: u32, req: RequestBody) -> ResponseBody {
    let opcode = req.opcode().as_u16();
    let payload = req.encode();
    send_frame(client, Frame::new(opcode, FLAG_EOS, stream_id, payload)).await;
    let resp = read_one_frame(client).await;
    let resp_opcode = resp.header.opcode_u16();
    ResponseBody::decode(
        Opcode::from_u16(resp_opcode).expect("known opcode"),
        &resp.payload,
    )
    .expect("decode resp")
}

async fn encode_one(client: &mut TcpStream, stream_id: u32, text: &str) -> MemoryId {
    let req = EncodeRequest {
        text: text.into(),
        context_id: 1,
        kind: MemoryKindWire::Episodic,
        salience_hint: 0.5,
        edges: vec![],
        request_id: *Uuid::now_v7().as_bytes(),
        txn_id: None,
        deduplicate: false,
    };
    let resp = round_trip(client, stream_id, RequestBody::Encode(req)).await;
    match resp {
        ResponseBody::Encode(ack) => MemoryId::from(ack.memory_id),
        other => panic!("expected Encode, got {other:?}"),
    }
}

/// Blocking POST that runs inside a `spawn_blocking`. Returns
/// `(status_code, body_string)`. The admin server uses hyper 1.x but
/// we want to keep the test's HTTP surface tiny (no axum / hyper-client
/// dep), matching the rest of the CLI commands.
fn http_post_no_body(admin_addr: &str, path: &str) -> (u16, String) {
    let mut stream = StdTcpStream::connect_timeout(
        &admin_addr.parse().expect("admin addr"),
        Duration::from_secs(5),
    )
    .expect("connect admin");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {admin_addr}\r\nContent-Length: 0\r\n\
         Connection: close\r\nAccept: */*\r\n\r\n",
    );
    stream.write_all(req.as_bytes()).unwrap();
    stream.flush().unwrap();
    let mut raw = Vec::with_capacity(1024);
    stream.read_to_end(&mut raw).unwrap();
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response delimiter");
    let head = std::str::from_utf8(&raw[..split]).unwrap();
    let status: u16 = head
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let body = String::from_utf8_lossy(&raw[split + 4..]).to_string();
    (status, body)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

/// Encode three memories, then POST `?all` and assert the report shape
/// + that the handler walked all three rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backfill_all_enqueues_every_memory() {
    let data_dir = TempDir::new().expect("tmp");
    let server = start_in(data_dir.path(), 1).await;

    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    handshake(&mut client).await;

    let _ = encode_one(&mut client, 1, "alpha memory one").await;
    let _ = encode_one(&mut client, 2, "beta memory two").await;
    let _ = encode_one(&mut client, 3, "gamma memory three").await;

    let admin_addr = server.admin_addr.to_string();
    let (status, body) = tokio::task::spawn_blocking(move || {
        http_post_no_body(&admin_addr, "/v1/extract/backfill?all")
    })
    .await
    .unwrap();
    assert_eq!(status, 200, "expected 200, got {status}: {body}");

    let report: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(report["shards"].as_u64(), Some(1), "body = {body}");
    // The sum must hit every active memory the handler walked. The
    // worker may have drained the channel before backfill ran, so
    // we don't pin an exact `enqueued` value — we assert at least
    // the three encodes landed somewhere (enqueued + skipped).
    let enqueued = report["enqueued"].as_u64().expect("enqueued u64");
    let skipped = report["skipped"].as_u64().expect("skipped u64");
    assert!(
        enqueued + skipped >= 3,
        "expected ≥3 rows considered; enqueued={enqueued} skipped={skipped}",
    );

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backfill_since_zero_matches_all_active() {
    let data_dir = TempDir::new().expect("tmp");
    let server = start_in(data_dir.path(), 1).await;

    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    handshake(&mut client).await;

    let _ = encode_one(&mut client, 1, "alpha").await;
    let _ = encode_one(&mut client, 2, "beta").await;

    let admin_addr = server.admin_addr.to_string();
    let (status, body) = tokio::task::spawn_blocking(move || {
        http_post_no_body(&admin_addr, "/v1/extract/backfill?since=0")
    })
    .await
    .unwrap();
    assert_eq!(status, 200, "body = {body}");

    let report: serde_json::Value = serde_json::from_str(&body).expect("json");
    let enqueued = report["enqueued"].as_u64().unwrap();
    let skipped = report["skipped"].as_u64().unwrap();
    assert!(
        enqueued + skipped >= 2,
        "since=0 must hit every active memory; enqueued={enqueued} skipped={skipped}",
    );

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backfill_memory_id_targets_one_row() {
    let data_dir = TempDir::new().expect("tmp");
    let server = start_in(data_dir.path(), 1).await;

    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    handshake(&mut client).await;

    let mem = encode_one(&mut client, 1, "single memory body").await;
    let mem_u128: u128 = u128::from_be_bytes(mem.to_be_bytes());

    let admin_addr = server.admin_addr.to_string();
    let (status, body) = tokio::task::spawn_blocking(move || {
        http_post_no_body(
            &admin_addr,
            &format!("/v1/extract/backfill?memory={mem_u128}"),
        )
    })
    .await
    .unwrap();
    assert_eq!(status, 200, "body = {body}");

    let report: serde_json::Value = serde_json::from_str(&body).expect("json");
    let enqueued = report["enqueued"].as_u64().unwrap();
    let skipped = report["skipped"].as_u64().unwrap();
    assert_eq!(
        enqueued + skipped,
        1,
        "memory selector must touch exactly one row; enqueued={enqueued} skipped={skipped}",
    );

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backfill_rejects_empty_selector_400() {
    let data_dir = TempDir::new().expect("tmp");
    let server = start_in(data_dir.path(), 1).await;

    let admin_addr = server.admin_addr.to_string();
    let (status, body) =
        tokio::task::spawn_blocking(move || http_post_no_body(&admin_addr, "/v1/extract/backfill"))
            .await
            .unwrap();
    assert_eq!(status, 400, "body = {body}");
    assert!(body.contains("missing selector"), "body = {body}");

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backfill_rejects_conflicting_selectors_400() {
    let data_dir = TempDir::new().expect("tmp");
    let server = start_in(data_dir.path(), 1).await;

    let admin_addr = server.admin_addr.to_string();
    let (status, body) = tokio::task::spawn_blocking(move || {
        http_post_no_body(&admin_addr, "/v1/extract/backfill?memory=1&all")
    })
    .await
    .unwrap();
    assert_eq!(status, 400, "body = {body}");
    assert!(body.contains("conflicting"), "body = {body}");

    server.stop().await;
}
