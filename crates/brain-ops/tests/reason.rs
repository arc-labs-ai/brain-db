//! Integration tests for `handle_reason` (sub-task 7.6).
//!
//! Drives:
//!   dispatcher → handle_reason → plan_reason_inner → execute_reason
//!   → wire ReasonResponseFrame
//!
//! Edges are inserted via brain-metadata::tables::edge::link directly
//! (LINK handler ships in sub-task 7.8).

use std::sync::Arc;

use brain_core::{AgentId, ContextId, EdgeKind, MemoryId, MemoryKind};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::edge::{link, EdgeData, EDGES_IN_TABLE, EDGES_OUT_TABLE};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::MetadataDb;
use brain_ops::{dispatch, ErrorCode, OpError, OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::request::{ObservationInput, ReasonRequest, RequestBody};
use brain_protocol::response::{
    InferenceKind, ReasonResponseFrame, ReasonStatus as WireReasonStatus, ResponseBody,
};
use parking_lot::Mutex;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Dispatcher.
// ---------------------------------------------------------------------------

struct NopDispatcher;

impl Dispatcher for NopDispatcher {
    fn embed(&self, _: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        Ok([0.0; VECTOR_DIM])
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        Ok(vec![[0.0; VECTOR_DIM]; texts.len()])
    }
    fn fingerprint(&self) -> [u8; 16] {
        [0; 16]
    }
}

// ---------------------------------------------------------------------------
// Fixture.
// ---------------------------------------------------------------------------

struct Fixture {
    ctx: OpsContext,
    ids: Vec<MemoryId>,
    _tempdir: tempfile::TempDir,
}

fn make_id(i: u64) -> MemoryId {
    let mut b = [0u8; 16];
    b[0..8].copy_from_slice(&i.to_be_bytes());
    MemoryId::from_be_bytes(b)
}

fn build_fixture(n_memories: usize, edges: &[(usize, EdgeKind, usize)]) -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let mut metadata = MetadataDb::open(&db_path).unwrap();

    let agent = AgentId(Uuid::nil());
    let mut ids = Vec::with_capacity(n_memories);

    let wtxn = metadata.write_txn().unwrap();
    {
        let mut table = wtxn.open_table(MEMORIES_TABLE).unwrap();
        for i in 0..n_memories {
            let id = make_id((i as u64) + 1);
            ids.push(id);
            let meta = MemoryMetadata::new_active(
                id,
                agent,
                ContextId(42),
                (i + 1) as u64,
                1,
                MemoryKind::Episodic,
                [0x11; 16],
                0.5,
                16,
                1_000_000 + i as u64,
            );
            table.insert(id.to_be_bytes(), meta).unwrap();
        }
    }
    {
        let mut out = wtxn.open_table(EDGES_OUT_TABLE).unwrap();
        let mut inn = wtxn.open_table(EDGES_IN_TABLE).unwrap();
        for (src, kind, tgt) in edges {
            let data = EdgeData::new(1.0, 0, 0, 1_700_000_000_000_000_000);
            link(&mut out, &mut inn, ids[*src], *kind, ids[*tgt], &data).unwrap();
        }
    }
    wtxn.commit().unwrap();

    let (shared, hnsw_writer) = SharedHnsw::<VECTOR_DIM>::new(IndexParams::default_v1()).unwrap();
    let metadata: SharedMetadataDb = Arc::new(Mutex::new(metadata));
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
    let executor = ExecutorContext::new(
        Arc::new(NopDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata,
        writer as Arc<dyn WriterHandle>,
    );

    Fixture {
        ctx: OpsContext::new(executor),
        ids,
        _tempdir: tempdir,
    }
}

fn reason_req(observation: ObservationInput, depth: u32, max_inferences: u32) -> ReasonRequest {
    ReasonRequest {
        observation,
        depth,
        confidence_threshold: 0.0,
        context_filter: None,
        max_inferences,
        budget_wall_time_ms: 1000,
        request_id: None,
    }
}

fn unwrap_reason(body: ResponseBody) -> ReasonResponseFrame {
    match body {
        ResponseBody::Reason(r) => r,
        other => panic!("expected ResponseBody::Reason, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 1. Full pipeline: supports + contradicts → one InferenceStep.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reason_full_pipeline_emits_one_inference() {
    let fix = build_fixture(
        4,
        &[
            (0, EdgeKind::Supports, 1),
            (0, EdgeKind::Supports, 2),
            (0, EdgeKind::Contradicts, 3),
        ],
    );
    let req = reason_req(ObservationInput::ByMemoryId(fix.ids[0].into()), 2, 10);
    let frame = unwrap_reason(dispatch(RequestBody::Reason(req), &fix.ctx).await.unwrap());

    assert!(frame.is_final);
    assert_eq!(frame.reason_status, Some(WireReasonStatus::Complete));
    assert_eq!(frame.inferences.len(), 1);
    let inf = &frame.inferences[0];
    assert_eq!(inf.step_index, 0);
    assert_eq!(inf.inference_kind, InferenceKind::EvidenceAccumulation);
    // base + 2 traversed supports = 3 supporting; 1 contradicting.
    assert_eq!(inf.supporting_memories.len(), 3);
    assert_eq!(inf.contradicting_memories.len(), 1);
    assert!(inf.confidence > 0.0);
    // ByMemoryId observation → claim is empty (documented v1 gap).
    assert_eq!(inf.claim, "");
}

// ---------------------------------------------------------------------------
// 2. No evidence → confidence reflects only the direct-similarity base.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reason_isolated_base_returns_only_self() {
    let fix = build_fixture(1, &[]);
    let req = reason_req(ObservationInput::ByMemoryId(fix.ids[0].into()), 2, 10);
    let frame = unwrap_reason(dispatch(RequestBody::Reason(req), &fix.ctx).await.unwrap());
    let inf = &frame.inferences[0];
    assert_eq!(inf.supporting_memories.len(), 1);
    assert!(inf.contradicting_memories.is_empty());
    // sum_s = 1.0, sum_c = 0 → confidence = 1.0.
    assert_eq!(inf.confidence, 1.0);
    assert_eq!(frame.reason_status, Some(WireReasonStatus::Complete));
}

// ---------------------------------------------------------------------------
// 3. Invalid depth → planner validation error.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reason_invalid_depth_returns_plan_error() {
    let fix = build_fixture(1, &[]);
    let req = reason_req(ObservationInput::ByMemoryId(fix.ids[0].into()), 0, 5);
    let err = dispatch(RequestBody::Reason(req), &fix.ctx)
        .await
        .unwrap_err();
    assert!(
        matches!(err, OpError::PlanError(_)),
        "depth=0 must be a planner validation failure, got {err:?}"
    );
    assert_eq!(err.error_code(), ErrorCode::InvalidRequest);
}

// ---------------------------------------------------------------------------
// 4. Inference kind is EvidenceAccumulation for v1.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reason_kind_categorisation_uses_evidence_accumulation() {
    let fix = build_fixture(2, &[(0, EdgeKind::Supports, 1)]);
    let req = reason_req(ObservationInput::ByMemoryId(fix.ids[0].into()), 2, 10);
    let frame = unwrap_reason(dispatch(RequestBody::Reason(req), &fix.ctx).await.unwrap());
    assert_eq!(
        frame.inferences[0].inference_kind,
        InferenceKind::EvidenceAccumulation
    );
}

// ---------------------------------------------------------------------------
// 5. ByText observation: claim is preserved on the wire.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reason_by_text_preserves_claim() {
    let fix = build_fixture(1, &[]);
    let req = reason_req(ObservationInput::ByText("is the sky blue?".into()), 2, 5);
    let frame = unwrap_reason(dispatch(RequestBody::Reason(req), &fix.ctx).await.unwrap());
    let inf = &frame.inferences[0];
    assert_eq!(inf.claim, "is the sky blue?");
    // Empty index + NopDispatcher → no base, no evidence.
    assert!(inf.supporting_memories.is_empty());
    assert!(inf.contradicting_memories.is_empty());
    assert_eq!(inf.confidence, 0.0);
}
