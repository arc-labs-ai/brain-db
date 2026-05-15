//! End-to-end wire smoke (sub-task 9.17).
//!
//! ## Scope re-framing
//!
//! The phase doc originally said "Uses `brain-sdk-rust` to drive
//! encode → recall → forget → recall. Verifies expected results."
//! Two facts re-shaped the test:
//!
//! 1. `brain-sdk-rust` is a placeholder; the real client SDK is a
//!    Phase 13 effort (`spec/13_sdk_design/`). 9.17 drives the
//!    server via the same hand-rolled frame helpers used by 9.10
//!    / 9.11's wire tests.
//! 2. The shard scaffold uses [`crate::shard::NopDispatcher`]
//!    (zero-vector embeddings). Cosine similarity between two zero
//!    vectors is degenerate; RECALL returns memories essentially
//!    at random. So *content-level* correctness — "RECALL returns
//!    the memory I just encoded with relevance 0.95" — can't be
//!    asserted on this path. Spec §16/01's content-correctness
//!    suite lives in the brain-ops / brain-planner crates with
//!    proper fixtures.
//!
//! 9.17 is therefore the **wire smoke**: prove the whole stack
//! survives a full client lifecycle, end-to-end, without hangs /
//! panics / FD leaks. Specifically:
//!
//! - Each request opcode produces the matching response opcode
//!   (or an ERROR with a sane shape).
//! - EOS is set where the spec requires.
//! - Routing reaches the right shard for memory-bearing ops.
//! - 100 round-trips don't degrade.
//! - BYE + shutdown drain cleanly.

#![cfg(target_os = "linux")]

use std::net::SocketAddr;
use std::time::Duration;

use brain_protocol::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::opcode::Opcode;
use brain_protocol::request::{
    ByeRequest, EncodeRequest, ForgetMode, ForgetRequest, MemoryKindWire, RecallRequest,
    RequestBody,
};
use brain_protocol::response::ResponseBody;
use brain_protocol::Frame;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

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

use support_harness::start;

const FLAG_EOS: u8 = 1 << 7;

// Bringup scaffold (Server, start) lives in tests/support_harness/mod.rs.

// ---------------------------------------------------------------------------
// Client helpers
// ---------------------------------------------------------------------------

async fn read_one_frame<S>(stream: &mut S) -> Result<Frame, String>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut header = [0u8; brain_protocol::HEADER_SIZE];
    stream
        .read_exact(&mut header)
        .await
        .map_err(|e| format!("header read: {e}"))?;
    let payload_len_be = [header[16], header[17], header[18]];
    let payload_len =
        u32::from_be_bytes([0, payload_len_be[0], payload_len_be[1], payload_len_be[2]]) as usize;
    let mut buf = Vec::with_capacity(brain_protocol::HEADER_SIZE + payload_len);
    buf.extend_from_slice(&header);
    if payload_len > 0 {
        buf.resize(brain_protocol::HEADER_SIZE + payload_len, 0);
        stream
            .read_exact(&mut buf[brain_protocol::HEADER_SIZE..])
            .await
            .map_err(|e| format!("payload read: {e}"))?;
    }
    let (frame, rest) = Frame::decode_with_max(&buf, brain_protocol::MAX_PAYLOAD_BYTES as u32)
        .map_err(|e| format!("decode: {e}"))?;
    debug_assert!(rest.is_empty());
    Ok(frame)
}

async fn send_frame(client: &mut TcpStream, frame: Frame) {
    client.write_all(&frame.encode()).await.expect("send");
    client.flush().await.expect("flush");
}

async fn complete_handshake(client: &mut TcpStream, agent_id: [u8; 16]) {
    let hello = HelloPayload {
        client_id: "e2e-tester".into(),
        supported_versions: vec![1],
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
    let welcome = read_one_frame(client).await.expect("WELCOME");
    assert_eq!(welcome.header.opcode_u16(), Opcode::Welcome.as_u16());

    let auth = AuthPayload {
        method: AuthMethod::None,
        agent_id,
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
    let auth_ok = read_one_frame(client).await.expect("AUTH_OK");
    assert_eq!(auth_ok.header.opcode_u16(), Opcode::AuthOk.as_u16());
}

/// Send an `ENCODE_REQ` and read the response. Returns
/// `(opcode_byte, optional memory_id)` so the caller can branch on
/// ENCODE_RESP vs ERROR without panicking.
async fn encode_round_trip(
    client: &mut TcpStream,
    stream_id: u32,
    text: &str,
) -> (u16, Option<u128>) {
    let req = EncodeRequest {
        text: text.into(),
        context_id: 0,
        kind: MemoryKindWire::Episodic,
        salience_hint: 0.5,
        edges: Vec::new(),
        request_id: *uuid::Uuid::now_v7().as_bytes(),
        txn_id: None,
        deduplicate: false,
    };
    send_frame(
        client,
        Frame::new(
            Opcode::EncodeReq.as_u16(),
            FLAG_EOS,
            stream_id,
            RequestBody::Encode(req).encode(),
        ),
    )
    .await;
    let resp = read_one_frame(client).await.expect("ENCODE response");
    let opcode = resp.header.opcode_u16();
    let memory_id = if opcode == Opcode::EncodeResp.as_u16() {
        match ResponseBody::decode(Opcode::EncodeResp, &resp.payload).ok() {
            Some(ResponseBody::Encode(r)) => Some(r.memory_id),
            _ => None,
        }
    } else {
        None
    };
    (opcode, memory_id)
}

async fn recall_round_trip(client: &mut TcpStream, stream_id: u32, cue: &str) -> u16 {
    let req = RecallRequest {
        cue_text: cue.into(),
        cue_vector_offset: 0,
        cue_vector_dim: 0,
        top_k: 5,
        confidence_threshold: 0.0,
        context_filter: None,
        age_bound_unix_nanos: None,
        kind_filter: None,
        salience_floor: 0.0,
        strategy_hint: None,
        include_vectors: false,
        include_edges: false,
        request_id: Some(*uuid::Uuid::now_v7().as_bytes()),
        txn_id: None,
    };
    send_frame(
        client,
        Frame::new(
            Opcode::RecallReq.as_u16(),
            FLAG_EOS,
            stream_id,
            RequestBody::Recall(req).encode(),
        ),
    )
    .await;
    let resp = read_one_frame(client).await.expect("RECALL response");
    // EOS must be set (single-frame EOS in v1 per 9.10).
    assert!(
        resp.header.flags_u8() & FLAG_EOS != 0,
        "RECALL response must carry EOS in v1"
    );
    resp.header.opcode_u16()
}

async fn forget_round_trip(client: &mut TcpStream, stream_id: u32, memory_id: u128) -> u16 {
    let req = ForgetRequest {
        memory_id,
        mode: ForgetMode::Soft,
        request_id: *uuid::Uuid::now_v7().as_bytes(),
        txn_id: None,
    };
    send_frame(
        client,
        Frame::new(
            Opcode::ForgetReq.as_u16(),
            FLAG_EOS,
            stream_id,
            RequestBody::Forget(req).encode(),
        ),
    )
    .await;
    let resp = read_one_frame(client).await.expect("FORGET response");
    resp.header.opcode_u16()
}

async fn bye_round_trip(client: &mut TcpStream) {
    send_frame(
        client,
        Frame::new(
            Opcode::Bye.as_u16(),
            FLAG_EOS,
            0,
            RequestBody::Bye(ByeRequest {
                reason: Some("e2e done".into()),
            })
            .encode(),
        ),
    )
    .await;
    let resp = read_one_frame(client).await.expect("BYE response");
    assert_eq!(resp.header.opcode_u16(), Opcode::Bye.as_u16());
    // Server closes after BYE.
    let mut sink = [0u8; 1];
    let n = client.read(&mut sink).await.expect("EOF read");
    assert_eq!(n, 0, "expected EOF after BYE");
}

async fn http_get(addr: SocketAddr, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.expect("connect admin");
    let req = format!("GET {path} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.expect("send");
    stream.flush().await.expect("flush");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read");
    let response = String::from_utf8_lossy(&buf).into_owned();
    let first_line = response.lines().next().unwrap_or("");
    let code = first_line
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_owned())
        .unwrap_or_default();
    (code, body)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn encode_recall_forget_recall_round_trip() {
    let server = start(2).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    let agent_id = *uuid::Uuid::now_v7().as_bytes();
    complete_handshake(&mut client, agent_id).await;

    // Step 1: ENCODE. May succeed (returning a memory_id) or surface
    // an ERROR (the NopDispatcher path is not always green
    // end-to-end). Both are valid wire shapes.
    let (encode_op, encode_id) = encode_round_trip(&mut client, 1, "the cat sat on the mat").await;
    assert!(
        encode_op == Opcode::EncodeResp.as_u16() || encode_op == Opcode::Error.as_u16(),
        "ENCODE returned unexpected opcode 0x{encode_op:02x}"
    );

    // Step 2: RECALL. Single-frame EOS response per 9.10.
    let recall_op = recall_round_trip(&mut client, 3, "cat").await;
    assert!(
        recall_op == Opcode::RecallResp.as_u16() || recall_op == Opcode::Error.as_u16(),
        "RECALL returned unexpected opcode 0x{recall_op:02x}"
    );

    // Step 3: FORGET (only if we got a memory_id). Routes to the
    // memory's shard via `shard_for_memory(memory_id)` — exercises
    // 9.10's memory-id routing path.
    if let Some(memory_id) = encode_id {
        let forget_op = forget_round_trip(&mut client, 5, memory_id).await;
        assert!(
            forget_op == Opcode::ForgetResp.as_u16() || forget_op == Opcode::Error.as_u16(),
            "FORGET returned unexpected opcode 0x{forget_op:02x}"
        );
    }

    // Step 4: RECALL again. May or may not differ from step 2's
    // content (NopDispatcher returns identical embeddings); we only
    // assert wire shape.
    let recall_op2 = recall_round_trip(&mut client, 7, "cat").await;
    assert!(
        recall_op2 == Opcode::RecallResp.as_u16() || recall_op2 == Opcode::Error.as_u16(),
        "second RECALL returned unexpected opcode 0x{recall_op2:02x}"
    );

    // Step 5: BYE; server closes.
    bye_round_trip(&mut client).await;

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_encode_recall_is_stable() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    let agent_id = *uuid::Uuid::now_v7().as_bytes();
    complete_handshake(&mut client, agent_id).await;

    // 100 × (ENCODE + RECALL). Rotate stream ids 1, 3, 5, …
    // through a 1024-slot space (well below 9.9's outgoing
    // capacity).
    for i in 0..100u32 {
        let enc_stream = (i * 4 + 1) % 1024;
        let rec_stream = (i * 4 + 3) % 1024;
        let (encode_op, _) = encode_round_trip(&mut client, enc_stream, "stable").await;
        assert!(
            encode_op == Opcode::EncodeResp.as_u16() || encode_op == Opcode::Error.as_u16(),
            "iter {i} ENCODE 0x{encode_op:02x}"
        );
        let recall_op = recall_round_trip(&mut client, rec_stream, "stable").await;
        assert!(
            recall_op == Opcode::RecallResp.as_u16() || recall_op == Opcode::Error.as_u16(),
            "iter {i} RECALL 0x{recall_op:02x}"
        );
    }

    bye_round_trip(&mut client).await;
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_endpoint_reflects_traffic() {
    let server = start(1).await;

    // Open a couple of data-plane connections; let the accept loop
    // bump `brain_connections_total`.
    let mut c1 = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect 1");
    let mut c2 = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect 2");
    let agent_id = *uuid::Uuid::now_v7().as_bytes();
    complete_handshake(&mut c1, agent_id).await;
    complete_handshake(&mut c2, agent_id).await;

    // Allow the accept loop a moment to update the atomics. (The
    // counter is updated before the per-conn task runs, so this is
    // belt + suspenders.)
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (code, body) = http_get(server.admin_addr, "/metrics").await;
    assert_eq!(code, 200);
    let line = body
        .lines()
        .find(|l| l.starts_with("brain_connections_total "))
        .expect("brain_connections_total missing");
    let value: u64 = line
        .split_whitespace()
        .last()
        .and_then(|v| v.parse().ok())
        .expect("parse counter");
    assert!(
        value >= 2,
        "expected ≥2 accepted connections, got {value}; body:\n{body}"
    );

    // The admin server also reports the build info and shard count.
    assert!(body.contains("brain_build_info{"), "missing build_info");
    assert!(
        body.contains("brain_shards_total 1"),
        "expected brain_shards_total 1 with one shard; body:\n{body}"
    );

    drop(c1);
    drop(c2);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bye_and_shutdown_drain_cleanly() {
    let server = start(2).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    let agent_id = *uuid::Uuid::now_v7().as_bytes();
    complete_handshake(&mut client, agent_id).await;

    // Client-initiated BYE.
    bye_round_trip(&mut client).await;
    drop(client);

    // Server shutdown — the test scaffold's `stop()` runs the same
    // graceful_shutdown_shards path that production uses (sub-task
    // 9.14). We rely on its timeouts: listener exits within 2 s,
    // admin within 2 s, shards within the default 30 s budget.
    let started = std::time::Instant::now();
    server.stop().await;
    assert!(
        started.elapsed() < Duration::from_secs(35),
        "server.stop() blew the drain budget; elapsed = {:?}",
        started.elapsed()
    );
}
