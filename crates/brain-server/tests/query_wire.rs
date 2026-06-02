//! Hybrid query wire-op smoke tests.
//!
//! Drives the four hybrid-query opcodes through the full data-plane
//! stack:
//!
//! - `QUERY`          (0x0160) — plan + execute → `QueryResponse`.
//! - `QUERY_EXPLAIN`  (0x0161) — plan only → plan text.
//! - `QUERY_TRACE`    (0x0162) — plan + execute → trace text.
//! - `RECALL_HYBRID`  (0x0163) — narrow projection → memory ids.
//!
//! These tests run against the shared in-process harness (one shard,
//! empty fixture). Retrievers are wired automatically by `spawn_shard`,
//! so a text-only auto-routed query is expected
//! to return an empty result set with the per-retriever outcome list
//! populated.

#![cfg(target_os = "linux")]

use brain_protocol::codec::opcode::Opcode;
use brain_protocol::connection::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::envelope::request::RequestBody;
use brain_protocol::envelope::response::ResponseBody;
use brain_protocol::Frame;
use brain_protocol::{
    QueryExplainRequest, QueryRequest, QueryTraceRequest, RecallHybridRequest,
    RetrieverSelectionWire, RetrieverWire,
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

// ---------------------------------------------------------------------------
// Wire helpers — copied from sibling *_wire.rs tests.
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
        client_id: "query-tester".into(),
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
// Fixtures.
// ---------------------------------------------------------------------------

fn sample_request_id() -> [u8; 16] {
    *uuid::Uuid::now_v7().as_bytes()
}

fn text_only_query(text: &str) -> QueryRequest {
    QueryRequest {
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
        request_id: sample_request_id(),
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn query_smoke_round_trips_a_simple_request() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let (opcode, body) =
        round_trip(&mut client, 1, RequestBody::Query(text_only_query("topic"))).await;
    assert_eq!(opcode, Opcode::QueryResp.as_u16());
    match body {
        ResponseBody::Query(r) => {
            // Empty fixture — no hits expected; but the per-retriever
            // outcome list still surfaces what the planner picked.
            assert!(r.items.is_empty(), "no data indexed in fixture");
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

#[tokio::test(flavor = "current_thread")]
async fn query_explain_returns_plan_text_without_execution() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let (opcode, body) = round_trip(
        &mut client,
        1,
        RequestBody::QueryExplain(QueryExplainRequest {
            query: text_only_query("budget pushback"),
        }),
    )
    .await;
    assert_eq!(opcode, Opcode::QueryExplainResp.as_u16());
    match body {
        ResponseBody::QueryExplain(r) => {
            assert!(
                r.plan_text.contains("PLAN:"),
                "plan text should contain PLAN header — got: {}",
                r.plan_text,
            );
            assert!(
                r.plan_text.contains("RETRIEVERS:"),
                "plan text should list retrievers — got: {}",
                r.plan_text,
            );
            assert!(r.plan_text.contains("FUSION:"));
            assert!(r.estimated_cost_ms > 0.0);
        }
        other => panic!("expected QueryExplainResp, got {other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn query_trace_returns_execution_block() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let (opcode, body) = round_trip(
        &mut client,
        1,
        RequestBody::QueryTrace(QueryTraceRequest {
            query: text_only_query("budget pushback"),
        }),
    )
    .await;
    assert_eq!(opcode, Opcode::QueryTraceResp.as_u16());
    match body {
        ResponseBody::QueryTrace(r) => {
            assert!(r.trace_text.contains("PLAN:"));
            assert!(
                r.trace_text.contains("EXECUTION:"),
                "trace text should contain EXECUTION block — got: {}",
                r.trace_text,
            );
            assert!(r.trace_text.contains("TOTAL LATENCY"));
            assert!(r.total_latency_ms >= 0.0);
        }
        other => panic!("expected QueryTraceResp, got {other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn recall_hybrid_returns_memory_only_results() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let (opcode, body) = round_trip(
        &mut client,
        1,
        RequestBody::RecallHybrid(RecallHybridRequest {
            text: "anything".into(),
            agent_id_filter: None,
            limit: 5,
            request_id: sample_request_id(),
        }),
    )
    .await;
    assert_eq!(opcode, Opcode::RecallHybridResp.as_u16());
    match body {
        ResponseBody::RecallHybrid(r) => {
            // Empty fixture; just verify the wire path and shape.
            assert!(r.items.is_empty());
        }
        other => panic!("expected RecallHybridResp, got {other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn query_no_signal_returns_error() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    // Empty text + no entity anchor → planner reports NoSignal →
    // handler maps to InvalidRequest → server returns ERROR frame.
    let mut req = text_only_query("");
    req.text.clear();
    let (opcode, body) = round_trip(&mut client, 1, RequestBody::Query(req)).await;
    assert_eq!(opcode, Opcode::Error.as_u16());
    match body {
        ResponseBody::Error(e) => {
            assert!(
                e.message.to_lowercase().contains("text")
                    || e.message.to_lowercase().contains("anchor"),
                "expected message to mention missing signal — got: {}",
                e.message,
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn query_explicit_retriever_list_overflow_is_rejected() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let mut req = text_only_query("topic");
    // Force a 4-entry explicit list (> MAX_EXPLICIT_RETRIEVERS = 3).
    req.retrievers = RetrieverSelectionWire::Explicit(vec![
        RetrieverWire::Semantic,
        RetrieverWire::Lexical,
        RetrieverWire::Graph,
        RetrieverWire::Semantic,
    ]);
    let (opcode, body) = round_trip(&mut client, 1, RequestBody::Query(req)).await;
    assert_eq!(opcode, Opcode::Error.as_u16());
    match body {
        ResponseBody::Error(e) => {
            assert!(
                e.message.to_lowercase().contains("retriever"),
                "expected message to mention retriever list — got: {}",
                e.message,
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }

    server.stop().await;
}
