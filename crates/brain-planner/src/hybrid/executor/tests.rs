//! Unit tests for the hybrid query executor (phase 23.7).

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::thread;
use std::time::Duration;

use brain_core::{EntityId, MemoryId};
use brain_index::{
    GraphError, GraphQuery, GraphRetriever, GraphRetrieverConfig, LexicalError, LexicalQuery,
    LexicalRetriever, LexicalRetrieverConfig, LexicalScope, RankedItem, RankedItemId,
    SemanticError, SemanticQuery, SemanticRetriever, SemanticRetrieverConfig, SemanticScope,
};
use brain_metadata::MetadataDb;
use parking_lot::Mutex;
use tempfile::TempDir;

use super::{
    execute, HybridExecutorContext, QueryMetadata, QueryResult, RetrieverStatus,
};
use crate::hybrid::planner::plan;
use crate::hybrid::router::{QueryRequest, Retriever, RetrieverSelection};

// ---------------------------------------------------------------------------
// Mock retrievers.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct MockSemantic {
    response: Arc<StdMutex<Result<Vec<RankedItem>, String>>>,
    delay: Option<Duration>,
}

impl SemanticRetriever for MockSemantic {
    fn retrieve(
        &self,
        _query: &SemanticQuery,
        _scope: SemanticScope,
        _config: &SemanticRetrieverConfig,
    ) -> Result<Vec<RankedItem>, SemanticError> {
        if let Some(d) = self.delay {
            thread::sleep(d);
        }
        match self.response.lock().expect("lock").clone() {
            Ok(items) => Ok(items),
            Err(msg) => Err(SemanticError::Internal(msg)),
        }
    }
}

#[derive(Clone)]
struct MockLexical {
    response: Arc<StdMutex<Result<Vec<RankedItem>, String>>>,
}

impl LexicalRetriever for MockLexical {
    fn retrieve(
        &self,
        _query: &LexicalQuery,
        _scope: LexicalScope,
        _config: &LexicalRetrieverConfig,
    ) -> Result<Vec<RankedItem>, LexicalError> {
        match self.response.lock().expect("lock").clone() {
            Ok(items) => Ok(items),
            Err(msg) => Err(LexicalError::Internal(msg)),
        }
    }
}

#[derive(Clone)]
struct MockGraph {
    response: Arc<StdMutex<Result<Vec<RankedItem>, String>>>,
}

impl GraphRetriever for MockGraph {
    fn retrieve(
        &self,
        _query: &GraphQuery,
        _config: &GraphRetrieverConfig,
    ) -> Result<Vec<RankedItem>, GraphError> {
        match self.response.lock().expect("lock").clone() {
            Ok(items) => Ok(items),
            Err(msg) => Err(GraphError::Internal(msg)),
        }
    }
}

fn ranked_memory(slot: u64, rank: u32, score: f32) -> RankedItem {
    RankedItem {
        id: RankedItemId::Memory(MemoryId::pack(0, slot, 0)),
        rank,
        score,
        snippet: None,
    }
}

fn make_ctx(
    semantic: Option<MockSemantic>,
    lexical: Option<MockLexical>,
    graph: Option<MockGraph>,
) -> (TempDir, HybridExecutorContext) {
    let dir = TempDir::new().expect("tempdir");
    let metadata = MetadataDb::open(dir.path().join("metadata.redb")).expect("open");
    let sem_arc: Arc<dyn SemanticRetriever> = match semantic {
        Some(m) => Arc::new(m),
        None => Arc::new(MockSemantic {
            response: Arc::new(StdMutex::new(Ok(Vec::new()))),
            delay: None,
        }),
    };
    let lex_arc: Arc<dyn LexicalRetriever> = match lexical {
        Some(m) => Arc::new(m),
        None => Arc::new(MockLexical {
            response: Arc::new(StdMutex::new(Ok(Vec::new()))),
        }),
    };
    let graph_arc: Arc<dyn GraphRetriever> = match graph {
        Some(m) => Arc::new(m),
        None => Arc::new(MockGraph {
            response: Arc::new(StdMutex::new(Ok(Vec::new()))),
        }),
    };
    let ctx = HybridExecutorContext {
        semantic: sem_arc,
        lexical: lex_arc,
        graph: graph_arc,
        metadata: Arc::new(Mutex::new(metadata)),
        cross_encoder: None,
    };
    (dir, ctx)
}

fn outcome_status(metadata: &QueryMetadata, r: Retriever) -> Option<RetrieverStatus> {
    metadata
        .retriever_outcomes
        .iter()
        .find(|o| o.retriever == r)
        .map(|o| o.status.clone())
}

// ---------------------------------------------------------------------------
// Happy path.
// ---------------------------------------------------------------------------

#[test]
fn executes_single_semantic_retriever() {
    let sem = MockSemantic {
        response: Arc::new(StdMutex::new(Ok(vec![
            ranked_memory(1, 1, 0.95),
            ranked_memory(2, 2, 0.80),
        ]))),
        delay: None,
    };
    let (_dir, ctx) = make_ctx(Some(sem), None, None);

    let req = QueryRequest {
        text: Some("budget".into()),
        retrievers: RetrieverSelection::Explicit(vec![Retriever::Semantic]),
        ..Default::default()
    };
    let qp = plan(&req).expect("plan");
    let result: QueryResult = futures_lite::future::block_on(execute(&qp, &req, &ctx)).expect("execute");

    assert_eq!(result.items.len(), 2);
    assert_eq!(result.metadata.retriever_latencies_ms.len(), 1);
    assert_eq!(
        outcome_status(&result.metadata, Retriever::Semantic),
        Some(RetrieverStatus::Success)
    );
}

#[test]
fn executes_three_retrievers_and_fuses() {
    // Three retrievers each return a hit with the same memory id
    // but different ranks → fused once.
    let same_id = MemoryId::pack(0, 7, 0);
    let item = |rank: u32| RankedItem {
        id: RankedItemId::Memory(same_id),
        rank,
        score: 0.9,
        snippet: None,
    };
    let sem = MockSemantic {
        response: Arc::new(StdMutex::new(Ok(vec![item(1)]))),
        delay: None,
    };
    let lex = MockLexical {
        response: Arc::new(StdMutex::new(Ok(vec![item(2)]))),
    };
    let gr = MockGraph {
        response: Arc::new(StdMutex::new(Ok(vec![item(3)]))),
    };
    let (_dir, ctx) = make_ctx(Some(sem), Some(lex), Some(gr));

    let req = QueryRequest {
        text: Some("topic".into()),
        entity_anchor: Some(EntityId::new()),
        retrievers: RetrieverSelection::Explicit(vec![
            Retriever::Semantic,
            Retriever::Lexical,
            Retriever::Graph,
        ]),
        ..Default::default()
    };
    let qp = plan(&req).expect("plan");
    let result = futures_lite::future::block_on(execute(&qp, &req, &ctx)).expect("execute");

    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].contributing.len(), 3);
    assert_eq!(result.metadata.retriever_outcomes.len(), 3);
}

// ---------------------------------------------------------------------------
// Skips.
// ---------------------------------------------------------------------------

#[test]
fn graph_runs_in_memory_mode_when_no_entity_anchor() {
    // Text + no entity anchor → graph runs in
    // MemoryFromSemantic mode, anchored at semantic top-K.
    // The graph mock returns a hit regardless of input — we
    // assert graph succeeded (not skipped) and its hit shows
    // up in the fused result.
    let sem = MockSemantic {
        response: Arc::new(StdMutex::new(Ok(vec![ranked_memory(1, 1, 0.9)]))),
        delay: None,
    };
    let lex = MockLexical {
        response: Arc::new(StdMutex::new(Ok(vec![ranked_memory(2, 1, 0.9)]))),
    };
    let gr = MockGraph {
        response: Arc::new(StdMutex::new(Ok(vec![ranked_memory(3, 1, 0.9)]))),
    };
    let (_dir, ctx) = make_ctx(Some(sem), Some(lex), Some(gr));

    let req = QueryRequest {
        text: Some("budget".into()),
        // No entity_anchor.
        retrievers: RetrieverSelection::Explicit(vec![
            Retriever::Semantic,
            Retriever::Lexical,
            Retriever::Graph,
        ]),
        ..Default::default()
    };
    let qp = plan(&req).expect("plan");
    let result = futures_lite::future::block_on(execute(&qp, &req, &ctx)).expect("execute");

    assert_eq!(
        outcome_status(&result.metadata, Retriever::Graph),
        Some(RetrieverStatus::Success)
    );
    // Semantic + Lexical + Graph each produced one hit with
    // distinct ids → three fused entries.
    assert_eq!(result.items.len(), 3);
}

#[test]
fn memory_anchor_graph_skips_when_semantic_returns_nothing() {
    // Semantic returns no hits → there are no memory anchors
    // → graph in memory-anchor mode has nothing to walk from.
    // Skipped, not failed: the absence of anchors is a
    // signal, not an error.
    let sem = MockSemantic {
        response: Arc::new(StdMutex::new(Ok(Vec::new()))),
        delay: None,
    };
    let gr = MockGraph {
        response: Arc::new(StdMutex::new(Ok(vec![ranked_memory(3, 1, 0.9)]))),
    };
    let (_dir, ctx) = make_ctx(Some(sem), None, Some(gr));

    let req = QueryRequest {
        text: Some("budget".into()),
        retrievers: RetrieverSelection::Explicit(vec![Retriever::Semantic, Retriever::Graph]),
        ..Default::default()
    };
    let qp = plan(&req).expect("plan");
    let result = futures_lite::future::block_on(execute(&qp, &req, &ctx)).expect("execute");

    assert_eq!(
        outcome_status(&result.metadata, Retriever::Graph),
        Some(RetrieverStatus::Skipped(
            "no memory hits from semantic to anchor graph walk"
        ))
    );
}

#[test]
fn skips_semantic_when_no_text() {
    let sem = MockSemantic {
        response: Arc::new(StdMutex::new(Ok(vec![ranked_memory(1, 1, 0.9)]))),
        delay: None,
    };
    let (_dir, ctx) = make_ctx(Some(sem), None, None);

    // Request has only an anchor (no text). Plan will pick
    // Semantic (Rule 1) but execute should skip semantic
    // because there's no text to embed.
    let req = QueryRequest {
        entity_anchor: Some(EntityId::new()),
        retrievers: RetrieverSelection::Explicit(vec![Retriever::Semantic]),
        ..Default::default()
    };
    let qp = plan(&req).expect("plan");
    let result = futures_lite::future::block_on(execute(&qp, &req, &ctx)).expect("execute");
    assert_eq!(
        outcome_status(&result.metadata, Retriever::Semantic),
        Some(RetrieverStatus::Skipped("no query text"))
    );
    assert!(result.items.is_empty());
}

// ---------------------------------------------------------------------------
// Failures + timeouts.
// ---------------------------------------------------------------------------

#[test]
fn failing_retriever_returns_partial_results() {
    let sem = MockSemantic {
        response: Arc::new(StdMutex::new(Ok(vec![ranked_memory(1, 1, 0.9)]))),
        delay: None,
    };
    let lex = MockLexical {
        response: Arc::new(StdMutex::new(Err("boom".into()))),
    };
    let (_dir, ctx) = make_ctx(Some(sem), Some(lex), None);

    let req = QueryRequest {
        text: Some("topic".into()),
        retrievers: RetrieverSelection::Explicit(vec![Retriever::Semantic, Retriever::Lexical]),
        ..Default::default()
    };
    let qp = plan(&req).expect("plan");
    let result = futures_lite::future::block_on(execute(&qp, &req, &ctx)).expect("execute");

    // Semantic succeeds with one hit; lexical failed →
    // partial fused result.
    assert_eq!(result.items.len(), 1);
    match outcome_status(&result.metadata, Retriever::Lexical) {
        Some(RetrieverStatus::Failure(msg)) => assert!(msg.contains("boom"), "got {msg}"),
        other => panic!("expected Failure, got {other:?}"),
    }
}

#[test]
fn timeout_records_status() {
    let sem = MockSemantic {
        response: Arc::new(StdMutex::new(Ok(vec![ranked_memory(1, 1, 0.9)]))),
        delay: Some(Duration::from_millis(60)),
    };
    let (_dir, ctx) = make_ctx(Some(sem), None, None);

    // Plan's default semantic timeout is 50 ms; the mock sleeps 60.
    let req = QueryRequest {
        text: Some("topic".into()),
        retrievers: RetrieverSelection::Explicit(vec![Retriever::Semantic]),
        ..Default::default()
    };
    let qp = plan(&req).expect("plan");
    let result = futures_lite::future::block_on(execute(&qp, &req, &ctx)).expect("execute");
    assert_eq!(
        outcome_status(&result.metadata, Retriever::Semantic),
        Some(RetrieverStatus::Timeout)
    );
    // Items still included — soft timeout.
    assert_eq!(result.items.len(), 1);
}

// ---------------------------------------------------------------------------
// Metadata sanity.
// ---------------------------------------------------------------------------

#[test]
fn total_latency_at_least_sum_of_per_retriever() {
    let sem = MockSemantic {
        response: Arc::new(StdMutex::new(Ok(vec![ranked_memory(1, 1, 0.9)]))),
        delay: Some(Duration::from_millis(5)),
    };
    let lex = MockLexical {
        response: Arc::new(StdMutex::new(Ok(vec![ranked_memory(2, 1, 0.9)]))),
    };
    let (_dir, ctx) = make_ctx(Some(sem), Some(lex), None);

    let req = QueryRequest {
        text: Some("topic".into()),
        retrievers: RetrieverSelection::Explicit(vec![Retriever::Semantic, Retriever::Lexical]),
        ..Default::default()
    };
    let qp = plan(&req).expect("plan");
    let result = futures_lite::future::block_on(execute(&qp, &req, &ctx)).expect("execute");

    let sum: f64 = result
        .metadata
        .retriever_latencies_ms
        .iter()
        .map(|(_, ms)| *ms)
        .sum();
    assert!(
        result.metadata.total_latency_ms >= sum - 0.5,
        "total {} should be ≥ sum {}",
        result.metadata.total_latency_ms,
        sum,
    );
}

#[test]
fn empty_retriever_result_doesnt_break_fusion() {
    let sem = MockSemantic {
        response: Arc::new(StdMutex::new(Ok(Vec::new()))),
        delay: None,
    };
    let lex = MockLexical {
        response: Arc::new(StdMutex::new(Ok(vec![ranked_memory(1, 1, 0.9)]))),
    };
    let (_dir, ctx) = make_ctx(Some(sem), Some(lex), None);

    let req = QueryRequest {
        text: Some("topic".into()),
        retrievers: RetrieverSelection::Explicit(vec![Retriever::Semantic, Retriever::Lexical]),
        ..Default::default()
    };
    let qp = plan(&req).expect("plan");
    let result = futures_lite::future::block_on(execute(&qp, &req, &ctx)).expect("execute");
    assert_eq!(result.items.len(), 1);
    assert_eq!(
        outcome_status(&result.metadata, Retriever::Semantic),
        Some(RetrieverStatus::Success)
    );
}

#[test]
fn limit_truncates_after_filters() {
    // 5 distinct memory items, all pass filters, limit = 3.
    let items: Vec<RankedItem> = (1..=5).map(|i| ranked_memory(i, i as u32, 0.9)).collect();
    let sem = MockSemantic {
        response: Arc::new(StdMutex::new(Ok(items))),
        delay: None,
    };
    let (_dir, ctx) = make_ctx(Some(sem), None, None);

    let req = QueryRequest {
        text: Some("topic".into()),
        retrievers: RetrieverSelection::Explicit(vec![Retriever::Semantic]),
        limit: 3,
        // Memory rows don't exist in the metadata DB, so the
        // tombstone filter would drop them. Allow tombstoned
        // to keep this test focused on truncation.
        include_tombstoned: true,
        include_superseded: true,
        ..Default::default()
    };
    let qp = plan(&req).expect("plan");
    let result = futures_lite::future::block_on(execute(&qp, &req, &ctx)).expect("execute");
    assert_eq!(result.items.len(), 3);
}
