# Plan: Phase 23 — Task 04, RRF fusion

**Status:** awaiting-confirmation
**Date:** 2026-05-17
**Author:** Claude (autonomous)
**Estimated commits:** 1

---

## 1. Scope

Implement Reciprocal Rank Fusion per §23/01. Given the ranked
output of N retrievers + a per-retriever weight map + a smoothing
constant `k`, compute a fused ranked list whose order is determined
by RRF score. The 23.6 planner / 23.7 executor consumes this.

Concrete deliverables:

1. New module `crates/brain-planner/src/knowledge/fusion.rs`:
   - `FusedItem { id, fused_score, contributing }`.
   - `RetrieverContribution { retriever, rank, raw_score }`.
   - `fuse_rrf(outputs: &[(Retriever, Vec<RankedItem>)],
              k: u32, weights: &PerRetrieverWeights) -> Vec<FusedItem>`.
2. Formula (§23/01):
   ```
   RRF_score(d) = Σ_i  w_i / (k + rank_i(d))
   ```
   - `d` is the document.
   - `i` iterates the retrievers that returned `d`.
   - `rank_i(d)` is 1-based.
   - `w_i` is the per-retriever weight (default 1.0).
   - `k` defaults to 60.
3. Stable ordering at score ties — break by `RankedItemId`
   discriminant + bytes ascending (matches 23.2's graph
   retriever tie-break).
4. Unit tests covering the formula, weight handling, k
   sensitivity, score-scale invariance (large vs small raw
   scores fuse identically), ties, single-retriever passthrough,
   empty inputs.

NOT in scope:
- `k` per-query override from `FusionConfig.k` — the planner
  (23.6) reads `req.fusion_config` and passes `k` into
  `fuse_rrf`; this sub-task just exposes the parameter.
- Streaming fusion (incremental merge) — v1 fuses on complete
  per-retriever lists; streaming is post-v1 (§24/00 §"Streaming
  results").
- Cost estimation — planner's job (23.6).

## 2. Spec references

- `spec/23_retrievers/01_rrf_fusion.md` — full formula + `k=60`
  default + score-scale invariance + per-retriever weights +
  per-query weights via the router.
- `spec/24_hybrid_query/00_purpose.md` §"Result shape" — defines
  `ResultItem` / `RetrieverContribution` shape that `FusedItem`
  mirrors.

## 3. External validation

| Item | Source | Confirmed |
|---|---|---|
| `RankedItem` shape (id + rank + score) | `brain-index::RankedItem` (22.5) | Yes — `id: RankedItemId`, `rank: u32`, `score: f32`, `snippet: Option<String>`. |
| `RankedItemId` is `Eq + Hash` | `brain-index::tantivy_shard::retriever` | Yes (`#[derive(PartialEq, Eq, Hash)]` via the existing variants; Memory + Statement + Entity + Relation all derive Hash via brain-core ids). Verify at code time. |
| `PerRetrieverWeights` shape | `brain-planner::knowledge::router::PerRetrieverWeights` (23.3) | Defines `semantic`, `lexical`, `graph` f32 fields with `Default = 1.0`. |
| `Retriever` enum | `brain-planner::knowledge::router::Retriever` (23.3) | Yes — `Semantic`, `Lexical`, `Graph`. |

The `RankedItemId` Hash bound is the one open question — if
any variant doesn't derive Hash, we either add the derive
(brain-core may need a one-line patch) or key the fusion map
on a serialised byte form. I'll verify in step 4 below.

## 4. Architecture sketch

```rust
// crates/brain-planner/src/knowledge/fusion.rs

use std::collections::HashMap;

use brain_index::{RankedItem, RankedItemId};

use super::router::{PerRetrieverWeights, Retriever};

/// RRF smoothing constant default (§23/01 §"Choice of k").
pub const DEFAULT_K: u32 = 60;

/// One fused result.
#[derive(Debug, Clone)]
pub struct FusedItem {
    pub id: RankedItemId,
    pub fused_score: f64,
    pub contributing: Vec<RetrieverContribution>,
}

/// Per-retriever contribution to a fused item — surfaces in
/// EXPLAIN/TRACE (§24/00 §"Result shape").
#[derive(Debug, Clone, Copy)]
pub struct RetrieverContribution {
    pub retriever: Retriever,
    pub rank: u32,
    pub raw_score: f32,
}

/// Fuse multiple ranked lists into a single ranked list.
///
/// `outputs` is a slice of `(Retriever, ranked_items)` pairs.
/// One pair per retriever; the same retriever should not
/// appear twice. `k` is the smoothing constant (default
/// `DEFAULT_K` = 60). `weights` is the per-retriever weight
/// map; missing retrievers default to weight 1.0.
///
/// Returns a `Vec<FusedItem>` sorted by `fused_score`
/// descending. Ties broken by `(discriminant, id-bytes)`
/// ascending for deterministic output.
#[must_use]
pub fn fuse_rrf(
    outputs: &[(Retriever, Vec<RankedItem>)],
    k: u32,
    weights: &PerRetrieverWeights,
) -> Vec<FusedItem> {
    let k_f = f64::from(k);
    let mut accum: HashMap<RankedItemId, FusedItem> = HashMap::new();

    for (retriever, items) in outputs {
        let w = weight_for(*retriever, weights) as f64;
        for item in items {
            let rank = item.rank as f64;
            let contribution = w / (k_f + rank);
            let entry = accum.entry(item.id).or_insert_with(|| FusedItem {
                id: item.id,
                fused_score: 0.0,
                contributing: Vec::new(),
            });
            entry.fused_score += contribution;
            entry.contributing.push(RetrieverContribution {
                retriever: *retriever,
                rank: item.rank,
                raw_score: item.score,
            });
        }
    }

    let mut out: Vec<FusedItem> = accum.into_values().collect();
    out.sort_by(|a, b| {
        b.fused_score
            .partial_cmp(&a.fused_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| id_sort_key(&a.id).cmp(&id_sort_key(&b.id)))
    });
    out
}

fn weight_for(r: Retriever, w: &PerRetrieverWeights) -> f32 {
    match r {
        Retriever::Semantic => w.semantic,
        Retriever::Lexical => w.lexical,
        Retriever::Graph => w.graph,
    }
}

fn id_sort_key(id: &RankedItemId) -> [u8; 17] {
    let mut key = [0u8; 17];
    match id {
        RankedItemId::Memory(m) => {
            key[0] = 0;
            key[1..].copy_from_slice(&m.raw().to_be_bytes());
        }
        RankedItemId::Statement(s) => {
            key[0] = 1;
            key[1..].copy_from_slice(&s.to_bytes());
        }
        RankedItemId::Entity(e) => {
            key[0] = 2;
            key[1..].copy_from_slice(&e.to_bytes());
        }
        RankedItemId::Relation(r) => {
            key[0] = 3;
            key[1..].copy_from_slice(&r.to_bytes());
        }
    }
    key
}
```

Sort secondary key (`id_sort_key`) duplicates the function in
the graph retriever (23.2). v1 keeps them separate; if a
third caller appears, promote to a shared helper in
`brain-index` (§17 secondary index conventions).

## 5. Trade-offs considered

| Alternative | Pros | Cons | Verdict |
|---|---|---|---|
| HashMap<RankedItemId> for accumulation (this plan) | O(N) per retriever; clean | Needs `Hash` on `RankedItemId` (and all four variant types) | ✓ — verified in §3 |
| BTreeMap with same key | Deterministic without explicit tie-break | Adds log factor; tie-break can still be derived from `RankedItemId` bytes | rejected — HashMap + explicit sort is faster |
| Sort each list first, do merge-sort fusion | Streaming-friendly | More complex; v1 doesn't need streaming | rejected — defer to streaming post-v1 |
| `f64` for fused_score (this plan) | Cumulative additions don't lose precision | Slightly more memory than f32 | ✓ — sums of `1/(60+r)` over multiple retrievers benefit from higher precision |
| `Vec<RankedItem>` consumed by value | One less clone | Caller may want to retain originals (EXPLAIN/TRACE) | rejected — pass by slice |

## 6. Risks / open questions

- **Risk:** `RankedItemId` variants need `Hash`. Per `brain-index::tantivy_shard::retriever`, the enum already derives `Clone + Copy + PartialEq + Eq`. Confirm `Hash` exists at code time; if missing, add the derive (one-line patch). EntityId / RelationId / MemoryId / StatementId all derive Hash in brain-core.
- **Risk:** Duplicate retrievers in `outputs` slice silently sum contributions. **Mitigation:** documented in the doc-comment ("the same retriever should not appear twice"); the planner (23.6) constructs the slice and won't duplicate.
- **Risk:** Empty `outputs` — returns empty Vec; consistent with §23/01 "documents not present in retriever i's output contribute 0".
- **Open question:** Should EXPLAIN/TRACE surface the secondary tie-break? **Resolution:** out of scope for 23.4; the `contributing` field already carries per-retriever ranks, and EXPLAIN/TRACE (23.8) renders them.

## 7. Test plan

Unit tests in `crates/brain-planner/src/knowledge/fusion/tests.rs`:

- `single_retriever_passthrough` — one retriever with 3 hits, equal weights, `k=60` → ranks preserved; `fused_score = 1/61, 1/62, 1/63`.
- `formula_matches_spec_example` — §23/01's worked example (k=60, rank 1 = `1/61 ≈ 0.0164`, rank 10 = `1/70 ≈ 0.0143`).
- `weight_doubles_contribution` — single retriever, weight 2.0 → fused_score exactly 2× the equal-weight result.
- `union_two_retrievers_same_doc` — doc D ranks 1 in retriever A and rank 2 in retriever B; weights equal; fused_score = `1/61 + 1/62`.
- `score_scale_invariance` — same ranks, wildly different raw_score values → identical fused outputs. Demonstrates the §23/01 property.
- `k_lower_promotes_top_results` — k=30 vs k=120 → at k=30 the rank-1 contribution is ~5× rank-10; at k=120 the ratio is much flatter.
- `ties_break_deterministically` — two docs with identical fused scores → stable ordering by id_sort_key.
- `empty_outputs_returns_empty` — `fuse_rrf(&[], 60, &weights) == []`.
- `documents_not_in_a_retriever_contribute_zero` — doc D appears only in retriever A → fused score equals only A's contribution.
- `mixed_id_variants_fuse_together` — outputs include `Memory(_)`, `Statement(_)`, `Entity(_)`; each appears in its own retriever's output; fusion produces 3 items with isolated scores.
- `graph_retriever_with_no_results` — graph retriever returns empty list; other retrievers' contributions stand alone.

## 8. Commit shape

Single commit:

```
feat(planner): 23.4 — Reciprocal Rank Fusion (RRF)

- crates/brain-planner/src/knowledge/fusion.rs (new):
  fuse_rrf + FusedItem + RetrieverContribution +
  DEFAULT_K = 60. Score formula matches §23/01 verbatim.
  Stable tie-break by (discriminant, id bytes).
- crates/brain-planner/src/knowledge/mod.rs: pub mod fusion.
- crates/brain-planner/src/knowledge/fusion/tests.rs (new):
  11 unit tests covering the formula, score-scale invariance,
  k sensitivity, ties, empty inputs, and multi-variant ids.
```

If `RankedItemId` is missing `Hash`:
- One-line patch in `brain-index/src/tantivy_shard/retriever.rs`
  to add `Hash` to the derive list.
- Mention in the commit message.

## 9. Confirmation

Please confirm:

1. **Module lives in `brain-planner/src/knowledge/fusion.rs`** alongside the router. No new crate deps.
2. **`f64` for fused_score** (vs f32 — better precision under summation).
3. **Stable tie-break by `(discriminant, id-bytes)`** matching the graph retriever's convention (23.2).
4. **Default `k = 60`** exposed as `DEFAULT_K`; per-query `k` flows through the `fuse_rrf` parameter (planner 23.6 reads `req.fusion_config.k`).
5. **Duplicate retrievers in the input slice** is documented as caller bug — fusion sums their contributions; the planner won't construct such slices.

After approval: implement + tests + commit.
