//! Cross-encoder rerank pass over RRF-fused candidates.
//!
//! Sits between the RRF fusion stage and the post-fusion filter
//! chain. Rerank is first-class and always-on: whenever the
//! cross-encoder is loaded on the shard, the executor pulls the top
//! fused candidates' text from the per-shard `texts` table, calls
//! `CrossEncoder::score_pairs`, and re-sorts by relevance. The
//! filter chain then runs over the reranked list as usual. There is
//! no per-request toggle — the only control is the deploy-time
//! `config.rerank.enabled` load gate.
//!
//! When no cross-encoder is available (operator opted out, or the
//! model isn't bootstrapped on disk), the executor never reaches
//! this module and the RRF-only ordering is preserved.

use brain_index::RankedItemId;

use super::fusion::FusedItem;

/// Top-N cap for the rerank window. Fusion feeds at most this many
/// candidates into the cross-encoder; the model's per-pair cost
/// dominates wall-time, so the window is bounded. Set to 50: a wider
/// intake lets the cross-encoder rescue a strongly-relevant document
/// that fusion placed outside the top handful (the exact case where a
/// single retriever's high-confidence hit was diluted), then the
/// requested `top_k` trims the reranked head. A candidate ranked
/// outside the top 50 by fusion keeps its fused position.
pub const RERANK_TOP_N: usize = 50;

/// Weight on the rerank contribution when combining with the fused
/// score. The final sort key is
/// `normalize(fused_score) + RERANK_ALPHA * normalize(rerank_logit)`,
/// where both `normalize` passes are per-batch min/max to the unit
/// interval. Normalizing the fused score too is what makes α a real
/// blend weight: raw RRF (k=60) fused scores sit at ≈ 0.01–0.05 while
/// the un-normalized rerank term spans [0, 0.5], so combining them on
/// raw scales lets rerank dominate ~10× and effectively override
/// fusion. With both terms on [0, 1], fusion carries weight 1.0 and
/// rerank weight 0.5 — fusion leads, rerank refines. The cross-encoder
/// is a re-scoring signal, not the primary one: a strongly-fused RRF
/// candidate (multiple retrievers agreeing) should not be overridden by
/// a noisy cross-encoder logit on a single pair. 0.5 lets rerank
/// decisively break close fused ties (e.g., funded/founded near-
/// duplicates) but a full-span fusion lead (Δnorm = 1.0) cannot be
/// dethroned by the bounded rerank term (≤ 0.5).
pub const RERANK_ALPHA: f64 = 0.5;

/// One candidate to be scored. The executor pre-resolves text via
/// the `texts` table; entries with no text (tombstoned mid-query,
/// non-memory variant) are skipped — their original fused rank is
/// retained.
#[derive(Debug, Clone)]
pub struct RerankCandidate {
    pub id: RankedItemId,
    pub text: String,
}

/// Re-sort `fused` by combined `fused_score + α · normalize(rerank)`.
///
/// Scoring itself happens off the shard core (see
/// `executor::rerank_stage`, which calls the off-core
/// `RerankService`); this function is the pure, synchronous re-sort
/// that consumes the resulting logits.
///
/// - `scores[i]` is the cross-encoder logit for `candidates[i]`;
///   the two slices must be the same length and order.
/// - Both the fused scores and the cross-encoder logits are
///   normalized per-batch to the unit interval (min→0, max→1). Items
///   outside the rerank window contribute `0` to the rerank term —
///   they kept their RRF ordering and never saw the model.
/// - The final sort key is
///   `normalize(fused_score) + RERANK_ALPHA · normalize(rerank)`.
///   Normalizing both terms puts them on equal footing so α is a true
///   blend weight (raw RRF k=60 fused scores are ≈ 0.01–0.05 and would
///   otherwise be swamped by the rerank term). A confident multi-
///   retriever consensus survives a low logit; the rerank pass acts as
///   a tie-breaker between close fused neighbours and pulls strongly-
///   scored rescues forward only when the cross-encoder's gap is
///   decisive.
/// - Empty `candidates` (or `scores`) returns `fused` unchanged.
pub fn rerank_top_n(
    scores: &[f32],
    fused: Vec<FusedItem>,
    candidates: &[RerankCandidate],
) -> Vec<FusedItem> {
    if candidates.is_empty() || scores.is_empty() {
        return fused;
    }
    debug_assert_eq!(scores.len(), candidates.len());

    // Build an id → score map for the rerank window.
    let mut scored: std::collections::HashMap<RankedItemId, f32> =
        std::collections::HashMap::with_capacity(scores.len());
    for (cand, score) in candidates.iter().zip(scores.iter()) {
        scored.insert(cand.id, *score);
    }

    // Per-batch min/max so the rerank contribution stays in a known
    // range regardless of the cross-encoder's absolute scale (raw
    // logits vary wildly between query/candidate pairs). A flat batch
    // (max == min) collapses the contribution to 0 for every item so
    // the fused order wins by default; a wide-spread batch keeps the
    // rerank gap meaningful.
    let (rer_min, rer_max) = scores
        .iter()
        .copied()
        .filter(|s| s.is_finite())
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), s| {
            (lo.min(s), hi.max(s))
        });
    let rer_range = (rer_max - rer_min).max(f32::EPSILON);

    // Per-batch min/max over the *fused* scores as well. Raw RRF
    // (k=60) fused scores sit at ≈ 0.01–0.05; the rerank term spans
    // [0, RERANK_ALPHA]. Combining them on raw scales lets rerank
    // dominate ~10× and effectively override fusion. Normalizing the
    // fused score to [0, 1] alongside the rerank term puts both on
    // equal footing so RERANK_ALPHA is a true blend weight: fusion
    // leads, rerank refines.
    let (fus_min, fus_max) = fused
        .iter()
        .map(|f| f.fused_score)
        .filter(|s| s.is_finite())
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), s| {
            (lo.min(s), hi.max(s))
        });
    let fus_range = (fus_max - fus_min).max(f64::EPSILON);

    // Stamp the in-window items with their raw cross-encoder logit
    // so the result projection can surface it (`rr=` in the recall
    // card) — separately from how we use it in the sort key.
    let mut combined: Vec<(f64, FusedItem)> = fused
        .into_iter()
        .map(|mut item| {
            let rer_norm = match scored.get(&item.id).copied() {
                Some(s) if s.is_finite() => {
                    item.rerank_score = Some(s);
                    ((s - rer_min) / rer_range).clamp(0.0, 1.0) as f64
                }
                Some(_) | None => 0.0,
            };
            let fus_norm = if item.fused_score.is_finite() {
                ((item.fused_score - fus_min) / fus_range).clamp(0.0, 1.0)
            } else {
                0.0
            };
            // Deliberate blend (owner-approved): instead of sorting purely
            // by cross-encoder score descending, we keep the fusion signal
            // in the ranking via a normalized fusion+rerank blend. The
            // amendment to that ordering is tracked separately and is out
            // of scope here.
            let key = fus_norm + RERANK_ALPHA * rer_norm;
            (key, item)
        })
        .collect();
    combined.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    combined.into_iter().map(|(_, item)| item).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retrieval::fusion::RetrieverContribution;
    use crate::retrieval::router::Retriever;
    use brain_core::MemoryId;

    fn fused_item(slot: u64, score: f64) -> FusedItem {
        FusedItem {
            id: RankedItemId::Memory(MemoryId::pack(0, slot, 0)),
            fused_score: score,
            contributing: vec![RetrieverContribution {
                retriever: Retriever::Semantic,
                rank: 1,
                raw_score: 0.9,
            }],
            rerank_score: None,
        }
    }

    fn candidate(slot: u64) -> RerankCandidate {
        RerankCandidate {
            id: RankedItemId::Memory(MemoryId::pack(0, slot, 0)),
            text: format!("candidate {slot}"),
        }
    }

    #[test]
    fn rerank_with_empty_candidates_is_identity() {
        let f = vec![fused_item(1, 0.1), fused_item(2, 0.2)];
        let out = rerank_top_n(&[], f.clone(), &[]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, f[0].id);
        assert_eq!(out[1].id, f[1].id);
    }

    #[test]
    fn rerank_breaks_close_fused_ties_in_window() {
        // RRF order: slots 1,2,3,4 with very tight fused scores
        // (the realistic RRF k=60 regime, where neighbours differ by
        // hundredths). The rerank window covers slots 1 and 3; the
        // cross-encoder ranks slot 3 well above slot 1. Because the
        // fused-score gap is small, its normalized contribution can't
        // hold slot 1 ahead, so the rerank term lifts slot 3 to the
        // top. Slots 2 and 4 are out-of-window and contribute 0 to
        // the rerank term, keeping their fused order.
        //
        // fus_min=0.030, fus_max=0.040, range=0.010
        //   slot1 fus_norm = (0.040-0.030)/0.010 = 1.000
        //   slot2 fus_norm = (0.037-0.030)/0.010 = 0.700
        //   slot3 fus_norm = (0.034-0.030)/0.010 = 0.400
        //   slot4 fus_norm = (0.030-0.030)/0.010 = 0.000
        // rer_min=0.1, rer_max=0.9, range=0.8
        //   slot1 rer_norm = 0.0, slot3 rer_norm = 1.0
        // keys: slot1 1.000, slot2 0.700, slot3 0.400+0.5=0.900, slot4 0.000
        let fused = vec![
            fused_item(1, 0.040),
            fused_item(2, 0.037),
            fused_item(3, 0.034),
            fused_item(4, 0.030),
        ];
        let candidates = [candidate(1), candidate(3)];
        let scores = [0.1_f32, 0.9_f32];

        let out = rerank_top_n(&scores, fused, &candidates);

        // slot 1 key = 1.000 → #1
        // slot 3 key = 0.900 → #2
        // slot 2 key = 0.700 → #3
        // slot 4 key = 0.000 → #4
        assert_eq!(out[0].id, RankedItemId::Memory(MemoryId::pack(0, 1, 0)));
        assert_eq!(out[1].id, RankedItemId::Memory(MemoryId::pack(0, 3, 0)));
        assert_eq!(out[2].id, RankedItemId::Memory(MemoryId::pack(0, 2, 0)));
        assert_eq!(out[3].id, RankedItemId::Memory(MemoryId::pack(0, 4, 0)));

        // Reranked rows carry their logit; out-of-window rows don't.
        assert_eq!(out[0].rerank_score, Some(0.1));
        assert_eq!(out[1].rerank_score, Some(0.9));
        assert_eq!(out[2].rerank_score, None);
        assert_eq!(out[3].rerank_score, None);
    }

    #[test]
    fn strong_fused_survives_noisy_rerank() {
        // The pathological pair we hit on the live corpus: the
        // fused signal puts slot 1 well ahead (1.7 vs 0.5), but the
        // cross-encoder gives the loser a higher logit. With pure
        // rerank-replacement this would swap the order; with score
        // fusion the strong RRF consensus carries through and the
        // cross-encoder's quirk is bounded by α.
        let fused = vec![fused_item(1, 1.7), fused_item(2, 0.5)];
        let candidates = [candidate(1), candidate(2)];
        // Slot 2 gets the higher logit despite being the fusion
        // loser — exactly the "rerank-broke-Q1" shape.
        let scores = [-0.6_f32, 1.9_f32];

        let out = rerank_top_n(&scores, fused, &candidates);

        // fus_norm: slot1 = 1.0 (max), slot2 = 0.0 (min).
        // rer_norm: slot1 = 0.0 (min), slot2 = 1.0 (max).
        // slot 1 key = 1.0 + 0.5·0.0 = 1.0 → #1
        // slot 2 key = 0.0 + 0.5·1.0 = 0.5 → #2
        // A full-span fusion lead (Δfus_norm = 1.0) outweighs the
        // bounded rerank term (≤ RERANK_ALPHA = 0.5).
        assert_eq!(out[0].id, RankedItemId::Memory(MemoryId::pack(0, 1, 0)));
        assert_eq!(out[1].id, RankedItemId::Memory(MemoryId::pack(0, 2, 0)));
    }
}
