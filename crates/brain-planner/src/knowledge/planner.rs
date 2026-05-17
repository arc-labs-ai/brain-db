//! Query planner (phase 23.6).
//!
//! Implements §24/00 §"Plan structure". Takes a `QueryRequest`,
//! calls the router (23.3), expands the routing decision into a
//! concrete plan DAG (which retrievers run, with what
//! pre-filters and configs; which filters land post-fusion;
//! what `k` and weights feed RRF; what limit and cost estimate
//! the executor should respect).
//!
//! The plan is immutable, serialisable-shaped, and consumed by
//! both:
//!
//! - The executor (23.7) — runs each retriever in parallel,
//!   then fuses (23.4), then applies the filter chain (23.5),
//!   then truncates to `limit`.
//! - EXPLAIN / TRACE (23.8) — renders the plan without
//!   executing.

use brain_core::knowledge::StatementKind;
use brain_core::{AgentId, MemoryKind, PredicateId, RelationTypeId};
use brain_index::Direction as GraphDirection;

use super::filters::FilterChain;
use super::fusion::DEFAULT_K;
use super::router::{
    route, PerRetrieverWeights, QueryRequest, Retriever, RetrieverSelection, RoutingDecision,
    TimeRange,
};

// ---------------------------------------------------------------------------
// Defaults.
// ---------------------------------------------------------------------------

/// Default top_k returned to the caller if `req.limit == 0`.
pub const DEFAULT_RESULT_LIMIT: u32 = 20;

/// Minimum per-retriever `top_n` — guarantees fusion has
/// candidates to work with even when one retriever's top-K
/// overlaps poorly with another's (§24/00 §"Limits and budgets").
pub const MIN_TOP_N: usize = 100;

/// Hard cap on per-retriever `top_n` — matches §24/00's
/// `top_n_per_retriever ≤ 200` limit.
pub const MAX_TOP_N: usize = 200;

// Retriever-config defaults (mirror §23/02/§23/03/§23/04 §8 +
// §06/02 substrate HNSW knobs):

const SEMANTIC_DEFAULT_EF_SEARCH: usize = 64;
const SEMANTIC_DEFAULT_SIM_THRESHOLD: f32 = 0.0;
const LEXICAL_DEFAULT_BM25_K1: f32 = 1.2;
const LEXICAL_DEFAULT_BM25_B: f32 = 0.75;
const GRAPH_DEFAULT_MAX_DEPTH: u8 = 3;
const GRAPH_DEFAULT_MAX_BRANCHING: u32 = 200;
const PER_RETRIEVER_TIMEOUT_MS: u32 = 50;

// ---------------------------------------------------------------------------
// Public types.
// ---------------------------------------------------------------------------

/// The output of [`plan`]. Immutable; passed to the executor
/// (23.7) and surfaced verbatim by EXPLAIN (23.8).
#[derive(Debug, Clone)]
pub struct QueryPlan {
    pub routing: RoutingDecision,
    pub retrievers: Vec<PlannedRetriever>,
    pub fusion: FusionStep,
    pub post_filters: FilterChain,
    pub limit: u32,
    pub estimated_cost_ms: f32,
}

/// Per-retriever invocation. Richer than the router's
/// `RetrieverInvocation` (which only carries discriminant +
/// weight); the planner expands it with a config and a
/// (single) pre-filter.
#[derive(Debug, Clone)]
pub struct PlannedRetriever {
    pub retriever: Retriever,
    pub weight: f32,
    pub top_n: usize,
    pub config: RetrieverConfig,
    pub pre_filter: Option<PreFilter>,
}

/// Per-retriever knobs. Variants match the 23.1 / 22.5 / 23.2
/// retriever configs.
#[derive(Debug, Clone)]
pub enum RetrieverConfig {
    Semantic {
        ef_search: usize,
        similarity_threshold: f32,
        timeout_ms: u32,
    },
    Lexical {
        bm25_k1: f32,
        bm25_b: f32,
        min_score: Option<f32>,
        timeout_ms: u32,
    },
    Graph {
        max_depth: u8,
        max_branching: u32,
        direction: GraphDirection,
        relation_types: Option<Vec<RelationTypeId>>,
        include_statements: bool,
        timeout_ms: u32,
    },
}

/// A single pre-filter pushed down into one retriever
/// invocation. v1 emits at most one per retriever; remaining
/// filters apply post-fusion via [`FilterChain`].
#[derive(Debug, Clone)]
pub enum PreFilter {
    AgentId(AgentId),
    MemoryKind(Vec<MemoryKind>),
    StatementKind(Vec<StatementKind>),
    PredicateId(Vec<PredicateId>),
    Temporal(TimeRange),
}

/// RRF step.
#[derive(Debug, Clone)]
pub struct FusionStep {
    pub k: u32,
    pub weights: PerRetrieverWeights,
}

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("query has no retrievable signal (no text, no entity anchor)")]
    NoSignal,
}

// ---------------------------------------------------------------------------
// Entry point.
// ---------------------------------------------------------------------------

/// Build a [`QueryPlan`] from a [`QueryRequest`].
///
/// Returns [`PlanError::NoSignal`] if the request has neither
/// text nor an entity anchor — without one of those, no
/// retriever would match (v1 limitation; filter-only mode
/// post-v1).
pub fn plan(req: &QueryRequest) -> Result<QueryPlan, PlanError> {
    let routing = route(req);

    if routing.retrievers.is_empty() {
        return Err(PlanError::NoSignal);
    }

    let limit = if req.limit == 0 {
        DEFAULT_RESULT_LIMIT
    } else {
        req.limit
    };
    let top_n = top_n_for(limit);

    let retrievers: Vec<PlannedRetriever> = routing
        .retrievers
        .iter()
        .map(|inv| PlannedRetriever {
            retriever: inv.retriever,
            weight: inv.weight,
            top_n,
            config: retriever_config_for(inv.retriever, req),
            pre_filter: pre_filter_for(req, inv.retriever, &routing),
        })
        .collect();

    let fusion = build_fusion_step(req, &retrievers);
    let post_filters = build_post_filters(req);
    let estimated_cost_ms = estimate_cost(&retrievers, limit);

    Ok(QueryPlan {
        routing,
        retrievers,
        fusion,
        post_filters,
        limit,
        estimated_cost_ms,
    })
}

// ---------------------------------------------------------------------------
// Builders.
// ---------------------------------------------------------------------------

fn top_n_for(limit: u32) -> usize {
    let from_limit = (limit as usize).saturating_mul(3);
    from_limit.max(MIN_TOP_N).min(MAX_TOP_N)
}

fn retriever_config_for(retriever: Retriever, req: &QueryRequest) -> RetrieverConfig {
    let timeout_ms = PER_RETRIEVER_TIMEOUT_MS;
    match retriever {
        Retriever::Semantic => RetrieverConfig::Semantic {
            ef_search: SEMANTIC_DEFAULT_EF_SEARCH,
            similarity_threshold: SEMANTIC_DEFAULT_SIM_THRESHOLD,
            timeout_ms,
        },
        Retriever::Lexical => RetrieverConfig::Lexical {
            bm25_k1: LEXICAL_DEFAULT_BM25_K1,
            bm25_b: LEXICAL_DEFAULT_BM25_B,
            min_score: None,
            timeout_ms,
        },
        Retriever::Graph => {
            // Direction defaults to Outgoing unless the
            // request carries one in a future extension.
            let _ = req;
            RetrieverConfig::Graph {
                max_depth: GRAPH_DEFAULT_MAX_DEPTH,
                max_branching: GRAPH_DEFAULT_MAX_BRANCHING,
                direction: GraphDirection::Outgoing,
                relation_types: None,
                include_statements: true,
                timeout_ms,
            }
        }
    }
}

/// Decide the single push-down pre-filter for this retriever,
/// per the v1 precedence: temporal > predicate > kind.
fn pre_filter_for(
    req: &QueryRequest,
    retriever: Retriever,
    routing: &RoutingDecision,
) -> Option<PreFilter> {
    // Temporal first — works on every retriever.
    if routing.temporal_pushdown {
        if let Some(range) = req.time_filter {
            return Some(PreFilter::Temporal(range));
        }
    }

    // Predicate — semantic + graph only (lexical handles it
    // natively via `LexicalFilters`).
    if !req.predicate_filter.is_empty()
        && matches!(retriever, Retriever::Semantic | Retriever::Graph)
    {
        return Some(PreFilter::PredicateId(req.predicate_filter.clone()));
    }

    // Statement kind — semantic + graph.
    if !req.kind_filter.is_empty() && matches!(retriever, Retriever::Semantic | Retriever::Graph) {
        return Some(PreFilter::StatementKind(req.kind_filter.clone()));
    }

    None
}

fn build_fusion_step(req: &QueryRequest, retrievers: &[PlannedRetriever]) -> FusionStep {
    // k: per-query override if present, else default.
    let k = req.fusion_config.as_ref().map(|c| c.k).unwrap_or(DEFAULT_K);

    // Weights: start from per-query weights if non-default;
    // overlay router-derived weights (max of the two per
    // retriever) so the strongest signal wins.
    let request_weights = req
        .fusion_config
        .as_ref()
        .map(|c| c.weights.clone())
        .unwrap_or_default();

    let router_weights = router_weights_from(retrievers);

    let weights = PerRetrieverWeights {
        semantic: request_weights.semantic.max(router_weights.semantic),
        lexical: request_weights.lexical.max(router_weights.lexical),
        graph: request_weights.graph.max(router_weights.graph),
    };

    FusionStep { k, weights }
}

fn router_weights_from(retrievers: &[PlannedRetriever]) -> PerRetrieverWeights {
    let mut w = PerRetrieverWeights {
        semantic: 0.0,
        lexical: 0.0,
        graph: 0.0,
    };
    for r in retrievers {
        match r.retriever {
            Retriever::Semantic => w.semantic = w.semantic.max(r.weight),
            Retriever::Lexical => w.lexical = w.lexical.max(r.weight),
            Retriever::Graph => w.graph = w.graph.max(r.weight),
        }
    }
    w
}

fn build_post_filters(req: &QueryRequest) -> FilterChain {
    FilterChain {
        kind_filter: req.kind_filter.clone(),
        memory_kind_filter: Vec::new(),
        predicate_filter: req.predicate_filter.clone(),
        time_filter: req.time_filter,
        confidence_min: req.confidence_min,
        include_tombstoned: req.include_tombstoned,
        include_superseded: req.include_superseded,
    }
}

// ---------------------------------------------------------------------------
// Cost estimate (linear-sum, ms units).
// ---------------------------------------------------------------------------

fn estimate_cost(retrievers: &[PlannedRetriever], limit: u32) -> f32 {
    let mut cost = 0.0_f32;
    for r in retrievers {
        cost += match &r.config {
            RetrieverConfig::Semantic { ef_search, .. } => 5.0 + (*ef_search as f32) * 0.05,
            RetrieverConfig::Lexical { .. } => 10.0 + (r.top_n as f32) * 0.05,
            RetrieverConfig::Graph { max_depth, .. } => {
                let d = f32::from(*max_depth);
                5.0 * d * d
            }
        };
    }
    // Fusion: ~0.1 ms per retriever (RRF is essentially free).
    cost += 0.1 * retrievers.len() as f32;
    // Filter chain: ~1 ms per result candidate (read-txn +
    // metadata lookups).
    cost += limit as f32;
    cost
}

// ---------------------------------------------------------------------------
// Plan accessors / utilities (used by EXPLAIN and tests).
// ---------------------------------------------------------------------------

impl QueryPlan {
    /// Was the retriever set chosen by the router or by the
    /// caller's `RetrieverSelection::Explicit(...)`?
    #[must_use]
    pub fn is_explicit(&self) -> bool {
        matches!(
            self.routing.override_kind,
            super::router::OverrideKind::Explicit
        )
    }

    /// True if a temporal pre-filter is attached to **every**
    /// retriever in the plan.
    #[must_use]
    pub fn temporal_pushed_down(&self) -> bool {
        !self.retrievers.is_empty()
            && self
                .retrievers
                .iter()
                .all(|r| matches!(r.pre_filter, Some(PreFilter::Temporal(_))))
    }
}

// `RetrieverSelection` is consumed indirectly via the router;
// re-export the discriminant for tests.
#[allow(dead_code)]
pub use super::router::OverrideKind;
#[allow(dead_code)]
pub use super::router::RetrieverSelection as PlannerRetrieverSelection;

// Silence the unused-import warning if some downstream callers
// don't reach for these.
#[allow(dead_code)]
fn _ensure_used(s: RetrieverSelection) {
    let _ = s;
}

#[cfg(test)]
mod tests;
