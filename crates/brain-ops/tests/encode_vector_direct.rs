//! Integration tests for `handle_encode_vector_direct`.
//!
//! Power-user encode path: client supplies the vector + fingerprint;
//! the server skips its own embed step but still runs idempotency,
//! dedup, slot reservation, edges, and write submission.

use std::sync::Arc;

use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::MetadataDb;
use brain_ops::test_support::{run_in_glommio, single_body};
use brain_ops::{dispatch, DispatchOutcome, ErrorCode, OpError, OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::envelope::request::{
    EdgeKindWire, EdgeRequest, EncodeVectorDirectRequest, MemoryKindWire, RequestBody,
};
use brain_protocol::envelope::response::{EncodeResponse, ResponseBody};
use parking_lot::Mutex;

// ---------------------------------------------------------------------------
// Mock dispatcher: deterministic per-text vector + stable fingerprint.
// ---------------------------------------------------------------------------

const MOCK_FP: [u8; 16] = [0xAB; 16];

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
        MOCK_FP
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

    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));

    let embedder: Arc<dyn Dispatcher> = Arc::new(MockDispatcher);
    let executor =
        ExecutorContext::new(embedder, shared, metadata, writer as Arc<dyn WriterHandle>);

    Fixture {
        ctx: OpsContext::new(executor),
        _tempdir: tempdir,
    }
}

/// Build a unit-norm 384-d vector with a single 1.0 at the requested
/// index. Deterministic, trivially normalised, distinct per `slot`.
fn unit_vector(slot: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; VECTOR_DIM];
    v[slot % VECTOR_DIM] = 1.0;
    v
}

fn vector_direct_req(request_id: [u8; 16], slot: usize) -> EncodeVectorDirectRequest {
    EncodeVectorDirectRequest {
        text: format!("vector-direct slot {slot}"),
        vector: unit_vector(slot),
        model_fingerprint: MOCK_FP,
        context_id: 42,
        kind: MemoryKindWire::Episodic,
        salience_hint: 0.5,
        edges: vec![],
        request_id,
        txn_id: None,
        deduplicate: false,
    }
}

fn unwrap_resp(outcome: DispatchOutcome) -> EncodeResponse {
    match single_body(outcome) {
        ResponseBody::EncodeVectorDirect(r) => r,
        other => panic!("expected ResponseBody::EncodeVectorDirect, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 1. Happy path.
// ---------------------------------------------------------------------------

#[test]
fn vector_direct_full_pipeline_returns_memory_id() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let req = vector_direct_req([1; 16], 7);
        let resp = dispatch(
            RequestBody::EncodeVectorDirect(req),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap();
        let enc = unwrap_resp(resp);

        assert_ne!(enc.memory_id, 0, "memory_id must be non-zero");
        assert!(!enc.was_deduplicated);
        assert_eq!(enc.salience, 0.5);
        assert_eq!(enc.auto_edges_added, 0);
        assert_eq!(enc.embedding_model_fp, MOCK_FP);
    })
}

// ---------------------------------------------------------------------------
// 2. Fingerprint mismatch.
// ---------------------------------------------------------------------------

#[test]
fn vector_direct_fingerprint_mismatch_rejected() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let mut req = vector_direct_req([2; 16], 7);
        req.model_fingerprint = [0xCC; 16]; // Differs from MOCK_FP.

        let err = dispatch(
            RequestBody::EncodeVectorDirect(req),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, OpError::InvalidRequest(ref s) if s.contains("fingerprint")),
            "expected fingerprint InvalidRequest, got {err:?}"
        );
        assert_eq!(err.error_code(), ErrorCode::InvalidRequest);
    })
}

// ---------------------------------------------------------------------------
// 3. Non-normalised vector.
// ---------------------------------------------------------------------------

#[test]
fn vector_direct_non_unit_norm_rejected() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let mut req = vector_direct_req([3; 16], 7);
        // Norm sqrt(2), way outside the +/- 1e-3 window.
        req.vector[0] = 1.0;
        req.vector[1] = 1.0;

        let err = dispatch(
            RequestBody::EncodeVectorDirect(req),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, OpError::InvalidRequest(ref s) if s.contains("L2 norm")),
            "expected L2-norm InvalidRequest, got {err:?}"
        );
        assert_eq!(err.error_code(), ErrorCode::InvalidRequest);
    })
}

// ---------------------------------------------------------------------------
// 4. Wrong-dimensional vector.
// ---------------------------------------------------------------------------

#[test]
fn vector_direct_wrong_dim_rejected() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let mut req = vector_direct_req([4; 16], 7);
        req.vector = vec![1.0f32]; // length 1, not 384.

        let err = dispatch(
            RequestBody::EncodeVectorDirect(req),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, OpError::InvalidRequest(ref s) if s.contains("dimension")),
            "expected dimension InvalidRequest, got {err:?}"
        );
    })
}

// ---------------------------------------------------------------------------
// 5. NaN element rejected.
// ---------------------------------------------------------------------------

#[test]
fn vector_direct_nan_element_rejected() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let mut req = vector_direct_req([5; 16], 7);
        req.vector[0] = f32::NAN;

        let err = dispatch(
            RequestBody::EncodeVectorDirect(req),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, OpError::InvalidRequest(ref s) if s.contains("NaN") || s.contains("Inf")),
            "expected NaN/Inf InvalidRequest, got {err:?}"
        );
    })
}

// ---------------------------------------------------------------------------
// 6. Idempotency replay returns the same memory id.
// ---------------------------------------------------------------------------

#[test]
fn vector_direct_replay_returns_same_response() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let req = vector_direct_req([6; 16], 7);

        let first = unwrap_resp(
            dispatch(
                RequestBody::EncodeVectorDirect(req.clone()),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        let second = unwrap_resp(
            dispatch(
                RequestBody::EncodeVectorDirect(req),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(
            first.memory_id, second.memory_id,
            "retry returns the same MemoryId"
        );
    })
}

// ---------------------------------------------------------------------------
// 7. Txn id rejected (not supported on this path in v1).
// ---------------------------------------------------------------------------

#[test]
fn vector_direct_txn_id_rejected() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let mut req = vector_direct_req([7; 16], 7);
        req.txn_id = Some([0xAB; 16]);

        let err = dispatch(
            RequestBody::EncodeVectorDirect(req),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, OpError::InvalidRequest(ref s) if s.contains("txn_id")),
            "expected txn_id InvalidRequest, got {err:?}"
        );
    })
}

// ---------------------------------------------------------------------------
// 8. Edge attached to a missing target degrades gracefully.
// ---------------------------------------------------------------------------

#[test]
fn vector_direct_missing_edge_target_silently_dropped() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let mut req = vector_direct_req([8; 16], 7);
        req.edges = vec![EdgeRequest {
            target: 0xDEAD_BEEF_u128,
            kind: EdgeKindWire::References,
            weight: 0.5,
        }];

        let resp = unwrap_resp(
            dispatch(
                RequestBody::EncodeVectorDirect(req),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(
            resp.auto_edges_added, 0,
            "missing target drops the edge silently"
        );
    })
}
