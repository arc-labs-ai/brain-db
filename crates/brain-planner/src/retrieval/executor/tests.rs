//! Unit tests for the retrieval query executor.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::thread;
use std::time::Duration;

use brain_core::{AgentId, ContextId, Entity, EntityId, EntityType, MemoryId, MemoryKind};
use brain_index::{
    GraphError, GraphQuery, GraphRetriever, GraphRetrieverConfig, LexicalError, LexicalQuery,
    LexicalRetriever, LexicalRetrieverConfig, LexicalScope, RankedItem, RankedItemId,
    SemanticError, SemanticQuery, SemanticRetriever, SemanticRetrieverConfig, SemanticScope,
};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::MetadataDb;
use tempfile::TempDir;

use super::{execute, QueryMetadata, QueryResult, RetrievalExecutorContext, RetrieverStatus};
use crate::retrieval::planner::{plan, RetrieverConfig};
use crate::retrieval::router::{GraphAnchorMode, QueryRequest, Retriever, RetrieverSelection};

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

/// Semantic mock whose result depth tracks the config — it returns one
/// ranked hit per requested `top_k` slot (slots `1..=top_k`) and counts
/// invocations. Lets the dynamic-k tests observe both the deeper page a
/// re-query produces and whether a second pass ran at all.
#[derive(Clone)]
struct CountingSemantic {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl SemanticRetriever for CountingSemantic {
    fn retrieve(
        &self,
        _query: &SemanticQuery,
        _scope: SemanticScope,
        config: &SemanticRetrieverConfig,
    ) -> Result<Vec<RankedItem>, SemanticError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let items = (1..=config.top_k as u64)
            .map(|slot| ranked_memory(slot, slot as u32, 0.9))
            .collect();
        Ok(items)
    }
}

/// Seed only the given slots as live ACTIVE rows. Unlike
/// [`seed_active_memories`] (a contiguous range) this leaves gaps so the
/// always-on tombstone filter thins a retriever's page predictably.
fn seed_active_slots(metadata: &mut MetadataDb, slots: impl IntoIterator<Item = u64>) {
    let wtxn = metadata.write_txn().expect("wtxn");
    {
        let mut t = wtxn.open_table(MEMORIES_TABLE).expect("open");
        for slot in slots {
            let id = MemoryId::pack(0, slot, 0);
            let row = MemoryMetadata::new_active(
                id,
                AgentId::new(),
                ContextId::from(0),
                id.slot(),
                id.version(),
                MemoryKind::Semantic,
                [0u8; 16],
                0.5,
                0,
                0,
            );
            t.insert(&id.raw().to_be_bytes(), &row).expect("insert");
        }
    }
    wtxn.commit().expect("commit");
}

/// Seed `slots` as live (ACTIVE) memory rows so the executor's
/// always-on tombstone filter keeps mock retriever hits — it drops any
/// candidate whose row is absent (`memory_active` → None → drop). Ids
/// mirror the `MemoryId::pack(0, slot, 0)` the mock retrievers emit.
fn seed_active_memories(metadata: &mut MetadataDb, slots: std::ops::Range<u64>) {
    let wtxn = metadata.write_txn().expect("wtxn");
    {
        let mut t = wtxn.open_table(MEMORIES_TABLE).expect("open");
        for slot in slots {
            let id = MemoryId::pack(0, slot, 0);
            let row = MemoryMetadata::new_active(
                id,
                AgentId::new(),
                ContextId::from(0),
                id.slot(),
                id.version(),
                MemoryKind::Semantic,
                [0u8; 16],
                0.5,
                0,
                0,
            );
            t.insert(&id.raw().to_be_bytes(), &row).expect("insert");
        }
    }
    wtxn.commit().expect("commit");
}

fn make_ctx(
    semantic: Option<MockSemantic>,
    lexical: Option<MockLexical>,
    graph: Option<MockGraph>,
) -> (TempDir, RetrievalExecutorContext) {
    let dir = TempDir::new().expect("tempdir");
    let mut metadata = MetadataDb::open(dir.path().join("metadata.redb")).expect("open");
    // The executor's always-on tombstone filter drops any memory hit
    // whose row is absent from metadata (`memory_active` → None → drop).
    // The mock retrievers return ids `MemoryId::pack(0, slot, 0)` for
    // small slots, so seed those slots as live rows; otherwise every
    // fused candidate is filtered out and the executor returns nothing.
    seed_active_memories(&mut metadata, 0..256);
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
    let ctx = RetrievalExecutorContext {
        semantic: sem_arc,
        lexical: lex_arc,
        graph: graph_arc,
        metadata: Arc::new(metadata),
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
    let result: QueryResult =
        futures_lite::future::block_on(execute(&qp, &req, false, &ctx)).expect("execute");

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
    let result = futures_lite::future::block_on(execute(&qp, &req, false, &ctx)).expect("execute");

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
    let result = futures_lite::future::block_on(execute(&qp, &req, false, &ctx)).expect("execute");

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
    let result = futures_lite::future::block_on(execute(&qp, &req, false, &ctx)).expect("execute");

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
    let result = futures_lite::future::block_on(execute(&qp, &req, false, &ctx)).expect("execute");
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
    let result = futures_lite::future::block_on(execute(&qp, &req, false, &ctx)).expect("execute");

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

    let req = QueryRequest {
        text: Some("topic".into()),
        retrievers: RetrieverSelection::Explicit(vec![Retriever::Semantic]),
        ..Default::default()
    };
    // Force a tight semantic budget so the mock's 60 ms delay trips the
    // soft timeout deterministically — the planner default is 1 s, far
    // above any sleep we'd want in a unit test.
    let mut qp = plan(&req).expect("plan");
    for r in &mut qp.retrievers {
        if let RetrieverConfig::Semantic { timeout_ms, .. } = &mut r.config {
            *timeout_ms = 10;
        }
    }
    let result = futures_lite::future::block_on(execute(&qp, &req, false, &ctx)).expect("execute");
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
    let result = futures_lite::future::block_on(execute(&qp, &req, false, &ctx)).expect("execute");

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
    let result = futures_lite::future::block_on(execute(&qp, &req, false, &ctx)).expect("execute");
    assert_eq!(result.items.len(), 1);
    assert_eq!(
        outcome_status(&result.metadata, Retriever::Semantic),
        Some(RetrieverStatus::Success)
    );
}

#[test]
fn lexical_terms_drop_stopwords_and_question_words() {
    let terms = super::lexical_content_terms("When did Caroline go to the LGBTQ support group?");
    assert_eq!(terms, vec!["caroline", "go", "lgbtq", "support", "group"]);
}

#[test]
fn lexical_terms_dedup_preserves_first_seen_order() {
    let terms = super::lexical_content_terms("Paris loves Paris and London");
    // "and" dropped; second "paris" deduped; order preserved.
    assert_eq!(terms, vec!["paris", "loves", "london"]);
}

#[test]
fn lexical_terms_preserve_inner_apostrophe_and_hyphen() {
    let terms = super::lexical_content_terms("Caroline's co-worker, Bob.");
    assert_eq!(terms, vec!["caroline's", "co-worker", "bob"]);
}

#[test]
fn lexical_terms_all_stopwords_falls_back_to_raw_split() {
    // A cue made entirely of stopwords must not yield an empty term
    // set; the raw whitespace split is preserved as a fallback.
    let terms = super::lexical_content_terms("What did you do?");
    assert_eq!(terms, vec!["What", "did", "you", "do?"]);
}

// ---------------------------------------------------------------------------
// Dynamic-k runtime deepening.
// ---------------------------------------------------------------------------

#[test]
fn dynamic_k_deepens_when_filters_thin_the_pool() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::retrieval::planner::RetrieverConfig;

    // Only every 10th slot is a live row, so the tombstone filter keeps
    // ~top_n/10 of a returned page. A 100-deep first pass yields 10
    // survivors (< limit 15); the deeper 200-pass yields 20 (truncated
    // to 15). Without deepening the caller would get only 10.
    let dir = TempDir::new().expect("tempdir");
    let mut metadata = MetadataDb::open(dir.path().join("metadata.redb")).expect("open");
    seed_active_slots(&mut metadata, (10..=400).step_by(10).map(|s| s as u64));

    let calls = Arc::new(AtomicUsize::new(0));
    let ctx = RetrievalExecutorContext {
        semantic: Arc::new(CountingSemantic {
            calls: calls.clone(),
        }),
        lexical: Arc::new(MockLexical {
            response: Arc::new(StdMutex::new(Ok(Vec::new()))),
        }),
        graph: Arc::new(MockGraph {
            response: Arc::new(StdMutex::new(Ok(Vec::new()))),
        }),
        metadata: Arc::new(metadata),
        cross_encoder: None,
    };

    let req = QueryRequest {
        text: Some("budget".into()),
        retrievers: RetrieverSelection::Explicit(vec![Retriever::Semantic]),
        limit: 15,
        ..Default::default()
    };
    let mut qp = plan(&req).expect("plan");
    // Pin the first pass below the ceiling so there is room to deepen,
    // independent of how the router classified the query.
    for r in &mut qp.retrievers {
        r.top_n = 100;
        if let RetrieverConfig::Semantic { ef_search, .. } = &mut r.config {
            *ef_search = 100;
        }
    }

    let result = futures_lite::future::block_on(execute(&qp, &req, false, &ctx)).expect("execute");

    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "under-target + saturated first pass should trigger one deeper re-query"
    );
    assert_eq!(
        result.items.len(),
        15,
        "the deeper pass surfaces enough survivors to fill the limit"
    );
}

#[test]
fn dynamic_k_no_deepen_when_first_pass_fills_limit() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::retrieval::planner::RetrieverConfig;

    // All returned slots are live (default 0..256 seed), so a 100-deep
    // first pass already returns far more survivors than the limit. No
    // saturation-driven deepening: the retriever runs exactly once.
    let (_dir, ctx) = make_ctx(
        Some(MockSemantic {
            // placeholder; replaced below with the counting mock via ctx
            response: Arc::new(StdMutex::new(Ok(Vec::new()))),
            delay: None,
        }),
        None,
        None,
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let ctx = RetrievalExecutorContext {
        semantic: Arc::new(CountingSemantic {
            calls: calls.clone(),
        }),
        ..ctx
    };

    let req = QueryRequest {
        text: Some("budget".into()),
        retrievers: RetrieverSelection::Explicit(vec![Retriever::Semantic]),
        limit: 10,
        ..Default::default()
    };
    let mut qp = plan(&req).expect("plan");
    for r in &mut qp.retrievers {
        r.top_n = 100;
        if let RetrieverConfig::Semantic { ef_search, .. } = &mut r.config {
            *ef_search = 100;
        }
    }

    let result = futures_lite::future::block_on(execute(&qp, &req, false, &ctx)).expect("execute");

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a first pass that already fills the limit must not re-query"
    );
    assert_eq!(result.items.len(), 10);
}

// ---------------------------------------------------------------------------
// Cue→anchor entity-graph expansion.
// ---------------------------------------------------------------------------

fn graph_anchor_mode_of(plan: &crate::retrieval::planner::QueryPlan) -> Option<GraphAnchorMode> {
    plan.retrievers.iter().find_map(|r| match &r.config {
        RetrieverConfig::Graph { anchor_mode, .. } => Some(*anchor_mode),
        _ => None,
    })
}

fn cue_ctx(metadata: MetadataDb) -> RetrievalExecutorContext {
    RetrievalExecutorContext {
        semantic: Arc::new(MockSemantic {
            response: Arc::new(StdMutex::new(Ok(Vec::new()))),
            delay: None,
        }),
        lexical: Arc::new(MockLexical {
            response: Arc::new(StdMutex::new(Ok(Vec::new()))),
        }),
        graph: Arc::new(MockGraph {
            response: Arc::new(StdMutex::new(Ok(Vec::new()))),
        }),
        metadata: Arc::new(metadata),
        cross_encoder: None,
    }
}

fn cue_request(text: &str) -> QueryRequest {
    QueryRequest {
        text: Some(text.into()),
        retrievers: RetrieverSelection::Explicit(vec![Retriever::Semantic, Retriever::Graph]),
        ..Default::default()
    }
}

#[test]
fn cue_anchor_upgrades_blind_graph_to_resolved_entity() {
    let dir = TempDir::new().expect("tempdir");
    let metadata = MetadataDb::open(dir.path().join("metadata.redb")).expect("open");
    let sarah = Entity::new_active(
        EntityId::new(),
        EntityType::PERSON_ID,
        "Sarah".to_owned(),
        brain_metadata::normalize_name("Sarah"),
        0,
    );
    let sarah_id = sarah.id;
    {
        let wtxn = metadata.write_txn().expect("wtxn");
        brain_metadata::entity_put(&wtxn, &sarah).expect("put");
        wtxn.commit().expect("commit");
    }
    let ctx = cue_ctx(metadata);

    let req = cue_request("Tell me about Sarah");
    let qp = plan(&req).expect("plan");
    // A blind memory-from-semantic graph lane is what the resolver upgrades.
    assert_eq!(
        graph_anchor_mode_of(&qp),
        Some(GraphAnchorMode::MemoryFromSemantic)
    );

    let rewritten = super::resolve_cue_anchor(&qp, &req, &ctx).expect("Sarah resolves");
    assert_eq!(
        graph_anchor_mode_of(&rewritten),
        Some(GraphAnchorMode::MemoryFromEntityCue(sarah_id)),
        "the named subject becomes the graph anchor"
    );
}

#[test]
fn cue_anchor_falls_back_when_no_entity_matches() {
    let dir = TempDir::new().expect("tempdir");
    let metadata = MetadataDb::open(dir.path().join("metadata.redb")).expect("open");
    let ctx = cue_ctx(metadata);

    let req = cue_request("Tell me about Nobody");
    let qp = plan(&req).expect("plan");
    // No entity named in the query exists → keep the original plan.
    assert!(super::resolve_cue_anchor(&qp, &req, &ctx).is_none());
}

#[test]
fn cue_anchor_falls_back_on_two_distinct_entities() {
    let dir = TempDir::new().expect("tempdir");
    let metadata = MetadataDb::open(dir.path().join("metadata.redb")).expect("open");
    for name in ["Sarah", "Aurora"] {
        let e = Entity::new_active(
            EntityId::new(),
            EntityType::PERSON_ID,
            name.to_owned(),
            brain_metadata::normalize_name(name),
            0,
        );
        let wtxn = metadata.write_txn().expect("wtxn");
        brain_metadata::entity_put(&wtxn, &e).expect("put");
        wtxn.commit().expect("commit");
    }
    let ctx = cue_ctx(metadata);

    // Two known entities named → ambiguous for a single-anchor walk → fall back.
    let req = cue_request("Did Sarah meet Aurora");
    let qp = plan(&req).expect("plan");
    assert!(super::resolve_cue_anchor(&qp, &req, &ctx).is_none());
}

// ---------------------------------------------------------------------------
// Pseudo-relevance feedback (non-LLM read-time query expansion).
// ---------------------------------------------------------------------------

/// Lexical mock that records the term set of every call and replays a
/// queued response per call (the last response repeats once drained).
/// Lets a PRF test observe both the bare probe and the expanded re-probe.
#[derive(Clone)]
struct RecordingLexical {
    captured: Arc<StdMutex<Vec<Vec<String>>>>,
    responses: Arc<StdMutex<std::collections::VecDeque<Vec<RankedItem>>>>,
}

impl LexicalRetriever for RecordingLexical {
    fn retrieve(
        &self,
        query: &LexicalQuery,
        _scope: LexicalScope,
        _config: &LexicalRetrieverConfig,
    ) -> Result<Vec<RankedItem>, LexicalError> {
        self.captured
            .lock()
            .expect("lock")
            .push(query.terms.clone());
        let mut r = self.responses.lock().expect("lock");
        let resp = if r.len() > 1 {
            r.pop_front().unwrap_or_default()
        } else {
            r.front().cloned().unwrap_or_default()
        };
        Ok(resp)
    }
}

/// Write UTF-8 text rows for the given `(slot, text)` pairs so the PRF
/// feedback harvest (which reads `TEXTS_TABLE`) has something to mine.
fn seed_texts<'a>(metadata: &MetadataDb, rows: impl IntoIterator<Item = (u64, &'a str)>) {
    use brain_metadata::tables::text::TEXTS_TABLE;
    let wtxn = metadata.write_txn().expect("wtxn");
    {
        let mut t = wtxn.open_table(TEXTS_TABLE).expect("open texts");
        for (slot, text) in rows {
            let id = MemoryId::pack(0, slot, 0);
            t.insert(&id.to_be_bytes(), text.as_bytes())
                .expect("insert text");
        }
    }
    wtxn.commit().expect("commit");
}

fn prf_request(text: &str) -> QueryRequest {
    QueryRequest {
        text: Some(text.into()),
        retrievers: RetrieverSelection::Explicit(vec![Retriever::Semantic, Retriever::Lexical]),
        limit: 10,
        ..Default::default()
    }
}

#[test]
fn prf_expansion_terms_keeps_recurring_topical_words() {
    let texts = [
        "a gift from my grandma in my home country sweden",
        "i still miss sweden and the long winters there",
        "we moved away from sweden many years ago",
    ];
    let refs: Vec<&str> = texts.to_vec();
    let query = vec!["caroline".to_string(), "move".to_string()];
    let terms = super::prf_expansion_terms(&refs, &query);
    // "sweden" recurs across all three feedback docs → survives the
    // doc-freq >= 2 bar; single-doc words ("grandma", "winters") do not.
    assert!(terms.contains(&"sweden".to_string()), "got {terms:?}");
    assert!(!terms.contains(&"grandma".to_string()), "got {terms:?}");
    // Query terms are never re-harvested.
    assert!(!terms.contains(&"caroline".to_string()));
    assert!(terms.len() <= super::PRF_EXPANSION_TERMS);
}

#[test]
fn prf_expansion_terms_drops_short_and_stopwords() {
    let texts = ["the of an it sweden id ok", "the of an it sweden by no"];
    let refs: Vec<&str> = texts.to_vec();
    let terms = super::prf_expansion_terms(&refs, &[]);
    // Only "sweden" clears the stopword + min-length filters in both docs.
    assert_eq!(terms, vec!["sweden".to_string()]);
}

#[test]
fn prf_expansion_terms_empty_without_recurrence() {
    // Each topical word appears in exactly one doc → nothing clears the
    // doc-freq >= 2 bar → no expansion (fail-open to the bare query).
    let texts = ["alpha beta", "gamma delta"];
    let refs: Vec<&str> = texts.to_vec();
    assert!(super::prf_expansion_terms(&refs, &[]).is_empty());
}

#[test]
fn merge_lexical_hits_is_recall_additive_and_reranked() {
    let mut existing = vec![ranked_memory(1, 1, 3.0), ranked_memory(2, 2, 1.0)];
    // slot 2 reappears with a higher score; slot 9 is new.
    let expanded = vec![ranked_memory(2, 1, 5.0), ranked_memory(9, 2, 2.0)];
    super::merge_lexical_hits(&mut existing, expanded, 10);
    let ids: Vec<u64> = existing
        .iter()
        .map(|it| match it.id {
            RankedItemId::Memory(m) => m.slot(),
            _ => unreachable!(),
        })
        .collect();
    // No original hit dropped; new id added.
    assert!(ids.contains(&1) && ids.contains(&2) && ids.contains(&9));
    // Re-sorted by (best) score: slot2 (5.0) > slot1 (3.0) > slot9 (2.0).
    assert_eq!(ids, vec![2, 1, 9]);
    // Dense 1-based ranks.
    assert_eq!(existing[0].rank, 1);
    assert_eq!(existing[2].rank, 3);
}

#[test]
fn prf_reprobes_lexical_with_expansion_on_low_specificity_query() {
    let dir = TempDir::new().expect("tempdir");
    let mut metadata = MetadataDb::open(dir.path().join("metadata.redb")).expect("open");
    seed_active_memories(&mut metadata, 0..256);
    seed_texts(
        &metadata,
        [
            (1, "a gift from my grandma in my home country sweden"),
            (2, "i still miss sweden and the long winters there"),
            (3, "we moved away from sweden many years ago"),
        ],
    );

    // Semantic supplies the feedback set (slots 1-3); bare lexical is
    // empty, the expanded re-probe surfaces the answer (slot 42).
    let semantic = MockSemantic {
        response: Arc::new(StdMutex::new(Ok(vec![
            ranked_memory(1, 1, 0.9),
            ranked_memory(2, 2, 0.8),
            ranked_memory(3, 3, 0.7),
        ]))),
        delay: None,
    };
    let captured = Arc::new(StdMutex::new(Vec::new()));
    let mut responses = std::collections::VecDeque::new();
    responses.push_back(Vec::new()); // bare probe: no lexical hit
    responses.push_back(vec![ranked_memory(42, 1, 2.5)]); // expanded probe
    let lexical = RecordingLexical {
        captured: captured.clone(),
        responses: Arc::new(StdMutex::new(responses)),
    };

    let ctx = RetrievalExecutorContext {
        semantic: Arc::new(semantic),
        lexical: Arc::new(lexical),
        graph: Arc::new(MockGraph {
            response: Arc::new(StdMutex::new(Ok(Vec::new()))),
        }),
        metadata: Arc::new(metadata),
        cross_encoder: None,
    };

    let req = prf_request("Where did Caroline move from?");
    let qp = plan(&req).expect("plan");
    let result = futures_lite::future::block_on(execute(&qp, &req, false, &ctx)).expect("execute");

    let calls = captured.lock().expect("lock").clone();
    assert_eq!(calls.len(), 2, "bare probe + one PRF re-probe: {calls:?}");
    assert!(
        calls[1].iter().any(|t| t == "sweden"),
        "re-probe carries the harvested term: {:?}",
        calls[1]
    );
    let has_answer = result.items.iter().any(|it| match it.id {
        RankedItemId::Memory(m) => m.slot() == 42,
        _ => false,
    });
    assert!(has_answer, "PRF-surfaced answer reaches the result set");
}

#[test]
fn prf_skips_high_specificity_query() {
    let dir = TempDir::new().expect("tempdir");
    let mut metadata = MetadataDb::open(dir.path().join("metadata.redb")).expect("open");
    seed_active_memories(&mut metadata, 0..256);
    seed_texts(&metadata, [(1, "sweden sweden sweden")]);

    let semantic = MockSemantic {
        response: Arc::new(StdMutex::new(Ok(vec![ranked_memory(1, 1, 0.9)]))),
        delay: None,
    };
    let captured = Arc::new(StdMutex::new(Vec::new()));
    let mut responses = std::collections::VecDeque::new();
    responses.push_back(vec![ranked_memory(5, 1, 1.0)]);
    let lexical = RecordingLexical {
        captured: captured.clone(),
        responses: Arc::new(StdMutex::new(responses)),
    };
    let ctx = RetrievalExecutorContext {
        semantic: Arc::new(semantic),
        lexical: Arc::new(lexical),
        graph: Arc::new(MockGraph {
            response: Arc::new(StdMutex::new(Ok(Vec::new()))),
        }),
        metadata: Arc::new(metadata),
        cross_encoder: None,
    };

    // Six content words → above the low-specificity gate → no PRF pass.
    let req = prf_request("What special gift did grandma send Caroline from Sweden");
    let qp = plan(&req).expect("plan");
    let _ = futures_lite::future::block_on(execute(&qp, &req, false, &ctx)).expect("execute");

    assert_eq!(
        captured.lock().expect("lock").len(),
        1,
        "high-specificity query must not trigger a PRF re-probe",
    );
}
