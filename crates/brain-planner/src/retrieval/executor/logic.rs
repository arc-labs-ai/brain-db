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

use brain_core::EntityId;
use brain_index::{
    GraphAnchor, GraphQuery, GraphRetriever, GraphRetrieverConfig, LexicalFilters, LexicalQuery,
    LexicalRetriever, LexicalRetrieverConfig, LexicalScope, RankedItem, RankedItemId,
    SemanticFilters, SemanticFiltersConfigSlot, SemanticQuery, SemanticRetriever,
    SemanticRetrieverConfig, SemanticScope,
};
use brain_metadata::MetadataDb;
use brain_rerank::RerankService;
use futures_lite::future::poll_fn;

use crate::retrieval::diversity::{mmr_reorder, tokenize, MMR_LAMBDA_LIST, MMR_WINDOW};
use crate::retrieval::filters::{apply_filter_chain, FilterChainStats, FilterError};
use crate::retrieval::fusion::{fuse, FusedItem};
use crate::retrieval::planner::{PreFilter, QueryPlan, RetrieverConfig, LIST_MAX_TOP_N, MAX_TOP_N};
use crate::retrieval::recency::apply_recency_boost;
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
    #[error("recency ranking: {0}")]
    Recency(#[from] crate::retrieval::recency::RecencyError),
}

// ---------------------------------------------------------------------------
// Entry point.
// ---------------------------------------------------------------------------

/// Run a plan, with one optional dynamic-k deepening pass.
///
/// The first pass runs at the planner's per-class `top_n`. If its
/// post-filter survivor pool comes back below the caller's `limit`
/// **and** at least one retriever returned a full page (its candidate
/// cap was the binding constraint, so deeper retrieval can surface
/// more) **and** the initial depth was below the per-class ceiling, the
/// executor re-runs once at the ceiling and keeps whichever pass
/// yielded more results. Recall-additive by construction: it fires only
/// when the shallow pass would otherwise under-fill the request, so it
/// can never trade away a hit the shallow pass already found. The cost
/// is one extra fan-out, bounded to the under-recall case.
pub async fn execute(
    plan: &QueryPlan,
    request: &QueryRequest,
    include_statements: bool,
    ctx: &RetrievalExecutorContext,
) -> Result<QueryResult, ExecutionError> {
    // Cue→anchor: when the router left a blind memory-from-semantic graph
    // lane and the query names exactly one known entity, upgrade that
    // lane to an entity-anchored walk seeded from the resolved subject.
    // Returns a rewritten plan only when resolution succeeds; otherwise
    // the original plan stands.
    let rewritten = resolve_cue_anchor(plan, request, ctx);
    let plan = rewritten.as_ref().unwrap_or(plan);

    let result = execute_once(plan, request, include_statements, ctx).await?;

    let ceiling = if plan.routing.list_intent {
        LIST_MAX_TOP_N
    } else {
        MAX_TOP_N
    };
    // A retriever that returned at least its `top_n` page hit the cap —
    // deeper retrieval may yield more. One that returned fewer has
    // exhausted what the index holds for this query, so deepening it is
    // wasted work. `retriever_total_results` is built in plan order.
    let saturated = plan
        .retrievers
        .iter()
        .zip(result.metadata.retriever_total_results.iter())
        .any(|(r, (_retriever, total))| r.top_n > 0 && *total >= r.top_n);
    let under_target = plan.limit > 0 && (result.items.len() as u32) < plan.limit;
    let room_to_deepen = plan.retrievers.iter().any(|r| r.top_n < ceiling);

    if under_target && saturated && room_to_deepen {
        let mut deepened = plan.clone();
        for r in &mut deepened.retrievers {
            r.top_n = ceiling;
            // Raise HNSW exploration to match the deeper page — a
            // semantic top_k above ef_search would silently under-return.
            if let RetrieverConfig::Semantic { ef_search, .. } = &mut r.config {
                *ef_search = (*ef_search).max(ceiling);
            }
        }
        let retry = execute_once(&deepened, request, include_statements, ctx).await?;
        if retry.items.len() > result.items.len() {
            return Ok(retry);
        }
    }

    Ok(result)
}

/// Try to upgrade a blind memory-from-semantic graph lane to an
/// entity-anchored walk by resolving the query's named subject.
///
/// Fires only when (a) the caller gave no explicit `entity_anchor`,
/// (b) the plan has a `MemoryFromSemantic` graph lane to upgrade, and
/// (c) the query text names **exactly one** entity that resolves by
/// exact canonical name. Two distinct named entities, an unresolvable
/// cue, or any resolution error all fall back to the original plan — a
/// noisy guess is worse than the existing semantic-seeded walk, so the
/// bar to switch is unambiguous single-subject resolution. Returns the
/// rewritten plan, or `None` to keep the original.
fn resolve_cue_anchor(
    plan: &QueryPlan,
    request: &QueryRequest,
    ctx: &RetrievalExecutorContext,
) -> Option<QueryPlan> {
    if request.entity_anchor.is_some() {
        return None;
    }
    let text = request.text.as_deref()?;
    let has_blind_graph = plan.retrievers.iter().any(|r| {
        matches!(
            &r.config,
            RetrieverConfig::Graph {
                anchor_mode: GraphAnchorMode::MemoryFromSemantic,
                ..
            }
        )
    });
    if !has_blind_graph {
        return None;
    }

    let cues = crate::retrieval::router::entity_cue_candidates(text);
    if cues.is_empty() {
        return None;
    }
    let rtxn = ctx.metadata.read_txn().ok()?;
    let mut resolved: Option<EntityId> = None;
    for cue in &cues {
        let ids = brain_metadata::entity_resolve_canonical_all_types(&rtxn, cue).ok()?;
        for id in ids {
            match resolved {
                None => resolved = Some(id),
                Some(prev) if prev == id => {}
                // A second distinct entity — the query names more than
                // one subject. A single-anchor walk can't honour both,
                // so fall back rather than pick arbitrarily.
                Some(_) => return None,
            }
        }
    }
    let eid = resolved?;

    let mut p = plan.clone();
    for r in &mut p.retrievers {
        if let RetrieverConfig::Graph { anchor_mode, .. } = &mut r.config {
            if matches!(anchor_mode, GraphAnchorMode::MemoryFromSemantic) {
                *anchor_mode = GraphAnchorMode::MemoryFromEntityCue(eid);
            }
        }
    }
    Some(p)
}

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
async fn execute_once(
    plan: &QueryPlan,
    request: &QueryRequest,
    include_statements: bool,
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
        let invocation = invoke_retriever(sem, request, ctx, None, include_statements);
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
            let invocation =
                invoke_retriever(planned, request, ctx, pre_anchors, include_statements);
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

    // Read pipeline order: filter the fused set BEFORE rerank. Reranking
    // first would spend the cross-encoder window on tombstoned/superseded
    // items and could push a valid hit out of the window behind junk that
    // is about to be dropped. Filter WITHOUT the final limit cut (pass 0),
    // rerank the survivors, then truncate to `limit` — applying the limit
    // before rerank would collapse the rerank window whenever `limit` is
    // smaller than it.
    let (mut filtered, mut filter_stats) =
        apply_filter_chain(fused, &plan.post_filters, ctx.metadata.as_ref(), 0)?;

    // Recency ranking (soft, additive, RRF-scale). Only when the query
    // carries a temporal signal — a temporal expression / explicit time
    // filter (`temporal_pushdown`) or an `as_of` anchor — so timeless
    // facts aren't penalised for being old. Reference point is the
    // `as_of` anchor when set, else wall-clock now. Folded into
    // `fused_score` BEFORE rerank so a fresh hit both enters the rerank
    // window and carries its recency into the rerank blend.
    let as_of = plan.post_filters.as_of_record_time_unix_nanos;
    if plan.routing.temporal_pushdown || as_of.is_some() {
        let reference_time = as_of.unwrap_or_else(now_unix_nanos);
        apply_recency_boost(
            &mut filtered,
            ctx.metadata.as_ref(),
            reference_time,
            plan.fusion.weights.temporal,
            plan.fusion.k,
        )?;
    }

    // Rerank is always-on: the stage fires whenever the shard has a
    // cross-encoder loaded, regardless of any request field. When
    // the operator disabled the load (`cross_encoder` is `None`),
    // the result is RRF-only with no error.
    let (reranked, rerank_outcome) = if ctx.cross_encoder.is_some() {
        rerank_stage(filtered, request, ctx).await
    } else {
        (filtered, None)
    };

    // Merge / diversity stage — internal, router-decided. Runs only when
    // the router detected list/aggregation intent (a set question), so a
    // single-answer query is never re-ordered. Spreads near-duplicate
    // members across the head before the caller's `top_k` cut, so a list
    // result is distinct items rather than paraphrases of the top one.
    let mut items = reranked;
    if plan.routing.list_intent {
        apply_diversity(&mut items, ctx);
    }

    if plan.limit > 0 && items.len() > plan.limit as usize {
        items.truncate(plan.limit as usize);
    }
    filter_stats.after_limit = items.len() as u32;

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

/// Run the merge / diversity (MMR) stage over the head of `items`.
///
/// Fetches text for the windowed memory hits, tokenizes it for the
/// Jaccard redundancy term, and reorders in place via
/// [`mmr_reorder`]. Best-effort: a text-fetch error or a non-memory /
/// text-less hit just yields an empty token set (treated as maximally
/// novel), so diversity degrades to relevance order rather than failing
/// the read.
fn apply_diversity(items: &mut Vec<FusedItem>, ctx: &RetrievalExecutorContext) {
    let window = items.len().min(MMR_WINDOW);
    if window <= 2 {
        return;
    }

    let mem_ids: Vec<brain_core::MemoryId> = items
        .iter()
        .take(window)
        .filter_map(|it| match it.id {
            RankedItemId::Memory(m) => Some(m),
            _ => None,
        })
        .collect();

    let text_by_id: std::collections::HashMap<brain_core::MemoryId, String> =
        match fetch_texts(&mem_ids, ctx) {
            Ok(cands) => cands
                .into_iter()
                .filter_map(|c| match c.id {
                    RankedItemId::Memory(m) => Some((m, c.text)),
                    _ => None,
                })
                .collect(),
            Err(e) => {
                tracing::warn!(
                    target: "brain_planner::executor",
                    error = %e,
                    "diversity text fetch failed; returning relevance order",
                );
                return;
            }
        };

    let token_sets: Vec<std::collections::HashSet<String>> = items
        .iter()
        .take(window)
        .map(|it| match it.id {
            RankedItemId::Memory(m) => text_by_id.get(&m).map(|t| tokenize(t)).unwrap_or_default(),
            _ => std::collections::HashSet::new(),
        })
        .collect();

    mmr_reorder(items, &token_sets, MMR_LAMBDA_LIST);
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
    include_statements: bool,
) -> Result<Vec<RankedItem>, RetrieverInvocationError> {
    match planned.retriever {
        Retriever::Semantic => invoke_semantic(planned, req, ctx, include_statements),
        Retriever::Lexical => invoke_lexical(planned, req, ctx, include_statements),
        Retriever::Graph => invoke_graph(planned, req, ctx, pre_anchors),
    }
}

fn invoke_semantic(
    planned: &crate::retrieval::planner::PlannedRetriever,
    req: &QueryRequest,
    ctx: &RetrievalExecutorContext,
    include_statements: bool,
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

    // Scope: search the statement corpus alongside memories when the
    // caller is a typed-graph QUERY (`include_statements`) or anchored
    // an entity. RECALL stays memory-only — its projector drops
    // non-memory hits, so statement candidates there are pure overhead.
    // (Statement HNSW may be empty → silent Ok([]).)
    let scope = if include_statements || req.entity_anchor.is_some() {
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

/// English stopwords plus interrogatives, dropped from the BM25 term
/// set. A raw cue like "When did Caroline go to the LGBTQ support
/// group?" otherwise dilutes BM25 toward high-document-frequency
/// function words; ranking on the content words ({caroline, go, lgbtq,
/// support, group}) lets the lexical signal land on what the question
/// is actually about. Kept lowercase so the comparison can be done on
/// already-lowercased tokens.
static LEXICAL_STOPWORDS: &[&str] = &[
    // interrogatives
    "when", "what", "where", "who", "whom", "whose", "why", "how", "which",
    // auxiliaries / copulas
    "did", "does", "do", "is", "are", "was", "were", "be", "been", "being", "am", "has", "have",
    "had", "will", "would", "shall", "should", "can", "could", "may", "might", "must",
    // articles / determiners
    "the", "a", "an", "this", "that", "these", "those", "some", "any", "no",
    // prepositions / conjunctions
    "of", "to", "in", "on", "at", "for", "and", "or", "with", "about", "from", "as", "by", "into",
    "over", "after", "before", "between", "out", "up", "down", "off", "than", "then", "so", "but",
    "if", "because", // pronouns
    "it", "its", "i", "you", "he", "she", "they", "we", "his", "her", "hers", "their", "theirs",
    "your", "yours", "my", "mine", "our", "ours", "me", "him", "them", "us",
];

/// Strip ASCII leading/trailing punctuation from a token, preserving
/// inner apostrophes/hyphens (so "caroline's", "co-worker" survive).
fn trim_token_punct(tok: &str) -> &str {
    tok.trim_matches(|c: char| c.is_ascii_punctuation())
}

/// Build the BM25 term set from content words only: lowercase, strip
/// surrounding punctuation, drop stopwords/question-words, dedup
/// preserving first-seen order. If filtering empties the set (a cue
/// made entirely of stopwords), fall back to the raw whitespace split
/// so the lexical query is never empty.
fn lexical_content_terms(text: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut terms: Vec<String> = Vec::new();
    for raw in text.split_whitespace() {
        let trimmed = trim_token_punct(raw);
        if trimmed.is_empty() {
            continue;
        }
        let lowered = trimmed.to_lowercase();
        if LEXICAL_STOPWORDS.contains(&lowered.as_str()) {
            continue;
        }
        if seen.insert(lowered.clone()) {
            terms.push(lowered);
        }
    }
    if terms.is_empty() {
        // Degenerate guard: an all-stopword question keeps the raw
        // split so BM25 still has something to match.
        return text.split_whitespace().map(str::to_owned).collect();
    }
    terms
}

fn invoke_lexical(
    planned: &crate::retrieval::planner::PlannedRetriever,
    req: &QueryRequest,
    ctx: &RetrievalExecutorContext,
    include_statements: bool,
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

    let terms = lexical_content_terms(text);
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

    let mut hits = handle
        .retrieve(&query, LexicalScope::MemoryText, &config)
        .map_err(|e| RetrieverInvocationError::Failure(e.to_string()))?;

    // Typed-graph QUERY also searches the statement-text index. The
    // StatementText scope rejects the memory-only filters (agent_id,
    // memory_kind, created_at_ms), so build a statement-scoped filter
    // carrying only the predicate / statement-kind pre-filter and the
    // shared context scope. The two corpora return disjoint id variants
    // (Memory vs Statement), so fusion merges them without collision.
    if include_statements {
        let mut stmt_filters = LexicalFilters {
            context_ids: req.context_filter.clone(),
            ..Default::default()
        };
        apply_pre_filter_to_lexical_statement(&planned.pre_filter, &mut stmt_filters);
        let stmt_query = LexicalQuery {
            terms: query.terms.clone(),
            phrase_clauses: Vec::new(),
            filters: stmt_filters,
        };
        let stmt_hits = handle
            .retrieve(&stmt_query, LexicalScope::StatementText, &config)
            .map_err(|e| RetrieverInvocationError::Failure(e.to_string()))?;
        hits.extend(stmt_hits);
    }

    Ok(hits)
}

/// Project a pre-filter onto the statement-text lexical scope. Only the
/// statement-relevant predicates carry over; the memory-only filters
/// (agent_id / memory_kind / created_at_ms) would be rejected by the
/// StatementText scope, so they are dropped here.
fn apply_pre_filter_to_lexical_statement(pre: &Option<PreFilter>, filters: &mut LexicalFilters) {
    let Some(pf) = pre else {
        return;
    };
    match pf {
        PreFilter::StatementKind(ks) => filters.statement_kind = ks.first().copied(),
        PreFilter::PredicateId(ps) => filters.predicate_id = ps.first().map(|p| p.raw()),
        // Memory-only pre-filters don't apply to the statement corpus.
        PreFilter::AgentIds(_) | PreFilter::MemoryKind(_) | PreFilter::Temporal(_) => {}
    }
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
        GraphAnchorMode::MemoryFromEntityCue(entity_id) => {
            // The executor resolved the query's named subject to this
            // entity. Walk one hop in both directions with no relation
            // filter so the `Mentions` edges (Memory → Entity) traverse
            // in reverse — the neighbours are exactly the memories that
            // name the subject. Depth stays at 1: the direct mentions
            // are the answer to "tell me about X"; deeper hops pull in
            // query-independent neighbours that flood fusion (the same
            // noise that keeps the memory-from-semantic walk shallow).
            let query = GraphQuery::Star {
                anchor: GraphAnchor::Entity(*entity_id),
                depth: 1,
                direction: brain_index::Direction::Both,
                relation_types: None,
                include_statements: false,
            };
            let mut items = handle
                .retrieve(&query, &config)
                .map_err(|e| RetrieverInvocationError::Failure(e.to_string()))?;
            // Entity / relation nodes reached during the walk are not
            // recall results — keep only the mentioning memories, same
            // as the memory-from-semantic lane.
            items.retain(|item| matches!(item.id, RankedItemId::Memory(_)));
            items.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            items.truncate(planned.top_n);
            for (i, item) in items.iter_mut().enumerate() {
                item.rank = (i as u32) + 1;
            }
            Ok(items)
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

/// Wall-clock now in unix nanoseconds. Used as the recency-decay
/// reference point when the query carries no explicit `as_of` anchor.
/// A clock before the epoch (impossible in practice) reads as 0, which
/// makes every memory look future-dated and saturate at full freshness
/// — harmless for a soft ranking term.
fn now_unix_nanos() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
