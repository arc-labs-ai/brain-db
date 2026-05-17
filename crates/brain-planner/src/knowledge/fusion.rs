//! Reciprocal Rank Fusion (phase 23.4).
//!
//! Implements §23/01 — combines multiple retrievers' ranked
//! outputs into one ranked list using score-scale-invariant
//! rank fusion:
//!
//! ```text
//! RRF_score(d) = Σ_i  w_i / (k + rank_i(d))
//! ```
//!
//! Where `i` iterates the retrievers that returned `d`, `w_i`
//! is the per-retriever weight (default 1.0), and `k` is the
//! smoothing constant (default 60).

use std::collections::HashMap;

use brain_index::{RankedItem, RankedItemId};

use super::router::{PerRetrieverWeights, Retriever};

/// RRF smoothing-constant default (§23/01 §"Choice of k" —
/// from Cormack et al. 2009).
pub const DEFAULT_K: u32 = 60;

/// One fused result. Shape mirrors §24/00 §"Result shape".
#[derive(Debug, Clone)]
pub struct FusedItem {
    pub id: RankedItemId,
    pub fused_score: f64,
    pub contributing: Vec<RetrieverContribution>,
}

/// Per-retriever contribution to a fused item — surfaces in
/// EXPLAIN/TRACE (23.8) so clients can see which retriever
/// brought this item into the result.
#[derive(Debug, Clone, Copy)]
pub struct RetrieverContribution {
    pub retriever: Retriever,
    pub rank: u32,
    pub raw_score: f32,
}

/// Fuse multiple ranked lists into a single ranked list.
///
/// `outputs`: one `(Retriever, ranked_items)` pair per
/// retriever. The same retriever should not appear twice —
/// duplicates silently sum their contributions (caller bug;
/// the planner won't construct such inputs).
///
/// `k`: smoothing constant. Use [`DEFAULT_K`] (60) for the
/// canonical Cormack et al. default; smaller values
/// emphasise top results, larger values flatten the curve
/// (§23/01 §"Choice of k").
///
/// `weights`: per-retriever weights. Defaults are 1.0; the
/// router or `FusionConfig` override.
///
/// Returns a `Vec<FusedItem>` sorted by `fused_score`
/// descending; ties broken by `(id-discriminant, id-bytes)`
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
        let w = f64::from(weight_for(*retriever, weights));
        for item in items {
            let rank = f64::from(item.rank);
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

/// Deterministic 17-byte sort key for `RankedItemId`. Tag
/// byte distinguishes variants; the trailing 16 bytes are the
/// inner id's big-endian representation. Matches the
/// `BrainGraphRetriever`'s tie-break convention (§23.2).
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

#[cfg(test)]
mod tests;
