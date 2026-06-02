//! Rule-based query router.
//!
//! Given a [`QueryRequest`], classifies the query against a small set
//! of features and selects retrievers + weights per the 5 routing
//! rules. Output is a [`RoutingDecision`] consumed by the planner.

use std::collections::HashMap;
use std::sync::LazyLock;

use brain_core::StatementKind;
use brain_core::{AgentId, EntityId, PredicateId};
use regex::Regex;

// ---------------------------------------------------------------------------
// Constants.
// ---------------------------------------------------------------------------

/// Max retrievers selected by the auto router.
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

/// Structured query request.
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
    /// Memory-context scope. When non-empty the front gate restricts
    /// every retriever to memories with `context_id` in this set —
    /// pushed into the semantic closure and the lexical query's
    /// boolean MUST clause so the filter scopes the search universe
    /// before any expensive stage, not as a post-projection prune.
    pub context_filter: Vec<u64>,
    /// Agent-scope filter. When non-empty the front gate restricts
    /// every retriever to memories whose `agent_id` is in this set.
    /// The recall handler defaults this to `[caller_agent]` so each
    /// agent sees only its own memories without any explicit flag —
    /// `--include-other-agents` on the wire side empties the list to
    /// recover the across-agents view.
    pub agent_filter: Vec<AgentId>,
    /// Bi-temporal time-travel — return only statements the substrate
    /// believed at this record-time unix-nanos. `None` is the default
    /// "current state" query. Server-internal in v1.0: not exposed on
    /// the wire `RecallRequest` (would require an additive rkyv archive
    /// bump). Callers route through it via the hybrid query API and
    /// admin / explore tooling.
    pub as_of_record_time_unix_nanos: Option<u64>,
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

/// Per-query fusion config that the planner reads.
#[derive(Debug, Clone, PartialEq)]
pub struct FusionConfig {
    pub k: u32,
    pub weights: PerRetrieverWeights,
}

/// Per-retriever RRF weight. Semantic / lexical / graph default
/// to 1.0; temporal is reserved for a future temporal retriever
/// and defaults to 0.5 so that — when wired — it never dominates
/// the three primary signals.
#[derive(Debug, Clone, PartialEq)]
pub struct PerRetrieverWeights {
    pub semantic: f32,
    pub lexical: f32,
    pub graph: f32,
    pub temporal: f32,
}

impl Default for PerRetrieverWeights {
    fn default() -> Self {
        Self {
            semantic: 1.0,
            lexical: 1.0,
            graph: 1.0,
            temporal: 0.5,
        }
    }
}

/// Coarse query class used to pick a [`RetrievalProfile`]. The
/// router derives this from the same features it uses for
/// rule-routing; the planner reads it to pick adaptive top-K and
/// per-retriever weights.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryClass {
    /// Caller supplied an `EntityId` anchor, or the text contains
    /// a Title-Case mention strong enough to be treated as one.
    /// Graph retriever is the precision driver — we widen its
    /// weight and narrow per-retriever pool.
    EntityAnchored,
    /// Text contains an exact identifier (`ACME-1247`) or all-caps
    /// tokens. Lexical match is more reliable than semantic.
    ExactTerm,
    /// Free-form natural language with no exact-term or anchor
    /// signal. Semantic embedding does the heavy lifting; pool
    /// widens so RRF has overlap to fuse.
    Paraphrase,
    /// Everything else — used when no other class fits and as the
    /// fallback for empty / filter-only queries.
    Default,
}

/// What the planner should plug into the per-query fusion config
/// based on the [`QueryClass`]. Weights tune which retriever's
/// rank-1 hit dominates; per-retriever top-N tunes how wide each
/// retriever's pool is before fusion narrows it back down.
#[derive(Debug, Clone, PartialEq)]
pub struct RetrievalProfile {
    pub weights: PerRetrieverWeights,
    pub per_retriever_top_n: usize,
    pub final_top_k: usize,
}

impl RetrievalProfile {
    /// Build a profile for the given class. `requested_top_k` is
    /// the caller's `limit` (or the planner's default when
    /// unspecified) and rides through unchanged so the user's
    /// `top_k` request always wins.
    #[must_use]
    pub fn for_class(class: QueryClass, requested_top_k: usize) -> Self {
        match class {
            QueryClass::EntityAnchored => Self {
                weights: PerRetrieverWeights {
                    semantic: 1.0,
                    lexical: 0.7,
                    graph: 2.0,
                    temporal: 0.5,
                },
                per_retriever_top_n: 50,
                final_top_k: requested_top_k,
            },
            QueryClass::ExactTerm => Self {
                weights: PerRetrieverWeights {
                    semantic: 1.0,
                    lexical: 2.0,
                    graph: 0.7,
                    temporal: 0.5,
                },
                per_retriever_top_n: 100,
                final_top_k: requested_top_k,
            },
            QueryClass::Paraphrase => Self {
                weights: PerRetrieverWeights {
                    semantic: 1.5,
                    lexical: 1.0,
                    // The unanchored memory-rider is a weak,
                    // lower-precision signal; keep it from swaying the
                    // semantic-led ordering on free-text queries.
                    graph: 0.5,
                    temporal: 0.5,
                },
                per_retriever_top_n: 200,
                final_top_k: requested_top_k,
            },
            QueryClass::Default => Self {
                weights: PerRetrieverWeights::default(),
                per_retriever_top_n: 100,
                final_top_k: requested_top_k,
            },
        }
    }
}

/// Classify a request into a single [`QueryClass`]. The priority
/// is: entity anchor or Title-Case mention → `EntityAnchored`;
/// exact-id or all-caps tokens → `ExactTerm`; free text → `Paraphrase`;
/// no text + no anchor → `Default`. Filter-only requests fall to
/// `Default` because no retriever signal is left to weight.
#[must_use]
pub fn classify_query(features: &ClassificationFeatures) -> QueryClass {
    if features.has_entity_anchor || features.contains_entity_mention_heuristic {
        QueryClass::EntityAnchored
    } else if features.contains_exact_id || features.is_all_caps_tokens {
        QueryClass::ExactTerm
    } else if features.has_text {
        QueryClass::Paraphrase
    } else {
        QueryClass::Default
    }
}

/// Retriever discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Retriever {
    Semantic,
    Lexical,
    Graph,
}

/// What the router decided. Consumed by the planner to build the
/// executable plan DAG.
#[derive(Debug, Clone)]
pub struct RoutingDecision {
    pub features: ClassificationFeatures,
    pub retrievers: Vec<RetrieverInvocation>,
    pub override_kind: OverrideKind,
    /// The filter chain sees this — if true, push the temporal
    /// predicate into the retrievers as a pre-filter rather than
    /// applying it post-fusion.
    pub temporal_pushdown: bool,
    /// How the graph retriever (if any) anchors its walk.
    /// `Entity` is the typed-typed-graph mode (relations table);
    /// `MemoryFromSemantic` tells the executor to materialise
    /// the anchor set from semantic top-K and walk the substrate
    /// edge tables. `None` when graph isn't selected.
    pub graph_anchor_mode: Option<GraphAnchorMode>,
    /// Coarse classification used downstream to pick adaptive
    /// top-K and per-retriever weights via [`RetrievalProfile`].
    pub query_class: QueryClass,
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
/// so operators can see why a query routed the way it did.
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

/// Route a [`QueryRequest`] into a [`RoutingDecision`].
#[must_use]
pub fn route(req: &QueryRequest) -> RoutingDecision {
    let features = classify(req);

    // Explicit override — honour client choice verbatim, flat
    // weight 1.0 per retriever (per-retriever weight tuning
    // rides in fusion_config).
    //
    // Max retrievers per query: 3 (all of semantic, lexical, graph
    // if matched). The cap applies uniformly — the explicit override does NOT
    // bypass it. The client already enforces this at construction
    // (`RetrieverSelection::explicit()`), but a raw wire caller
    // or another-language client could submit a longer list. We
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
        let query_class = classify_query(&features);
        return RoutingDecision {
            features,
            retrievers,
            override_kind: OverrideKind::Explicit,
            temporal_pushdown,
            graph_anchor_mode,
            query_class,
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

    // Rules 3 + 4 add no retrievers — they signal the filter chain.
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
    let query_class = classify_query(&features);
    RoutingDecision {
        features,
        retrievers,
        override_kind: OverrideKind::Auto,
        temporal_pushdown,
        graph_anchor_mode,
        query_class,
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
