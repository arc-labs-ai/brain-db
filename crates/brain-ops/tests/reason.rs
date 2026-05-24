//! Integration tests for `handle_reason` (sub-task 7.6; refactored
//! in 7.8 to insert edges via wire LINK).

use std::sync::Arc;

use brain_core::{AgentId, ContextId, EdgeKind, MemoryId, MemoryKind};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::MetadataDb;
use brain_ops::test_support::run_in_glommio;
use brain_ops::{dispatch, DispatchOutcome, ErrorCode, OpError, OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::envelope::request::{
    EdgeKindWire, LinkRequest, ObservationInput, ReasonRequest, RequestBody,
};
use brain_protocol::envelope::response::{
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

async fn build_fixture(n_memories: usize, edges: &[(usize, EdgeKind, usize)]) -> Fixture {
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
    wtxn.commit().unwrap();

    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let metadata: SharedMetadataDb = Arc::new(Mutex::new(metadata));
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
    let executor = ExecutorContext::new(
        Arc::new(NopDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata,
        writer as Arc<dyn WriterHandle>,
    );
    let ctx = brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor);

    for (i, (src, kind, tgt)) in edges.iter().enumerate() {
        let mut request_id = [0u8; 16];
        request_id[..2].copy_from_slice(&(i as u16).to_be_bytes());
        request_id[2] = 0xEE;
        let req = LinkRequest {
            source: ids[*src].raw(),
            target: ids[*tgt].raw(),
            kind: EdgeKindWire::from(*kind),
            weight: 1.0,
            request_id,
            txn_id: None,
        };
        let _ = dispatch(
            RequestBody::Link(req),
            brain_ops::RequestCaller::anonymous(),
            &ctx,
        )
        .await
        .unwrap();
    }

    Fixture {
        ctx,
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
        txn_id: None,
    }
}

/// Collapse the streamed REASON frames into a single observation:
/// concatenated inference steps from every mid-stream frame and the
/// terminal frame's `reason_status` + `is_final`. Mirrors the v1
/// single-frame shape so existing assertions read unchanged.
fn collect_reason_outcome(outcome: DispatchOutcome) -> ReasonResponseFrame {
    let frames = unwrap_reason_stream(outcome);
    let mut inferences = Vec::new();
    let mut terminal = None;
    for f in frames {
        if f.is_final {
            terminal = Some(f);
        } else {
            inferences.extend(f.inferences);
        }
    }
    let terminal = terminal.expect("REASON stream must end with a terminal frame");
    ReasonResponseFrame {
        inferences,
        is_final: terminal.is_final,
        reason_status: terminal.reason_status,
    }
}

fn unwrap_reason_stream(outcome: DispatchOutcome) -> Vec<ReasonResponseFrame> {
    match outcome {
        DispatchOutcome::Stream(bodies) => bodies
            .into_iter()
            .map(|b| match b {
                ResponseBody::Reason(r) => r,
                other => panic!("expected ResponseBody::Reason in stream, got {other:?}"),
            })
            .collect(),
        DispatchOutcome::Single(other) => {
            panic!("expected DispatchOutcome::Stream of Reason frames, got Single({other:?})")
        }
    }
}

// ---------------------------------------------------------------------------
// 1. Full pipeline: supports + contradicts → one InferenceStep.
// ---------------------------------------------------------------------------

#[test]
fn reason_full_pipeline_emits_one_inference() {
    run_in_glommio(|| async {
        let fix = build_fixture(
            4,
            &[
                (0, EdgeKind::Supports, 1),
                (0, EdgeKind::Supports, 2),
                (0, EdgeKind::Contradicts, 3),
            ],
        )
        .await;
        let req = reason_req(ObservationInput::ByMemoryId(fix.ids[0].into()), 2, 10);
        let frame = collect_reason_outcome(
            dispatch(
                RequestBody::Reason(req),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );

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
    })
}

// ---------------------------------------------------------------------------
// 2. No evidence → confidence reflects only the direct-similarity base.
// ---------------------------------------------------------------------------

#[test]
fn reason_isolated_base_returns_only_self() {
    run_in_glommio(|| async {
        let fix = build_fixture(1, &[]).await;
        let req = reason_req(ObservationInput::ByMemoryId(fix.ids[0].into()), 2, 10);
        let frame = collect_reason_outcome(
            dispatch(
                RequestBody::Reason(req),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        let inf = &frame.inferences[0];
        assert_eq!(inf.supporting_memories.len(), 1);
        assert!(inf.contradicting_memories.is_empty());
        // sum_s = 1.0, sum_c = 0 → confidence = 1.0.
        assert_eq!(inf.confidence, 1.0);
        assert_eq!(frame.reason_status, Some(WireReasonStatus::Complete));
    })
}

// ---------------------------------------------------------------------------
// 3. Invalid depth → planner validation error.
// ---------------------------------------------------------------------------

#[test]
fn reason_invalid_depth_returns_plan_error() {
    run_in_glommio(|| async {
        let fix = build_fixture(1, &[]).await;
        let req = reason_req(ObservationInput::ByMemoryId(fix.ids[0].into()), 0, 5);
        let err = dispatch(
            RequestBody::Reason(req),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, OpError::PlanError(_)),
            "depth=0 must be a planner validation failure, got {err:?}"
        );
        assert_eq!(err.error_code(), ErrorCode::InvalidRequest);
    })
}

// ---------------------------------------------------------------------------
// 4. Inference kind is EvidenceAccumulation for v1.
// ---------------------------------------------------------------------------

#[test]
fn reason_kind_categorisation_uses_evidence_accumulation() {
    run_in_glommio(|| async {
        let fix = build_fixture(2, &[(0, EdgeKind::Supports, 1)]).await;
        let req = reason_req(ObservationInput::ByMemoryId(fix.ids[0].into()), 2, 10);
        let frame = collect_reason_outcome(
            dispatch(
                RequestBody::Reason(req),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(
            frame.inferences[0].inference_kind,
            InferenceKind::EvidenceAccumulation
        );
    })
}

// ---------------------------------------------------------------------------
// 5. ByText observation: claim is preserved on the wire.
// ---------------------------------------------------------------------------

#[test]
fn reason_by_text_preserves_claim() {
    run_in_glommio(|| async {
        let fix = build_fixture(1, &[]).await;
        let req = reason_req(ObservationInput::ByText("is the sky blue?".into()), 2, 5);
        let frame = collect_reason_outcome(
            dispatch(
                RequestBody::Reason(req),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        let inf = &frame.inferences[0];
        assert_eq!(inf.claim, "is the sky blue?");
        // Empty index + NopDispatcher → no base, no evidence.
        assert!(inf.supporting_memories.is_empty());
        assert!(inf.contradicting_memories.is_empty());
        assert_eq!(inf.confidence, 0.0);
    })
}
