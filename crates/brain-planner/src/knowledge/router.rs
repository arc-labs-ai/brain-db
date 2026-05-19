//! Rule-based query router (phase 23.3).
//!
//! Implements §24/00 §"Query router". Given a [`QueryRequest`],
//! classifies the query against a small set of features and
//! selects retrievers + weights per the 5 routing rules in
//! §24/00 §"Routing rules". Output is a [`RoutingDecision`]
//! consumed by the planner (23.6).

use std::collections::HashMap;
use std::sync::LazyLock;

use brain_core::knowledge::StatementKind;
use brain_core::{EntityId, PredicateId};
use regex::Regex;

// ---------------------------------------------------------------------------
// Constants.
// ---------------------------------------------------------------------------

/// Max retrievers selected by the auto router (§24/00
/// §"Limits and budgets").
pub const MAX_RETRIEVERS: usize = 3;

/// Words that begin a question. Lowercased. Used to set
/// `ClassificationFeatures::is_question`.
const QUESTION_STARTS: &[&str] = &[
    "what", "who", "where", "when", "why", "how", "does", "do ", "is ", "are ", "can ", "could ",
];

// Regex literals are static-compiled via `LazyLock` so per-
// query routing has zero compile cost.
static EXACT_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[A-Z][A-Z0-9]+-\d+\b").expect("invariant: literal"));
static TITLE_CASE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[A-Z][a-z]+(?:\s+[A-Z][a-z]+)*\b").expect("invariant: literal")
});
static TEMPORAL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b(yesterday|today|tomorrow|last\s+(week|month|year|\d+\s+days?|\d+\s+weeks?|\d+\s+months?)|next\s+(week|month|year)|\d{4}-\d{2}-\d{2})\b",
    )
    .expect("invariant: literal")
});

// ---------------------------------------------------------------------------
// Public types.
// ---------------------------------------------------------------------------

/// Structured query request — mirrors §24/00 §"Structured request".
#[derive(Debug, Clone, Default)]
pub struct QueryRequest {
    pub text: Option<String>,
    pub entity_anchor: Option<EntityId>,
    pub kind_filter: Vec<StatementKind>,
    pub predicate_filter: Vec<PredicateId>,
    pub time_filter: Option<TimeRange>,
    pub confidence_min: Option<f32>,
    pub include_tombstoned: bool,
    pub include_superseded: bool,
    pub limit: u32,
    pub retrievers: RetrieverSelection,
    pub fusion_config: Option<FusionConfig>,
}

/// Inclusive-start / inclusive-end window. `None` bounds mean
/// open-ended.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TimeRange {
    pub from_unix_ms: Option<u64>,
    pub to_unix_ms: Option<u64>,
}

/// Either the router picks retrievers (`Auto`) or the client
/// names them (`Explicit`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum RetrieverSelection {
    #[default]
    Auto,
    Explicit(Vec<Retriever>),
}

/// Per-query fusion config that the planner reads (23.4 / 23.6).
#[derive(Debug, Clone, PartialEq)]
pub struct FusionConfig {
    pub k: u32,
    pub weights: PerRetrieverWeights,
}

/// Equal-weight default per §23/01 §"Per-retriever weights".
#[derive(Debug, Clone, PartialEq)]
pub struct PerRetrieverWeights {
    pub semantic: f32,
    pub lexical: f32,
    pub graph: f32,
}

impl Default for PerRetrieverWeights {
    fn default() -> Self {
        Self {
            semantic: 1.0,
            lexical: 1.0,
            graph: 1.0,
        }
    }
}

/// Retriever discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Retriever {
    Semantic,
    Lexical,
    Graph,
}

/// What the router decided. Consumed by the planner (23.6) to
/// build the executable plan DAG.
#[derive(Debug, Clone)]
pub struct RoutingDecision {
    pub features: ClassificationFeatures,
    pub retrievers: Vec<RetrieverInvocation>,
    pub override_kind: OverrideKind,
    /// Filter chain (23.5) sees this — if true, push the
    /// temporal predicate into the retrievers as a pre-filter
    /// rather than applying it post-fusion.
    pub temporal_pushdown: bool,
    /// How the graph retriever (if any) anchors its walk.
    /// `Entity` is the typed-knowledge mode (relations table);
    /// `MemoryFromSemantic` tells the executor to materialise
    /// the anchor set from semantic top-K and walk the substrate
    /// edge tables. `None` when graph isn't selected.
    pub graph_anchor_mode: Option<GraphAnchorMode>,
}

/// Where the graph retriever gets its starting nodes. The
/// router picks the mode; the executor reads `graph_anchor_mode`
/// to dispatch the right walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphAnchorMode {
    /// Caller supplied an entity in `QueryRequest.entity_anchor`.
    /// The graph walks the typed relations table from that
    /// entity.
    Entity,
    /// No entity anchor but the request carries cue text. The
    /// executor runs semantic first, takes its top-K memory
    /// hits, and walks substrate edges from each. This keeps
    /// graph contributing on schemaless deployments where no
    /// `EntityId` is ever known.
    MemoryFromSemantic,
}

#[derive(Debug, Clone, Copy)]
pub struct RetrieverInvocation {
    pub retriever: Retriever,
    pub weight: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverrideKind {
    Auto,
    Explicit,
}

/// Features extracted from the request — the router's
/// transparent decision input. Surfaces in EXPLAIN/TRACE
/// (23.8) so operators can see why a query routed the way it
/// did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClassificationFeatures {
    pub has_text: bool,
    pub has_entity_anchor: bool,
    pub has_time_filter: bool,
    pub has_type_filter: bool,
    pub has_predicate_filter: bool,
    pub contains_exact_id: bool,
    pub is_all_caps_tokens: bool,
    pub is_short_and_noun_heavy: bool,
    pub is_question: bool,
    pub contains_entity_mention_heuristic: bool,
    pub contains_temporal_expression: bool,
}

// ---------------------------------------------------------------------------
// Routing entry point.
// ---------------------------------------------------------------------------

/// Route a [`QueryRequest`] into a [`RoutingDecision`] per
/// §24/00 §"Routing rules".
#[must_use]
pub fn route(req: &QueryRequest) -> RoutingDecision {
    let features = classify(req);

    // Explicit override — honour client choice verbatim, flat
    // weight 1.0 per retriever (per-retriever weight tuning
    // rides in fusion_config; see §23/01).
    //
    // Spec §24/00 §"Limits and budgets": "Max retrievers per
    // query: 3 (all of semantic, lexical, graph if matched)".
    // The cap applies uniformly — the explicit override does NOT
    // bypass it. The SDK already enforces this at construction
    // (`RetrieverSelection::explicit()`), but a raw wire caller
    // or another-language SDK could submit a longer list. We
    // dedup + truncate here so the planner's downstream
    // assumptions (per-retriever slot count, fusion config arity)
    // hold regardless of caller.
    if let RetrieverSelection::Explicit(list) = &req.retrievers {
        tracing::info!(
            target: "brain_planner::router",
            count = list.len(),
            "retriever override accepted",
        );
        // Preserve the caller's order on dedup so trace output is
        // predictable.
        let mut seen: Vec<Retriever> = Vec::with_capacity(list.len().min(MAX_RETRIEVERS));
        for r in list {
            if !seen.contains(r) {
                seen.push(*r);
                if seen.len() == MAX_RETRIEVERS {
                    if list.len() > MAX_RETRIEVERS {
                        tracing::warn!(
                            target: "brain_planner::router",
                            requested = list.len(),
                            cap = MAX_RETRIEVERS,
                            "explicit retriever list exceeded cap; truncating",
                        );
                    }
                    break;
                }
            }
        }
        let retrievers: Vec<RetrieverInvocation> = seen
            .into_iter()
            .map(|retriever| RetrieverInvocation {
                retriever,
                weight: 1.0,
            })
            .collect();
        let temporal_pushdown = features.has_time_filter || features.contains_temporal_expression;
        let graph_anchor_mode = pick_graph_anchor_mode(&retrievers, &features);
        return RoutingDecision {
            features,
            retrievers,
            override_kind: OverrideKind::Explicit,
            temporal_pushdown,
            graph_anchor_mode,
        };
    }

    // Auto routing — union of matching rules with max-weight.
    let mut weights: HashMap<Retriever, f32> = HashMap::new();

    // Rule 1: entity-anchored.
    if features.has_entity_anchor || features.contains_entity_mention_heuristic {
        upsert_max(&mut weights, Retriever::Graph, 2.0);
        upsert_max(&mut weights, Retriever::Semantic, 1.0);
        if features.has_text {
            upsert_max(&mut weights, Retriever::Lexical, 0.5);
        }
    }

    // Rule 2: exact-term.
    if features.contains_exact_id || features.is_all_caps_tokens {
        upsert_max(&mut weights, Retriever::Lexical, 2.0);
        upsert_max(&mut weights, Retriever::Semantic, 0.5);
    }

    // Rule 5: default — fires only if no other rule matched
    // AND the query has text.
    //
    // Schemaless deployments live here: no entity anchor, no
    // exact-id signature. Hybrid is still the default — semantic
    // + lexical fuse, and graph rides along anchored at the
    // semantic top-K so substrate `SimilarTo` / `Caused` edges
    // contribute. The executor materialises the anchor set after
    // semantic runs.
    if weights.is_empty() && features.has_text {
        upsert_max(&mut weights, Retriever::Semantic, 1.0);
        upsert_max(&mut weights, Retriever::Lexical, 1.0);
        upsert_max(&mut weights, Retriever::Graph, 0.5);
    }

    // Rules 3 + 4 add no retrievers — they signal the filter
    // chain (23.5).
    let temporal_pushdown = features.has_time_filter || features.contains_temporal_expression;

    // Cap at MAX_RETRIEVERS by weight descending; stable
    // tie-break by retriever discriminant order.
    let mut sorted: Vec<(Retriever, f32)> = weights.into_iter().collect();
    sorted.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| discriminant_order(a.0).cmp(&discriminant_order(b.0)))
    });
    sorted.truncate(MAX_RETRIEVERS);

    let retrievers: Vec<RetrieverInvocation> = sorted
        .into_iter()
        .map(|(retriever, weight)| RetrieverInvocation { retriever, weight })
        .collect();

    let graph_anchor_mode = pick_graph_anchor_mode(&retrievers, &features);
    RoutingDecision {
        features,
        retrievers,
        override_kind: OverrideKind::Auto,
        temporal_pushdown,
        graph_anchor_mode,
    }
}

/// Decide which graph mode the executor should run. Entity mode
/// when the request carried an entity anchor; memory-from-
/// semantic when graph is selected without an anchor but with
/// text (the schemaless hybrid path). `None` when graph isn't in
/// the retriever set at all.
fn pick_graph_anchor_mode(
    retrievers: &[RetrieverInvocation],
    features: &ClassificationFeatures,
) -> Option<GraphAnchorMode> {
    let has_graph = retrievers.iter().any(|r| r.retriever == Retriever::Graph);
    if !has_graph {
        return None;
    }
    if features.has_entity_anchor {
        Some(GraphAnchorMode::Entity)
    } else if features.has_text {
        Some(GraphAnchorMode::MemoryFromSemantic)
    } else {
        // Graph selected with neither entity anchor nor text —
        // shouldn't reach here under the current rules, but if
        // it does the executor's Graph invocation will simply
        // skip with "no anchor" rather than panic.
        None
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn classify(req: &QueryRequest) -> ClassificationFeatures {
    let mut f = ClassificationFeatures {
        has_text: req.text.is_some(),
        has_entity_anchor: req.entity_anchor.is_some(),
        has_time_filter: req.time_filter.is_some(),
        has_type_filter: !req.kind_filter.is_empty(),
        has_predicate_filter: !req.predicate_filter.is_empty(),
        ..Default::default()
    };

    if let Some(text) = req.text.as_deref() {
        let trimmed = text.trim();
        let lower = trimmed.to_lowercase();

        f.contains_exact_id = EXACT_ID_RE.is_match(trimmed);

        let tokens: Vec<&str> = trimmed.split_whitespace().collect();
        f.is_all_caps_tokens = !tokens.is_empty()
            && tokens.iter().all(|w| {
                w.chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '-' || c == '_')
                    && w.chars()
                        .any(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            });

        f.is_short_and_noun_heavy = tokens.len() <= 4 && !trimmed.contains('?');

        f.is_question =
            trimmed.contains('?') || QUESTION_STARTS.iter().any(|q| lower.starts_with(q));

        f.contains_entity_mention_heuristic = TITLE_CASE_RE.is_match(trimmed);

        f.contains_temporal_expression = TEMPORAL_RE.is_match(&lower);
    }

    f
}

fn upsert_max(weights: &mut HashMap<Retriever, f32>, r: Retriever, w: f32) {
    weights
        .entry(r)
        .and_modify(|cur| {
            if w > *cur {
                *cur = w;
            }
        })
        .or_insert(w);
}

fn discriminant_order(r: Retriever) -> u8 {
    match r {
        Retriever::Semantic => 0,
        Retriever::Lexical => 1,
        Retriever::Graph => 2,
    }
}

#[cfg(test)]
mod tests;
