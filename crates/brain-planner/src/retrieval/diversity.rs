//! Maximal Marginal Relevance (MMR) merge / diversity stage.
//!
//! The coverage half of list handling widens the candidate pool so a
//! set question's members all reach fusion; this stage then *spreads*
//! them — when the caller asks "what are X's hobbies" the top results
//! should be distinct hobbies, not five paraphrases of the most salient
//! one. Entirely server-internal: the router decides when it runs (on
//! detected list intent), the client never asks for it.
//!
//! **Why it can't regress a single-answer query.** MMR is greedy and
//! its first pick is always the highest-relevance item — a lone strong
//! answer stays at rank 1 no matter what. Only positions 2..N are
//! re-ordered to reduce redundancy, and the stage only runs when the
//! router detected list intent. A factoid that slips the detector still
//! keeps its top hit.
//!
//! Redundancy is **text-Jaccard** over lowercased token sets, not vector
//! cosine: the candidate vectors aren't reachable post-fusion without a
//! retriever round-trip, and token overlap is a cheap, good-enough
//! near-duplicate signal for the merge decision.

use std::collections::HashSet;

use crate::retrieval::fusion::FusedItem;

/// MMR trade-off for list intent: 0.5 balances relevance against
/// novelty. Higher → more relevance-faithful (less spread); lower →
/// more diverse. List/aggregation wants a real spread, so 0.5.
pub const MMR_LAMBDA_LIST: f64 = 0.5;

/// Cap on how many top candidates the MMR pass reorders. MMR is
/// O(window² · tokens); the window only needs to cover what the caller
/// could plausibly receive plus headroom. Items past the window keep
/// their incoming order and sit after the reordered head.
pub const MMR_WINDOW: usize = 50;

/// Tokenize for the Jaccard redundancy term: lowercase, split on
/// non-alphanumerics, drop very short tokens (articles/punctuation
/// noise). Returns the distinct token set.
#[must_use]
pub fn tokenize(text: &str) -> HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 2)
        .map(str::to_lowercase)
        .collect()
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count();
    let union = a.len() + b.len() - inter;
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

/// Greedily reorder the head of `items` (up to [`MMR_WINDOW`]) by MMR.
///
/// `token_sets[i]` is the token set for `items[i]` (empty = no text /
/// treated as maximally novel — non-memory hits never look redundant).
/// Relevance is each item's effective score (rerank score when present,
/// else fused score), min-max normalized across the window so it shares
/// the `[0, 1]` scale of the Jaccard redundancy term. Items outside the
/// window are left untouched after the reordered head.
///
/// A non-positive `lambda` or a window of ≤ 2 is a no-op (nothing to
/// diversify).
pub fn mmr_reorder(items: &mut Vec<FusedItem>, token_sets: &[HashSet<String>], lambda: f64) {
    let window = items.len().min(MMR_WINDOW);
    if window <= 2 || lambda <= 0.0 {
        return;
    }

    // Effective relevance per windowed item, min-max normalized.
    let rel_raw: Vec<f64> = items
        .iter()
        .take(window)
        .map(|it| it.rerank_score.map_or(it.fused_score, f64::from))
        .collect();
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for &r in &rel_raw {
        lo = lo.min(r);
        hi = hi.max(r);
    }
    let span = hi - lo;
    let rel: Vec<f64> = rel_raw
        .iter()
        .map(|&r| {
            if span > f64::EPSILON {
                (r - lo) / span
            } else {
                1.0
            }
        })
        .collect();

    let empty = HashSet::new();
    let tokens = |i: usize| token_sets.get(i).unwrap_or(&empty);

    let mut selected: Vec<usize> = Vec::with_capacity(window);
    let mut remaining: Vec<usize> = (0..window).collect();

    // First pick = argmax relevance — a lone strong answer is never
    // displaced by diversity.
    let first_pos = remaining
        .iter()
        .copied()
        .enumerate()
        .max_by(|(_, a), (_, b)| {
            rel[*a]
                .partial_cmp(&rel[*b])
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map_or(0, |(pos, _)| pos);
    selected.push(remaining.remove(first_pos));

    while !remaining.is_empty() {
        let mut best_pos = 0;
        let mut best_score = f64::NEG_INFINITY;
        for (pos, &cand) in remaining.iter().enumerate() {
            let max_sim = selected
                .iter()
                .map(|&s| jaccard(tokens(cand), tokens(s)))
                .fold(0.0_f64, f64::max);
            let mmr = lambda * rel[cand] - (1.0 - lambda) * max_sim;
            if mmr > best_score {
                best_score = mmr;
                best_pos = pos;
            }
        }
        selected.push(remaining.remove(best_pos));
    }

    // Apply the new order to the windowed head, leaving the tail (past
    // the window) in its incoming order after it. `selected` is a
    // permutation of `0..window`; move each item out exactly once via a
    // take-out buffer so no `FusedItem` is cloned.
    let tail = items.split_off(window);
    let mut head: Vec<Option<FusedItem>> = items.drain(..).map(Some).collect();
    let mut reordered: Vec<FusedItem> = selected
        .into_iter()
        .map(|i| {
            head[i]
                .take()
                .expect("invariant: permutation index used once")
        })
        .collect();
    reordered.extend(tail);
    *items = reordered;
    // The returned slice is intentionally no longer globally
    // fused-score sorted — MMR order wins for the head.
}

#[cfg(test)]
#[path = "diversity_tests.rs"]
mod tests;
