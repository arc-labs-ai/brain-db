//! Reciprocal Rank Fusion.
//!
//! Combines multiple retrievers' ranked outputs into one ranked list
//! using score-scale-invariant rank fusion:
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

/// RRF smoothing-constant default (from Cormack et al. 2009).
pub const DEFAULT_K: u32 = 60;

/// Which fusion strategy combines the per-retriever ranked lists.
///
/// `Rrf` is pure rank-based reciprocal rank fusion: it ignores score
/// magnitude, so a near-perfect single-retriever match that is absent
/// from the other lanes loses to documents that are middling in every
/// lane (the coverage-bias pathology).
///
/// `RelativeScore` normalizes each retriever's raw scores per query
/// (min-max) and fuses as a weighted convex sum, so a strong single
/// signal carries its magnitude through and is not buried by coverage.
/// `RelativeScoreZScore` is the distribution-based variant (mean/std),
/// more robust when score scales differ widely or have outliers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FusionMethod {
    Rrf,
    #[default]
    RelativeScore,
    RelativeScoreZScore,
}

/// One fused result.
#[derive(Debug, Clone)]
pub struct FusedItem {
    pub id: RankedItemId,
    pub fused_score: f64,
    pub contributing: Vec<RetrieverContribution>,
    /// Cross-encoder relevance score, set by the rerank stage iff
    /// this item fell inside the rerank window and the encoder
    /// scored it. `None` means RRF-only — `fused_score` is the rank
    /// key. When `Some`, the list was re-sorted by this score.
    pub rerank_score: Option<f32>,
}

/// Per-retriever contribution to a fused item — surfaces in
/// EXPLAIN/TRACE so clients can see which retriever brought this item
/// into the result.
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
/// emphasise top results, larger values flatten the curve.
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
                rerank_score: None,
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

/// Dispatch to the configured fusion strategy.
#[must_use]
pub fn fuse(
    outputs: &[(Retriever, Vec<RankedItem>)],
    k: u32,
    weights: &PerRetrieverWeights,
    method: FusionMethod,
) -> Vec<FusedItem> {
    match method {
        FusionMethod::Rrf => fuse_rrf(outputs, k, weights),
        FusionMethod::RelativeScore => fuse_relative_score(outputs, weights, Normalization::MinMax),
        FusionMethod::RelativeScoreZScore => {
            fuse_relative_score(outputs, weights, Normalization::ZScore)
        }
    }
}

/// Per-query score normalization applied before a weighted sum.
#[derive(Debug, Clone, Copy)]
enum Normalization {
    /// Map each retriever's scores to `[0, 1]` by `(s - min)/(max - min)`.
    MinMax,
    /// Map to standard scores `(s - mean)/std` (distribution-based).
    ZScore,
}

/// Score-aware fusion. Each retriever's raw scores are normalized per
/// query, then `fused_score(d) = Σ_i w_i · norm_i(d)` over the
/// retrievers that returned `d`. Unlike RRF this preserves the
/// magnitude of a strong single-retriever signal, so a rank-1 cosine
/// match is not buried just because it is missing from another lane.
#[must_use]
fn fuse_relative_score(
    outputs: &[(Retriever, Vec<RankedItem>)],
    weights: &PerRetrieverWeights,
    norm: Normalization,
) -> Vec<FusedItem> {
    let mut accum: HashMap<RankedItemId, FusedItem> = HashMap::new();

    for (retriever, items) in outputs {
        if items.is_empty() {
            continue;
        }
        let w = f64::from(weight_for(*retriever, weights));
        let normalized = normalize_scores(items, norm);
        for (item, norm_score) in items.iter().zip(normalized) {
            let entry = accum.entry(item.id).or_insert_with(|| FusedItem {
                id: item.id,
                fused_score: 0.0,
                contributing: Vec::new(),
                rerank_score: None,
            });
            entry.fused_score += w * norm_score;
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

/// Normalize one retriever's raw scores into comparable per-query units.
fn normalize_scores(items: &[RankedItem], norm: Normalization) -> Vec<f64> {
    match norm {
        Normalization::MinMax => {
            let (mut min, mut max) = (f64::INFINITY, f64::NEG_INFINITY);
            for it in items {
                let s = f64::from(it.score);
                min = min.min(s);
                max = max.max(s);
            }
            let span = max - min;
            if span <= f64::EPSILON {
                // Single item or all-equal scores: uniform full signal.
                return vec![1.0; items.len()];
            }
            items
                .iter()
                .map(|it| (f64::from(it.score) - min) / span)
                .collect()
        }
        Normalization::ZScore => {
            let n = items.len() as f64;
            let mean = items.iter().map(|it| f64::from(it.score)).sum::<f64>() / n;
            let var = items
                .iter()
                .map(|it| {
                    let d = f64::from(it.score) - mean;
                    d * d
                })
                .sum::<f64>()
                / n;
            let std = var.sqrt();
            if std <= f64::EPSILON {
                return vec![0.0; items.len()];
            }
            items
                .iter()
                .map(|it| (f64::from(it.score) - mean) / std)
                .collect()
        }
    }
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
/// `BrainGraphRetriever`'s tie-break convention.
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
