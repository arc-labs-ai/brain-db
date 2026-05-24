//! Hybrid query executor (phase 23.7).
//!
//! Consumes a `QueryPlan` (23.6) + a `HybridExecutorContext`
//! (Arc handles to the three retrievers plus the `MetadataDb`
//! the filter chain reads), invokes each retriever per its
//! `PlannedRetriever` config, fuses via RRF (23.4), applies the
//! post-fusion filter chain (23.5), truncates to `limit`, and
//! returns a `QueryResult` with per-retriever latency and
//! outcome metadata for EXPLAIN/TRACE (23.8).
//!
//! v1 invokes retrievers **sequentially** — the retriever
//! traits are sync, and brain-planner's per-shard Glommio
//! executor is single-threaded. Parallel execution requires
//! async-trait migration and lands post-v1. Wall-time budget
//! per §16/02 §2.10 has comfortable headroom (3 × 10 ms =
//! ~30 ms total vs the 50 ms p99 target).

use std::sync::Arc;
use std::time::Instant;

use brain_index::{
    GraphAnchor, GraphQuery, GraphRetriever, GraphRetrieverConfig, LexicalFilters, LexicalQuery,
    LexicalRetriever, LexicalRetrieverConfig, LexicalScope, RankedItem, RankedItemId,
    SemanticFilters, SemanticFiltersConfigSlot, SemanticQuery, SemanticRetriever,
    SemanticRetrieverConfig, SemanticScope,
};
use brain_metadata::MetadataDb;
use brain_rerank::CrossEncoder;
use parking_lot::Mutex;

use super::filters::{apply_filter_chain, FilterChainStats, FilterError};
use super::fusion::{fuse_rrf, FusedItem};
use super::planner::{PreFilter, QueryPlan, RetrieverConfig};
use super::rerank::{rerank_top_n, RerankCandidate, RERANK_TOP_N};
use super::router::{GraphAnchorMode, QueryRequest, Retriever};

// ---------------------------------------------------------------------------
// Public types.
// ---------------------------------------------------------------------------

/// What the executor needs beyond a `QueryPlan`. Built from
/// `OpsContext`'s retriever slots in the caller. The three
/// retrievers are mandatory: every spawned shard wires real
/// impls and tests provide mocks.
#[derive(Clone)]
pub struct HybridExecutorContext {
    pub semantic: Arc<dyn SemanticRetriever>,
    pub lexical: Arc<dyn LexicalRetriever>,
    pub graph: Arc<dyn GraphRetriever>,
    pub metadata: Arc<Mutex<MetadataDb>>,
    /// Optional cross-encoder for the W2.2 rerank pass. When
    /// `Some` and the plan opted in (`QueryPlan.rerank == true`),
    /// the executor reranks the top fused candidates. When `None`
    /// (no model on disk, auto-discover failed) the rerank stage
    /// is silently skipped — RRF order wins.
    pub cross_encoder: Option<Arc<CrossEncoder>>,
}

/// Final hybrid-query result.
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub items: Vec<FusedItem>,
    pub metadata: QueryMetadata,
}

/// Per-execution observability data — surfaces in
/// EXPLAIN/TRACE (23.8). Operators read this to see which
/// retriever was slow, which filter narrowed results most,
/// total wall-time, etc.
#[derive(Debug, Clone, Default)]
pub struct QueryMetadata {
    pub retriever_latencies_ms: Vec<(Retriever, f64)>,
    pub retriever_outcomes: Vec<RetrieverOutcome>,
    pub retriever_total_results: Vec<(Retriever, usize)>,
    pub filter_stats: FilterChainStats,
    pub total_latency_ms: f64,
    /// Outcome of the optional cross-encoder rerank stage. `None`
    /// means the caller didn't opt in via `rerank=true`.
    pub rerank: Option<RerankOutcome>,
}

/// What the rerank stage did. Surfaces in trace output so callers
/// can see whether a `rerank=true` request actually ran the
/// cross-encoder.
#[derive(Debug, Clone, PartialEq)]
pub enum RerankOutcome {
    /// Cross-encoder ran and re-sorted the fused list.
    Applied { candidates: usize, latency_ms: f64 },
    /// Opt-in was set but the cross-encoder isn't loaded on this
    /// server. RRF-only order returned.
    SkippedUnavailable,
    /// Opt-in was set but the fused list had no candidates with
    /// fetchable text (tombstoned mid-query, non-memory variants).
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
/// Most retrievers run independently. The one exception is the
/// memory-anchor graph mode: when the schemaless hybrid path
/// selects graph without an entity anchor, the executor must
/// run semantic first and feed its top-K memory ids into the
/// graph walk. We detect this up-front, run semantic eagerly,
/// stash its output, and let the iteration loop reuse it
/// instead of re-invoking.
pub fn execute(
    plan: &QueryPlan,
    request: &QueryRequest,
    ctx: &HybridExecutorContext,
) -> Result<QueryResult, ExecutionError> {
    let total_started = Instant::now();
    let mut outputs: Vec<(Retriever, Vec<RankedItem>)> = Vec::new();
    let mut latencies: Vec<(Retriever, f64)> = Vec::new();
    let mut outcomes: Vec<RetrieverOutcome> = Vec::new();
    let mut totals: Vec<(Retriever, usize)> = Vec::new();

    // Pre-run semantic if graph depends on it. Without this the
    // sequential loop would invoke graph with no anchors and
    // either skip or error.
    let needs_semantic_first = plan.retrievers.iter().any(|r| {
        matches!(
            &r.config,
            RetrieverConfig::Graph {
                anchor_mode: GraphAnchorMode::MemoryFromSemantic,
                ..
            }
        )
    });
    let pre_semantic = if needs_semantic_first {
        plan.retrievers
            .iter()
            .find(|r| r.retriever == Retriever::Semantic)
            .cloned()
    } else {
        None
    };
    let mut cached_semantic: Option<Vec<RankedItem>> = None;
    if let Some(sem) = &pre_semantic {
        let started = Instant::now();
        let invocation = invoke_retriever(sem, request, ctx, None);
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        match invocation {
            Ok(items) => {
                cached_semantic = Some(items);
            }
            Err(_) => {
                // Semantic failed or was skipped. Graph will
                // then fall back to "no anchors" and skip
                // itself — let the main loop handle the
                // bookkeeping uniformly.
                let _ = elapsed_ms;
            }
        }
    }

    for planned in &plan.retrievers {
        let started = Instant::now();
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
        let invocation = match (planned.retriever, cached_semantic.as_ref()) {
            (Retriever::Semantic, Some(cached)) => {
                // Reuse the pre-run semantic output rather than
                // paying for the HNSW search twice.
                Ok(cached.clone())
            }
            _ => invoke_retriever(planned, request, ctx, pre_anchors),
        };
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
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

    let fused = fuse_rrf(&outputs, plan.fusion.k, &plan.fusion.weights);

    let (fused_after_rerank, rerank_outcome) = if plan.rerank {
        rerank_stage(fused, request, ctx)
    } else {
        (fused, None)
    };

    let (items, filter_stats) = {
        let metadata_guard = ctx.metadata.lock();
        apply_filter_chain(
            fused_after_rerank,
            &plan.post_filters,
            &metadata_guard,
            plan.limit,
        )?
    };

    let total_latency_ms = total_started.elapsed().as_secs_f64() * 1000.0;
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
/// per-shard `texts` table, then delegates to `rerank::rerank_top_n`.
/// Returns the (possibly re-ordered) fused list plus an outcome tag
/// for `QueryMetadata`.
fn rerank_stage(
    fused: Vec<FusedItem>,
    request: &QueryRequest,
    ctx: &HybridExecutorContext,
) -> (Vec<FusedItem>, Option<RerankOutcome>) {
    let Some(encoder) = ctx.cross_encoder.as_ref() else {
        tracing::info!(
            target: "brain_planner::executor",
            "rerank requested but cross-encoder unavailable; returning RRF-only ranking",
        );
        return (fused, Some(RerankOutcome::SkippedUnavailable));
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
    let reranked = rerank_top_n(encoder, query, fused, &candidates);
    let latency_ms = started.elapsed().as_secs_f64() * 1000.0;

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
    ctx: &HybridExecutorContext,
) -> Result<Vec<RerankCandidate>, String> {
    use brain_metadata::tables::text::TEXTS_TABLE;

    let metadata_guard = ctx.metadata.lock();
    let rtxn = metadata_guard
        .read_txn()
        .map_err(|e| format!("rerank read_txn: {e}"))?;
    let table = match rtxn.open_table(TEXTS_TABLE) {
        Ok(t) => t,
        // A shard that hasn't received any encode yet won't have a
        // texts table — treat as "no candidates" rather than fail.
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(format!("rerank open TEXTS_TABLE: {e}")),
    };

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
    planned: &super::planner::PlannedRetriever,
    req: &QueryRequest,
    ctx: &HybridExecutorContext,
    pre_anchors: Option<&[RankedItem]>,
) -> Result<Vec<RankedItem>, RetrieverInvocationError> {
    match planned.retriever {
        Retriever::Semantic => invoke_semantic(planned, req, ctx),
        Retriever::Lexical => invoke_lexical(planned, req, ctx),
        Retriever::Graph => invoke_graph(planned, req, ctx, pre_anchors),
    }
}

fn invoke_semantic(
    planned: &super::planner::PlannedRetriever,
    req: &QueryRequest,
    ctx: &HybridExecutorContext,
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

    // Scope: Both when both text and entity_anchor present
    // (statement HNSW may be empty in v1 → silent Ok([])
    // per §23/03 §9); Memory otherwise.
    let scope = if req.entity_anchor.is_some() {
        SemanticScope::Both
    } else {
        SemanticScope::Memory
    };

    let config = SemanticRetrieverConfig {
        top_k: planned.top_n,
        ef_search,
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
        PreFilter::AgentId(a) => filters.agent_id = Some(*a),
        PreFilter::MemoryKind(ks) => filters.memory_kind = ks.first().copied(),
        PreFilter::StatementKind(ks) => filters.statement_kind = ks.first().copied(),
        PreFilter::PredicateId(ps) => filters.predicate_id = ps.first().copied(),
        PreFilter::Temporal(range) => {
            filters.created_at_ms = range_to_inclusive(range.from_unix_ms, range.to_unix_ms);
        }
    }
}

fn invoke_lexical(
    planned: &super::planner::PlannedRetriever,
    req: &QueryRequest,
    ctx: &HybridExecutorContext,
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
        PreFilter::AgentId(a) => filters.agent_id = Some(*a),
        PreFilter::MemoryKind(ks) => filters.memory_kind = ks.first().copied(),
        PreFilter::StatementKind(ks) => filters.statement_kind = ks.first().copied(),
        PreFilter::PredicateId(ps) => filters.predicate_id = ps.first().map(|p| p.raw()),
        PreFilter::Temporal(range) => {
            filters.created_at_ms = range_to_inclusive(range.from_unix_ms, range.to_unix_ms);
        }
    }
}

fn invoke_graph(
    planned: &super::planner::PlannedRetriever,
    req: &QueryRequest,
    ctx: &HybridExecutorContext,
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
                let query = GraphQuery::Star {
                    anchor: GraphAnchor::Memory(anchor),
                    depth: *max_depth,
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

#[cfg(test)]
mod tests;
