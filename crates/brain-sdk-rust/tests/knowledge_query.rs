//! Query SDK integration tests. Phase 23.10.
//!
//! Uses the mock-server harness to verify that:
//!
//! - `client.query()` sends the right wire opcode + body shape
//!   for `.execute()`, `.explain()`, and `.trace()`.
//! - The server's response decodes into the typed SDK
//!   `QueryResult` / `ExplainResult` / `TraceResult`.
//! - Builder-side validation (NoSignal, oversized text) errors
//!   before any frame is sent.
//! - The documented "filter for memories" pattern works on a
//!   heterogeneous `QueryResult.items` list (since there's no
//!   `client.recall_hybrid` SDK verb).
//!
//! End-to-end against a real shard lives in
//! `crates/brain-server/tests/query_wire.rs`; this
//! file covers the SDK side without the storage stack.

mod common;

use brain_protocol::{
    ItemIdWire, QueryExplainResponse as WireExplainResp, QueryResponse,
    QueryResultItem as WireQueryResultItem, QueryTraceResponse as WireTraceResp,
    RetrieverContributionWire, RetrieverOutcomeWire, RetrieverWire,
};
use brain_protocol::codec::opcode::Opcode;
use brain_protocol::{RequestBody, ResponseBody};
use brain_sdk_rust::{
    Client, ItemKind, MemoryId, QueryBuilderError, Retriever, RetrieverOutcomeStatus,
    RetrieverSelection,
};

// ---------------------------------------------------------------------------
// execute() round-trip — empty fixture-style response.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn query_execute_round_trips_empty_response() {
    let (addr, _server) = common::spawn_mock_server(|mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::QueryReq.as_u16());
        let body = RequestBody::decode(Opcode::QueryReq, &frame.payload).unwrap();
        match body {
            RequestBody::Query(r) => {
                assert_eq!(r.text, "topic");
                assert_eq!(r.limit, 10);
                assert!(r.entity_anchor.is_none());
            }
            other => panic!("wrong variant: {other:?}"),
        }
        common::write_frame(
            &mut socket,
            Opcode::QueryResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::Query(QueryResponse {
                items: Vec::new(),
                total_latency_ms: 4.2,
                retriever_outcomes: vec![RetrieverOutcomeWire {
                    retriever: RetrieverWire::Semantic,
                    status: 0,
                    message: String::new(),
                    latency_ms: 3.1,
                    result_count: 0,
                }],
            })
            .encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let result = client
        .query()
        .text("topic")
        .limit(10)
        .execute()
        .await
        .expect("execute");

    assert!(result.items.is_empty());
    assert!((result.total_latency_ms - 4.2).abs() < 1e-6);
    assert_eq!(result.retriever_outcomes.len(), 1);
    let semantic = result.outcome(Retriever::Semantic).expect("outcome");
    assert!(semantic.status.is_success());
    assert!(!result.any_failure());
}

// ---------------------------------------------------------------------------
// execute() with a heterogeneous result — caller filters memories.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn query_filtered_to_memories_via_item_ref() {
    let mid_bytes = 0x0102_0304_0506_0708_090A_0B0C_0D0E_0F10u128.to_be_bytes();
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::QueryReq.as_u16());
        common::write_frame(
            &mut socket,
            Opcode::QueryResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::Query(QueryResponse {
                items: vec![
                    WireQueryResultItem {
                        id: ItemIdWire {
                            kind: 0,
                            bytes: mid_bytes,
                        },
                        fused_score: 0.91,
                        contributing: vec![RetrieverContributionWire {
                            retriever: RetrieverWire::Semantic,
                            rank: 1,
                            raw_score: 0.95,
                        }],
                    },
                    WireQueryResultItem {
                        id: ItemIdWire {
                            kind: 1, // Statement — should be filtered out by the user.
                            bytes: [2u8; 16],
                        },
                        fused_score: 0.4,
                        contributing: vec![],
                    },
                ],
                total_latency_ms: 5.5,
                retriever_outcomes: Vec::new(),
            })
            .encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let result = client
        .query()
        .text("topic")
        .execute()
        .await
        .expect("execute");

    // Caller's "memory only" filter — replaces a hypothetical
    // `client.recall_hybrid()` shortcut.
    let memories: Vec<(MemoryId, f64)> = result
        .items
        .iter()
        .filter_map(|h| h.id.as_memory().map(|id| (id, h.fused_score)))
        .collect();

    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].0.raw(), u128::from_be_bytes(mid_bytes));
    assert_eq!(result.items[0].id.kind(), ItemKind::Memory);
    assert!(result.items[0].contributed_by(Retriever::Semantic));
    assert_eq!(result.items[0].rank_in(Retriever::Semantic), Some(1));
}

// ---------------------------------------------------------------------------
// explain() round-trip.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn query_explain_round_trips() {
    let (addr, _server) = common::spawn_mock_server(|mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::QueryExplainReq.as_u16());
        let body = RequestBody::decode(Opcode::QueryExplainReq, &frame.payload).unwrap();
        match body {
            RequestBody::QueryExplain(r) => {
                assert_eq!(r.query.text, "explain this");
            }
            other => panic!("wrong variant: {other:?}"),
        }
        common::write_frame(
            &mut socket,
            Opcode::QueryExplainResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::QueryExplain(WireExplainResp {
                plan_text: "PLAN: ...".into(),
                estimated_cost_ms: 12.5,
            })
            .encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let explain = client
        .query()
        .text("explain this")
        .explain()
        .await
        .expect("explain");

    assert!(explain.plan_text.starts_with("PLAN:"));
    assert!((explain.estimated_cost_ms - 12.5).abs() < 1e-3);
}

// ---------------------------------------------------------------------------
// trace() round-trip.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn query_trace_round_trips() {
    let (addr, _server) = common::spawn_mock_server(|mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::QueryTraceReq.as_u16());
        common::write_frame(
            &mut socket,
            Opcode::QueryTraceResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::QueryTrace(WireTraceResp {
                trace_text: "PLAN ... EXECUTION ...".into(),
                total_latency_ms: 22.4,
            })
            .encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let trace = client
        .query()
        .text("trace this")
        .trace()
        .await
        .expect("trace");

    assert!(trace.trace_text.contains("EXECUTION"));
    assert!((trace.total_latency_ms - 22.4).abs() < 1e-6);
}

// ---------------------------------------------------------------------------
// Per-retriever outcome decoding.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn query_decodes_each_outcome_status_byte() {
    let (addr, _server) = common::spawn_mock_server(|mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        common::write_frame(
            &mut socket,
            Opcode::QueryResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::Query(QueryResponse {
                items: Vec::new(),
                total_latency_ms: 1.0,
                retriever_outcomes: vec![
                    RetrieverOutcomeWire {
                        retriever: RetrieverWire::Semantic,
                        status: 0,
                        message: String::new(),
                        latency_ms: 1.0,
                        result_count: 0,
                    },
                    RetrieverOutcomeWire {
                        retriever: RetrieverWire::Lexical,
                        status: 1,
                        message: "no text".into(),
                        latency_ms: 0.0,
                        result_count: 0,
                    },
                    RetrieverOutcomeWire {
                        retriever: RetrieverWire::Graph,
                        status: 3,
                        message: "boom".into(),
                        latency_ms: 0.5,
                        result_count: 0,
                    },
                ],
            })
            .encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let result = client.query().text("anything").execute().await.unwrap();

    assert!(matches!(
        result.outcome(Retriever::Semantic).unwrap().status,
        RetrieverOutcomeStatus::Success
    ));
    let lex = result.outcome(Retriever::Lexical).unwrap();
    assert!(matches!(
        &lex.status,
        RetrieverOutcomeStatus::Skipped { reason } if reason == "no text"
    ));
    let gr = result.outcome(Retriever::Graph).unwrap();
    assert!(matches!(
        &gr.status,
        RetrieverOutcomeStatus::Failure { message } if message == "boom"
    ));
    assert!(result.any_failure());
}

// ---------------------------------------------------------------------------
// Builder-side validation — fails before the round-trip.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn query_execute_rejects_no_signal_locally() {
    // No mock server needed; the builder should fail before
    // touching the socket. We still connect to a no-op server so
    // the Client exists.
    let (addr, _server) = common::spawn_mock_server(|socket| async move {
        // Hold the connection open; we don't expect any frames.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(socket);
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let err = client.query().execute().await.unwrap_err();
    // QueryBuilderError::NoSignal → ClientError::Internal.
    let msg = format!("{err}");
    assert!(
        msg.contains("neither text nor entity anchor"),
        "expected NoSignal message; got: {msg}",
    );
}

#[tokio::test]
async fn query_explicit_retriever_overflow_rejects_locally() {
    let err = RetrieverSelection::explicit([
        Retriever::Semantic,
        Retriever::Lexical,
        Retriever::Graph,
        Retriever::Semantic,
    ])
    .unwrap_err();
    assert!(matches!(
        err,
        QueryBuilderError::TooManyExplicitRetrievers { got: 4, .. }
    ));
}
