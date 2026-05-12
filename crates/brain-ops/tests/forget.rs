//! Integration tests for `handle_forget` (sub-task 7.7).
//!
//! Drives the full pipeline:
//!   dispatcher → handle_forget → plan_forget_inner → execute_forget
//!   → RealWriterHandle → wire ForgetResponse
//!
//! Pre-populates the index via ENCODE through the dispatcher so we
//! have real memories to forget.

use std::sync::Arc;

use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::MetadataDb;
use brain_ops::{dispatch, ErrorCode, OpError, OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::request::{
    EncodeRequest, ForgetMode, ForgetRequest, MemoryKindWire, RequestBody,
};
use brain_protocol::response::{EncodeResponse, ForgetResponse, ResponseBody};
use parking_lot::Mutex;

// ---------------------------------------------------------------------------
// Mock dispatcher.
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

// ---------------------------------------------------------------------------
// Fixture.
// ---------------------------------------------------------------------------

struct Fixture {
    ctx: OpsContext,
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
        metadata,
        writer as Arc<dyn WriterHandle>,
    );

    Fixture {
        ctx: OpsContext::new(executor),
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

fn forget_req(memory_id: u128, request_id: [u8; 16]) -> ForgetRequest {
    ForgetRequest {
        memory_id,
        mode: ForgetMode::Soft,
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

fn unwrap_forget_resp(body: ResponseBody) -> ForgetResponse {
    match body {
        ResponseBody::Forget(r) => r,
        other => panic!("expected ResponseBody::Forget, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 1. Fresh forget.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forget_full_pipeline_tombstones_memory() {
    let fix = build_fixture();
    let memory_id = encode(&fix, [1; 16], "forgetme").await;

    let resp = unwrap_forget_resp(
        dispatch(
            RequestBody::Forget(forget_req(memory_id, [2; 16])),
            &fix.ctx,
        )
        .await
        .unwrap(),
    );
    assert_eq!(resp.memory_id, memory_id);
    assert!(!resp.was_already_forgotten);
    assert_eq!(resp.edges_removed, 0);
}

// ---------------------------------------------------------------------------
// 2. Second forget with a fresh RequestId surfaces AlreadyTombstoned.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forget_already_tombstoned_returns_flag() {
    let fix = build_fixture();
    let memory_id = encode(&fix, [10; 16], "twice").await;

    let first = unwrap_forget_resp(
        dispatch(
            RequestBody::Forget(forget_req(memory_id, [11; 16])),
            &fix.ctx,
        )
        .await
        .unwrap(),
    );
    assert!(!first.was_already_forgotten);

    // Different RequestId targeting the same memory.
    let second = unwrap_forget_resp(
        dispatch(
            RequestBody::Forget(forget_req(memory_id, [12; 16])),
            &fix.ctx,
        )
        .await
        .unwrap(),
    );
    assert!(second.was_already_forgotten);
    assert_eq!(second.memory_id, memory_id);
}

// ---------------------------------------------------------------------------
// 3. MemoryNotFound is collapsed into the no-op flag per spec §09/06 §14.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forget_memory_not_found_returns_flag_not_error() {
    let fix = build_fixture();
    let phantom: u128 = 0xDEAD_BEEF_DEAD_BEEF_0000_0000_0000_0000;

    let resp = unwrap_forget_resp(
        dispatch(RequestBody::Forget(forget_req(phantom, [20; 16])), &fix.ctx)
            .await
            .unwrap(),
    );
    assert_eq!(resp.memory_id, phantom);
    assert!(resp.was_already_forgotten);
    assert_eq!(resp.edges_removed, 0);
}

// ---------------------------------------------------------------------------
// 4. Idempotent replay is transparent on the wire.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forget_idempotent_replay_returns_cached_response() {
    let fix = build_fixture();
    let memory_id = encode(&fix, [30; 16], "replay").await;

    let req = forget_req(memory_id, [31; 16]);
    let first = unwrap_forget_resp(dispatch(RequestBody::Forget(req), &fix.ctx).await.unwrap());
    let second = unwrap_forget_resp(dispatch(RequestBody::Forget(req), &fix.ctx).await.unwrap());

    // Replay returns the cached outcome: same was_already_forgotten,
    // same memory_id. (The writer's replay flag is internal — the
    // wire shape can't distinguish a replay from a fresh result.)
    assert_eq!(first.was_already_forgotten, second.was_already_forgotten);
    assert_eq!(first.memory_id, second.memory_id);
}

// ---------------------------------------------------------------------------
// 5. RequestId reuse with different memory_id → Conflict.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forget_idempotency_conflict_returns_error() {
    let fix = build_fixture();
    let a = encode(&fix, [40; 16], "first-target").await;
    let b = encode(&fix, [41; 16], "second-target").await;

    let _ok = dispatch(RequestBody::Forget(forget_req(a, [42; 16])), &fix.ctx)
        .await
        .unwrap();
    let err = dispatch(RequestBody::Forget(forget_req(b, [42; 16])), &fix.ctx)
        .await
        .unwrap_err();
    assert_eq!(err.error_code(), ErrorCode::Conflict);
    assert!(
        matches!(err, OpError::ExecError(_)),
        "Conflict surfaces from the writer through ExecError, got {err:?}"
    );
}
