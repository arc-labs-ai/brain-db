//! Relation SDK integration tests. Phase 18.9a.
//!
//! Uses the mock-server harness (`common::spawn_mock_server`) to
//! verify request/response shapes, RelationHandle projection, and
//! `ClientErrorRelationExt` classification.

mod common;

use brain_protocol::knowledge::statement_req::EvidenceRefWire;
use brain_protocol::knowledge::{
    RelationCreateResponse, RelationGetResponse, RelationListFromResponseFrame,
    RelationListToResponseFrame, RelationSupersedeResponse, RelationTombstoneResponse,
    RelationTraverseResponseFrame, RelationView, TraversalPathWire, TraversalStepWire,
};
use brain_protocol::opcode::Opcode;
use brain_protocol::responses::error::ErrorResponse;
use brain_protocol::responses::types::{ErrorCategoryWire, ErrorCodeWire};
use brain_protocol::{RequestBody, ResponseBody};
use brain_sdk_rust::knowledge::errors::{ClientErrorRelationExt, RelationErrorKind};
use brain_sdk_rust::{Client, EntityId, RelationId, TraverseDirection};

fn sample_view(id: [u8; 16], from: [u8; 16], to: [u8; 16]) -> RelationView {
    RelationView {
        relation_id: id,
        chain_root: id,
        relation_type: "brain:related_to".into(),
        from_entity: from,
        to_entity: to,
        properties_blob: Vec::new(),
        evidence: EvidenceRefWire::Inline(vec![]),
        extractor_id: 0,
        extracted_at_unix_nanos: 1_700_000_000_000_000_000,
        confidence: 0.9,
        valid_from_unix_nanos: 0,
        valid_to_unix_nanos: 0,
        version: 1,
        superseded_by: [0u8; 16],
        supersedes: [0u8; 16],
        tombstoned: false,
        tombstoned_at_unix_nanos: 0,
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
// relation().create()
// ---------------------------------------------------------------------------

#[tokio::test]
async fn relation_builder_create() {
    let rid = [7u8; 16];
    let from = [1u8; 16];
    let to = [2u8; 16];
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        // RELATION_CREATE
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::RelationCreateReq.as_u16());
        let body = RequestBody::decode(Opcode::RelationCreateReq, &frame.payload).unwrap();
        match body {
            RequestBody::RelationCreate(r) => {
                assert_eq!(r.relation_type, "brain:related_to");
                assert_eq!(r.from_entity, from);
                assert_eq!(r.to_entity, to);
                assert!((r.confidence - 0.9).abs() < 1e-3);
            }
            _ => panic!("wrong variant"),
        }
        common::write_frame(
            &mut socket,
            Opcode::RelationCreateResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::RelationCreate(RelationCreateResponse { relation_id: rid }).encode(),
            true,
        )
        .await;
        // GET round-trip.
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::RelationGetReq.as_u16());
        common::write_frame(
            &mut socket,
            Opcode::RelationGetResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::RelationGet(RelationGetResponse {
                relation: sample_view(rid, from, to),
                returned_via_supersession: false,
            })
            .encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let h = client
        .relation()
        .relation_type("brain:related_to")
        .from(EntityId::from(from))
        .to(EntityId::from(to))
        .confidence(0.9)
        .create()
        .await
        .expect("create");
    assert_eq!(h.id, RelationId::from_bytes(rid));
    assert_eq!(h.relation_type, "brain:related_to");
    assert!(h.is_chain_root());
}

// ---------------------------------------------------------------------------
// relation().supersedes() routes to RELATION_SUPERSEDE
// ---------------------------------------------------------------------------

#[tokio::test]
async fn relation_supersedes_routes_to_supersede_op() {
    let old = [9u8; 16];
    let new = [11u8; 16];
    let from = [1u8; 16];
    let to = [2u8; 16];
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(
            frame.header.opcode_u16(),
            Opcode::RelationSupersedeReq.as_u16(),
            "supersedes routes through SUPERSEDE op"
        );
        common::write_frame(
            &mut socket,
            Opcode::RelationSupersedeResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::RelationSupersede(RelationSupersedeResponse {
                new_relation_id: new,
                version: 2,
            })
            .encode(),
            true,
        )
        .await;
        // GET round-trip.
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::RelationGetReq.as_u16());
        let mut view = sample_view(new, from, to);
        view.supersedes = old;
        view.version = 2;
        common::write_frame(
            &mut socket,
            Opcode::RelationGetResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::RelationGet(RelationGetResponse {
                relation: view,
                returned_via_supersession: false,
            })
            .encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let h = client
        .relation()
        .relation_type("brain:related_to")
        .from(EntityId::from(from))
        .to(EntityId::from(to))
        .supersedes(RelationId::from_bytes(old))
        .create()
        .await
        .expect("supersede");
    assert_eq!(h.id, RelationId::from_bytes(new));
    assert_eq!(h.version, 2);
    assert_eq!(h.supersedes, Some(RelationId::from_bytes(old)));
}

// ---------------------------------------------------------------------------
// relations().get → None when server returns NotFound
// ---------------------------------------------------------------------------

#[tokio::test]
async fn relation_get_translates_not_found_to_none() {
    let rid = [21u8; 16];
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::RelationGetReq.as_u16());
        common::write_frame(
            &mut socket,
            Opcode::Error.as_u16(),
            frame.header.stream_id_u32(),
            server_error("relation RelationId(...) not found").encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let opt = client
        .relations()
        .get(RelationId::from_bytes(rid))
        .await
        .expect("get");
    assert!(opt.is_none());
}

// ---------------------------------------------------------------------------
// relations().tombstone returns timestamp
// ---------------------------------------------------------------------------

#[tokio::test]
async fn relations_tombstone_returns_timestamp() {
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(
            frame.header.opcode_u16(),
            Opcode::RelationTombstoneReq.as_u16()
        );
        common::write_frame(
            &mut socket,
            Opcode::RelationTombstoneResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::RelationTombstone(RelationTombstoneResponse {
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
        .relations()
        .tombstone(RelationId::from_bytes([50u8; 16]), "test")
        .await
        .expect("tombstone");
    assert!(ts > 0);
}

// ---------------------------------------------------------------------------
// list_from / list_to
// ---------------------------------------------------------------------------

#[tokio::test]
async fn relations_list_from_with_type_filter() {
    let rid = [40u8; 16];
    let from = [1u8; 16];
    let to = [2u8; 16];
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::RelationListFromReq.as_u16());
        let body = RequestBody::decode(Opcode::RelationListFromReq, &frame.payload).unwrap();
        match body {
            RequestBody::RelationListFrom(r) => {
                assert_eq!(r.from_entity, from);
                assert_eq!(r.relation_type_filter, "brain:related_to");
                assert!(!r.include_superseded);
                assert!(!r.include_tombstoned);
            }
            _ => panic!("wrong variant"),
        }
        common::write_frame(
            &mut socket,
            Opcode::RelationListFromResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::RelationListFrom(RelationListFromResponseFrame {
                items: vec![sample_view(rid, from, to)],
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
        .relations()
        .list_from(EntityId::from(from))
        .with_type("brain:related_to")
        .send()
        .await
        .expect("list_from");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, RelationId::from_bytes(rid));
}

#[tokio::test]
async fn relations_list_to_returns_handles() {
    let rid = [42u8; 16];
    let from = [1u8; 16];
    let to = [2u8; 16];
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::RelationListToReq.as_u16());
        common::write_frame(
            &mut socket,
            Opcode::RelationListToResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::RelationListTo(RelationListToResponseFrame {
                items: vec![sample_view(rid, from, to)],
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
        .relations()
        .list_to(EntityId::from(to))
        .send()
        .await
        .expect("list_to");
    assert_eq!(rows.len(), 1);
}

// ---------------------------------------------------------------------------
// traverse
// ---------------------------------------------------------------------------

#[tokio::test]
async fn relations_traverse_returns_paths() {
    let rid = [50u8; 16];
    let from = [1u8; 16];
    let to = [2u8; 16];
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::RelationTraverseReq.as_u16());
        let body = RequestBody::decode(Opcode::RelationTraverseReq, &frame.payload).unwrap();
        match body {
            RequestBody::RelationTraverse(r) => {
                assert_eq!(r.start_entity, from);
                assert_eq!(r.direction, 2, "Both → wire byte 2");
                assert_eq!(r.max_depth, 2);
            }
            _ => panic!("wrong variant"),
        }
        common::write_frame(
            &mut socket,
            Opcode::RelationTraverseResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::RelationTraverse(RelationTraverseResponseFrame {
                paths: vec![TraversalPathWire {
                    steps: vec![TraversalStepWire {
                        relation_id: rid,
                        from,
                        to,
                        relation_type: "brain:related_to".into(),
                        depth: 1,
                    }],
                }],
                total_paths: 1,
                truncated: false,
                is_final: true,
            })
            .encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let paths = client
        .relations()
        .traverse(EntityId::from(from))
        .direction(TraverseDirection::Both)
        .depth(2)
        .send()
        .await
        .expect("traverse");
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0].steps[0].relation_id, RelationId::from_bytes(rid));
    assert_eq!(paths[0].steps[0].depth, 1);
}

// ---------------------------------------------------------------------------
// Error classification
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_relation_type_classifies_via_extension_trait() {
    let from = [1u8; 16];
    let to = [2u8; 16];
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::RelationCreateReq.as_u16());
        common::write_frame(
            &mut socket,
            Opcode::Error.as_u16(),
            frame.header.stream_id_u32(),
            server_error("unknown relation_type \"user:x\"").encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let err = client
        .relation()
        .relation_type("user:x")
        .from(EntityId::from(from))
        .to(EntityId::from(to))
        .create()
        .await
        .expect_err("unknown type");
    assert_eq!(
        err.relation_error(),
        Some(RelationErrorKind::RelationTypeUnknown)
    );
    assert!(err.is_relation_type_unknown());
}
