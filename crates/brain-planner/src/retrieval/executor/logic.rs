//! Retrieval query executor.
//!
//! Consumes a `QueryPlan` + a `RetrievalExecutorContext`
//! (Arc handles to the three retrievers plus the `MetadataDb`
//! the filter chain reads), invokes each retriever per its
//! `PlannedRetriever` config, fuses via RRF, applies the
//! post-fusion filter chain, truncates to `limit`, and
//! returns a `QueryResult` with per-retriever latency and
//! outcome metadata for EXPLAIN/TRACE.
//!
//! Independent retrievers fan out concurrently — each
//! `invoke_retriever` call is wrapped in a future and the
//! whole set is driven via a small `join_all_local` helper.
//! The one dependency the executor honours is the memory-
//! anchor graph mode: when `GraphAnchorMode::MemoryFromSemantic`
//! is in the plan, the executor runs semantic eagerly and
//! feeds its top-K into the graph walk. Everything else runs
//! in parallel.
//!
//! Honest characterisation: the retriever traits are still
//! synchronous, so concurrent execution on a single-thread
//! Glommio executor gives **interleaving**, not true
//! parallelism. CPU-bound retrievers (HNSW search) only overlap
//! at task-poll boundaries. I/O-bound retrievers (Tantivy mmap
//! cold reads, redb cold reads) yield to the kernel inside
//! their syscalls and produce real overlap. The structural
//! fan-out is in place either way; the win arrives without
//! a planner / wire change when retrievers grow async I/O.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use brain_index::{
    GraphAnchor, GraphQuery, GraphRetriever, GraphRetrieverConfig, LexicalFilters, LexicalQuery,
    LexicalRetriever, LexicalRetrieverConfig, LexicalScope, RankedItem, RankedItemId,
    SemanticFilters, SemanticFiltersConfigSlot, SemanticQuery, SemanticRetriever,
    SemanticRetrieverConfig, SemanticScope,
};
use brain_metadata::MetadataDb;
use brain_rerank::RerankService;
use futures_lite::future::poll_fn;

use crate::retrieval::filters::{apply_filter_chain, FilterChainStats, FilterError};
use crate::retrieval::fusion::{fuse, FusedItem};
use crate::retrieval::planner::{PreFilter, QueryPlan, RetrieverConfig};
use crate::retrieval::rerank::{rerank_top_n, RerankCandidate, RERANK_TOP_N};
use crate::retrieval::router::{GraphAnchorMode, QueryRequest, Retriever};

// ---------------------------------------------------------------------------
// Public types.
// ---------------------------------------------------------------------------

/// What the executor needs beyond a `QueryPlan`. Built from
/// `OpsContext`'s retriever slots in the caller. The three
/// retrievers are mandatory: every spawned shard wires real
/// impls and tests provide mocks.
#[derive(Clone)]
pub struct RetrievalExecutorContext {
    pub semantic: Arc<dyn SemanticRetriever>,
    pub lexical: Arc<dyn LexicalRetriever>,
    pub graph: Arc<dyn GraphRetriever>,
    pub metadata: Arc<MetadataDb>,
    /// Off-core cross-encoder handle for the always-on rerank pass.
    /// When `Some`, the executor reranks the top fused candidates on
    /// every query — there is no per-request opt-in. The forward pass
    /// runs on the service's dedicated thread, so the shard core never
    /// blocks on it. When `None` (the operator set
    /// `config.rerank.enabled = false`, or no model is on disk) the
    /// rerank stage is skipped and RRF order wins. No error either way.
    pub cross_encoder: Option<Arc<RerankService>>,
}

/// Final retrieval-query result.
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub items: Vec<FusedItem>,
    pub metadata: QueryMetadata,
}

/// Per-execution observability data — surfaces in EXPLAIN/TRACE.
/// Operators read this to see which retriever was slow, which filter
/// narrowed results most, total wall-time, etc.
#[derive(Debug, Clone, Default)]
pub struct QueryMetadata {
    pub retriever_latencies_ms: Vec<(Retriever, f64)>,
    pub retriever_outcomes: Vec<RetrieverOutcome>,
    pub retriever_total_results: Vec<(Retriever, usize)>,
    pub filter_stats: FilterChainStats,
    pub total_latency_ms: f64,
    /// Outcome of the always-on cross-encoder rerank stage. `None`
    /// means the cross-encoder isn't loaded on this shard (operator
    /// opted out, or no model on disk) so the result is RRF-only.
    pub rerank: Option<RerankOutcome>,
}

/// What the rerank stage did. Surfaces in trace output so callers
/// can see whether the cross-encoder re-sorted the fused list. Only
/// produced when the cross-encoder is loaded; a shard with rerank
/// disabled leaves `QueryMetadata.rerank` as `None`.
#[derive(Debug, Clone, PartialEq)]
pub enum RerankOutcome {
    /// Cross-encoder ran and re-sorted the fused list.
    Applied { candidates: usize, latency_ms: f64 },
    /// Cross-encoder is loaded but the fused list had no candidates
    /// with fetchable text (no query text, tombstoned mid-query, or
    /// non-memory variants only). RRF order returned unchanged.
    SkippedNoCandidates,
}

#[derive(Debug, Clone)]
pub struct RetrieverOutcome {
    pub retriever: Retriever,
    pub status: RetrieverStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetrieverStatus {
    Success,
    /// Skipped because the request didn't have the right
    /// signal for this retriever (e.g. graph w/o anchor).
    /// `&'static str` reason surfaces in TRACE.
    Skipped(&'static str),
    /// Took longer than `config.timeout_ms`. Items still
    /// included; warn-logged. Hard cancellation deferred.
    Timeout,
    /// Retriever returned `Err(...)`. Items dropped from
    /// fusion; warn-logged. Other retrievers still
    /// contribute.
    Failure(String),
}

#[derive(Debug, thiserror::Error)]
pub enum ExecutionError {
    #[error("filter chain: {0}")]
    Filter(#[from] FilterError),
}

// ---------------------------------------------------------------------------
// Entry point.
// ---------------------------------------------------------------------------

/// Run a plan end-to-end. Returns the fused-then-filtered
/// result plus metadata.
///
/// Independent retrievers fan out concurrently. The one
/// exception is the memory-anchor graph mode: when the
/// schemaless retrieval path selects graph without an entity
/// anchor, the executor must run semantic first and feed its
/// top-K memory ids into the graph walk. We detect this
/// up-front, run semantic eagerly, stash its output, and let
/// the fan-out reuse it instead of re-invoking.
pub async fn execute(
    plan: &QueryPlan,
    request: &QueryRequest,
    ctx: &RetrievalExecutorContext,
) -> Result<QueryResult, ExecutionError> {
    let total_started = Instant::now();

    // Pre-run semantic if graph depends on it. Without this the
    // fan-out would invoke graph with no anchors and either skip
    // or error.
    let needs_semantic_first = plan.retrievers.iter().any(|r| {
        matches!(
            &r.config,
            RetrieverConfig::Graph {
                anchor_mode: GraphAnchorMode::MemoryFromSemantic,
                ..
            }
        )
    });
    let pre_semantic_planned = if needs_semantic_first {
        plan.retrievers
            .iter()
            .find(|r| r.retriever == Retriever::Semantic)
            .cloned()
    } else {
        None
    };
    let mut cached_semantic: Option<Vec<RankedItem>> = None;
    let mut pre_semantic_latency_ms: f64 = 0.0;
    let mut pre_semantic_invocation: Option<Result<Vec<RankedItem>, RetrieverInvocationError>> =
        None;
    if let Some(sem) = &pre_semantic_planned {
        let started = Instant::now();
        let invocation = invoke_retriever(sem, request, ctx, None);
        pre_semantic_latency_ms = started.elapsed().as_secs_f64() * 1000.0;
        if let Ok(items) = &invocation {
            cached_semantic = Some(items.clone());
        }
        pre_semantic_invocation = Some(invocation);
    }

    // Build one future per planned retriever. Each future
    // returns `(index, elapsed_ms, invocation)` so we can
    // rebuild the per-retriever bookkeeping in plan order
    // after the fan-out.
    //
    // For the semantic-reuse case (semantic already ran
    // eagerly) the corresponding future returns the cached
    // result with the eager-run latency attribution intact —
    // no second HNSW search.
    type FanoutFut<'a> = Pin<
        Box<
            dyn Future<
                    Output = (
                        usize,
                        f64,
                        Result<Vec<RankedItem>, RetrieverInvocationError>,
                    ),
                > + 'a,
        >,
    >;
    let mut futures: Vec<FanoutFut<'_>> = Vec::with_capacity(plan.retrievers.len());
    for (idx, planned) in plan.retrievers.iter().enumerate() {
        if planned.retriever == Retriever::Semantic && pre_semantic_invocation.is_some() {
            let invocation = pre_semantic_invocation
                .take()
                .expect("invariant: guarded by is_some check above");
            let elapsed_ms = pre_semantic_latency_ms;
            futures.push(Box::pin(async move { (idx, elapsed_ms, invocation) }));
            continue;
        }
        let pre_anchors = if planned.retriever == Retriever::Graph
            && matches!(
                &planned.config,
                RetrieverConfig::Graph {
                    anchor_mode: GraphAnchorMode::MemoryFromSemantic,
                    ..
                }
            ) {
            cached_semantic.as_deref()
        } else {
            None
        };
        futures.push(Box::pin(async move {
            let started = Instant::now();
            let invocation = invoke_retriever(planned, request, ctx, pre_anchors);
            let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
            (idx, elapsed_ms, invocation)
        }));
    }

    let mut fanout_results = join_all_local(futures).await;
    // `join_all_local` preserves submission order, but sort
    // defensively so any future shuffling stays correct.
    fanout_results.sort_by_key(|(idx, _, _)| *idx);

    // Rebuild per-retriever bookkeeping in plan order.
    let mut outputs: Vec<(Retriever, Vec<RankedItem>)> = Vec::new();
    let mut latencies: Vec<(Retriever, f64)> = Vec::with_capacity(plan.retrievers.len());
    let mut outcomes: Vec<RetrieverOutcome> = Vec::with_capacity(plan.retrievers.len());
    let mut totals: Vec<(Retriever, usize)> = Vec::with_capacity(plan.retrievers.len());
    for (idx, elapsed_ms, invocation) in fanout_results {
        let planned = &plan.retrievers[idx];
        latencies.push((planned.retriever, elapsed_ms));
        match invocation {
            Ok(items) => {
                totals.push((planned.retriever, items.len()));
                let status = if elapsed_ms > f64::from(timeout_ms(&planned.config)) {
                    tracing::warn!(
                        target: "brain_planner::executor",
                        retriever = ?planned.retriever,
                        elapsed_ms,
                        budget_ms = timeout_ms(&planned.config),
                        "retriever exceeded soft timeout",
                    );
                    RetrieverStatus::Timeout
                } else {
                    RetrieverStatus::Success
                };
                outcomes.push(RetrieverOutcome {
                    retriever: planned.retriever,
                    status,
                });
                outputs.push((planned.retriever, items));
            }
            Err(RetrieverInvocationError::Skipped(reason)) => {
                totals.push((planned.retriever, 0));
                outcomes.push(RetrieverOutcome {
                    retriever: planned.retriever,
                    status: RetrieverStatus::Skipped(reason),
                });
            }
            Err(RetrieverInvocationError::Failure(msg)) => {
                tracing::warn!(
                    target: "brain_planner::executor",
                    retriever = ?planned.retriever,
                    error = %msg,
                    "retriever failed; continuing with partial results",
                );
                totals.push((planned.retriever, 0));
                outcomes.push(RetrieverOutcome {
                    retriever: planned.retriever,
                    status: RetrieverStatus::Failure(msg),
                });
            }
        }
    }

    let fused = fuse(
        &outputs,
        plan.fusion.k,
        &plan.fusion.weights,
        plan.fusion.method,
    );
    let fused_len = fused.len();

    // Per-candidate fusion breakdown for deep diagnosis: which
    // retrievers brought each top hit in, at what rank and raw score.
    if tracing::enabled!(tracing::Level::DEBUG) {
        for (i, f) in fused.iter().take(8).enumerate() {
            let contribs: Vec<String> = f
                .contributing
                .iter()
                .map(|c| format!("{:?}#{}={:.3}", c.retriever, c.rank, c.raw_score))
                .collect();
            tracing::debug!(
                target: "brain_planner::executor",
                rank = i + 1,
                id = ?f.id,
                fused_score = f.fused_score,
                contributions = %contribs.join(" "),
                "fused candidate",
            );
        }
    }

    // Rerank is always-on: the stage fires whenever the shard has a
    // cross-encoder loaded, regardless of any request field. When
    // the operator disabled the load (`cross_encoder` is `None`),
    // the result is RRF-only with no error.
    let (fused_after_rerank, rerank_outcome) = if ctx.cross_encoder.is_some() {
        rerank_stage(fused, request, ctx).await
    } else {
        (fused, None)
    };

    let (items, filter_stats) = apply_filter_chain(
        fused_after_rerank,
        &plan.post_filters,
        ctx.metadata.as_ref(),
        plan.limit,
    )?;

    let total_latency_ms = total_started.elapsed().as_secs_f64() * 1000.0;

    // One-line recall summary. Counts where candidates came from and
    // where they survived to, so an empty/odd result is attributable
    // to a specific stage (retriever returned 0, fusion, or a filter).
    tracing::info!(
        target: "brain_planner::executor",
        query = request.text.as_deref().unwrap_or(""),
        class = ?plan.routing.query_class,
        fusion = ?plan.fusion.method,
        per_retriever = ?totals,
        outcomes = ?outcomes,
        fused = fused_len,
        filter = %format!(
            "before={} type={} temporal={} conf={} tomb={} sup={} limit={}",
            filter_stats.before,
            filter_stats.after_type,
            filter_stats.after_temporal,
            filter_stats.after_confidence,
            filter_stats.after_tombstone,
            filter_stats.after_supersession,
            filter_stats.after_limit,
        ),
        returned = items.len(),
        latency_ms = total_latency_ms,
        "recall executed",
    );

    Ok(QueryResult {
        items,
        metadata: QueryMetadata {
            retriever_latencies_ms: latencies,
            retriever_outcomes: outcomes,
            retriever_total_results: totals,
            filter_stats,
            total_latency_ms,
            rerank: rerank_outcome,
        },
    })
}

/// Run the cross-encoder rerank pass over the head of the fused
/// list. Fetches text for each in-window memory hit from the
/// per-shard `texts` table, scores the pairs on the off-core rerank
/// service (the shard core is parked, not blocked, while the model
/// runs), then delegates the re-sort to `rerank::rerank_top_n`.
/// Returns the (possibly re-ordered) fused list plus an outcome tag
/// for `QueryMetadata`.
async fn rerank_stage(
    fused: Vec<FusedItem>,
    request: &QueryRequest,
    ctx: &RetrievalExecutorContext,
) -> (Vec<FusedItem>, Option<RerankOutcome>) {
    // Only reached when `ctx.cross_encoder` is `Some` (the caller in
    // `execute` gates on it), so the service is guaranteed present.
    let Some(service) = ctx.cross_encoder.as_ref() else {
        return (fused, None);
    };
    let Some(query) = request.text.as_deref() else {
        // No query text → rerank has no `query` half of the pair.
        // Treat as "no candidates".
        return (fused, Some(RerankOutcome::SkippedNoCandidates));
    };

    // Walk the head of the fused list collecting memory hits up to
    // RERANK_TOP_N; fetch their text in a single read transaction.
    let head_ids: Vec<brain_core::MemoryId> = fused
        .iter()
        .take(RERANK_TOP_N)
        .filter_map(|item| match item.id {
            RankedItemId::Memory(m) => Some(m),
            _ => None,
        })
        .collect();

    if head_ids.is_empty() {
        return (fused, Some(RerankOutcome::SkippedNoCandidates));
    }

    let candidates = match fetch_texts(&head_ids, ctx) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                target: "brain_planner::executor",
                error = %e,
                "rerank text fetch failed; returning RRF-only ranking",
            );
            return (fused, Some(RerankOutcome::SkippedNoCandidates));
        }
    };
    if candidates.is_empty() {
        return (fused, Some(RerankOutcome::SkippedNoCandidates));
    }

    let candidate_count = candidates.len();
    let started = std::time::Instant::now();
    let texts: Vec<&str> = candidates.iter().map(|c| c.text.as_str()).collect();
    // Score off-core: `score_pairs` parks this task on the reply
    // channel while the worker thread runs the forward pass, so the
    // shard keeps serving other requests meanwhile.
    let scores = match service.score_pairs(query, &texts).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                target: "brain_planner::executor",
                error = %e,
                "cross-encoder scoring failed; returning RRF-only ranking",
            );
            return (fused, Some(RerankOutcome::SkippedNoCandidates));
        }
    };
    let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
    let reranked = rerank_top_n(&scores, fused, &candidates);

    (
        reranked,
        Some(RerankOutcome::Applied {
            candidates: candidate_count,
            latency_ms,
        }),
    )
}

/// Fetch text for each `MemoryId` in `ids` from the per-shard
/// `texts` table. Misses (tombstoned mid-query, never had text)
/// are silently skipped; the returned `Vec` may be shorter than
/// `ids.len()`. Order is preserved.
fn fetch_texts(
    ids: &[brain_core::MemoryId],
    ctx: &RetrievalExecutorContext,
) -> Result<Vec<RerankCandidate>, String> {
    use brain_metadata::tables::text::TEXTS_TABLE;

    let rtxn = ctx
        .metadata
        .read_txn()
        .map_err(|e| format!("rerank read_txn: {e}"))?;
    let table = rtxn
        .open_table(TEXTS_TABLE)
        .map_err(|e| format!("rerank open TEXTS_TABLE: {e}"))?;

    let mut out: Vec<RerankCandidate> = Vec::with_capacity(ids.len());
    for id in ids {
        match table.get(&id.to_be_bytes()) {
            Ok(Some(guard)) => {
                let text = std::str::from_utf8(guard.value())
                    .map_err(|e| format!("rerank TEXTS_TABLE non-UTF-8 for {id:?}: {e}"))?
                    .to_string();
                if text.is_empty() {
                    continue;
                }
                out.push(RerankCandidate {
                    id: RankedItemId::Memory(*id),
                    text,
                });
            }
            Ok(None) => continue,
            Err(e) => return Err(format!("rerank TEXTS_TABLE get: {e}")),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Per-retriever invocation.
// ---------------------------------------------------------------------------

enum RetrieverInvocationError {
    Skipped(&'static str),
    Failure(String),
}

fn invoke_retriever(
    planned: &crate::retrieval::planner::PlannedRetriever,
    req: &QueryRequest,
    ctx: &RetrievalExecutorContext,
    pre_anchors: Option<&[RankedItem]>,
) -> Result<Vec<RankedItem>, RetrieverInvocationError> {
    match planned.retriever {
        Retriever::Semantic => invoke_semantic(planned, req, ctx),
        Retriever::Lexical => invoke_lexical(planned, req, ctx),
        Retriever::Graph => invoke_graph(planned, req, ctx, pre_anchors),
    }
}

fn invoke_semantic(
    planned: &crate::retrieval::planner::PlannedRetriever,
    req: &QueryRequest,
    ctx: &RetrievalExecutorContext,
) -> Result<Vec<RankedItem>, RetrieverInvocationError> {
    let handle = &ctx.semantic;
    let Some(text) = req.text.as_ref() else {
        return Err(RetrieverInvocationError::Skipped("no query text"));
    };
    let RetrieverConfig::Semantic {
        ef_search,
        similarity_threshold,
        timeout_ms,
    } = planned.config
    else {
        return Err(RetrieverInvocationError::Failure(
            "config mismatch (expected Semantic)".into(),
        ));
    };

    let mut filters = SemanticFilters::default();
    apply_pre_filter_to_semantic(&planned.pre_filter, &mut filters);
    // Front-gate scope: when the caller specified a context filter,
    // restrict every HNSW visit to that context set. The semantic
    // closure already reads MemoryMetadata per visit, so adding the
    // check is free; cost stays bounded by HNSW visits, not corpus N.
    filters.context_ids = req.context_filter.clone();

    // Adaptive `ef` for filtered ANN. When a structural filter is
    // active the graph traversal can land on ineligible nodes and
    // exhaust the default beam before finding `k` eligible
    // neighbours. Widen `ef` modestly (4×) so the beam escapes
    // sparsity without over-expanding the candidate set (which would
    // cause graph rider hits to outrank semantic on near-ties). 500
    // is the spec-range hard ceiling for `ef_search`, so clamp.
    const FILTERED_EF_CEILING: usize = 500;
    let ef_search_effective = if filters.context_ids.is_empty() {
        ef_search
    } else {
        ef_search.saturating_mul(4).min(FILTERED_EF_CEILING)
    };

    // Scope: Both when both text and entity_anchor present
    // (statement HNSW may be empty in v1 → silent Ok([]));
    // Memory otherwise.
    let scope = if req.entity_anchor.is_some() {
        SemanticScope::Both
    } else {
        SemanticScope::Memory
    };

    let config = SemanticRetrieverConfig {
        top_k: planned.top_n,
        ef_search: ef_search_effective,
        similarity_threshold,
        timeout_ms,
        filters: SemanticFiltersConfigSlot(filters),
    };

    let query = SemanticQuery::Text(text.clone());
    handle
        .retrieve(&query, scope, &config)
        .map_err(|e| RetrieverInvocationError::Failure(e.to_string()))
}

fn apply_pre_filter_to_semantic(pre: &Option<PreFilter>, filters: &mut SemanticFilters) {
    let Some(pf) = pre else {
        return;
    };
    match pf {
        PreFilter::AgentIds(ids) => filters.agent_ids = ids.clone(),
        PreFilter::MemoryKind(ks) => filters.memory_kind = ks.first().copied(),
        PreFilter::StatementKind(ks) => filters.statement_kind = ks.first().copied(),
        PreFilter::PredicateId(ps) => filters.predicate_id = ps.first().copied(),
        PreFilter::Temporal(range) => {
            filters.created_at_ms = range_to_inclusive(range.from_unix_ms, range.to_unix_ms);
        }
    }
}

fn invoke_lexical(
    planned: &crate::retrieval::planner::PlannedRetriever,
    req: &QueryRequest,
    ctx: &RetrievalExecutorContext,
) -> Result<Vec<RankedItem>, RetrieverInvocationError> {
    let handle = &ctx.lexical;
    let Some(text) = req.text.as_ref() else {
        return Err(RetrieverInvocationError::Skipped("no query text"));
    };
    let RetrieverConfig::Lexical {
        bm25_k1,
        bm25_b,
        min_score,
        timeout_ms,
    } = planned.config
    else {
        return Err(RetrieverInvocationError::Failure(
            "config mismatch (expected Lexical)".into(),
        ));
    };

    let mut filters = LexicalFilters::default();
    apply_pre_filter_to_lexical(&planned.pre_filter, &mut filters);
    // Same front gate as the semantic invocation — BM25 ranks within
    // the requested context universe only.
    filters.context_ids = req.context_filter.clone();

    let terms: Vec<String> = text.split_whitespace().map(str::to_owned).collect();
    let query = LexicalQuery {
        terms,
        phrase_clauses: Vec::new(),
        filters,
    };

    let config = LexicalRetrieverConfig {
        top_k: planned.top_n,
        bm25_k1,
        bm25_b,
        min_score,
        timeout_ms,
    };

    handle
        .retrieve(&query, LexicalScope::MemoryText, &config)
        .map_err(|e| RetrieverInvocationError::Failure(e.to_string()))
}

fn apply_pre_filter_to_lexical(pre: &Option<PreFilter>, filters: &mut LexicalFilters) {
    let Some(pf) = pre else {
        return;
    };
    match pf {
        PreFilter::AgentIds(ids) => filters.agent_ids = ids.clone(),
        PreFilter::MemoryKind(ks) => filters.memory_kind = ks.first().copied(),
        PreFilter::StatementKind(ks) => filters.statement_kind = ks.first().copied(),
        PreFilter::PredicateId(ps) => filters.predicate_id = ps.first().map(|p| p.raw()),
        PreFilter::Temporal(range) => {
            filters.created_at_ms = range_to_inclusive(range.from_unix_ms, range.to_unix_ms);
        }
    }
}

fn invoke_graph(
    planned: &crate::retrieval::planner::PlannedRetriever,
    req: &QueryRequest,
    ctx: &RetrievalExecutorContext,
    pre_anchors: Option<&[RankedItem]>,
) -> Result<Vec<RankedItem>, RetrieverInvocationError> {
    let handle = &ctx.graph;
    let RetrieverConfig::Graph {
        max_depth,
        max_branching,
        direction,
        relation_types,
        include_statements,
        timeout_ms,
        anchor_mode,
        anchor_top_k,
    } = &planned.config
    else {
        return Err(RetrieverInvocationError::Failure(
            "config mismatch (expected Graph)".into(),
        ));
    };

    let config = GraphRetrieverConfig {
        top_k: planned.top_n,
        max_depth: *max_depth,
        max_branching: *max_branching,
        timeout_ms: *timeout_ms,
    };

    match anchor_mode {
        GraphAnchorMode::Entity => {
            let Some(anchor) = req.entity_anchor else {
                return Err(RetrieverInvocationError::Skipped("no entity anchor"));
            };
            let query = GraphQuery::Star {
                anchor: GraphAnchor::Entity(anchor),
                depth: *max_depth,
                direction: *direction,
                relation_types: relation_types.clone(),
                include_statements: *include_statements,
            };
            handle
                .retrieve(&query, &config)
                .map_err(|e| RetrieverInvocationError::Failure(e.to_string()))
        }
        GraphAnchorMode::MemoryFromSemantic => {
            // Materialise anchors from semantic top-K. The
            // executor runs semantic before us and stashes its
            // output; `pre_anchors` is `None` if semantic was
            // skipped or failed.
            let Some(semantic_items) = pre_anchors else {
                return Err(RetrieverInvocationError::Skipped(
                    "memory-anchor graph requires semantic output",
                ));
            };
            let cap = (*anchor_top_k) as usize;
            let anchors: Vec<brain_core::MemoryId> = semantic_items
                .iter()
                .filter_map(|item| match item.id {
                    RankedItemId::Memory(m) => Some(m),
                    _ => None,
                })
                .take(cap)
                .collect();
            if anchors.is_empty() {
                return Err(RetrieverInvocationError::Skipped(
                    "no memory hits from semantic to anchor graph walk",
                ));
            }
            // One walk per anchor; merged into a single Vec.
            // Per-anchor rank stays meaningful because all hits
            // are scored by `proximity_score(hop) * edge.weight`
            // — no cross-anchor normalisation needed for RRF
            // (fusion only cares about the per-retriever rank).
            let mut merged: Vec<RankedItem> = Vec::new();
            for anchor in anchors {
                // The memory-anchor rider surfaces only DIRECT
                // similar/causal neighbours of the semantic top hits.
                // We tried depth=2 to pivot Memory → Entity → Memory,
                // but on a topically diverse corpus the entity table
                // densely connects everything ("Sarah", "Aurora",
                // "Phoenix" appear in many memories), and a depth-2
                // walk from a high-mention seed memory floods fusion
                // with query-independent neighbours that then take
                // rank 1 with arbitrary inter-tie scores. Until
                // cue→anchor resolution lets us seed the entity-mode
                // walk from the actual subject in the query, keep the
                // memory-anchor walk at depth 1: a quiet graph lane
                // beats a noisy one.
                const MEMORY_ANCHOR_GRAPH_DEPTH: u8 = 1;
                let query = GraphQuery::Star {
                    anchor: GraphAnchor::Memory(anchor),
                    depth: MEMORY_ANCHOR_GRAPH_DEPTH,
                    direction: *direction,
                    relation_types: None,
                    include_statements: false,
                };
                match handle.retrieve(&query, &config) {
                    Ok(items) => merged.extend(items),
                    Err(brain_index::GraphError::MemoryAnchorNotFound(_)) => {
                        // The semantic anchor was tombstoned
                        // between the HNSW hit and the graph
                        // walk. Drop this one and continue —
                        // other anchors may still produce hits.
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "brain_planner::executor",
                            anchor = ?anchor,
                            error = %e,
                            "memory-anchor graph walk failed; continuing",
                        );
                    }
                }
            }
            // The memory-anchor rider exists to surface MEMORY
            // candidates similar to the semantic top-K. Entity nodes
            // reached during the walk (via `Mentions` edges) are
            // useful for enrichment but not for the recall result —
            // they'd otherwise dominate the fused top-K with high
            // proximity scores and then get dropped at projection,
            // leaving the user with `(no results)`. Keep only Memory
            // variants here.
            merged.retain(|item| matches!(item.id, RankedItemId::Memory(_)));
            // Re-sort + re-rank the merged set so the per-
            // retriever rank-1 spot is the strongest hit
            // overall.
            merged.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            merged.truncate(planned.top_n);
            for (i, item) in merged.iter_mut().enumerate() {
                item.rank = (i as u32) + 1;
            }
            Ok(merged)
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn timeout_ms(config: &RetrieverConfig) -> u32 {
    match config {
        RetrieverConfig::Semantic { timeout_ms, .. }
        | RetrieverConfig::Lexical { timeout_ms, .. }
        | RetrieverConfig::Graph { timeout_ms, .. } => *timeout_ms,
    }
}

fn range_to_inclusive(from: Option<u64>, to: Option<u64>) -> Option<std::ops::RangeInclusive<u64>> {
    let lo = from.unwrap_or(0);
    let hi = to.unwrap_or(u64::MAX);
    if from.is_none() && to.is_none() {
        None
    } else {
        Some(lo..=hi)
    }
}

// ---------------------------------------------------------------------------
// Concurrent fan-out helper.
// ---------------------------------------------------------------------------

/// Drive a small heterogeneous set of futures to completion
/// concurrently, returning their outputs in submission order.
/// Polls every still-pending future on each wakeup — fine for
/// the planner's bounded fan-out (at most three retrievers per
/// query) but not a substitute for a real `join_all` if
/// `futures` grows large.
///
/// Lives here instead of in a shared util because the planner
/// is the only caller and the helper deliberately stays
/// runtime-agnostic (no `spawn_local` — the caller's executor
/// already runs us inside one task).
async fn join_all_local<T>(mut futures: Vec<Pin<Box<dyn Future<Output = T> + '_>>>) -> Vec<T> {
    if futures.is_empty() {
        return Vec::new();
    }
    let mut results: Vec<Option<T>> = (0..futures.len()).map(|_| None).collect();
    let mut remaining = futures.len();
    poll_fn(move |cx: &mut Context<'_>| {
        for (slot, fut) in results.iter_mut().zip(futures.iter_mut()) {
            if slot.is_some() {
                continue;
            }
            if let Poll::Ready(v) = fut.as_mut().poll(cx) {
                *slot = Some(v);
                remaining -= 1;
            }
        }
        if remaining == 0 {
            // All futures resolved — drain results in submission
            // order. `take()` is safe because each slot was
            // marked ready exactly once.
            let out: Vec<T> = results
                .iter_mut()
                .map(|s| s.take().expect("invariant: slot resolved"))
                .collect();
            Poll::Ready(out)
        } else {
            Poll::Pending
        }
    })
    .await
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
