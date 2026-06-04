//! Query planner.
//!
//! Takes a `QueryRequest`, calls the router, expands the routing
//! decision into a concrete plan DAG (which retrievers run, with what
//! pre-filters and configs; which filters land post-fusion;
//! what `k` and weights feed RRF; what limit and cost estimate
//! the executor should respect).
//!
//! The plan is immutable, serialisable-shaped, and consumed by
//! both:
//!
//! - The executor — runs each retriever in parallel, then fuses,
//!   then applies the filter chain, then truncates to `limit`.
//! - EXPLAIN / TRACE — renders the plan without executing.

use brain_core::StatementKind;
use brain_core::{AgentId, MemoryKind, PredicateId, RelationTypeId};
use brain_index::Direction as GraphDirection;

use super::filters::FilterChain;
use super::fusion::{FusionMethod, DEFAULT_K};
use super::router::{
    route, GraphAnchorMode, PerRetrieverWeights, QueryRequest, RetrievalProfile, Retriever,
    RoutingDecision, TimeRange,
};

// ---------------------------------------------------------------------------
// Defaults.
// ---------------------------------------------------------------------------

/// Default top_k returned to the caller if `req.limit == 0`.
pub const DEFAULT_RESULT_LIMIT: u32 = 20;

/// Minimum per-retriever `top_n` — guarantees fusion has
/// candidates to work with even when one retriever's top-K
/// overlaps poorly with another's.
pub const MIN_TOP_N: usize = 100;

/// Hard cap on per-retriever `top_n` — `top_n_per_retriever ≤ 200`.
pub const MAX_TOP_N: usize = 200;

// Retriever-config defaults:

const SEMANTIC_DEFAULT_EF_SEARCH: usize = 64;
const SEMANTIC_DEFAULT_SIM_THRESHOLD: f32 = 0.0;
const LEXICAL_DEFAULT_BM25_K1: f32 = 1.2;
const LEXICAL_DEFAULT_BM25_B: f32 = 0.75;
const GRAPH_DEFAULT_MAX_DEPTH: u8 = 3;
const GRAPH_DEFAULT_MAX_BRANCHING: u32 = 200;
// Soft timeout per retriever — emits a WARN and treats the
// retriever's outcome as Timeout when exceeded, but does NOT
// kill in-flight work; results still flow through fusion if
// they arrive late. 50 ms is fine for HNSW + tantivy lookups
// in isolation, but the semantic retriever's CPU embed step
// (BGE-small via candle) takes 200–500 ms cold and 100–300 ms
// hot — and several hundred more when an LLM extractor worker
// is contending for the same core. 1 s gives the whole
// pipeline headroom without making genuinely stuck retrievers
// invisible.
const PER_RETRIEVER_TIMEOUT_MS: u32 = 1_000;
/// Memory-from-semantic anchor count. Small budget for the
/// per-anchor walk fan-out so total graph cost stays sub-ms.
const GRAPH_DEFAULT_MEMORY_ANCHOR_TOP_K: u8 = 3;

// ---------------------------------------------------------------------------
// Public types.
// ---------------------------------------------------------------------------

/// The output of [`plan`]. Immutable; passed to the executor and
/// surfaced verbatim by EXPLAIN.
#[derive(Debug, Clone)]
pub struct QueryPlan {
    pub routing: RoutingDecision,
    pub retrievers: Vec<PlannedRetriever>,
    pub fusion: FusionStep,
    pub post_filters: FilterChain,
    pub limit: u32,
    pub estimated_cost_ms: f32,
    /// Retrieval profile picked from `routing.query_class`. The
    /// executor uses `weights` for RRF and `final_top_k` after the
    /// rerank stage; the planner already baked `per_retriever_top_n`
    /// into each [`PlannedRetriever::top_n`].
    pub profile: RetrievalProfile,
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

/// Per-retriever knobs. Variants match the semantic / lexical / graph
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
        /// How the executor anchors the walk. Carried through
        /// from `RoutingDecision::graph_anchor_mode`.
        anchor_mode: GraphAnchorMode,
        /// When `anchor_mode == MemoryFromSemantic`, the
        /// executor walks from this many top semantic hits in
        /// parallel. Defaults to 3 — small enough to keep
        /// graph latency under a millisecond, large enough that
        /// at least one anchor will have rich neighbourhood.
        anchor_top_k: u8,
    },
}

/// A single pre-filter pushed down into one retriever
/// invocation. v1 emits at most one per retriever; remaining
/// filters apply post-fusion via [`FilterChain`].
#[derive(Debug, Clone)]
pub enum PreFilter {
    /// Restrict the retriever's candidate universe to memories
    /// owned by any of these agent ids. Empty `Vec` is invalid —
    /// the planner only emits this variant when the caller asked
    /// for an agent scope.
    AgentIds(Vec<AgentId>),
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
    pub method: FusionMethod,
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
    let profile = RetrievalProfile::for_class(routing.query_class, limit as usize);
    let top_n = top_n_for(limit, profile.per_retriever_top_n);

    let retrievers: Vec<PlannedRetriever> = routing
        .retrievers
        .iter()
        .map(|inv| PlannedRetriever {
            retriever: inv.retriever,
            weight: inv.weight,
            top_n,
            config: retriever_config_for(inv.retriever, req, &routing),
            pre_filter: pre_filter_for(req, inv.retriever, &routing),
        })
        .collect();

    let fusion = build_fusion_step(req, &retrievers, &profile);
    let post_filters = build_post_filters(req);
    let estimated_cost_ms = estimate_cost(&retrievers, limit);

    Ok(QueryPlan {
        routing,
        retrievers,
        fusion,
        post_filters,
        limit,
        estimated_cost_ms,
        profile,
    })
}

// ---------------------------------------------------------------------------
// Builders.
// ---------------------------------------------------------------------------

/// Combine the caller's `limit` (× 3 oversample) with the
/// retrieval profile's `per_retriever_top_n` hint, then clamp into
/// [`MIN_TOP_N`, `MAX_TOP_N`]. The max keeps any one retriever's
/// per-query work bounded; the min guarantees fusion has enough
/// candidates even on small `top_k` requests where the user's
/// `× 3` value would otherwise undershoot.
fn top_n_for(limit: u32, profile_hint: usize) -> usize {
    let from_limit = (limit as usize).saturating_mul(3);
    let raw = from_limit.max(profile_hint);
    raw.clamp(MIN_TOP_N, MAX_TOP_N)
}

fn retriever_config_for(
    retriever: Retriever,
    req: &QueryRequest,
    routing: &RoutingDecision,
) -> RetrieverConfig {
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
            let _ = req;
            // Default Outgoing for entity mode (typed relations
            // are directional); Both for memory mode (substrate
            // edges include symmetric SimilarTo / Contradicts).
            let anchor_mode = routing.graph_anchor_mode.unwrap_or(GraphAnchorMode::Entity);
            let direction = match anchor_mode {
                GraphAnchorMode::Entity => GraphDirection::Outgoing,
                GraphAnchorMode::MemoryFromSemantic => GraphDirection::Both,
            };
            // Memory-anchor walks include_statements is moot —
            // there are no statements on the substrate edge
            // table. Keep the field for entity mode.
            let include_statements = matches!(anchor_mode, GraphAnchorMode::Entity);
            RetrieverConfig::Graph {
                max_depth: GRAPH_DEFAULT_MAX_DEPTH,
                max_branching: GRAPH_DEFAULT_MAX_BRANCHING,
                direction,
                relation_types: None,
                include_statements,
                timeout_ms,
                anchor_mode,
                anchor_top_k: GRAPH_DEFAULT_MEMORY_ANCHOR_TOP_K,
            }
        }
    }
}

/// Decide the single push-down pre-filter for this retriever,
/// per the v1 precedence: agent > temporal > predicate > kind.
///
/// Agent scope is the most-selective, identity-based axis — it
/// defines *which memories are even visible* to this caller, so
/// every other filter (temporal, predicate, kind) is a refinement
/// inside the already-isolated agent universe. We push it first so
/// the retriever's candidate set is bounded by ownership before any
/// of the type / time / predicate refinements have to run.
fn pre_filter_for(
    req: &QueryRequest,
    retriever: Retriever,
    routing: &RoutingDecision,
) -> Option<PreFilter> {
    // Agent scope first — most selective, identity-based axis.
    // Applies to every retriever; the rest are refinements inside
    // the already-isolated agent universe.
    if !req.agent_filter.is_empty() {
        return Some(PreFilter::AgentIds(req.agent_filter.clone()));
    }

    // Temporal — works on every retriever.
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

fn build_fusion_step(
    req: &QueryRequest,
    retrievers: &[PlannedRetriever],
    profile: &RetrievalProfile,
) -> FusionStep {
    // k: per-query override if present, else default.
    let k = req.fusion_config.as_ref().map(|c| c.k).unwrap_or(DEFAULT_K);

    // Three weight sources, layered per priority:
    //   1. caller-supplied `fusion_config.weights` (explicit override)
    //   2. router-derived weights (per-rule signal strength)
    //   3. profile weights (adaptive defaults from QueryClass)
    // We max across all three so the strongest signal wins per retriever.
    let request_weights = req.fusion_config.as_ref().map(|c| c.weights.clone());
    let router_weights = router_weights_from(retrievers);
    let profile_weights = &profile.weights;

    let weights = match request_weights {
        Some(rw) => PerRetrieverWeights {
            semantic: rw
                .semantic
                .max(router_weights.semantic)
                .max(profile_weights.semantic),
            lexical: rw
                .lexical
                .max(router_weights.lexical)
                .max(profile_weights.lexical),
            graph: rw
                .graph
                .max(router_weights.graph)
                .max(profile_weights.graph),
            temporal: rw.temporal.max(profile_weights.temporal),
        },
        None => PerRetrieverWeights {
            semantic: router_weights.semantic.max(profile_weights.semantic),
            lexical: router_weights.lexical.max(profile_weights.lexical),
            graph: router_weights.graph.max(profile_weights.graph),
            temporal: profile_weights.temporal,
        },
    };

    FusionStep {
        k,
        weights,
        method: fusion_method_from_env(),
    }
}

/// Deploy-time fusion-method selector. Defaults to score-aware
/// (`RelativeScore`); `BRAIN_FUSION_METHOD=rrf|zscore` switches it
/// without a recompile so the strategies can be A/B'd on a fixed
/// corpus.
fn fusion_method_from_env() -> FusionMethod {
    match std::env::var("BRAIN_FUSION_METHOD")
        .ok()
        .as_deref()
        .map(str::trim)
    {
        Some("rrf") => FusionMethod::Rrf,
        Some("zscore" | "relative_zscore") => FusionMethod::RelativeScoreZScore,
        _ => FusionMethod::RelativeScore,
    }
}

fn router_weights_from(retrievers: &[PlannedRetriever]) -> PerRetrieverWeights {
    let mut w = PerRetrieverWeights {
        semantic: 0.0,
        lexical: 0.0,
        graph: 0.0,
        temporal: 0.0,
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
        as_of_record_time_unix_nanos: req.as_of_record_time_unix_nanos,
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

#[cfg(test)]
mod tests;
