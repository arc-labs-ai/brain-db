//! Integration tests for `handle_link` / `handle_unlink` (sub-task 7.8).
//!
//! Drives the full pipeline:
//!   dispatcher → handle_link/unlink → RealWriterHandle →
//!   redb edges_out + edges_in + memory edge-count denorms.
//!
//! Also pins the post-7.8 fix to the **encode flow**: inline
//! encode-edges now actually land in `edges_out` / `edges_in`. Prior
//! versions of the writer reported `EdgeOutcome::Inserted` but never
//! opened the edge tables — this file verifies the new behaviour.

use std::sync::Arc;

use brain_core::{EdgeKind as CoreEdgeKind, MemoryId};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::edge::EDGES_OUT_TABLE;
use brain_metadata::tables::memory::MEMORIES_TABLE;
use brain_metadata::MetadataDb;
use brain_ops::{dispatch, ErrorCode, OpError, OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::request::{
    EdgeKindWire, EdgeRequest, EncodeRequest, LinkRequest, MemoryKindWire, RequestBody,
    UnlinkRequest,
};
use brain_protocol::response::{EncodeResponse, LinkResponse, ResponseBody, UnlinkResponse};
use parking_lot::Mutex;

// ---------------------------------------------------------------------------
// Fixture.
// ---------------------------------------------------------------------------

struct MockDispatcher;

impl Dispatcher for MockDispatcher {
    fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        let mut v = [0.0f32; VECTOR_DIM];
        for (i, byte) in text.as_bytes().iter().enumerate() {
            v[i % VECTOR_DIM] += f32::from(*byte) / 255.0;
        }
        Ok(v)
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
    fn fingerprint(&self) -> [u8; 16] {
        [0xAB; 16]
    }
}

struct Fixture {
    ctx: OpsContext,
    metadata: SharedMetadataDb,
    _tempdir: tempfile::TempDir,
}

fn build_fixture() -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(Mutex::new(MetadataDb::open(&db_path).unwrap()));

    let (shared, hnsw_writer) = SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
    let executor = ExecutorContext::new(
        Arc::new(MockDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata.clone(),
        writer as Arc<dyn WriterHandle>,
    );
    Fixture {
        ctx: OpsContext::new(executor),
        metadata,
        _tempdir: tempdir,
    }
}

fn encode_req(request_id: [u8; 16], text: &str) -> EncodeRequest {
    EncodeRequest {
        text: text.into(),
        context_id: 42,
        kind: MemoryKindWire::Episodic,
        salience_hint: 0.5,
        edges: vec![],
        request_id,
        txn_id: None,
        deduplicate: false,
    }
}

fn link_req(
    source: u128,
    target: u128,
    kind: EdgeKindWire,
    weight: f32,
    request_id: [u8; 16],
) -> LinkRequest {
    LinkRequest {
        source,
        target,
        kind,
        weight,
        request_id,
        txn_id: None,
    }
}

fn unlink_req(
    source: u128,
    target: u128,
    kind: EdgeKindWire,
    request_id: [u8; 16],
) -> UnlinkRequest {
    UnlinkRequest {
        source,
        target,
        kind,
        request_id,
        txn_id: None,
    }
}

async fn encode(fix: &Fixture, request_id: [u8; 16], text: &str) -> u128 {
    let req = encode_req(request_id, text);
    match dispatch(RequestBody::Encode(req), &fix.ctx).await.unwrap() {
        ResponseBody::Encode(EncodeResponse { memory_id, .. }) => memory_id,
        other => panic!("expected Encode response, got {other:?}"),
    }
}

fn unwrap_link(body: ResponseBody) -> LinkResponse {
    match body {
        ResponseBody::Link(r) => r,
        other => panic!("expected ResponseBody::Link, got {other:?}"),
    }
}

fn unwrap_unlink(body: ResponseBody) -> UnlinkResponse {
    match body {
        ResponseBody::Unlink(r) => r,
        other => panic!("expected ResponseBody::Unlink, got {other:?}"),
    }
}

fn edge_exists(fix: &Fixture, source: u128, kind: CoreEdgeKind, target: u128) -> bool {
    let db = fix.metadata.lock();
    let rtxn = db.read_txn().unwrap();
    let table = rtxn.open_table(EDGES_OUT_TABLE).unwrap();
    let s = MemoryId::from(source).to_be_bytes();
    let t = MemoryId::from(target).to_be_bytes();
    table.get(&(s, kind as u8, t)).unwrap().is_some()
}

fn edge_counts(fix: &Fixture, memory_id: u128) -> (u32, u32) {
    let db = fix.metadata.lock();
    let rtxn = db.read_txn().unwrap();
    let table = rtxn.open_table(MEMORIES_TABLE).unwrap();
    let access = table
        .get(MemoryId::from(memory_id).to_be_bytes())
        .unwrap()
        .unwrap();
    let meta = access.value();
    (meta.edges_out_count, meta.edges_in_count)
}

// ===========================================================================
// LINK
// ===========================================================================

#[tokio::test]
async fn link_inserts_edge_and_bumps_counts() {
    let fix = build_fixture();
    let a = encode(&fix, [1; 16], "alpha").await;
    let b = encode(&fix, [2; 16], "beta").await;

    let resp = unwrap_link(
        dispatch(
            RequestBody::Link(link_req(a, b, EdgeKindWire::Caused, 0.7, [10; 16])),
            &fix.ctx,
        )
        .await
        .unwrap(),
    );
    assert_eq!(resp.source, a);
    assert_eq!(resp.target, b);
    assert!(!resp.already_existed);
    assert!((resp.weight - 0.7).abs() < 1e-6);

    // Edge actually present.
    assert!(edge_exists(&fix, a, CoreEdgeKind::Caused, b));

    // Counts bumped on both endpoints.
    let (a_out, _a_in) = edge_counts(&fix, a);
    let (_b_out, b_in) = edge_counts(&fix, b);
    assert_eq!(a_out, 1);
    assert_eq!(b_in, 1);
}

#[tokio::test]
async fn link_replays_same_request_id() {
    let fix = build_fixture();
    let a = encode(&fix, [1; 16], "alpha").await;
    let b = encode(&fix, [2; 16], "beta").await;

    let req = link_req(a, b, EdgeKindWire::Caused, 0.5, [10; 16]);
    let first = unwrap_link(dispatch(RequestBody::Link(req), &fix.ctx).await.unwrap());
    let second = unwrap_link(dispatch(RequestBody::Link(req), &fix.ctx).await.unwrap());
    assert_eq!(first.source, second.source);
    assert_eq!(first.target, second.target);
    assert!((first.weight - second.weight).abs() < 1e-6);
    // The denormalized count must not double-bump.
    assert_eq!(edge_counts(&fix, a).0, 1);
}

#[tokio::test]
async fn link_overwrite_with_new_request_id_marks_already_existed() {
    let fix = build_fixture();
    let a = encode(&fix, [1; 16], "alpha").await;
    let b = encode(&fix, [2; 16], "beta").await;

    let r1 = unwrap_link(
        dispatch(
            RequestBody::Link(link_req(a, b, EdgeKindWire::Caused, 0.5, [10; 16])),
            &fix.ctx,
        )
        .await
        .unwrap(),
    );
    assert!(!r1.already_existed);

    // Second LINK with a NEW request_id overwrites weight; already_existed=true.
    let r2 = unwrap_link(
        dispatch(
            RequestBody::Link(link_req(a, b, EdgeKindWire::Caused, 0.9, [11; 16])),
            &fix.ctx,
        )
        .await
        .unwrap(),
    );
    assert!(r2.already_existed);
    assert!((r2.weight - 0.9).abs() < 1e-6);
    // No double-count.
    assert_eq!(edge_counts(&fix, a).0, 1);
}

#[tokio::test]
async fn link_conflict_on_request_id_reuse_with_different_target() {
    let fix = build_fixture();
    let a = encode(&fix, [1; 16], "alpha").await;
    let b = encode(&fix, [2; 16], "beta").await;
    let c = encode(&fix, [3; 16], "gamma").await;

    let _ = dispatch(
        RequestBody::Link(link_req(a, b, EdgeKindWire::Caused, 0.5, [10; 16])),
        &fix.ctx,
    )
    .await
    .unwrap();

    let err = dispatch(
        RequestBody::Link(link_req(a, c, EdgeKindWire::Caused, 0.5, [10; 16])),
        &fix.ctx,
    )
    .await
    .unwrap_err();
    assert_eq!(err.error_code(), ErrorCode::Conflict);
}

#[tokio::test]
async fn link_missing_target_returns_not_found() {
    let fix = build_fixture();
    let a = encode(&fix, [1; 16], "alpha").await;
    let phantom: u128 = 0xDEAD_BEEF_DEAD_BEEF_0000_0000_0000_0000;

    let err = dispatch(
        RequestBody::Link(link_req(a, phantom, EdgeKindWire::Caused, 0.5, [10; 16])),
        &fix.ctx,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, OpError::NotFound { .. }));
    assert_eq!(err.error_code(), ErrorCode::NotFound);
}

#[tokio::test]
async fn link_invalid_weight_returns_invalid_request() {
    let fix = build_fixture();
    let a = encode(&fix, [1; 16], "alpha").await;
    let b = encode(&fix, [2; 16], "beta").await;

    let err = dispatch(
        RequestBody::Link(link_req(a, b, EdgeKindWire::Caused, 1.5, [10; 16])),
        &fix.ctx,
    )
    .await
    .unwrap_err();
    assert_eq!(err.error_code(), ErrorCode::InvalidRequest);
}

#[tokio::test]
async fn link_contradicts_allows_negative_weight() {
    let fix = build_fixture();
    let a = encode(&fix, [1; 16], "alpha").await;
    let b = encode(&fix, [2; 16], "beta").await;

    let resp = unwrap_link(
        dispatch(
            RequestBody::Link(link_req(a, b, EdgeKindWire::Contradicts, -0.7, [10; 16])),
            &fix.ctx,
        )
        .await
        .unwrap(),
    );
    assert!((resp.weight + 0.7).abs() < 1e-6);
}

// ===========================================================================
// UNLINK
// ===========================================================================

#[tokio::test]
async fn unlink_removes_existing_edge_and_decrements_counts() {
    let fix = build_fixture();
    let a = encode(&fix, [1; 16], "alpha").await;
    let b = encode(&fix, [2; 16], "beta").await;
    let _ = dispatch(
        RequestBody::Link(link_req(a, b, EdgeKindWire::Caused, 0.5, [10; 16])),
        &fix.ctx,
    )
    .await
    .unwrap();
    assert_eq!(edge_counts(&fix, a).0, 1);

    let resp = unwrap_unlink(
        dispatch(
            RequestBody::Unlink(unlink_req(a, b, EdgeKindWire::Caused, [20; 16])),
            &fix.ctx,
        )
        .await
        .unwrap(),
    );
    assert!(resp.removed);
    assert!(!edge_exists(&fix, a, CoreEdgeKind::Caused, b));
    let (a_out, _) = edge_counts(&fix, a);
    let (_, b_in) = edge_counts(&fix, b);
    assert_eq!(a_out, 0);
    assert_eq!(b_in, 0);
}

#[tokio::test]
async fn unlink_non_existent_edge_returns_false_not_error() {
    let fix = build_fixture();
    let a = encode(&fix, [1; 16], "alpha").await;
    let b = encode(&fix, [2; 16], "beta").await;

    let resp = unwrap_unlink(
        dispatch(
            RequestBody::Unlink(unlink_req(a, b, EdgeKindWire::Caused, [20; 16])),
            &fix.ctx,
        )
        .await
        .unwrap(),
    );
    assert!(!resp.removed);
}

#[tokio::test]
async fn unlink_idempotent_replay() {
    let fix = build_fixture();
    let a = encode(&fix, [1; 16], "alpha").await;
    let b = encode(&fix, [2; 16], "beta").await;
    let _ = dispatch(
        RequestBody::Link(link_req(a, b, EdgeKindWire::Caused, 0.5, [10; 16])),
        &fix.ctx,
    )
    .await
    .unwrap();

    let req = unlink_req(a, b, EdgeKindWire::Caused, [20; 16]);
    let first = unwrap_unlink(dispatch(RequestBody::Unlink(req), &fix.ctx).await.unwrap());
    let second = unwrap_unlink(dispatch(RequestBody::Unlink(req), &fix.ctx).await.unwrap());
    assert!(first.removed);
    // Replay must return the same outcome; counts must not double-decrement.
    assert_eq!(first.removed, second.removed);
    assert_eq!(edge_counts(&fix, a).0, 0);
}

#[tokio::test]
async fn unlink_conflict_on_request_id_reuse_with_different_target() {
    let fix = build_fixture();
    let a = encode(&fix, [1; 16], "alpha").await;
    let b = encode(&fix, [2; 16], "beta").await;
    let c = encode(&fix, [3; 16], "gamma").await;

    let _ = dispatch(
        RequestBody::Unlink(unlink_req(a, b, EdgeKindWire::Caused, [20; 16])),
        &fix.ctx,
    )
    .await
    .unwrap();

    let err = dispatch(
        RequestBody::Unlink(unlink_req(a, c, EdgeKindWire::Caused, [20; 16])),
        &fix.ctx,
    )
    .await
    .unwrap_err();
    assert_eq!(err.error_code(), ErrorCode::Conflict);
}

// ===========================================================================
// Encode-inline edge insertion (the bug fix)
// ===========================================================================

#[tokio::test]
async fn encode_inline_edges_actually_land_in_redb() {
    let fix = build_fixture();

    // First memory becomes a target.
    let target = encode(&fix, [1; 16], "target").await;

    // Second memory carries an inline edge to the target.
    let mut req = encode_req([2; 16], "linker");
    req.edges = vec![EdgeRequest {
        target,
        kind: EdgeKindWire::References,
        weight: 0.5,
    }];
    let linker = match dispatch(RequestBody::Encode(req), &fix.ctx).await.unwrap() {
        ResponseBody::Encode(r) => r.memory_id,
        other => panic!("got {other:?}"),
    };

    // The edge must actually exist in redb (pre-7.8 bug: it didn't).
    assert!(edge_exists(&fix, linker, CoreEdgeKind::References, target));

    // Edge counts must be set on BOTH endpoints.
    let (linker_out, _) = edge_counts(&fix, linker);
    let (_, target_in) = edge_counts(&fix, target);
    assert_eq!(linker_out, 1, "source memory tracks outgoing edges");
    assert_eq!(target_in, 1, "target memory tracks incoming edges");
}
