//! Concurrency tests for hybrid-default RECALL.
//!
//! Two scenarios pinned here:
//!
//! - **C1** — 50 parallel tokio tasks against a shared server, each
//!   task picks one of `Auto` (hybrid), `SubstrateOnly`, or
//!   `HybridOnly`. The strategy chosen by each task must be honoured
//!   in its own response: hybrid metadata appears iff the task chose
//!   a hybrid path, and never leaks across tasks. This rules out any
//!   per-shard mutable state aliasing the routing decision across
//!   concurrent calls.
//!
//! - **E4** — single client issues 10 sequential recalls alternating
//!   between `SubstrateOnly` and `Auto`. Each response respects its
//!   own strategy; no carry-over from the prior request.

#![cfg(target_os = "linux")]

use std::sync::Arc;

use brain_protocol::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::opcode::Opcode;
use brain_protocol::request::{EncodeRequest, MemoryKindWire, RecallRequest, RecallStrategy};
use brain_protocol::response::{RecallResponseFrame, ResponseBody};
use brain_protocol::Frame;
use brain_protocol::RequestBody;
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

// ---------------------------------------------------------------------------
// Wire helpers — copied minimally from `recall_hybrid_routing.rs` so each
// integration-test binary is self-contained.
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
    let (frame, rest) =
        Frame::decode_with_max(&buf, brain_protocol::MAX_PAYLOAD_BYTES as u32).expect("decode");
    debug_assert!(rest.is_empty());
    frame
}

async fn send_frame(client: &mut TcpStream, frame: Frame) {
    client.write_all(&frame.encode()).await.expect("send");
    client.flush().await.expect("flush");
}

async fn complete_handshake(client: &mut TcpStream, client_id: &str) {
    let hello = HelloPayload {
        client_id: client_id.into(),
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
    let welcome = read_one_frame(client).await;
    assert_eq!(welcome.header.opcode_u16(), Opcode::Welcome.as_u16());

    let auth = AuthPayload {
        method: AuthMethod::None,
        agent_id: *uuid::Uuid::now_v7().as_bytes(),
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

async fn round_trip(
    client: &mut TcpStream,
    stream_id: u32,
    req: RequestBody,
) -> (u16, ResponseBody) {
    let opcode = req.opcode().as_u16();
    let payload = req.encode();
    send_frame(client, Frame::new(opcode, FLAG_EOS, stream_id, payload)).await;
    let resp = read_one_frame(client).await;
    let resp_opcode = resp.header.opcode_u16();
    let body = ResponseBody::decode(
        Opcode::from_u16(resp_opcode).expect("known opcode"),
        &resp.payload,
    )
    .expect("decode resp");
    (resp_opcode, body)
}

fn recall_request(cue: &str, strategy: Option<RecallStrategy>) -> RecallRequest {
    RecallRequest {
        cue_text: cue.into(),
        cue_vector_offset: 0,
        cue_vector_dim: 0,
        top_k: 5,
        confidence_threshold: 0.0,
        context_filter: None,
        age_bound_unix_nanos: None,
        kind_filter: None,
        salience_floor: 0.0,
        strategy,
        include_vectors: false,
        include_edges: false,
        include_text: false,
        request_id: Some(*uuid::Uuid::now_v7().as_bytes()),
        txn_id: None,
    }
}

async fn encode_text(client: &mut TcpStream, stream_id: u32, text: &str) {
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
    let (opcode, body) = round_trip(client, stream_id, RequestBody::Encode(req)).await;
    if opcode != Opcode::EncodeResp.as_u16() {
        panic!("encode failed: opcode={opcode} body={body:?}");
    }
}

async fn seed_fixture(client: &mut TcpStream) {
    let phrases = [
        "Priya prefers async meetings over standups",
        "Async-first communication reduces context-switching",
        "Standups are a sync ritual we should retire",
        "Document driven design helps async teams",
        "Team prefers structured documents over live calls",
    ];
    for (i, p) in phrases.iter().enumerate() {
        encode_text(client, 101 + (i as u32) * 2, p).await;
    }
}

fn is_hybrid_response(frame: &RecallResponseFrame) -> bool {
    frame
        .results
        .iter()
        .any(|r| !r.contributing_retrievers.is_empty() || r.fused_score != 0.0)
}

fn assert_substrate(frame: &RecallResponseFrame) {
    for r in &frame.results {
        assert!(
            r.contributing_retrievers.is_empty(),
            "substrate path must not populate contributing_retrievers",
        );
        assert_eq!(
            r.fused_score, 0.0,
            "substrate path must leave fused_score zero",
        );
    }
}

// ---------------------------------------------------------------------------
// C1 — 50 parallel tasks, mixed strategies.
//
// Each task connects fresh (no shared client state), handshakes, and
// issues exactly one RECALL. The fixture is seeded by a setup client
// before any task is spawned; ENCODE is idempotent on RequestId so
// concurrent recalls all see a non-empty shard.
//
// The invariant: a task asking for `SubstrateOnly` MUST receive a
// substrate-shaped response (empty contributing_retrievers + zero
// fused_score), regardless of what its siblings asked for. A task
// asking for `Auto` MUST receive a hybrid-shaped response (at least
// one hit with non-empty contributing_retrievers and positive
// fused_score). A task asking for `HybridOnly` must not error
// outside a txn — the substrate isn't holding a write lock — and
// must also receive a hybrid-shaped response.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_recalls_each_honour_their_own_strategy() {
    let server = Arc::new(start(1).await);

    // Seed the shard once before fan-out.
    {
        let mut setup = TcpStream::connect(server.data_plane_addr)
            .await
            .expect("connect setup");
        complete_handshake(&mut setup, "recall-c1-setup").await;
        seed_fixture(&mut setup).await;
    }

    // 50 tasks; round-robin strategy among the three.
    let strategies = [
        Some(RecallStrategy::Auto),
        Some(RecallStrategy::SubstrateOnly),
        Some(RecallStrategy::HybridOnly),
    ];

    let mut handles = Vec::with_capacity(50);
    for i in 0..50u32 {
        let strategy = strategies[(i as usize) % strategies.len()];
        let addr = server.data_plane_addr;
        handles.push(tokio::spawn(async move {
            let mut client = TcpStream::connect(addr).await.expect("connect task");
            complete_handshake(&mut client, &format!("recall-c1-task-{i}")).await;
            let req = recall_request("meeting preferences", strategy);
            let (opcode, body) = round_trip(&mut client, 1, RequestBody::Recall(req)).await;
            (i, strategy, opcode, body)
        }));
    }

    let mut auto_count = 0usize;
    let mut substrate_count = 0usize;
    let mut hybrid_only_count = 0usize;

    for h in handles {
        let (i, strategy, opcode, body) = h.await.expect("join task");
        assert_eq!(
            opcode,
            Opcode::RecallResp.as_u16(),
            "task {i} ({strategy:?}) expected RecallResp, got opcode {opcode}",
        );
        let frame = match body {
            ResponseBody::Recall(r) => r,
            other => panic!("task {i} ({strategy:?}) expected RecallResp body, got {other:?}"),
        };
        assert!(frame.is_final, "task {i}: response not marked final");

        match strategy {
            Some(RecallStrategy::SubstrateOnly) => {
                substrate_count += 1;
                assert_substrate(&frame);
            }
            Some(RecallStrategy::Auto) => {
                auto_count += 1;
                assert!(
                    is_hybrid_response(&frame),
                    "task {i} (Auto): hybrid metadata absent — strategy leaked from a sibling",
                );
            }
            Some(RecallStrategy::HybridOnly) => {
                hybrid_only_count += 1;
                assert!(
                    is_hybrid_response(&frame),
                    "task {i} (HybridOnly): hybrid metadata absent",
                );
            }
            None => panic!("test bug: no None strategy in fan-out"),
        }
    }

    // Sanity: fan-out actually covered each strategy. With 50 tasks
    // and 3 strategies round-robin, each bucket has 16 or 17 cases.
    assert!(auto_count >= 16, "auto bucket too small: {auto_count}");
    assert!(
        substrate_count >= 16,
        "substrate bucket too small: {substrate_count}",
    );
    assert!(
        hybrid_only_count >= 16,
        "hybrid-only bucket too small: {hybrid_only_count}",
    );

    Arc::try_unwrap(server)
        .ok()
        .expect("server arc has outstanding clones at end of test")
        .stop()
        .await;
}

// ---------------------------------------------------------------------------
// E4 — single client, sequential alternation.
//
// A common operator pattern: a single SDK connection alternates
// hybrid recalls (default) with substrate-only audits. The router
// must not retain any per-connection "sticky" strategy; each
// request's `strategy` field is the single source of truth.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn sequential_recalls_alternate_strategies_without_carryover() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client, "recall-e4").await;
    seed_fixture(&mut client).await;

    let cues = [
        "async meetings",
        "standups",
        "document driven design",
        "structured documents",
        "context switching",
        "async teams",
        "live calls",
        "sync ritual",
        "preferences",
        "communication",
    ];

    let mut stream_id: u32 = 1001;
    for (i, cue) in cues.iter().enumerate() {
        // Alternate: even i → SubstrateOnly, odd i → Auto.
        let strategy = if i % 2 == 0 {
            Some(RecallStrategy::SubstrateOnly)
        } else {
            Some(RecallStrategy::Auto)
        };
        let req = recall_request(cue, strategy);
        let (opcode, body) = round_trip(&mut client, stream_id, RequestBody::Recall(req)).await;
        stream_id += 2;

        assert_eq!(
            opcode,
            Opcode::RecallResp.as_u16(),
            "iteration {i} ({strategy:?}) expected RecallResp, got opcode {opcode}",
        );
        let frame = match body {
            ResponseBody::Recall(r) => r,
            other => panic!("iteration {i}: expected RecallResp body, got {other:?}"),
        };
        assert!(frame.is_final, "iteration {i}: response not marked final");

        match strategy {
            Some(RecallStrategy::SubstrateOnly) => {
                assert_substrate(&frame);
            }
            Some(RecallStrategy::Auto) => {
                assert!(
                    is_hybrid_response(&frame),
                    "iteration {i} (Auto): hybrid metadata absent — substrate carryover from prior request",
                );
            }
            _ => unreachable!(),
        }
    }

    server.stop().await;
}
