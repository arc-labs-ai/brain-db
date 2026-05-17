# Plan: Phase 23 — Task 03, Query router

**Status:** awaiting-confirmation
**Date:** 2026-05-17
**Author:** Claude (autonomous)
**Estimated commits:** 1

---

## 1. Scope

Implement the rule-based query router defined in §24/00
§"Query router". The router takes a `QueryRequest` and produces
a `RoutingDecision`: which retrievers to invoke, with which
weights, and which features were detected. The decision is the
input to the planner (23.6).

Concrete deliverables:

1. New module `crates/brain-planner/src/knowledge/router.rs`:
   - `QueryRequest` (the structured input shape from §24/00
     §"Structured request").
   - `RoutingDecision { retrievers: Vec<RetrieverInvocation>,
     features: ClassificationFeatures, override_kind }`.
   - `Retriever` enum tag — `Semantic | Lexical | Graph`.
   - `route(&QueryRequest) -> RoutingDecision` function.
2. Classification features per §24/00 §"Classification features":
   - `has_text`, `has_entity_anchor`, `has_time_filter`,
     `has_type_filter`, `has_predicate_filter`.
   - `contains_exact_id` (regex `[A-Z][A-Z0-9]+-\d+`),
     `is_all_caps_tokens` (text looks like a code-style query),
     `is_short_and_noun_heavy` (≤ 4 tokens, no question
     punctuation), `is_question` (starts with what / who /
     where / when / why / how / does / is / are, or contains
     `?`).
   - `contains_entity_mention_heuristic` — simple Title-Case
     detection over the text as a v1 NER proxy. Full NER via
     the phase-20 classifier extractor is post-v1 (added cost
     per query is significant; the rule-based router degrades
     gracefully).
   - `contains_temporal_expression` — regex over a small set
     of obvious time phrases (`yesterday`, `today`, `last
     week`, `last month`, `last N days`, ISO dates).
3. Five routing rules (§24/00 §"Routing rules"):
   - Rule 1 entity-anchored → Graph(2.0) + Semantic(1.0) +
     (if text) Lexical(0.5).
   - Rule 2 exact-term → Lexical(2.0) + Semantic(0.5).
   - Rule 3 time-filtered → no new retriever; flag for the
     filter chain (23.5) to push down the temporal filter.
   - Rule 4 type-filtered → no new retriever; filter chain
     applies post-fusion.
   - Rule 5 default (free text, no other signal) →
     Semantic(1.0) + Lexical(1.0).
4. Rule union with `max(weight)` per retriever across matched
   rules; cap at 3 retrievers (matches §24/00 §"Limits and
   budgets").
5. Explicit override (`RetrieverSelection::Explicit(vec)`) — if
   the client supplied an explicit retriever list, the router
   honours it verbatim, logs the override.
6. Unit tests across the 5 rules, classification heuristics,
   union semantics, explicit override.

NOT in scope (per master plan §4 + §6):
- Real NER over query text (post-v1).
- Cost estimation — that's the planner's job (23.6); router
  emits a flat decision, the planner attaches cost.
- Per-retriever timeout / top_n decision — defaults from the
  retriever specs; the planner may override.
- Learned routing — explicit post-v1.

## 2. Spec references

- `spec/24_hybrid_query/00_purpose.md` §"Query router" — the
  five rules, classification features, limits, override.
- `spec/24_hybrid_query/00_purpose.md` §"Query shape" — the
  `QueryRequest` struct shape we mirror.
- `spec/23_retrievers/01_rrf_fusion.md` §"Per-query weights"
  — the router-chosen weights feed RRF (23.4).

## 3. External validation

| Item | Source | Confirmed |
|---|---|---|
| `regex` crate available | workspace dep (pulled by phase 20) | ✓ |
| `brain-planner` already depends on `brain-index` | `crates/brain-planner/Cargo.toml:14` | ✓ — can reference retriever types directly. |
| `QueryRequest` shape in §24/00 | spec | The fields map cleanly to a Rust struct; we adapt them to brain-core types (`EntityId`, `StatementKind`, etc.). |

## 4. Architecture sketch

### Module layout

```
crates/brain-planner/src/knowledge/
  mod.rs            // pub mod router (phase 23.3)
                    //                 // + fusion (23.4)
                    //                 // + filters (23.5)
                    //                 // + planner (23.6)
                    //                 // + executor (23.7)
  router.rs         (new)
```

`knowledge/mod.rs` is a new submodule added to
`brain-planner/src/lib.rs` so phase 23's planner pieces sit
together.

### Types

```rust
// crates/brain-planner/src/knowledge/router.rs

use brain_core::{EntityId, PredicateId};
use brain_core::knowledge::StatementKind;
use std::time::Duration;

/// The structured query input (§24/00 §"Structured request").
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

#[derive(Debug, Clone, Copy, Default)]
pub struct TimeRange {
    pub from_unix_ms: Option<u64>,
    pub to_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub enum RetrieverSelection {
    #[default]
    Auto,
    Explicit(Vec<Retriever>),
}

#[derive(Debug, Clone)]
pub struct FusionConfig {
    pub k: u32,
    pub weights: PerRetrieverWeights,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Retriever { Semantic, Lexical, Graph }

#[derive(Debug, Clone)]
pub struct RoutingDecision {
    pub features: ClassificationFeatures,
    pub retrievers: Vec<RetrieverInvocation>,
    pub override_kind: OverrideKind,
    pub temporal_pushdown: bool,
}

#[derive(Debug, Clone)]
pub struct RetrieverInvocation {
    pub retriever: Retriever,
    pub weight: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverrideKind { Auto, Explicit }

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

pub const MAX_RETRIEVERS: usize = 3;

/// Route a `QueryRequest` into a `RoutingDecision`.
pub fn route(req: &QueryRequest) -> RoutingDecision { ... }
```

### Heuristics (lib-internal)

```rust
fn classify(text: Option<&str>, req: &QueryRequest) -> ClassificationFeatures {
    let mut f = ClassificationFeatures::default();
    f.has_text = text.is_some();
    f.has_entity_anchor = req.entity_anchor.is_some();
    f.has_time_filter = req.time_filter.is_some();
    f.has_type_filter = !req.kind_filter.is_empty();
    f.has_predicate_filter = !req.predicate_filter.is_empty();

    if let Some(t) = text {
        f.contains_exact_id = EXACT_ID_RE.is_match(t);
        let tokens: Vec<&str> = t.split_whitespace().collect();
        f.is_all_caps_tokens =
            !tokens.is_empty() && tokens.iter().all(|w| w.chars().all(|c| c.is_uppercase() || c.is_ascii_digit() || c == '-' || c == '_'));
        f.is_short_and_noun_heavy = tokens.len() <= 4 && !t.contains('?');
        f.is_question = QUESTION_STARTS.iter().any(|q| t.trim_start().to_lowercase().starts_with(q))
                         || t.contains('?');
        f.contains_entity_mention_heuristic = TITLE_CASE_RE.is_match(t);
        f.contains_temporal_expression = TEMPORAL_RE.is_match(&t.to_lowercase());
    }
    f
}
```

### Rules

```rust
fn route(req: &QueryRequest) -> RoutingDecision {
    let features = classify(req.text.as_deref(), req);

    if let RetrieverSelection::Explicit(list) = &req.retrievers {
        tracing::info!(target: "brain_planner::router", "retriever override accepted");
        let retrievers = list.iter().map(|r| RetrieverInvocation {
            retriever: *r, weight: 1.0,
        }).collect();
        return RoutingDecision {
            features,
            retrievers,
            override_kind: OverrideKind::Explicit,
            temporal_pushdown: features.has_time_filter,
        };
    }

    // Auto routing — union of matching rules with max-weight.
    let mut weights: HashMap<Retriever, f32> = HashMap::new();
    let mut max_weight = |r: Retriever, w: f32| {
        weights.entry(r).and_modify(|cur| *cur = cur.max(w)).or_insert(w);
    };

    // Rule 1: entity-anchored.
    if features.has_entity_anchor || features.contains_entity_mention_heuristic {
        max_weight(Retriever::Graph, 2.0);
        max_weight(Retriever::Semantic, 1.0);
        if features.has_text {
            max_weight(Retriever::Lexical, 0.5);
        }
    }

    // Rule 2: exact-term.
    if features.contains_exact_id || features.is_all_caps_tokens {
        max_weight(Retriever::Lexical, 2.0);
        max_weight(Retriever::Semantic, 0.5);
    }

    // Rule 5: default (only if nothing else matched and text is present).
    if weights.is_empty() && features.has_text {
        max_weight(Retriever::Semantic, 1.0);
        max_weight(Retriever::Lexical, 1.0);
    }

    // Rules 3 + 4 are "no new retriever; filter chain applies".
    let temporal_pushdown = features.has_time_filter || features.contains_temporal_expression;

    // Cap at MAX_RETRIEVERS by weight descending.
    let mut sorted: Vec<(Retriever, f32)> = weights.into_iter().collect();
    sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    sorted.truncate(MAX_RETRIEVERS);

    let retrievers = sorted.into_iter().map(|(retriever, weight)| RetrieverInvocation {
        retriever, weight,
    }).collect();

    RoutingDecision {
        features,
        retrievers,
        override_kind: OverrideKind::Auto,
        temporal_pushdown,
    }
}
```

### Regex constants

```rust
static EXACT_ID_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b[A-Z][A-Z0-9]+-\d+\b").unwrap());
static TITLE_CASE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b[A-Z][a-z]+(?:\s+[A-Z][a-z]+)*\b").unwrap());
static TEMPORAL_RE: Lazy<Regex> = Lazy::new(|| Regex::new(
    r"\b(yesterday|today|tomorrow|last\s+(week|month|year|\d+\s+days?)|next\s+(week|month|year)|\d{4}-\d{2}-\d{2})\b"
).unwrap());
const QUESTION_STARTS: &[&str] = &["what", "who", "where", "when", "why", "how", "does", "is ", "are ", "do "];
```

### `OnceCell` / `once_cell` for static regex

The substrate already pulls `once_cell` via several crates;
v1 routes use it. If not present in brain-planner, the plan
adds the workspace dep.

## 5. Trade-offs considered

| Alternative | Pros | Cons | Verdict |
|---|---|---|---|
| Simple heuristic for entity mentions (Title-Case regex) | Zero added latency; no model dep | Misses lowercase entities and false-positives on titles | ✓ for v1; full NER deferred |
| Full NER via phase-20 classifier extractor | Higher quality | Adds 5–20 ms per query (model forward pass) + complicates deps | rejected for v1 |
| Hand-coded temporal parser | Catches more cases (yesterday at 3pm, etc.) | Significant code surface; chrono dep | rejected — regex over common forms is enough; full parsing post-v1 |
| Router emits cost estimate | Single decision unit | Cost belongs to the planner (23.6); router stays pure | rejected |
| Max-weight union (this plan) | Stable; preserves the strongest signal | A weak signal can't promote a retriever above its rule's weight cap | ✓ — matches §24/00 |
| Sum-of-weights union | Stronger signals when multiple rules match | Doesn't match §24/00's "maximum weight per retriever" | rejected |

## 6. Risks / open questions

- **Risk:** Title-Case regex matches "I", "The", any sentence-initial word. **Mitigation:** require ≥ 2 chars + reject common stopword starts. v1 keeps it simple and tolerates noise — the router is a heuristic, not a classifier.
- **Risk:** Temporal regex misses many phrasings (e.g. "the day before yesterday"). **Mitigation:** documented as best-effort; clients can pass `time_filter` explicitly to force temporal push-down.
- **Risk:** Rule 1 over-fires on any Title-Case text. **Mitigation:** require BOTH text contains Title-Case AND no explicit entity_anchor → still triggers Rule 1 but downgrades Graph weight to 1.0 if anchor wasn't provided. Actually the spec says either signal triggers; v1 follows spec literally.
- **Open question:** Should Rule 5 (default) fire when text is empty but filters are present (e.g. only `kind_filter`)? **Resolution:** no — without text + no entity anchor, there's no candidate corpus. Return an empty `retrievers` list; the planner will treat that as "filter-only" and use a non-retriever path. v1 documents this case but doesn't implement a filter-only retriever.
- **Open question:** Should `fusion_config` from the request override the default `k=60`? **Resolution:** yes; the planner reads `req.fusion_config` directly (router just passes it through in the decision metadata). For 23.3 we don't pass it through the decision; 23.6 reads from the request.

## 7. Test plan

Unit tests in `crates/brain-planner/src/knowledge/router_tests.rs`:

- `rule_1_entity_anchor_selects_graph_and_semantic` — request with `entity_anchor` set, no text → Graph(2.0) + Semantic(1.0); no Lexical.
- `rule_1_with_text_adds_lexical` — entity_anchor + text → Graph + Semantic + Lexical(0.5).
- `rule_2_exact_id_promotes_lexical` — text `"ACME-1247 broke prod"` → Lexical(2.0) + Semantic(0.5); no Graph.
- `rule_2_all_caps_promotes_lexical` — text `"ACME XYZ"` (all-caps tokens) → Lexical + Semantic.
- `rule_5_default_free_text` — text `"what does Priya prefer"` with no anchor and no exact-id → Semantic(1.0) + Lexical(1.0).
- `rule_3_time_filter_sets_temporal_pushdown` — `time_filter: Some(...)` → `temporal_pushdown = true`.
- `rule_4_type_filter_no_retriever_change` — `kind_filter: [Fact]` alone → empty retriever list (no other signal); decision is filter-only.
- `union_takes_max_weight` — request matching both Rule 1 (Graph 2.0, Semantic 1.0) AND Rule 2 (Lexical 2.0, Semantic 0.5) → Semantic weight is 1.0 (max), Lexical 2.0, Graph 2.0.
- `union_caps_at_three_retrievers` — request matching enough rules to produce 3+ retrievers → exactly 3 returned, top-3 by weight.
- `explicit_override_skips_rules` — `RetrieverSelection::Explicit(vec![Semantic, Graph])` → only those two, with weight 1.0 each, regardless of features.
- `empty_query_returns_empty_decision` — no text, no anchor, no filters → empty retriever list + `OverrideKind::Auto`.
- `question_text_is_detected` — `"who is Priya?"` → `is_question = true`.
- `title_case_triggers_entity_mention_heuristic` — `"Alice met Bob in Paris"` → `contains_entity_mention_heuristic = true` → Rule 1 fires.
- `temporal_regex_catches_common_forms` — `"meeting last week"` and `"2024-03-15"` → `contains_temporal_expression = true`.

## 8. Commit shape

Single commit:

```
feat(planner): 23.3 — rule-based query router

- crates/brain-planner/src/knowledge/mod.rs (new): module
  parent for phase 23's planner pieces (router / fusion /
  filters / planner / executor).
- crates/brain-planner/src/knowledge/router.rs (new):
  QueryRequest + RoutingDecision + ClassificationFeatures +
  Retriever enum + RetrieverInvocation + 5-rule routing fn
  with max-weight union and 3-retriever cap.
- crates/brain-planner/src/knowledge/router_tests.rs (new):
  ~14 unit tests covering each rule, classification
  heuristics, union semantics, explicit override.
- crates/brain-planner/src/lib.rs: pub mod knowledge.
- crates/brain-planner/Cargo.toml: regex + once_cell direct
  deps if not transitive.
```

## 9. Confirmation

Please confirm:

1. **Title-Case regex as a v1 NER proxy** (vs. full NER via the classifier extractor).
2. **Temporal expression regex covers the common forms** listed (yesterday / today / last week / last N days / ISO dates) — anything else explicit time-filter via `time_filter` field.
3. **Empty retriever list is a valid output** when only filters are set — the planner treats it as filter-only (or returns empty results). No retriever fall-back.
4. **Max-weight union** when rules overlap (vs sum or last-write-wins).
5. **Explicit override flat-weights** all listed retrievers at 1.0 — no spec text suggests otherwise; per-query weights ride in `fusion_config`.

After approval: implement + tests + commit.
