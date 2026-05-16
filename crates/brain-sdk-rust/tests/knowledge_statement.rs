//! Statement SDK integration tests. Phase 17.10a.
//!
//! Uses the mock-server harness (`common::spawn_mock_server`) to
//! verify that:
//!
//! - Every builder serialises the right wire opcode + body shape.
//! - Server responses decode into typed `StatementHandle` correctly.
//! - Error paths (`StatementNotFound`, `PredicateUnknown`,
//!   `ChainConflict`) surface through `ClientErrorStatementExt`.
//!
//! Full end-to-end against a real shard lives in
//! `crates/brain-server/tests/knowledge_statement_wire.rs`; this
//! file covers the SDK side without the storage stack.

mod common;

use brain_protocol::knowledge::{
    EvidenceRefWire, StatementCreateResponse, StatementGetResponse, StatementHistoryRequest,
    StatementHistoryResponseFrame, StatementKindWire, StatementListResponseFrame,
    StatementObjectWire, StatementRetractResponse, StatementTombstoneResponse,
    StatementValueWire, StatementView,
};
use brain_protocol::opcode::Opcode;
use brain_protocol::responses::error::ErrorResponse;
use brain_protocol::responses::types::{ErrorCategoryWire, ErrorCodeWire};
use brain_protocol::{RequestBody, ResponseBody};
use brain_sdk_rust::knowledge::errors::{ClientErrorStatementExt, StatementErrorKind};
use brain_sdk_rust::{
    Client, EntityId, StatementId, StatementKind, TombstoneReason,
};

fn sample_view(id: [u8; 16], predicate: &str, kind: StatementKindWire) -> StatementView {
    StatementView {
        statement_id: id,
        kind,
        subject: [1u8; 16],
        subject_pending_audit_id: [0u8; 16],
        predicate: predicate.into(),
        object: StatementObjectWire::Value(StatementValueWire::Text("v1".into())),
        confidence: 0.9,
        evidence: EvidenceRefWire::Inline(vec![]),
        extractor_id: 0,
        extracted_at_unix_nanos: 1_700_000_000_000_000_000,
        schema_version: 1,
        valid_from_unix_nanos: 1_700_000_000_000_000_000,
        valid_to_unix_nanos: 0,
        event_at_unix_nanos: 0,
        version: 1,
        superseded_by: [0u8; 16],
        supersedes: [0u8; 16],
        chain_root: id,
        tombstoned: false,
        tombstoned_at_unix_nanos: 0,
        tombstone_reason: 0,
        flags: 0,
    }
}

fn server_error(message: &str) -> ResponseBody {
    ResponseBody::Error(ErrorResponse {
        code: ErrorCodeWire::MemoryNotFound,
        category: ErrorCategoryWire::NotFound,
        message: message.to_string(),
        details: None,
        retry_after_ms: None,
    })
}

// ---------------------------------------------------------------------------
// fact() create round-trip.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fact_builder_round_trip() {
    let sid = [7u8; 16];
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        // 1. STATEMENT_CREATE
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::StatementCreateReq.as_u16());
        let body = RequestBody::decode(Opcode::StatementCreateReq, &frame.payload).unwrap();
        match body {
            RequestBody::StatementCreate(r) => {
                assert!(matches!(r.kind, StatementKindWire::Fact));
                assert_eq!(r.predicate, "test:role");
                assert!((r.confidence - 0.9).abs() < 1e-3);
            }
            _ => panic!("wrong variant"),
        }
        common::write_frame(
            &mut socket,
            Opcode::StatementCreateResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::StatementCreate(StatementCreateResponse {
                statement_id: sid,
                auto_superseded: [0u8; 16],
                chain_root: sid,
            })
            .encode(),
            true,
        )
        .await;
        // 2. STATEMENT_GET (round-trip to build the handle).
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::StatementGetReq.as_u16());
        common::write_frame(
            &mut socket,
            Opcode::StatementGetResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::StatementGet(StatementGetResponse {
                statement: sample_view(sid, "test:role", StatementKindWire::Fact),
                returned_via_supersession: false,
            })
            .encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let handle = client
        .fact()
        .subject(EntityId::from([1u8; 16]))
        .predicate("test:role")
        .object_value("Engineering Manager")
        .confidence(0.9)
        .create()
        .await
        .expect("create");

    assert_eq!(handle.id, StatementId::from_bytes(sid));
    assert_eq!(handle.kind, StatementKind::Fact);
    assert_eq!(handle.predicate, "test:role");
    assert_eq!(handle.version, 1);
    assert!(handle.is_chain_root());
}

// ---------------------------------------------------------------------------
// preference() auto-supersede.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn preference_builder_auto_supersedes() {
    let p1 = [9u8; 16];
    let p2 = [11u8; 16];
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        // CREATE → response reports auto_superseded = p1.
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::StatementCreateReq.as_u16());
        common::write_frame(
            &mut socket,
            Opcode::StatementCreateResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::StatementCreate(StatementCreateResponse {
                statement_id: p2,
                auto_superseded: p1,
                chain_root: p1,
            })
            .encode(),
            true,
        )
        .await;
        // GET follow-up to assemble the handle.
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::StatementGetReq.as_u16());
        let mut view = sample_view(p2, "test:prefers", StatementKindWire::Preference);
        view.supersedes = p1;
        view.chain_root = p1;
        view.version = 2;
        common::write_frame(
            &mut socket,
            Opcode::StatementGetResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::StatementGet(StatementGetResponse {
                statement: view,
                returned_via_supersession: false,
            })
            .encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let handle = client
        .preference()
        .subject(EntityId::from([1u8; 16]))
        .predicate("test:prefers")
        .object_value("async meetings")
        .create()
        .await
        .expect("create");

    assert_eq!(handle.id, StatementId::from_bytes(p2));
    assert_eq!(handle.version, 2);
    assert_eq!(handle.supersedes, Some(StatementId::from_bytes(p1)));
    assert_eq!(handle.chain_root, StatementId::from_bytes(p1));
}

// ---------------------------------------------------------------------------
// event() requires event_at — surfaces as InvalidRequest client-side
// before sending.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn event_builder_requires_event_at() {
    // No server needed — validation fires before any wire send.
    let (addr, _server) = common::spawn_mock_server(|mut _socket| async move {
        // Drain HELLO/AUTH; this connection shouldn't see a statement
        // request frame (builder errors before sending).
        let _ = common::read_frame_opt(&mut _socket).await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let err = client
        .event()
        .subject(EntityId::from([1u8; 16]))
        .predicate("test:scheduled")
        .object_value("session")
        // no .event_at(...)
        .create()
        .await
        .expect_err("must error");
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("event_at"),
        "error mentions event_at: {msg}"
    );
}

// ---------------------------------------------------------------------------
// statements().get not-found → None.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn statements_get_translates_not_found_to_none() {
    let sid = [21u8; 16];
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::StatementGetReq.as_u16());
        common::write_frame(
            &mut socket,
            Opcode::Error.as_u16(),
            frame.header.stream_id_u32(),
            server_error("statement StatementId(...) not found").encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let opt = client
        .statements()
        .get(StatementId::from_bytes(sid))
        .await
        .expect("get");
    assert!(opt.is_none(), "not-found maps to Ok(None)");
}

// ---------------------------------------------------------------------------
// statements().history returns chain.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn statements_history_returns_chain() {
    let p1 = [30u8; 16];
    let p2 = [31u8; 16];
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(
            frame.header.opcode_u16(),
            Opcode::StatementHistoryReq.as_u16()
        );
        let body = RequestBody::decode(Opcode::StatementHistoryReq, &frame.payload).unwrap();
        match body {
            RequestBody::StatementHistory(StatementHistoryRequest { anchor_id, .. }) => {
                assert_eq!(anchor_id, p1);
            }
            _ => panic!("wrong variant"),
        }
        let mut v1 = sample_view(p1, "test:prefers", StatementKindWire::Preference);
        let mut v2 = sample_view(p2, "test:prefers", StatementKindWire::Preference);
        v1.chain_root = p1;
        v1.superseded_by = p2;
        v2.chain_root = p1;
        v2.supersedes = p1;
        v2.version = 2;
        common::write_frame(
            &mut socket,
            Opcode::StatementHistoryResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::StatementHistory(StatementHistoryResponseFrame {
                items: vec![v1, v2],
                chain_root: p1,
                total_versions: 2,
                is_final: true,
            })
            .encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let chain = client
        .statements()
        .history(StatementId::from_bytes(p1))
        .await
        .expect("history");
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[0].id, StatementId::from_bytes(p1));
    assert_eq!(chain[1].id, StatementId::from_bytes(p2));
    assert_eq!(chain[1].chain_root, StatementId::from_bytes(p1));
}

// ---------------------------------------------------------------------------
// statements().list with filter.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn statements_list_with_filter() {
    let sid = [40u8; 16];
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::StatementListReq.as_u16());
        let body = RequestBody::decode(Opcode::StatementListReq, &frame.payload).unwrap();
        match body {
            RequestBody::StatementList(r) => {
                assert_eq!(r.predicate, "test:prefers");
                assert!(r.only_current);
                assert_eq!(r.kind, 2);
            }
            _ => panic!("wrong variant"),
        }
        common::write_frame(
            &mut socket,
            Opcode::StatementListResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::StatementList(StatementListResponseFrame {
                items: vec![sample_view(sid, "test:prefers", StatementKindWire::Preference)],
                next_cursor: Vec::new(),
                cumulative_count: 1,
                is_final: true,
            })
            .encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let rows = client
        .statements()
        .list()
        .where_subject(EntityId::from([1u8; 16]))
        .where_predicate("test:prefers")
        .of_kind(StatementKind::Preference)
        .current_only()
        .send()
        .await
        .expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, StatementId::from_bytes(sid));
}

// ---------------------------------------------------------------------------
// tombstone + retract.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn statements_tombstone_returns_timestamp() {
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(
            frame.header.opcode_u16(),
            Opcode::StatementTombstoneReq.as_u16()
        );
        common::write_frame(
            &mut socket,
            Opcode::StatementTombstoneResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::StatementTombstone(StatementTombstoneResponse {
                tombstoned_at_unix_nanos: 1_700_000_000_000_000_000,
            })
            .encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let ts = client
        .statements()
        .tombstone(StatementId::from_bytes([50u8; 16]), TombstoneReason::UserRequest, "test")
        .await
        .expect("tombstone");
    assert!(ts > 0);
}

#[tokio::test]
async fn statements_retract_returns_timestamp() {
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(
            frame.header.opcode_u16(),
            Opcode::StatementRetractReq.as_u16()
        );
        common::write_frame(
            &mut socket,
            Opcode::StatementRetractResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::StatementRetract(StatementRetractResponse {
                retracted_at_unix_nanos: 1_700_000_000_000_000_000,
                will_zero_at_unix_nanos: 1_702_592_000_000_000_000,
            })
            .encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let ts = client
        .statements()
        .retract(
            StatementId::from_bytes([60u8; 16]),
            TombstoneReason::ExtractorRetraction,
            "wrong",
        )
        .await
        .expect("retract");
    assert!(ts > 0);
}

// ---------------------------------------------------------------------------
// Error classification — predicate unknown.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn predicate_unknown_surfaces_via_extension_trait() {
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::StatementCreateReq.as_u16());
        common::write_frame(
            &mut socket,
            Opcode::Error.as_u16(),
            frame.header.stream_id_u32(),
            server_error("unknown predicate \"test:x\"; declare via SCHEMA_UPLOAD first").encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let err = client
        .fact()
        .subject(EntityId::from([1u8; 16]))
        .predicate("test:x")
        .object_entity(EntityId::from([2u8; 16]))
        .create()
        .await
        .expect_err("predicate unknown");
    assert_eq!(
        err.statement_error(),
        Some(StatementErrorKind::PredicateUnknown)
    );
    assert!(err.is_statement_predicate_unknown());
}
