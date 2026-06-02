//! Recall latency floor — `#[ignore]`-gated p95 regression gates.
//!
//! Two budgets pinned independently against `brain-ops::recall`
//! (not the wire — wire latency is bench-driven, not test-gated):
//!
//! - **Substrate path** — must hold p95 ≤ 1 ms. The substrate
//!   path over a 5-doc fixture has no HNSW pressure beyond the
//!   substrate memory index and no tantivy round-trip; this is
//!   the cache-warm floor reachable from RECALL inside a txn.
//! - **Hybrid path** — must hold p95 ≤ 12 ms. Hybrid is the
//!   default for every wire RECALL; a 12 ms p95 keeps interactive
//!   flows responsive at K=10 without text.
//!
//! Gated behind `#[ignore]` because:
//!
//! 1. The 100-iteration loop dominates wall time in `cargo test`.
//! 2. The thresholds are workstation-tuned; CI hardware skew can
//!    legitimately blow past 12 ms without the underlying code
//!    being slower. The acceptance suite is the
//!    production-reference gate.
//!
//! Run with: `cargo test -p brain-server --test recall_perf --
//! --ignored --test-threads=1`.

#![cfg(target_os = "linux")]

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

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
use brain_ops::{OpsContext, RealWriterHandle};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::envelope::request::{
    EncodeRequest, MemoryKindWire, RecallRequest, TxnBeginRequest,
};

const ITERATIONS: usize = 100;
const WARMUP_ITERATIONS: usize = 10;

// ---------------------------------------------------------------------------
// Mock dispatcher: same deterministic shape as the brain-ops fixture.
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
// Canned retrievers — return one hit each so the hybrid path
// exercises the full RRF + projection codepath. Production deployments
// hit real tantivy and HNSW shards; this measurement is the in-process
// floor, not an upper bound.
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

// ---------------------------------------------------------------------------
// Fixture.
// ---------------------------------------------------------------------------

struct Fixture {
    ctx: OpsContext,
    _tempdir: tempfile::TempDir,
}

fn build_fixture() -> Fixture {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(MetadataDb::open(&db_path).expect("open metadata"));

    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).expect("hnsw");
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));

    let executor = ExecutorContext::new(
        Arc::new(MockDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata,
        writer as Arc<dyn WriterHandle>,
    );

    Fixture {
        ctx: brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor),
        _tempdir: tempdir,
    }
}

fn attach_hybrid_mocks(fix: &mut Fixture, memory_id: u128) {
    let item = RankedItem {
        id: RankedItemId::Memory(MemoryId::from_raw(memory_id)),
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
        .clone()
        .with_semantic_retriever(Arc::new(semantic) as Arc<dyn SemanticRetriever>)
        .with_lexical_retriever(Arc::new(lexical) as Arc<dyn LexicalRetriever>)
        .with_graph_retriever(Arc::new(graph) as Arc<dyn GraphRetriever>);
}

async fn encode(fix: &Fixture, request_id: [u8; 16], text: &str) -> u128 {
    use brain_protocol::envelope::request::RequestBody;
    use brain_protocol::envelope::response::{EncodeResponse, ResponseBody};

    let req = EncodeRequest {
        text: text.into(),
        context_id: 0,
        kind: MemoryKindWire::Episodic,
        salience_hint: 0.5,
        edges: Vec::new(),
        request_id,
        txn_id: None,
        deduplicate: false,
    };
    let outcome = brain_ops::dispatch(
        RequestBody::Encode(req),
        brain_ops::RequestCaller::anonymous(),
        &fix.ctx,
    )
    .await
    .expect("encode");
    match single_body(outcome) {
        ResponseBody::Encode(EncodeResponse { memory_id, .. }) => memory_id,
        other => panic!("expected Encode response, got {other:?}"),
    }
}

async fn seed(fix: &Fixture) -> u128 {
    let phrases = [
        "Priya prefers async meetings over standups",
        "Async-first communication reduces context-switching",
        "Standups are a sync ritual we should retire",
        "Document driven design helps async teams",
        "Team prefers structured documents over live calls",
    ];
    let mut first = 0u128;
    for (i, p) in phrases.iter().enumerate() {
        let mut req_id = [0u8; 16];
        req_id[0] = 0xC0 + i as u8;
        let id = encode(fix, req_id, p).await;
        if i == 0 {
            first = id;
        }
    }
    first
}

fn recall_req(txn_id: Option<[u8; 16]>) -> RecallRequest {
    RecallRequest {
        cue_text: "meeting preferences".into(),
        top_k: 5,
        confidence_threshold: 0.0,
        context_filter: None,
        age_bound_unix_nanos: None,
        kind_filter: None,
        salience_floor: 0.0,
        include_edges: false,
        include_graph: false,
        include_text: false,
        request_id: Some(*uuid::Uuid::now_v7().as_bytes()),
        txn_id,
        agent_filter: Vec::new(),
        include_other_agents: false,
    }
}

fn percentile(sorted_ns: &[u128], p: f64) -> Duration {
    assert!(!sorted_ns.is_empty());
    let rank = (p * (sorted_ns.len() as f64 - 1.0)).round() as usize;
    Duration::from_nanos(sorted_ns[rank.min(sorted_ns.len() - 1)] as u64)
}

async fn measure(fix: &Fixture, txn_id: Option<[u8; 16]>) -> (Duration, Duration) {
    // Warm the embedder cache, HNSW heuristics, and allocator
    // pools before the measured loop.
    for _ in 0..WARMUP_ITERATIONS {
        let _ = brain_ops::recall::handle_recall(recall_req(txn_id), &fix.ctx)
            .await
            .expect("warmup recall");
    }

    let mut samples_ns: Vec<u128> = Vec::with_capacity(ITERATIONS);
    for _ in 0..ITERATIONS {
        let start = Instant::now();
        let _ = brain_ops::recall::handle_recall(recall_req(txn_id), &fix.ctx)
            .await
            .expect("measured recall");
        samples_ns.push(start.elapsed().as_nanos());
    }
    samples_ns.sort_unstable();
    (percentile(&samples_ns, 0.50), percentile(&samples_ns, 0.95))
}

// ---------------------------------------------------------------------------
// PERF1A — memory-HNSW-only path. Reached by `handle_recall` when a
// txn is attached; gated at 1 ms p95 because there's no fusion, no
// tantivy round-trip, no graph traversal — just the memory HNSW.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "perf gate: workstation-tuned thresholds, run explicitly"]
fn recall_p95_substrate_via_internal_entry_point() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let _ = seed(&fix).await;

        // Attach a txn so handle_recall takes the substrate branch.
        let txn_id = *uuid::Uuid::now_v7().as_bytes();
        brain_ops::txn::handle_txn_begin(
            TxnBeginRequest {
                txn_id,
                timeout_seconds: 60,
            },
            [0u8; 16],
            &fix.ctx,
        )
        .await
        .expect("txn_begin");

        let (p50, p95) = measure(&fix, Some(txn_id)).await;
        let budget = Duration::from_millis(1);
        assert!(
            p95 <= budget,
            "substrate p95 {p95:?} exceeds budget {budget:?} (p50 {p50:?})",
        );
    })
}

// ---------------------------------------------------------------------------
// PERF1B — hybrid path. Reached by `handle_recall` when no txn is
// attached and all three retrievers are wired. Gated at 12 ms p95;
// canned retrievers keep this an in-process floor measurement, not
// an end-to-end wire test.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "perf gate: workstation-tuned thresholds, run explicitly"]
fn recall_p95_hybrid_via_handle_recall() {
    run_in_glommio(|| async {
        let mut fix = build_fixture();
        let first = seed(&fix).await;
        attach_hybrid_mocks(&mut fix, first);

        let (p50, p95) = measure(&fix, None).await;
        let budget = Duration::from_millis(12);
        assert!(
            p95 <= budget,
            "hybrid p95 {p95:?} exceeds budget {budget:?} (p50 {p50:?})",
        );
    })
}
