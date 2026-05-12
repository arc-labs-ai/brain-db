//! Integration tests for `handle_encode` (sub-task 7.3).
//!
//! Drives the full pipeline:
//!   dispatcher → handle_encode → plan_encode_inner → execute_encode
//!   → RealWriterHandle → metadata + HNSW
//!
//! Embedder is a deterministic mock for offline runs. One test
//! exercises the real BGE dispatcher when `BRAIN_EMBED_MODEL_DIR` is
//! set.

use std::sync::Arc;

use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::MetadataDb;
use brain_ops::{dispatch, OpError, OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::request::{
    EdgeKindWire, EdgeRequest, EncodeRequest, MemoryKindWire, RequestBody,
};
use brain_protocol::response::{EncodeResponse, ResponseBody};
use parking_lot::Mutex;

// ---------------------------------------------------------------------------
// Mock dispatcher: deterministic per-text vector + stable fingerprint.
// ---------------------------------------------------------------------------

struct MockDispatcher;

impl Dispatcher for MockDispatcher {
    fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        let mut v = [0.0f32; VECTOR_DIM];
        // Hash text bytes into a few slots so distinct texts yield
        // distinct vectors. Norm doesn't matter for these tests.
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
    build_fixture_with_embedder(Arc::new(MockDispatcher) as Arc<dyn Dispatcher>)
}

fn build_fixture_with_embedder(embedder: Arc<dyn Dispatcher>) -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(Mutex::new(MetadataDb::open(&db_path).unwrap()));

    let (shared, hnsw_writer) = SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));

    let executor =
        ExecutorContext::new(embedder, shared, metadata, writer as Arc<dyn WriterHandle>);

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

fn unwrap_encode_resp(body: ResponseBody) -> EncodeResponse {
    match body {
        ResponseBody::Encode(r) => r,
        other => panic!("expected ResponseBody::Encode, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 1. Full pipeline.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn encode_full_pipeline_returns_memory_id() {
    let fix = build_fixture();
    let req = encode_req([1; 16], "hello world");
    let resp = dispatch(RequestBody::Encode(req), &fix.ctx).await.unwrap();
    let enc = unwrap_encode_resp(resp);

    assert_ne!(enc.memory_id, 0, "memory_id must be non-zero");
    assert!(!enc.was_deduplicated);
    assert_eq!(enc.salience, 0.5, "salience echoes the request hint");
    assert_eq!(enc.auto_edges_added, 0);
}

// ---------------------------------------------------------------------------
// 2. Replay sets was_deduplicated.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn encode_replay_sets_was_deduplicated() {
    let fix = build_fixture();
    let req = encode_req([2; 16], "replay me");

    let first = unwrap_encode_resp(
        dispatch(RequestBody::Encode(req.clone()), &fix.ctx)
            .await
            .unwrap(),
    );
    assert!(!first.was_deduplicated);

    let second = unwrap_encode_resp(dispatch(RequestBody::Encode(req), &fix.ctx).await.unwrap());
    assert!(second.was_deduplicated);
    assert_eq!(first.memory_id, second.memory_id);
}

// ---------------------------------------------------------------------------
// 3. Conflict path.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn encode_conflict_returns_conflict_error_code() {
    use brain_ops::ErrorCode;

    let fix = build_fixture();
    let first = encode_req([3; 16], "original");
    let conflicting = encode_req([3; 16], "DIFFERENT");

    let _ok = dispatch(RequestBody::Encode(first), &fix.ctx)
        .await
        .unwrap();
    let err = dispatch(RequestBody::Encode(conflicting), &fix.ctx)
        .await
        .unwrap_err();
    assert_eq!(err.error_code(), ErrorCode::Conflict);
}

// ---------------------------------------------------------------------------
// 4. Consolidated kind rejected at planning.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn encode_consolidated_kind_rejected() {
    let fix = build_fixture();
    let mut req = encode_req([4; 16], "no consolidated");
    req.kind = MemoryKindWire::Consolidated;

    let err = dispatch(RequestBody::Encode(req), &fix.ctx)
        .await
        .unwrap_err();
    assert!(
        matches!(err, OpError::PlanError(_)),
        "Consolidated rejection comes from the planner, got {err:?}"
    );
    assert_eq!(
        err.error_code(),
        brain_ops::ErrorCode::InvalidRequest,
        "Consolidated kind must map to InvalidRequest"
    );
}

// ---------------------------------------------------------------------------
// 5. Edges: insert count is reflected in auto_edges_added.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn encode_auto_edges_added_counts_inserted_only() {
    let fix = build_fixture();

    // First, write a target memory we can link to.
    let target = unwrap_encode_resp(
        dispatch(RequestBody::Encode(encode_req([5; 16], "target")), &fix.ctx)
            .await
            .unwrap(),
    );
    assert_ne!(target.memory_id, 0);

    // Now encode with two edges: one valid, one to a non-existent id.
    let mut req = encode_req([6; 16], "linker");
    req.edges = vec![
        EdgeRequest {
            target: target.memory_id,
            kind: EdgeKindWire::References,
            weight: 0.5,
        },
        EdgeRequest {
            target: 0xDEAD_BEEF_u128,
            kind: EdgeKindWire::References,
            weight: 0.5,
        },
    ];
    let resp = unwrap_encode_resp(dispatch(RequestBody::Encode(req), &fix.ctx).await.unwrap());
    assert_eq!(
        resp.auto_edges_added, 1,
        "only the edge to the live target counts"
    );
}

// ---------------------------------------------------------------------------
// 6. Real-embedder gated test. Skips when env var is unset.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn encode_with_real_embedder_end_to_end() {
    let Ok(model_dir) = std::env::var("BRAIN_EMBED_MODEL_DIR") else {
        eprintln!("BRAIN_EMBED_MODEL_DIR unset; skipping BGE end-to-end test");
        return;
    };

    let model_dir = std::path::PathBuf::from(model_dir);
    let handle = brain_embed::ModelHandle::load(&brain_embed::EmbedderConfig::new(model_dir))
        .expect("BGE model loads");
    let dispatcher = brain_embed::CpuDispatcher::new(handle);
    let fix = build_fixture_with_embedder(Arc::new(dispatcher) as Arc<dyn Dispatcher>);

    let req = encode_req([0x7E; 16], "the real embedder is plumbed end-to-end");
    let resp = unwrap_encode_resp(dispatch(RequestBody::Encode(req), &fix.ctx).await.unwrap());
    assert_ne!(resp.memory_id, 0);
    assert!(!resp.was_deduplicated);
}
