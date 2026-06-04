//! Integration tests for `handle_recall`.
//!
//! Drives the full pipeline:
//!   dispatcher → handle_recall → plan_recall_inner → execute_recall
//!   → wire RecallResponseFrame
//!
//! Pre-populates the index by calling ENCODE through the dispatcher
//! first, then runs RECALL against it.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use brain_core::MemoryId;
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{
    GraphError, GraphQuery, GraphRetriever, GraphRetrieverConfig, IndexParams, LexicalError,
    LexicalQuery, LexicalRetriever, LexicalRetrieverConfig, LexicalScope, RankedItem, RankedItemId,
    SemanticError, SemanticQuery, SemanticRetriever, SemanticRetrieverConfig, SemanticScope,
    SharedHnsw,
};
use brain_metadata::MetadataDb;
use brain_ops::test_support::{run_in_glommio, single_body};
use brain_ops::{dispatch, DispatchOutcome, ErrorCode, OpError, OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::envelope::request::{
    EncodeRequest, MemoryKindWire, RecallRequest, RequestBody,
};
use brain_protocol::envelope::response::{EncodeResponse, RecallResponseFrame, ResponseBody};

// ---------------------------------------------------------------------------
// Mock dispatcher: text-driven deterministic vectors.
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
    build_fixture_with_embedder(Arc::new(MockDispatcher) as Arc<dyn Dispatcher>)
}

fn build_fixture_with_embedder(embedder: Arc<dyn Dispatcher>) -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(MetadataDb::open(&db_path).unwrap());

    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));

    let executor =
        ExecutorContext::new(embedder, shared, metadata, writer as Arc<dyn WriterHandle>);

    Fixture {
        ctx: brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor),
        _tempdir: tempdir,
    }
}

fn encode_req(request_id: [u8; 16], text: &str, kind: MemoryKindWire) -> EncodeRequest {
    EncodeRequest {
        text: text.into(),
        context_id: 42,
        kind,
        salience_hint: 0.5,
        edges: vec![],
        request_id,
        txn_id: None,
        deduplicate: false,
    }
}

fn recall_req(cue: &str, top_k: u32) -> RecallRequest {
    RecallRequest {
        cue_text: cue.into(),
        top_k,
        confidence_threshold: 0.0,
        context_filter: None,
        age_bound_unix_nanos: None,
        kind_filter: None,
        salience_floor: 0.0,
        include_edges: false,
        include_graph: false,
        include_text: false,
        request_id: None,
        txn_id: None,
        agent_filter: Vec::new(),
        include_other_agents: false,
    }
}

async fn encode(fix: &Fixture, request_id: [u8; 16], text: &str, kind: MemoryKindWire) -> u128 {
    let req = encode_req(request_id, text, kind);
    let outcome = dispatch(
        RequestBody::Encode(req),
        brain_ops::RequestCaller::anonymous(),
        &fix.ctx,
    )
    .await
    .unwrap();
    match single_body(outcome) {
        ResponseBody::Encode(EncodeResponse { memory_id, .. }) => memory_id,
        other => panic!("expected Encode response, got {other:?}"),
    }
}

fn unwrap_recall_resp(outcome: DispatchOutcome) -> RecallResponseFrame {
    match single_body(outcome) {
        ResponseBody::Recall(r) => r,
        other => panic!("expected ResponseBody::Recall, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 1. Full pipeline.
// ---------------------------------------------------------------------------

#[test]
fn recall_full_pipeline_returns_top_k() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        encode(&fix, [1; 16], "alpha", MemoryKindWire::Episodic).await;
        encode(&fix, [2; 16], "beta", MemoryKindWire::Episodic).await;
        encode(&fix, [3; 16], "gamma", MemoryKindWire::Episodic).await;

        let frame = unwrap_recall_resp(
            dispatch(
                RequestBody::Recall(recall_req("alpha", 2)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert!(frame.is_final);
        assert_eq!(frame.results.len(), 2, "k=2 → exactly 2 results");
        assert_eq!(frame.cumulative_count, 2);
        // Sorted by score descending.
        assert!(
            frame.results[0].similarity_score >= frame.results[1].similarity_score,
            "results must be sorted by score desc"
        );
        // Fields plumbed through.
        let top = &frame.results[0];
        assert_ne!(top.memory_id, 0);
        assert_eq!(top.context_id, 42);
        assert_eq!(top.kind, MemoryKindWire::Episodic);
        assert!((top.salience - 0.5).abs() < 1e-6);
        // The retrieval pipeline carries two distinct scores per hit:
        // `similarity_score` is the semantic retriever's raw cosine and
        // `confidence` mirrors it — both bounded in [0, 1]. The unbounded
        // RRF rank-fusion sum is a separate diagnostic (`fused_score`),
        // never surfaced as confidence.
        assert!(top.similarity_score > 0.0, "similarity_score populated");
        assert!(
            top.confidence > 0.0 && top.confidence <= 1.0,
            "confidence is a bounded [0,1] similarity, got {}",
            top.confidence
        );
        assert!(
            (top.confidence - top.similarity_score).abs() < 1e-6,
            "confidence mirrors similarity_score on the retrieval path"
        );
        assert_eq!(
            top.last_accessed_at_unix_nanos, top.created_at_unix_nanos,
            "v1: last_accessed mirrors created_at"
        );
        assert!(top.edges.is_none());
    })
}

// ---------------------------------------------------------------------------
// 2. Empty index → empty frame.
// ---------------------------------------------------------------------------

#[test]
fn recall_empty_index_returns_empty_frame() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let frame = unwrap_recall_resp(
            dispatch(
                RequestBody::Recall(recall_req("nothing", 10)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert!(frame.results.is_empty());
        assert!(frame.is_final);
        assert_eq!(frame.cumulative_count, 0);
        assert!(frame.estimated_remaining.is_none());
    })
}

// ---------------------------------------------------------------------------
// 3. K-truncation.
// ---------------------------------------------------------------------------

#[test]
fn recall_k_truncation() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        for i in 0..5u8 {
            let mut req_id = [0u8; 16];
            req_id[0] = 0x10 + i;
            let text = format!("doc-{i}");
            encode(&fix, req_id, &text, MemoryKindWire::Episodic).await;
        }
        let frame = unwrap_recall_resp(
            dispatch(
                RequestBody::Recall(recall_req("doc-2", 3)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(frame.results.len(), 3, "k=3 → exactly 3 results");
    })
}

// ---------------------------------------------------------------------------
// 4. Kind filter.
// ---------------------------------------------------------------------------

#[test]
fn recall_kind_filter_rejects_off_kind_hits() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        encode(&fix, [20; 16], "ep-a", MemoryKindWire::Episodic).await;
        encode(&fix, [21; 16], "ep-b", MemoryKindWire::Episodic).await;
        encode(&fix, [22; 16], "sem-a", MemoryKindWire::Semantic).await;
        encode(&fix, [23; 16], "sem-b", MemoryKindWire::Semantic).await;

        let mut req = recall_req("ep-a", 10);
        req.kind_filter = Some(vec![MemoryKindWire::Semantic]);
        let frame = unwrap_recall_resp(
            dispatch(
                RequestBody::Recall(req),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );

        assert!(
            !frame.results.is_empty(),
            "the semantic memories must be in candidates"
        );
        for r in &frame.results {
            assert_eq!(r.kind, MemoryKindWire::Semantic);
        }
    })
}

// ---------------------------------------------------------------------------
// 5. Confidence floor.
// ---------------------------------------------------------------------------

#[test]
fn recall_confidence_floor_drops_low_score_hits() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        encode(&fix, [30; 16], "alpha", MemoryKindWire::Episodic).await;
        encode(
            &fix,
            [31; 16],
            "completely-different-cue",
            MemoryKindWire::Episodic,
        )
        .await;

        let mut req = recall_req("totally-unrelated-query-xyz", 10);
        // 0.999 is so strict that the unrelated cue should drop everything.
        req.confidence_threshold = 0.999;
        let frame = unwrap_recall_resp(
            dispatch(
                RequestBody::Recall(req),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        for r in &frame.results {
            assert!(
                r.similarity_score >= 0.999,
                "every result must clear the floor; got {}",
                r.similarity_score
            );
        }
    })
}

// ---------------------------------------------------------------------------
// 6. Invalid top_k → planner rejects.
// ---------------------------------------------------------------------------

#[test]
fn recall_invalid_top_k_returns_plan_error() {
    // top_k=0 has no meaningful "give me zero results" interpretation;
    // it's an obvious client bug. The handler rejects it up front rather
    // than letting the retrieval planner silently clamp it to a default.
    run_in_glommio(|| async {
        let fix = build_fixture();
        let req = recall_req("anything", 0);
        let err = dispatch(
            RequestBody::Recall(req),
            brain_ops::RequestCaller::anonymous(),
            &fix.ctx,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, OpError::InvalidRequest(_)),
            "top_k=0 must be rejected as invalid input, got {err:?}"
        );
        assert_eq!(err.error_code(), ErrorCode::InvalidRequest);
    })
}

// ---------------------------------------------------------------------------
// 7. include_text — substrate path round-trip.
// ---------------------------------------------------------------------------

#[test]
fn recall_include_text_false_returns_empty_text_field() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        encode(&fix, [40; 16], "alpha-text-rev0", MemoryKindWire::Episodic).await;
        encode(&fix, [41; 16], "beta-text-rev0", MemoryKindWire::Episodic).await;

        let frame = unwrap_recall_resp(
            dispatch(
                RequestBody::Recall(recall_req("alpha-text-rev0", 2)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(frame.results.len(), 2);
        for r in &frame.results {
            assert!(
                r.text.is_empty(),
                "include_text default=false must return empty text, got {:?}",
                r.text
            );
        }
    })
}

#[test]
fn recall_include_text_true_returns_stored_text() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let ids = [
            (
                encode(&fix, [50; 16], "alpha-text-rev1", MemoryKindWire::Episodic).await,
                "alpha-text-rev1",
            ),
            (
                encode(&fix, [51; 16], "beta-text-rev1", MemoryKindWire::Episodic).await,
                "beta-text-rev1",
            ),
            (
                encode(&fix, [52; 16], "gamma-text-rev1", MemoryKindWire::Episodic).await,
                "gamma-text-rev1",
            ),
        ];
        let by_id: std::collections::HashMap<u128, &'static str> = ids.iter().copied().collect();

        let mut req = recall_req("alpha-text-rev1", 3);
        req.include_text = true;
        let frame = unwrap_recall_resp(
            dispatch(
                RequestBody::Recall(req),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );

        assert_eq!(frame.results.len(), 3);
        for r in &frame.results {
            let want = by_id.get(&r.memory_id).copied().expect("known id");
            assert_eq!(
                r.text, want,
                "include_text=true must return the exact UTF-8 we encoded",
            );
        }
    })
}

// ---------------------------------------------------------------------------
// 8. Real-embedder gated test. Skips when env var is unset.
// ---------------------------------------------------------------------------

#[test]
fn recall_with_real_embedder_end_to_end() {
    run_in_glommio(|| async {
        let Ok(model_dir) = std::env::var("BRAIN_EMBED_MODEL_DIR") else {
            eprintln!("BRAIN_EMBED_MODEL_DIR unset; skipping BGE end-to-end test");
            return;
        };

        let model_dir = std::path::PathBuf::from(model_dir);
        let handle = brain_embed::ModelHandle::load(&brain_embed::EmbedderConfig::new(model_dir))
            .expect("BGE model loads");
        let dispatcher = brain_embed::CpuDispatcher::new(handle);
        let fix = build_fixture_with_embedder(Arc::new(dispatcher) as Arc<dyn Dispatcher>);

        let cats_id = encode(
            &fix,
            [0x70; 16],
            "the cat sat on the mat",
            MemoryKindWire::Episodic,
        )
        .await;
        let _physics_id = encode(
            &fix,
            [0x71; 16],
            "quantum entanglement collapses on observation",
            MemoryKindWire::Episodic,
        )
        .await;

        let frame = unwrap_recall_resp(
            dispatch(
                RequestBody::Recall(recall_req("a cat resting on a rug", 2)),
                brain_ops::RequestCaller::anonymous(),
                &fix.ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(frame.results.len(), 2);
        assert_eq!(
            frame.results[0].memory_id, cats_id,
            "the cat memory must rank higher than the physics memory"
        );
    })
}

// ---------------------------------------------------------------------------
// 9. handle_recall routing — txn_id determines substrate vs retrieval.
//
// RECALL is one verb with one server-side rule: a txn forces the
// substrate path (read-your-writes), everything else fuses through the
// retrieval retrievers. These tests pin both branches with a context
// that has all three retrievers wired, so the cold-start fallback at
// `recall.rs::handle_recall` doesn't mask a regression in the txn
// gate.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct CannedSemantic {
    items: Arc<StdMutex<Vec<RankedItem>>>,
}

impl SemanticRetriever for CannedSemantic {
    fn retrieve(
        &self,
        _query: &SemanticQuery,
        _scope: SemanticScope,
        _config: &SemanticRetrieverConfig,
    ) -> Result<Vec<RankedItem>, SemanticError> {
        Ok(self.items.lock().expect("canned semantic lock").clone())
    }
}

#[derive(Clone)]
struct CannedLexical {
    items: Arc<StdMutex<Vec<RankedItem>>>,
}

impl LexicalRetriever for CannedLexical {
    fn retrieve(
        &self,
        _query: &LexicalQuery,
        _scope: LexicalScope,
        _config: &LexicalRetrieverConfig,
    ) -> Result<Vec<RankedItem>, LexicalError> {
        Ok(self.items.lock().expect("canned lexical lock").clone())
    }
}

#[derive(Clone)]
struct CannedGraph {
    items: Arc<StdMutex<Vec<RankedItem>>>,
}

impl GraphRetriever for CannedGraph {
    fn retrieve(
        &self,
        _query: &GraphQuery,
        _config: &GraphRetrieverConfig,
    ) -> Result<Vec<RankedItem>, GraphError> {
        Ok(self.items.lock().expect("canned graph lock").clone())
    }
}

/// Build a fixture whose `OpsContext` has all three retrieval retrievers
/// replaced by canned mocks returning a hit for `memory_id`. The mocks
/// override the real retrievers wired by `build_fixture` so the test
/// can assert the retrieval path's behavior with deterministic per-retriever
/// outputs.
fn build_fixture_with_retrieval_mocks(memory_id: u128) -> Fixture {
    let mut fix = build_fixture();
    let mid = MemoryId::from_raw(memory_id);
    let item = RankedItem {
        id: RankedItemId::Memory(mid),
        rank: 1,
        score: 0.95,
        snippet: None,
    };

    let semantic = CannedSemantic {
        items: Arc::new(StdMutex::new(vec![item.clone()])),
    };
    let lexical = CannedLexical {
        items: Arc::new(StdMutex::new(vec![item.clone()])),
    };
    let graph = CannedGraph {
        items: Arc::new(StdMutex::new(vec![item])),
    };

    fix.ctx = fix
        .ctx
        .with_semantic_retriever(Arc::new(semantic) as Arc<dyn SemanticRetriever>)
        .with_lexical_retriever(Arc::new(lexical) as Arc<dyn LexicalRetriever>)
        .with_graph_retriever(Arc::new(graph) as Arc<dyn GraphRetriever>);
    fix
}

#[test]
fn handle_recall_routes_to_retrieval_when_no_txn() {
    // No txn → retrieval runs. The canned retrievers all return the
    // same memory id, so RRF fusion produces one hit with three
    // contributors and a non-zero fused score. A regression that
    // re-routed to substrate would zero `fused_score` and clear
    // `contributing_retrievers`.
    run_in_glommio(|| async {
        let fix0 = build_fixture();
        let mid = encode(&fix0, [0xA0; 16], "beta", MemoryKindWire::Episodic).await;
        drop(fix0);

        let fix = build_fixture_with_retrieval_mocks(mid);
        let _ = encode(&fix, [0xA0; 16], "beta", MemoryKindWire::Episodic).await;

        let frame = brain_ops::recall::handle_recall(recall_req("beta", 5), &fix.ctx)
            .await
            .expect("retrieval recall");

        assert!(frame.is_final);
        assert!(
            !frame.results.is_empty(),
            "retrieval recall returned no hits",
        );
        let any_with_retrievers = frame
            .results
            .iter()
            .any(|r| !r.contributing_retrievers.is_empty());
        let any_nonzero_fused = frame.results.iter().any(|r| r.fused_score > 0.0);
        assert!(
            any_with_retrievers,
            "retrieval path must populate contributing_retrievers on at least one hit",
        );
        assert!(
            any_nonzero_fused,
            "retrieval path must produce a non-zero fused_score on at least one hit",
        );
    })
}
