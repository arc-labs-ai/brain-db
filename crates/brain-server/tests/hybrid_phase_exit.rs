//! Hybrid query exit integration test.
//!
//! Drives the full hybrid query pipeline end-to-end through the
//! live wire-op dispatcher:
//!
//! - **QUERY** end-to-end after a schema is declared and a memory
//!   is indexed.
//! - **QUERY_EXPLAIN** renders the planner's plan text without
//!   execution.
//! - **QUERY_TRACE** runs the executor and renders the plan +
//!   per-retriever metrics.
//! - **Substrate RECALL** transparently routes through the hybrid
//!   pipeline on schema-declared deployments.
//!
//! Complements the per-op wire tests
//! (`query_wire.rs`, `recall_hybrid_routing.rs`) — those
//! check each opcode in isolation; this one checks them all against
//! a populated shard.
//!
//! Linux-only because the shard runtime uses Glommio.

#![cfg(target_os = "linux")]

use brain_protocol::codec::opcode::Opcode;
use brain_protocol::connection::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::envelope::request::{
    EncodeRequest, MemoryKindWire, RecallRequest, RequestBody,
};
use brain_protocol::envelope::response::ResponseBody;
use brain_protocol::Frame;
use brain_protocol::{
    QueryExplainRequest, QueryRequest as WireQueryRequest, QueryTraceRequest,
    RetrieverSelectionWire, SchemaUploadRequest,
};
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
        client_id: "phase-23-exit".into(),
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

// ---------------------------------------------------------------------------
// Fixture helpers.
// ---------------------------------------------------------------------------

fn upload_schema_request() -> RequestBody {
    RequestBody::SchemaUpload(SchemaUploadRequest {
        schema_document: ACME_V1.into(),
        dry_run: false,
        allow_breaking: false,
        request_id: *uuid::Uuid::now_v7().as_bytes(),
    })
}

fn encode_request(text: &str) -> RequestBody {
    RequestBody::Encode(EncodeRequest {
        text: text.into(),
        context_id: 0,
        kind: MemoryKindWire::Episodic,
        salience_hint: 0.7,
        edges: Vec::new(),
        request_id: *uuid::Uuid::now_v7().as_bytes(),
        txn_id: None,
        deduplicate: false,
    })
}

fn text_query(text: &str) -> WireQueryRequest {
    WireQueryRequest {
        text: text.into(),
        entity_anchor: None,
        kind_filter: Vec::new(),
        predicate_filter: Vec::new(),
        time_filter: None,
        confidence_min: None,
        include_tombstoned: false,
        include_superseded: false,
        limit: 10,
        retrievers: RetrieverSelectionWire::Auto,
        fusion_config: None,
        request_id: *uuid::Uuid::now_v7().as_bytes(),
    }
}

fn recall_request(text: &str) -> RequestBody {
    RequestBody::Recall(RecallRequest {
        cue_text: text.into(),
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
        txn_id: None,
        agent_filter: Vec::new(),
        include_other_agents: false,
    })
}

// ---------------------------------------------------------------------------
// Phase-exit tests.
// ---------------------------------------------------------------------------

/// QUERY end-to-end after a schema is declared and one memory is
/// indexed. The auto-router picks Semantic + Lexical for text-only
/// queries; the HNSW write is synchronous on ENCODE so the semantic
/// retriever surfaces the hit immediately (the lexical drain task
/// commits asynchronously and may or may not be ready — we don't
/// gate on it).
#[tokio::test(flavor = "current_thread")]
async fn hybrid_query_surfaces_an_indexed_memory() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    // 1. Declare schema → flips the per-shard gate.
    let (opcode, _) = round_trip(&mut client, 1, upload_schema_request()).await;
    assert_eq!(opcode, Opcode::SchemaUploadResp.as_u16());

    // 2. ENCODE a memory.
    let (opcode, _) = round_trip(
        &mut client,
        3,
        encode_request("ticket budget pushback meeting"),
    )
    .await;
    assert_eq!(opcode, Opcode::EncodeResp.as_u16());

    // 3. QUERY — semantic should surface the hit on the first try.
    let (opcode, body) = round_trip(
        &mut client,
        5,
        RequestBody::Query(text_query("budget meeting")),
    )
    .await;
    assert_eq!(opcode, Opcode::QueryResp.as_u16());
    match body {
        ResponseBody::Query(r) => {
            assert!(
                !r.items.is_empty(),
                "expected at least one hit after ENCODE, got 0",
            );
            assert!(
                !r.retriever_outcomes.is_empty(),
                "router must pick at least one retriever for a text query",
            );
            assert!(r.total_latency_ms >= 0.0);
        }
        other => panic!("expected QueryResp, got {other:?}"),
    }

    server.stop().await;
}

/// EXPLAIN renders the planner's plan text without executing
/// retrievers — `EXPLAIN` (plan-only) p50 500 µs
/// / p99 2 ms.
#[tokio::test(flavor = "current_thread")]
async fn hybrid_explain_renders_plan_text() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let (_, _) = round_trip(&mut client, 1, upload_schema_request()).await;

    let (opcode, body) = round_trip(
        &mut client,
        3,
        RequestBody::QueryExplain(QueryExplainRequest {
            query: text_query("anything"),
        }),
    )
    .await;
    assert_eq!(opcode, Opcode::QueryExplainResp.as_u16());
    match body {
        ResponseBody::QueryExplain(r) => {
            for marker in ["PLAN:", "RETRIEVERS:", "FUSION:", "POST_FILTERS:", "LIMIT:"] {
                assert!(
                    r.plan_text.contains(marker),
                    "plan_text missing {marker:?} — got: {}",
                    r.plan_text,
                );
            }
            assert!(r.estimated_cost_ms > 0.0);
        }
        other => panic!("expected QueryExplainResp, got {other:?}"),
    }

    server.stop().await;
}

/// TRACE runs the executor and appends an EXECUTION block to the
/// plan text.
#[tokio::test(flavor = "current_thread")]
async fn hybrid_trace_includes_execution_block() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let (_, _) = round_trip(&mut client, 1, upload_schema_request()).await;
    let (_, _) = round_trip(&mut client, 3, encode_request("trace target")).await;

    let (opcode, body) = round_trip(
        &mut client,
        5,
        RequestBody::QueryTrace(QueryTraceRequest {
            query: text_query("trace target"),
        }),
    )
    .await;
    assert_eq!(opcode, Opcode::QueryTraceResp.as_u16());
    match body {
        ResponseBody::QueryTrace(r) => {
            assert!(r.trace_text.contains("PLAN:"));
            assert!(
                r.trace_text.contains("EXECUTION:"),
                "trace_text missing EXECUTION block — got: {}",
                r.trace_text,
            );
            assert!(r.trace_text.contains("TOTAL LATENCY"));
            assert!(r.total_latency_ms >= 0.0);
        }
        other => panic!("expected QueryTraceResp, got {other:?}"),
    }

    server.stop().await;
}

/// Substrate RECALL transparently routes through the hybrid path
/// when a schema is declared. Returned
/// `MemoryResult`s have `contributing_retrievers` populated and
/// `fused_score > 0`.
#[tokio::test(flavor = "current_thread")]
async fn recall_after_schema_routes_through_hybrid_pipeline() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let (_, _) = round_trip(&mut client, 1, upload_schema_request()).await;
    let (_, _) = round_trip(
        &mut client,
        3,
        encode_request("strawberry rhubarb cobbler recipe"),
    )
    .await;

    let (opcode, body) = round_trip(&mut client, 5, recall_request("strawberry recipe")).await;
    assert_eq!(opcode, Opcode::RecallResp.as_u16());
    match body {
        ResponseBody::Recall(r) => {
            assert!(r.is_final);
            assert!(!r.results.is_empty(), "expected at least one hit, got 0",);
            let first = &r.results[0];
            assert!(
                !first.contributing_retrievers.is_empty(),
                "hybrid path must populate contributing_retrievers",
            );
            assert!(
                first.fused_score > 0.0,
                "hybrid path must populate fused_score > 0",
            );
        }
        other => panic!("expected RecallResp, got {other:?}"),
    }

    server.stop().await;
}
