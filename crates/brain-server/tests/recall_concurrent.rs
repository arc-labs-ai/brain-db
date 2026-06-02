//! Concurrency test for RECALL routing.
//!
//! N parallel tokio tasks against a shared server. Each task picks
//! txn-attached or non-txn at random and asserts its own response
//! carries the right shape: txn → substrate (empty
//! `contributing_retrievers`, zero `fused_score`); no-txn → hybrid
//! (at least one hit carries retrievers + a non-zero fused_score).
//! Interleaving must not leak per-shard state — a hybrid hit's
//! retriever list from one task showing up in a sibling's substrate
//! response would prove a routing-state race.

#![cfg(target_os = "linux")]

use std::sync::Arc;

use brain_protocol::codec::opcode::Opcode;
use brain_protocol::connection::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::envelope::request::{
    EncodeRequest, MemoryKindWire, RecallRequest, TxnBeginRequest,
};
use brain_protocol::envelope::response::{RecallResponseFrame, ResponseBody};
use brain_protocol::Frame;
use brain_protocol::RequestBody;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

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

fn recall_request(cue: &str, txn_id: Option<[u8; 16]>) -> RecallRequest {
    RecallRequest {
        cue_text: cue.into(),
        top_k: 5,
        confidence_threshold: 0.0,
        context_filter: None,
        age_bound_unix_nanos: None,
        kind_filter: None,
        salience_floor: 0.0,
        include_edges: false,
        include_graph: false,
        include_text: false,
        request_id: Some(*uuid::Uuid::now_v7().as_bytes()),
        txn_id,
        agent_filter: Vec::new(),
        include_other_agents: false,
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
// C1 — 50 parallel tasks, mix of txn-attached and non-txn recalls.
//
// Each task connects fresh, handshakes, and (for txn tasks) opens its
// own transaction before issuing a single RECALL. The fixture is
// seeded by a setup client before any task is spawned; ENCODE is
// idempotent on RequestId so concurrent recalls all see a non-empty
// shard.
//
// The invariant: a task whose RECALL carries a txn_id MUST receive a
// substrate-shaped response (empty contributing_retrievers + zero
// fused_score). A task with no txn MUST receive a hybrid-shaped
// response on a non-empty fixture (at least one hit reports
// contributing_retrievers + positive fused_score). Either bucket
// leaking the other's shape would indicate per-shard routing state
// crossing tasks.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_txn_and_non_txn_recalls_route_correctly() {
    let server = Arc::new(start(1).await);

    // Seed the shard once before fan-out.
    {
        let mut setup = TcpStream::connect(server.data_plane_addr)
            .await
            .expect("connect setup");
        complete_handshake(&mut setup, "recall-c1-setup").await;
        seed_fixture(&mut setup).await;
    }

    // 50 tasks; alternate txn-attached and non-txn.
    let mut handles = Vec::with_capacity(50);
    for i in 0..50u32 {
        let use_txn = i % 2 == 0;
        let addr = server.data_plane_addr;
        handles.push(tokio::spawn(async move {
            let mut client = TcpStream::connect(addr).await.expect("connect task");
            complete_handshake(&mut client, &format!("recall-c1-task-{i}")).await;

            let txn_id = if use_txn {
                let id = *uuid::Uuid::now_v7().as_bytes();
                let (opcode, _body) = round_trip(
                    &mut client,
                    1,
                    RequestBody::TxnBegin(TxnBeginRequest {
                        txn_id: id,
                        timeout_seconds: 30,
                    }),
                )
                .await;
                assert_eq!(opcode, Opcode::TxnBeginResp.as_u16());
                Some(id)
            } else {
                None
            };

            let req = recall_request("meeting preferences", txn_id);
            let (opcode, body) = round_trip(&mut client, 3, RequestBody::Recall(req)).await;
            (i, use_txn, opcode, body)
        }));
    }

    let mut txn_count = 0usize;
    let mut non_txn_count = 0usize;

    for h in handles {
        let (i, use_txn, opcode, body) = h.await.expect("join task");
        assert_eq!(
            opcode,
            Opcode::RecallResp.as_u16(),
            "task {i} (use_txn={use_txn}) expected RecallResp, got opcode {opcode}",
        );
        let frame = match body {
            ResponseBody::Recall(r) => r,
            other => panic!("task {i} (use_txn={use_txn}) expected RecallResp body, got {other:?}"),
        };
        assert!(frame.is_final, "task {i}: response not marked final");

        if use_txn {
            txn_count += 1;
            assert_substrate(&frame);
        } else {
            non_txn_count += 1;
            assert!(
                is_hybrid_response(&frame),
                "task {i} (no txn): hybrid metadata absent — substrate signature leaked into a hybrid response",
            );
        }
    }

    assert_eq!(txn_count, 25, "expected 25 txn tasks; got {txn_count}");
    assert_eq!(
        non_txn_count, 25,
        "expected 25 non-txn tasks; got {non_txn_count}",
    );

    Arc::try_unwrap(server)
        .ok()
        .expect("server arc has outstanding clones at end of test")
        .stop()
        .await;
}
