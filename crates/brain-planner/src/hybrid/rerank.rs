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

/// Top-N cap for the rerank window. RRF feeds at most this many
/// candidates into the cross-encoder; the model's per-pair cost
/// dominates wall-time, so the window is kept narrow. Set to 10: the
/// cross-encoder forward is the recall hot-path cost (especially in
/// debug builds and on CPU), and reranking the top-10 RRF candidates
/// captures nearly all the ordering benefit at a fraction of the
/// per-query cost. A candidate ranked outside the top 10 by RRF keeps
/// its RRF position.
pub const RERANK_TOP_N: usize = 10;

/// One candidate to be scored. The executor pre-resolves text via
/// the `texts` table; entries with no text (tombstoned mid-query,
/// non-memory variant) are skipped — their original fused rank is
/// retained.
#[derive(Debug, Clone)]
pub struct RerankCandidate {
    pub id: RankedItemId,
    pub text: String,
}

/// Re-sort the head of `fused` by pre-computed cross-encoder scores.
///
/// Scoring itself happens off the shard core (see
/// `executor::rerank_stage`, which calls the off-core
/// `RerankService`); this function is the pure, synchronous re-sort
/// that consumes the resulting logits.
///
/// - `scores[i]` is the cross-encoder logit for `candidates[i]`;
///   the two slices must be the same length and order.
/// - Reranked candidates lead, in descending score order;
///   un-reranked candidates (text not available, scoring window
///   exceeded) keep their RRF order behind them.
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

    // Partition fused into (in-window, out-of-window). In-window
    // gets sorted by rerank score descending; out-of-window keeps
    // RRF order. NaN scores fall to the bottom of the in-window
    // group (treated as -inf).
    let (mut in_window, out_window): (Vec<_>, Vec<_>) = fused
        .into_iter()
        .partition(|item| scored.contains_key(&item.id));
    // Stamp each scored item with its cross-encoder logit so the
    // result projection can surface it (`rr=` in the recall card)
    // and callers can tell reranked rows from RRF-only ones.
    for item in &mut in_window {
        item.rerank_score = scored.get(&item.id).copied();
    }
    in_window.sort_by(|a, b| {
        let sa = scored.get(&a.id).copied().unwrap_or(f32::NEG_INFINITY);
        let sb = scored.get(&b.id).copied().unwrap_or(f32::NEG_INFINITY);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut out = in_window;
    out.extend(out_window);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hybrid::fusion::RetrieverContribution;
    use crate::hybrid::router::Retriever;
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
    fn reranks_in_window_by_score_keeps_out_window_order() {
        // RRF order: slots 1,2,3,4. The rerank window covers slots 1
        // and 3; the cross-encoder ranks slot 3 above slot 1. Slots 2
        // and 4 are out-of-window and must keep their RRF order behind
        // the reranked pair.
        let fused = vec![
            fused_item(1, 0.9),
            fused_item(2, 0.8),
            fused_item(3, 0.7),
            fused_item(4, 0.6),
        ];
        let candidates = [candidate(1), candidate(3)];
        let scores = [0.1_f32, 0.9_f32];

        let out = rerank_top_n(&scores, fused, &candidates);

        // In-window reordered by score desc (slot 3 then slot 1),
        // out-window preserved (slot 2 then slot 4).
        assert_eq!(out[0].id, RankedItemId::Memory(MemoryId::pack(0, 3, 0)));
        assert_eq!(out[1].id, RankedItemId::Memory(MemoryId::pack(0, 1, 0)));
        assert_eq!(out[2].id, RankedItemId::Memory(MemoryId::pack(0, 2, 0)));
        assert_eq!(out[3].id, RankedItemId::Memory(MemoryId::pack(0, 4, 0)));

        // Reranked rows carry their logit; out-of-window rows don't.
        assert_eq!(out[0].rerank_score, Some(0.9));
        assert_eq!(out[1].rerank_score, Some(0.1));
        assert_eq!(out[2].rerank_score, None);
        assert_eq!(out[3].rerank_score, None);
    }
}
