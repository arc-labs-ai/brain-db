//! Hybrid query criterion benches against §16/02 §2.10 (phase 23.12).
//!
//! Three benches:
//!
//! 1. Hybrid 3-retriever end-to-end — `plan + execute` with
//!    Semantic + Lexical + Graph mocked at the trait level (target
//!    p50 10 ms / p99 50 ms; the wall-time gate is phase 14
//!    acceptance — this bench is a regression detector for the
//!    plan/fuse/filter/project glue).
//! 2. Router-degraded single-retriever path — text-only query with
//!    only Semantic available (target p50 7 ms / p99 30 ms).
//! 3. EXPLAIN — `plan` only, no execute (target p50 500 µs /
//!    p99 2 ms).
//!
//! Mocked retrievers return a fixed list of `RankedItem` so the
//! bench measures planner + executor + RRF fusion + filter chain +
//! result projection. Production-scale (real BGE embedder + 100K
//! HNSW + tantivy + redb-backed graph) is validated in phase 14.
//!
//! Run:
//!
//! ```bash
//! cargo bench -p brain-planner --bench hybrid_query
//! cargo bench -p brain-planner --bench hybrid_query -- --quick
//! ```

use std::sync::Arc;

use brain_core::{EntityId, MemoryId};
use brain_index::{
    GraphError, GraphQuery, GraphRetriever, GraphRetrieverConfig, LexicalError, LexicalQuery,
    LexicalRetriever, LexicalRetrieverConfig, LexicalScope, RankedItem, RankedItemId,
    SemanticError, SemanticQuery, SemanticRetriever, SemanticRetrieverConfig, SemanticScope,
};
use brain_metadata::MetadataDb;
use brain_planner::knowledge::executor::{execute, HybridExecutorContext};
use brain_planner::knowledge::planner::{plan, QueryPlan};
use brain_planner::knowledge::router::{QueryRequest as PlannerQueryRequest, RetrieverSelection};
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use parking_lot::Mutex;
use tempfile::TempDir;

const TOP_N: usize = 100;

// ---------------------------------------------------------------------------
// Mock retrievers — return canned RankedItem lists with no I/O.
// ---------------------------------------------------------------------------

struct CannedSemanticRetriever {
    items: Vec<RankedItem>,
}

impl SemanticRetriever for CannedSemanticRetriever {
    fn retrieve(
        &self,
        _query: &SemanticQuery,
        _scope: SemanticScope,
        _config: &SemanticRetrieverConfig,
    ) -> Result<Vec<RankedItem>, SemanticError> {
        Ok(self.items.clone())
    }
}

struct CannedLexicalRetriever {
    items: Vec<RankedItem>,
}

impl LexicalRetriever for CannedLexicalRetriever {
    fn retrieve(
        &self,
        _query: &LexicalQuery,
        _scope: LexicalScope,
        _config: &LexicalRetrieverConfig,
    ) -> Result<Vec<RankedItem>, LexicalError> {
        Ok(self.items.clone())
    }
}

struct CannedGraphRetriever {
    items: Vec<RankedItem>,
}

impl GraphRetriever for CannedGraphRetriever {
    fn retrieve(
        &self,
        _query: &GraphQuery,
        _config: &GraphRetrieverConfig,
    ) -> Result<Vec<RankedItem>, GraphError> {
        Ok(self.items.clone())
    }
}

// ---------------------------------------------------------------------------
// Fixture construction. Built once per Criterion bench fn and reused
// across the timed loop via `iter`.
// ---------------------------------------------------------------------------

fn make_items(corpus: usize, kind: impl Fn(usize) -> RankedItemId) -> Vec<RankedItem> {
    (0..corpus)
        .map(|i| RankedItem {
            id: kind(i),
            rank: (i + 1) as u32,
            score: 1.0 / (i as f32 + 1.0),
            snippet: None,
        })
        .collect()
}

struct Fixture {
    _dir: TempDir,
    ctx: HybridExecutorContext,
}

fn build_fixture(
    semantic: Option<Vec<RankedItem>>,
    lexical: Option<Vec<RankedItem>>,
    graph: Option<Vec<RankedItem>>,
) -> Fixture {
    let dir = TempDir::new().expect("tempdir");
    let metadata = MetadataDb::open(dir.path().join("md.redb")).expect("open metadata");
    let metadata = Arc::new(Mutex::new(metadata));

    let ctx = HybridExecutorContext {
        semantic: semantic
            .map(|items| Arc::new(CannedSemanticRetriever { items }) as Arc<dyn SemanticRetriever>),
        lexical: lexical
            .map(|items| Arc::new(CannedLexicalRetriever { items }) as Arc<dyn LexicalRetriever>),
        graph: graph
            .map(|items| Arc::new(CannedGraphRetriever { items }) as Arc<dyn GraphRetriever>),
        metadata,
    };

    Fixture { _dir: dir, ctx }
}

fn text_anchor_request() -> PlannerQueryRequest {
    PlannerQueryRequest {
        text: Some("budget pushback meeting".into()),
        entity_anchor: Some(EntityId::new()),
        kind_filter: Vec::new(),
        predicate_filter: Vec::new(),
        time_filter: None,
        confidence_min: None,
        include_tombstoned: false,
        include_superseded: false,
        limit: 20,
        retrievers: RetrieverSelection::Auto,
        fusion_config: None,
    }
}

fn text_only_request() -> PlannerQueryRequest {
    PlannerQueryRequest {
        text: Some("budget pushback meeting".into()),
        entity_anchor: None,
        kind_filter: Vec::new(),
        predicate_filter: Vec::new(),
        time_filter: None,
        confidence_min: None,
        include_tombstoned: false,
        include_superseded: false,
        limit: 20,
        retrievers: RetrieverSelection::Auto,
        fusion_config: None,
    }
}

fn build_plan(req: &PlannerQueryRequest) -> QueryPlan {
    plan(req).expect("plan")
}

// ---------------------------------------------------------------------------
// Benches.
// ---------------------------------------------------------------------------

/// Hybrid 3-retriever end-to-end: text + entity anchor invites
/// Semantic, Lexical, and Graph. §16/02 §2.10 target p50 10 ms /
/// p99 50 ms (production wall-time gate is phase 14; this bench
/// measures plan/fuse/filter/project glue with mocked retrievers).
fn bench_hybrid_three_retriever(c: &mut Criterion) {
    let semantic_items = make_items(TOP_N, |i| {
        RankedItemId::Memory(MemoryId::from_raw(i as u128))
    });
    let lexical_items = make_items(TOP_N, |i| {
        RankedItemId::Memory(MemoryId::from_raw((TOP_N + i) as u128))
    });
    let graph_items = make_items(TOP_N / 2, |_| RankedItemId::Entity(EntityId::new()));

    let fx = build_fixture(Some(semantic_items), Some(lexical_items), Some(graph_items));
    let req = text_anchor_request();
    let plan = build_plan(&req);

    c.bench_function("hybrid_three_retriever", |b| {
        b.iter(|| {
            let result =
                execute(black_box(&plan), black_box(&req), black_box(&fx.ctx)).expect("execute");
            black_box(result.items.len());
        });
    });
}

/// Router-degraded path: text-only query → router picks Semantic +
/// Lexical (no Graph). §16/02 §2.10 target p50 7 ms / p99 30 ms.
fn bench_hybrid_router_degraded(c: &mut Criterion) {
    let semantic_items = make_items(TOP_N, |i| {
        RankedItemId::Memory(MemoryId::from_raw(i as u128))
    });
    let lexical_items = make_items(TOP_N, |i| {
        RankedItemId::Memory(MemoryId::from_raw((TOP_N + i) as u128))
    });
    let fx = build_fixture(Some(semantic_items), Some(lexical_items), None);
    let req = text_only_request();
    let plan = build_plan(&req);

    c.bench_function("hybrid_router_degraded", |b| {
        b.iter(|| {
            let result =
                execute(black_box(&plan), black_box(&req), black_box(&fx.ctx)).expect("execute");
            black_box(result.items.len());
        });
    });
}

/// EXPLAIN — `plan(req)` only. No retrievers invoked. §16/02 §2.10
/// target p50 500 µs / p99 2 ms.
fn bench_explain_plan_only(c: &mut Criterion) {
    let req = text_anchor_request();

    c.bench_function("explain_plan_only", |b| {
        b.iter(|| {
            let qp = plan(black_box(&req)).expect("plan");
            black_box(qp.estimated_cost_ms);
        });
    });
}

criterion_group!(
    hybrid_query,
    bench_hybrid_three_retriever,
    bench_hybrid_router_degraded,
    bench_explain_plan_only,
);
criterion_main!(hybrid_query);
