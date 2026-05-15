//! Entity SDK integration tests. Phase 16.8.5.
//!
//! Uses the mock-server harness (`common::spawn_mock_server`) to
//! verify that:
//!
//! - Every builder serialises the right wire opcode + body shape.
//! - Server responses decode into typed `EntityHandle<Person>` /
//!   `MergeOutcome` / `ResolutionOutcome<Person>` correctly.
//! - Error paths (`EntityNotFound`, type mismatch, merge conflict)
//!   surface through `ClientErrorEntityExt`.
//!
//! Full end-to-end against a real shard lives in
//! `crates/brain-server/tests/knowledge_entity_*_wire.rs`; this file
//! covers the SDK side without the storage stack.

mod common;

use brain_protocol::knowledge::{
    EntityCreateResponse, EntityGetResponse, EntityListItem, EntityListResponseFrame,
    EntityMergeResponse, EntityRenameResponse, EntityResolveResponse, EntityTombstoneResponse,
    EntityUnmergeResponse, EntityUpdateResponse, EntityView, ResolutionOutcomeWire,
};
use brain_protocol::opcode::Opcode;
use brain_protocol::responses::error::ErrorResponse;
use brain_protocol::responses::types::{ErrorCategoryWire, ErrorCodeWire};
use brain_protocol::{RequestBody, ResponseBody};
use brain_sdk_rust::knowledge::errors::{ClientErrorEntityExt, EntityErrorKind};
use brain_sdk_rust::knowledge::ResolutionOutcome;
use brain_sdk_rust::{BrainEntityType, Client, EntityId, Person, PersonAttributes};

const PERSON_TYPE_ID: u32 = 1;

fn sample_view(id: [u8; 16], canonical: &str, attrs_blob: Vec<u8>) -> EntityView {
    EntityView {
        entity_id: id,
        entity_type_id: PERSON_TYPE_ID,
        canonical_name: canonical.to_string(),
        normalized_name: canonical.to_lowercase(),
        aliases: vec![],
        attributes_blob: attrs_blob,
        mention_count: 0,
        created_at_unix_nanos: 1_700_000_000_000_000_000,
        updated_at_unix_nanos: 1_700_000_000_000_000_000,
        merged_into: [0u8; 16],
        embedding_version: 0,
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
// CREATE
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_person_round_trip() {
    let entity_id = [7u8; 16];
    let attrs_blob = Person::encode_attributes(&PersonAttributes {
        email: Some("alice@example.com".into()),
        role: Some("Engineer".into()),
        team: None,
        timezone: None,
    });

    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        // ENTITY_CREATE
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::EntityCreateReq.as_u16());
        let body = RequestBody::decode(Opcode::EntityCreateReq, &frame.payload).unwrap();
        match body {
            RequestBody::EntityCreate(r) => {
                assert_eq!(r.entity_type_id, PERSON_TYPE_ID);
                assert_eq!(r.canonical_name, "Alice");
                assert_eq!(r.aliases, vec!["A.".to_string()]);
                assert_eq!(r.attributes_blob, attrs_blob);
            }
            _ => panic!("wrong variant"),
        }
        common::write_frame(
            &mut socket,
            Opcode::EntityCreateResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::EntityCreate(EntityCreateResponse { entity_id }).encode(),
            true,
        )
        .await;

        // Builder follows up with GET to populate the typed handle.
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::EntityGetReq.as_u16());
        let view = sample_view(entity_id, "Alice", attrs_blob);
        common::write_frame(
            &mut socket,
            Opcode::EntityGetResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::EntityGet(EntityGetResponse { entity: view }).encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let handle = client
        .entity::<Person>()
        .create()
        .canonical_name("Alice")
        .alias("A.")
        .with_email("alice@example.com")
        .with_role("Engineer")
        .send()
        .await
        .expect("create");

    assert_eq!(handle.id, EntityId::from(entity_id));
    assert_eq!(handle.canonical_name, "Alice");
    assert_eq!(handle.attributes.email.as_deref(), Some("alice@example.com"));
    assert_eq!(handle.attributes.role.as_deref(), Some("Engineer"));
    assert_eq!(handle.attributes.team, None);
}

// ---------------------------------------------------------------------------
// GET
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_returns_typed_handle() {
    let entity_id = [9u8; 16];
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::EntityGetReq.as_u16());
        let view = sample_view(entity_id, "Bob", Vec::new());
        common::write_frame(
            &mut socket,
            Opcode::EntityGetResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::EntityGet(EntityGetResponse { entity: view }).encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let opt = client
        .entity::<Person>()
        .get(EntityId::from(entity_id))
        .await
        .expect("get");
    let handle = opt.expect("Some handle");
    assert_eq!(handle.canonical_name, "Bob");
}

#[tokio::test]
async fn get_translates_not_found_to_none() {
    let entity_id = [9u8; 16];
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::EntityGetReq.as_u16());
        common::write_frame(
            &mut socket,
            Opcode::Error.as_u16(),
            frame.header.stream_id_u32(),
            server_error("entity EntityId(...) not found").encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let opt = client
        .entity::<Person>()
        .get(EntityId::from(entity_id))
        .await
        .expect("get");
    assert!(opt.is_none());
}

// ---------------------------------------------------------------------------
// RENAME shortcut
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rename_returns_post_rename_handle() {
    let entity_id = [3u8; 16];
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::EntityRenameReq.as_u16());
        let body = RequestBody::decode(Opcode::EntityRenameReq, &frame.payload).unwrap();
        match body {
            RequestBody::EntityRename(r) => {
                assert_eq!(r.entity_id, entity_id);
                assert_eq!(r.new_canonical_name, "Alice Cooper");
                assert!(r.move_to_alias);
            }
            _ => panic!(),
        }
        let mut view = sample_view(entity_id, "Alice Cooper", Vec::new());
        view.aliases = vec!["Alice".into()];
        common::write_frame(
            &mut socket,
            Opcode::EntityRenameResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::EntityRename(EntityRenameResponse { entity: view }).encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let handle = client
        .entity::<Person>()
        .rename(EntityId::from(entity_id), "Alice Cooper")
        .await
        .expect("rename");
    assert_eq!(handle.canonical_name, "Alice Cooper");
    assert!(handle.aliases.contains(&"Alice".into()));
}

// ---------------------------------------------------------------------------
// UPDATE
// ---------------------------------------------------------------------------

#[tokio::test]
async fn update_inherits_unset_canonical_and_aliases() {
    // Verifies the builder's "fetch-current-for-unset" path: caller
    // provides only new attributes; canonical_name + aliases come from
    // the GET that the builder issues first.
    //
    // Attribute *patching* (vs full-replace) is documented as explicit
    // — callers either build a complete PersonAttributes via
    // .attributes(full) or compose .with_*() into a known starting
    // point. Full attribute-merge semantics are a phase-19 follow-up
    // when the derive macro can introspect schema-declared fields.
    let entity_id = [5u8; 16];

    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        // First: GET to capture current canonical_name + aliases.
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::EntityGetReq.as_u16());
        let mut view = sample_view(entity_id, "Charlie", Vec::new());
        view.aliases = vec!["C.".into()];
        common::write_frame(
            &mut socket,
            Opcode::EntityGetResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::EntityGet(EntityGetResponse { entity: view }).encode(),
            true,
        )
        .await;

        // Second: UPDATE — canonical_name + aliases inherited, new
        // attributes block applied verbatim.
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::EntityUpdateReq.as_u16());
        let body = RequestBody::decode(Opcode::EntityUpdateReq, &frame.payload).unwrap();
        match body {
            RequestBody::EntityUpdate(r) => {
                assert_eq!(r.canonical_name, "Charlie"); // preserved
                assert_eq!(r.aliases, vec!["C.".to_string()]); // preserved
                let attrs = Person::decode_attributes(&r.attributes_blob);
                assert_eq!(attrs.team.as_deref(), Some("Platform"));
                assert_eq!(attrs.email.as_deref(), Some("a@b"));
            }
            _ => panic!(),
        }
        let mut view = sample_view(
            entity_id,
            "Charlie",
            Person::encode_attributes(&PersonAttributes {
                email: Some("a@b".into()),
                role: None,
                team: Some("Platform".into()),
                timezone: None,
            }),
        );
        view.aliases = vec!["C.".into()];
        common::write_frame(
            &mut socket,
            Opcode::EntityUpdateResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::EntityUpdate(EntityUpdateResponse { entity: view }).encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    // Caller provides full attribute snapshot via .attributes(...). The
    // builder still fetches GET to inherit canonical_name + aliases.
    let handle = client
        .entity::<Person>()
        .update(EntityId::from(entity_id))
        .attributes(PersonAttributes {
            email: Some("a@b".into()),
            role: None,
            team: Some("Platform".into()),
            timezone: None,
        })
        .send()
        .await
        .expect("update");
    assert_eq!(handle.canonical_name, "Charlie");
    assert_eq!(handle.attributes.team.as_deref(), Some("Platform"));
    assert_eq!(handle.attributes.email.as_deref(), Some("a@b"));
}

// ---------------------------------------------------------------------------
// MERGE / UNMERGE
// ---------------------------------------------------------------------------

#[tokio::test]
async fn merge_returns_audit_id_and_grace_window() {
    let survivor = [1u8; 16];
    let merged = [2u8; 16];
    let audit_id = [9u8; 16];

    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::EntityMergeReq.as_u16());
        let body = RequestBody::decode(Opcode::EntityMergeReq, &frame.payload).unwrap();
        match body {
            RequestBody::EntityMerge(r) => {
                assert_eq!(r.survivor, survivor);
                assert_eq!(r.merged, merged);
                assert!((r.confidence - 0.93).abs() < 1e-6);
                assert_eq!(r.reason, "dup");
            }
            _ => panic!(),
        }
        common::write_frame(
            &mut socket,
            Opcode::EntityMergeResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::EntityMerge(EntityMergeResponse {
                audit_id,
                grace_period_seconds: 7 * 24 * 60 * 60,
            })
            .encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let outcome = client
        .entity::<Person>()
        .merge(EntityId::from(survivor), EntityId::from(merged))
        .confidence(0.93)
        .reason("dup")
        .send()
        .await
        .expect("merge");
    assert_eq!(outcome.audit_id, audit_id);
    assert_eq!(outcome.grace_period_seconds, 7 * 24 * 60 * 60);
}

#[tokio::test]
async fn unmerge_returns_restored_id() {
    let merged = [4u8; 16];
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::EntityUnmergeReq.as_u16());
        common::write_frame(
            &mut socket,
            Opcode::EntityUnmergeResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::EntityUnmerge(EntityUnmergeResponse {
                restored_entity_id: merged,
            })
            .encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let restored = client
        .entity::<Person>()
        .unmerge(EntityId::from(merged))
        .await
        .expect("unmerge");
    assert_eq!(restored, EntityId::from(merged));
}

// ---------------------------------------------------------------------------
// RESOLVE
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resolve_resolved_outcome() {
    let entity_id = [11u8; 16];
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::EntityResolveReq.as_u16());
        let body = RequestBody::decode(Opcode::EntityResolveReq, &frame.payload).unwrap();
        match body {
            RequestBody::EntityResolve(r) => {
                assert_eq!(r.candidate_name, "Alice");
                assert_eq!(r.entity_type_hint, PERSON_TYPE_ID);
            }
            _ => panic!(),
        }
        common::write_frame(
            &mut socket,
            Opcode::EntityResolveResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::EntityResolve(EntityResolveResponse {
                outcome: ResolutionOutcomeWire::Resolved,
                tier: 1,
                confidence: 1.0,
                resolved_entity: entity_id,
                candidate_ids: vec![],
                audit_id: [0u8; 16],
            })
            .encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let outcome = client
        .entity::<Person>()
        .resolve("Alice")
        .send()
        .await
        .expect("resolve");
    match outcome {
        ResolutionOutcome::Resolved { entity_id: id, tier, confidence, .. } => {
            assert_eq!(id, EntityId::from(entity_id));
            assert_eq!(tier, 1);
            assert!((confidence - 1.0).abs() < 1e-6);
        }
        other => panic!("expected Resolved, got {other:?}"),
    }
}

#[tokio::test]
async fn resolve_ambiguous_outcome() {
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        common::write_frame(
            &mut socket,
            Opcode::EntityResolveResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::EntityResolve(EntityResolveResponse {
                outcome: ResolutionOutcomeWire::Ambiguous,
                tier: 2,
                confidence: 0.86,
                resolved_entity: [0u8; 16],
                candidate_ids: vec![[1u8; 16], [2u8; 16]],
                audit_id: [3u8; 16],
            })
            .encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let outcome = client
        .entity::<Person>()
        .resolve("Priya")
        .send()
        .await
        .expect("resolve");
    assert!(outcome.is_ambiguous());
    match outcome {
        ResolutionOutcome::Ambiguous { candidates, .. } => {
            assert_eq!(candidates.len(), 2);
        }
        _ => panic!(),
    }
}

#[tokio::test]
async fn resolve_not_found_outcome() {
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        common::write_frame(
            &mut socket,
            Opcode::EntityResolveResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::EntityResolve(EntityResolveResponse {
                outcome: ResolutionOutcomeWire::NotFound,
                tier: 0,
                confidence: 0.0,
                resolved_entity: [0u8; 16],
                candidate_ids: vec![],
                audit_id: [0u8; 16],
            })
            .encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let outcome = client
        .entity::<Person>()
        .resolve("Unknown")
        .send()
        .await
        .expect("resolve");
    assert!(matches!(outcome, ResolutionOutcome::NotFound));
}

// ---------------------------------------------------------------------------
// LIST
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_returns_typed_handles() {
    let alice = [1u8; 16];
    let bob = [2u8; 16];

    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::EntityListReq.as_u16());
        let body = RequestBody::decode(Opcode::EntityListReq, &frame.payload).unwrap();
        match body {
            RequestBody::EntityList(r) => {
                assert_eq!(r.entity_type_id, PERSON_TYPE_ID);
                assert_eq!(r.limit, 50);
                assert!(r.cursor.is_empty());
            }
            _ => panic!(),
        }
        let frame_body = EntityListResponseFrame {
            items: vec![
                EntityListItem {
                    entity: sample_view(alice, "Alice", Vec::new()),
                },
                EntityListItem {
                    entity: sample_view(bob, "Bob", Vec::new()),
                },
            ],
            next_cursor: Vec::new(),
            cumulative_count: 2,
            is_final: true,
        };
        common::write_frame(
            &mut socket,
            Opcode::EntityListResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::EntityList(frame_body).encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let handles = client
        .entity::<Person>()
        .list()
        .limit(50)
        .fetch()
        .await
        .expect("list");
    assert_eq!(handles.len(), 2);
    assert_eq!(handles[0].canonical_name, "Alice");
    assert_eq!(handles[1].canonical_name, "Bob");
}

// ---------------------------------------------------------------------------
// TOMBSTONE
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tombstone_returns_timestamp() {
    let entity_id = [7u8; 16];
    let now = 1_700_000_000_000_000_000u64;
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(
            frame.header.opcode_u16(),
            Opcode::EntityTombstoneReq.as_u16()
        );
        common::write_frame(
            &mut socket,
            Opcode::EntityTombstoneResp.as_u16(),
            frame.header.stream_id_u32(),
            ResponseBody::EntityTombstone(EntityTombstoneResponse {
                tombstoned_at_unix_nanos: now,
            })
            .encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let t = client
        .entity::<Person>()
        .tombstone(EntityId::from(entity_id), "obsolete")
        .await
        .expect("tombstone");
    assert_eq!(t, now);
}

// ---------------------------------------------------------------------------
// Error path: merge conflict surfaces through ClientErrorEntityExt
// ---------------------------------------------------------------------------

#[tokio::test]
async fn merge_conflict_categorised_via_extension_trait() {
    let (addr, _server) = common::spawn_mock_server(move |mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        common::write_frame(
            &mut socket,
            Opcode::Error.as_u16(),
            frame.header.stream_id_u32(),
            server_error("entity EntityId(...) already merged into EntityId(...)").encode(),
            true,
        )
        .await;
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let err = client
        .entity::<Person>()
        .merge(EntityId::from([1u8; 16]), EntityId::from([2u8; 16]))
        .confidence(0.9)
        .reason("test")
        .send()
        .await
        .expect_err("merge should fail");
    assert_eq!(err.entity_error(), Some(EntityErrorKind::MergeConflict));
    assert!(err.is_entity_merge_conflict());
}
