//! Hybrid-default RECALL routing.
//!
//! Hybrid retrieval is the default for every deployment. These
//! tests pin the three routing outcomes:
//!
//! 1. **Schemaless** — hybrid runs. Encoded memories show up with
//!    `contributing_retrievers` populated by the semantic +
//!    lexical retrievers (graph contributes when substrate edges
//!    are present) and `fused_score > 0`.
//! 2. **Schema declared** — hybrid runs (same path; the schema
//!    is strictness-only, not a retrieval gate).
//! 3. **Txn attached** — substrate path, because hybrid
//!    retrievers can't see the txn buffer and would silently miss
//!    pending writes.

#![cfg(target_os = "linux")]

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
use brain_protocol::SchemaUploadRequest;
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

const ACME_V1: &str = "namespace acme\n\
                       define entity_type Foo { attributes {} }\n";

// ---------------------------------------------------------------------------
// Wire helpers.
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

async fn complete_handshake(client: &mut TcpStream) {
    let hello = HelloPayload {
        client_id: "recall-router".into(),
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

fn recall_request(txn_id: Option<[u8; 16]>) -> RecallRequest {
    RecallRequest {
        cue_text: "meeting preferences".into(),
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
        // Client-initiated streams must be odd per spec.
        encode_text(client, 101 + (i as u32) * 2, p).await;
    }
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

fn assert_hybrid(frame: &RecallResponseFrame) {
    assert!(!frame.results.is_empty(), "expected hybrid hits, got none");
    let mut any_with_retrievers = false;
    for r in &frame.results {
        if !r.contributing_retrievers.is_empty() {
            any_with_retrievers = true;
            assert!(
                r.fused_score > 0.0,
                "hybrid hit reports retrievers but fused_score=0",
            );
        }
    }
    assert!(
        any_with_retrievers,
        "at least one hit must report contributing_retrievers on hybrid path",
    );
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn recall_without_schema_uses_hybrid_path() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    seed_fixture(&mut client).await;

    let (opcode, body) =
        round_trip(&mut client, 1, RequestBody::Recall(recall_request(None))).await;
    assert_eq!(opcode, Opcode::RecallResp.as_u16());
    match body {
        ResponseBody::Recall(r) => {
            assert!(r.is_final);
            // No schema, no txn, default strategy → hybrid runs.
            assert_hybrid(&r);
        }
        other => panic!("expected RecallResp, got {other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn recall_after_schema_upload_uses_hybrid_path() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let (opcode, _body) = round_trip(
        &mut client,
        1,
        RequestBody::SchemaUpload(SchemaUploadRequest {
            schema_document: ACME_V1.into(),
            dry_run: false,
            allow_breaking: false,
            request_id: *uuid::Uuid::now_v7().as_bytes(),
        }),
    )
    .await;
    assert_eq!(opcode, Opcode::SchemaUploadResp.as_u16());

    seed_fixture(&mut client).await;

    let (opcode, body) =
        round_trip(&mut client, 3, RequestBody::Recall(recall_request(None))).await;
    assert_eq!(opcode, Opcode::RecallResp.as_u16());
    match body {
        ResponseBody::Recall(r) => {
            assert!(r.is_final);
            assert_hybrid(&r);
        }
        other => panic!("expected RecallResp, got {other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn recall_inside_txn_uses_substrate_path_even_with_schema() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let (opcode, _body) = round_trip(
        &mut client,
        1,
        RequestBody::SchemaUpload(SchemaUploadRequest {
            schema_document: ACME_V1.into(),
            dry_run: false,
            allow_breaking: false,
            request_id: *uuid::Uuid::now_v7().as_bytes(),
        }),
    )
    .await;
    assert_eq!(opcode, Opcode::SchemaUploadResp.as_u16());

    seed_fixture(&mut client).await;

    let txn_id = *uuid::Uuid::now_v7().as_bytes();
    let (opcode, _body) = round_trip(
        &mut client,
        3,
        RequestBody::TxnBegin(TxnBeginRequest {
            txn_id,
            timeout_seconds: 30,
        }),
    )
    .await;
    assert_eq!(opcode, Opcode::TxnBeginResp.as_u16());

    let (opcode, body) = round_trip(
        &mut client,
        5,
        RequestBody::Recall(recall_request(Some(txn_id))),
    )
    .await;
    assert_eq!(opcode, Opcode::RecallResp.as_u16());
    match body {
        ResponseBody::Recall(r) => {
            assert!(r.is_final);
            assert_substrate(&r);
        }
        other => panic!("expected RecallResp, got {other:?}"),
    }

    server.stop().await;
}

// ---------------------------------------------------------------------------
// E2 — cold-start safety. A hybrid recall against a server with zero
// memories must return an empty result, not an error or a hang. tantivy
// + HNSW have both historically returned errors on cold indexes; the
// substrate path must surface this as an empty `RecallResp`.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn recall_against_zero_memories_returns_empty_response() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;
    // No seed_fixture: the shard has zero memories encoded.

    let (opcode, body) =
        round_trip(&mut client, 1, RequestBody::Recall(recall_request(None))).await;
    assert_eq!(opcode, Opcode::RecallResp.as_u16());
    match body {
        ResponseBody::Recall(r) => {
            assert!(r.is_final, "cold-start hybrid must mark response final");
            assert!(
                r.results.is_empty(),
                "zero-memory shard must return an empty result, got {} hits",
                r.results.len(),
            );
        }
        other => panic!("expected RecallResp, got {other:?}"),
    }

    server.stop().await;
}

// ---------------------------------------------------------------------------
// E3 — Unicode cue text. Both the embedding tokenizer and the tantivy
// analyser have historically truncated multibyte characters on byte
// boundaries instead of code-point boundaries. Encode and recall round
// trips with mixed scripts + emoji must succeed without panic and
// produce non-error responses.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn unicode_cue_text_roundtrips_through_hybrid_recall() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let phrases = [
        "Niraj met Александра in 北京",
        "deploy day on Friday",
        "اللقاء غدا",
    ];
    for (i, p) in phrases.iter().enumerate() {
        encode_text(&mut client, 101 + (i as u32) * 2, p).await;
    }

    // Recall with one of the unicode strings as cue text. The
    // response must be a `RecallResp` (not Error), regardless of
    // hit count — the property under test is "no decode panic /
    // no tokenizer crash", not relevance.
    let mut req = recall_request(None);
    req.cue_text = "Александра 北京".into();
    let (opcode, body) = round_trip(&mut client, 1, RequestBody::Recall(req)).await;
    assert_eq!(opcode, Opcode::RecallResp.as_u16());
    match body {
        ResponseBody::Recall(r) => {
            assert!(r.is_final);
        }
        other => panic!("expected RecallResp, got {other:?}"),
    }

    server.stop().await;
}

// ---------------------------------------------------------------------------
// P3 — txn-vs-non-txn routing invariant. For varied request shapes
// (cue text, top_k, salience floor), the per-hit signature must match
// the request's txn attachment: a recall with `txn_id` set must
// produce empty `contributing_retrievers` and zero `fused_score`
// (substrate); a recall without it must produce populated
// `contributing_retrievers` and a non-zero `fused_score` on at least
// one hit (hybrid). One shared server across iterations keeps wall
// time bounded; each recall is idempotent given a fresh request_id,
// so reuse is safe.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn txn_recall_invariants_hold_across_request_shapes() {
    use proptest::strategy::{Strategy, ValueTree};
    use proptest::test_runner::{Config, TestRunner};

    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;
    seed_fixture(&mut client).await;

    // 16 cases — heavy fixture (server + handshake reused via
    // outer scope). We can't use TestRunner::run because its `Fn`
    // closure forbids awaits; instead we manually draw value-trees
    // from the strategy and await each round-trip inline.
    let mut runner = TestRunner::new(Config {
        cases: 16,
        ..Config::default()
    });
    let strategy = (
        proptest::collection::vec("[a-z]{1,8}", 1..=20),
        1u32..=10,
        proptest::num::f32::POSITIVE | proptest::num::f32::ZERO,
        proptest::bool::ANY,
    );
    let bounded_strategy = strategy.prop_map(|(tokens, top_k, salience_raw, attach_txn)| {
        let salience = if salience_raw.is_finite() {
            salience_raw.clamp(0.0, 1.0)
        } else {
            0.0
        };
        (tokens.join(" "), top_k, salience, attach_txn)
    });

    let mut sid: u32 = 1001;
    let mut txn_count: usize = 0;
    let mut hybrid_count: usize = 0;
    for _ in 0..16 {
        let tree = bounded_strategy
            .new_tree(&mut runner)
            .expect("strategy must produce a value tree");
        let (cue, top_k, salience, attach_txn) = tree.current();

        let txn_id = if attach_txn {
            let id = *uuid::Uuid::now_v7().as_bytes();
            let (opcode, _body) = round_trip(
                &mut client,
                sid,
                RequestBody::TxnBegin(TxnBeginRequest {
                    txn_id: id,
                    timeout_seconds: 30,
                }),
            )
            .await;
            assert_eq!(opcode, Opcode::TxnBeginResp.as_u16());
            sid += 2;
            Some(id)
        } else {
            None
        };

        let mut req = recall_request(txn_id);
        req.cue_text = cue;
        req.top_k = top_k;
        req.salience_floor = salience;
        let (opcode, body) = round_trip(&mut client, sid, RequestBody::Recall(req)).await;
        sid += 2;
        assert_eq!(
            opcode,
            Opcode::RecallResp.as_u16(),
            "expected RecallResp; got opcode {opcode}",
        );
        match body {
            ResponseBody::Recall(r) => {
                if attach_txn {
                    txn_count += 1;
                    // Txn path is substrate-shaped; every hit
                    // (when any) must carry the substrate
                    // signature.
                    assert_substrate(&r);
                } else {
                    hybrid_count += 1;
                    // Empty fixtures or strict salience floors
                    // can legitimately return zero hits; only
                    // assert the hybrid shape when there's
                    // something to inspect.
                    if !r.results.is_empty() {
                        assert_hybrid(&r);
                    }
                }
            }
            other => panic!("expected RecallResp, got {other:?}"),
        }
    }
    assert_eq!(
        txn_count + hybrid_count,
        16,
        "must execute exactly 16 proptest-drawn cases; ran txn={txn_count} hybrid={hybrid_count}",
    );

    server.stop().await;
}
