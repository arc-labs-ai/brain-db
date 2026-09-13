//! Re-rank pass: turn PQ-ADC-approximate candidates into exact-ranked
//! results by scoring each against the full-precision arena vector.
//!
//! The HNSW search returns
//! candidates ordered by ADC distance — a lossy proxy for true cosine
//! similarity. Re-ranking reads each candidate's full-precision
//! vector from the caller-owned arena, computes exact cosine
//! similarity against the query, and re-sorts. The truncated top-K
//! is then the same shape the pure-HNSW search returns: pairs of
//! `(MemoryId, similarity)` sorted descending.
//!
//! Tombstoned candidates whose arena slot has been reclaimed between
//! the HNSW traversal and the re-rank read are silently dropped; the
//! returned list may be shorter than `k` in that case.

use brain_core::MemoryId;

use crate::params::VECTOR_DIM;

/// Re-rank PQ candidates against full-precision arena vectors.
///
/// `candidates` is the output of [`crate::hnsw::HnswIndex::search`]
/// (or a streaming-friendly equivalent): each pair carries a memory id
/// and an ADC distance. The distance is used only as input ordering;
/// the output scores are exact cosine similarities computed here.
///
/// `arena_lookup` resolves a memory id to its full-precision vector.
/// `None` indicates the slot has been reclaimed (tombstoned between
/// the HNSW traversal and this read); the candidate is silently
/// dropped.
///
/// `k` caps the output length. If fewer than `k` candidates survive
/// the arena lookup, the returned list is shorter — callers can
/// detect this by comparing `result.len()` to `k`.
///
/// The query and every arena vector are assumed L2-normalised
/// (BGE-small output is normalised by construction). Cosine similarity
/// reduces to a dot product in that
/// case, so the inner loop is `D` multiply-adds — much cheaper than
/// rebuilding the norm per call.
#[must_use]
pub fn rerank<F>(
    candidates: &[(MemoryId, f32)],
    query: &[f32; VECTOR_DIM],
    k: usize,
    arena_lookup: F,
) -> Vec<(MemoryId, f32)>
where
    F: Fn(MemoryId) -> Option<[f32; VECTOR_DIM]>,
{
    if k == 0 || candidates.is_empty() {
        return Vec::new();
    }

    let mut scored: Vec<(MemoryId, f32)> = Vec::with_capacity(candidates.len());
    for &(memory_id, _adc_distance) in candidates {
        let Some(vector) = arena_lookup(memory_id) else {
            // Slot reclaimed between traversal and re-rank read.
            // Surface as "fewer results" rather than a hard error;
            // matches the partial-results contract of the pure-HNSW
            // search bailout.
            continue;
        };
        let similarity = cosine_similarity_normalised(query, &vector);
        scored.push((memory_id, similarity));
    }

    // Descending by similarity — best match first, mirroring
    // HnswIndex::search's output contract.
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(k);
    scored
}

/// Cosine similarity between two L2-normalised vectors. Equivalent to
/// their dot product. Inputs that are not unit-norm produce a value
/// outside `[-1, 1]` — the caller is responsible for the precondition.
///
/// Inlined into the re-rank loop. The arena vectors that feed this
/// were normalised by the embedder before storage; the query is
/// likewise normalised by the embedding service.
#[inline]
#[must_use]
fn cosine_similarity_normalised(a: &[f32; VECTOR_DIM], b: &[f32; VECTOR_DIM]) -> f32 {
    let mut dot = 0.0_f32;
    for i in 0..VECTOR_DIM {
        dot += a[i] * b[i];
    }
    dot
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn mid(slot: u8) -> MemoryId {
        MemoryId::pack(1, slot as u64, 1)
    }

    /// Build a unit-norm vector whose first component is `1.0 / sqrt(2)`
    /// and second component is the supplied `cos_theta`-driven value,
    /// such that two such vectors with cos_theta=cos_θ have dot
    /// product cos_θ. Simpler form: vary the first component and zero
    /// the rest, normalising explicitly.
    fn unit_at_angle(angle_radians: f32) -> [f32; VECTOR_DIM] {
        let mut v = [0.0_f32; VECTOR_DIM];
        v[0] = angle_radians.cos();
        v[1] = angle_radians.sin();
        v
    }

    #[test]
    fn empty_inputs_return_empty() {
        let q = unit_at_angle(0.0);
        assert!(rerank::<fn(_) -> _>(&[], &q, 5, |_| None).is_empty());
    }

    #[test]
    fn zero_k_returns_empty() {
        let q = unit_at_angle(0.0);
        let candidates = vec![(mid(1), 0.0)];
        let arena = |_id| Some(unit_at_angle(0.0));
        assert!(rerank(&candidates, &q, 0, arena).is_empty());
    }

    #[test]
    fn perfect_match_scores_one() {
        let q = unit_at_angle(0.0);
        let candidates = vec![(mid(1), 0.0)];
        let arena = |id| if id == mid(1) { Some(q) } else { None };
        let result = rerank(&candidates, &q, 1, arena);
        assert_eq!(result.len(), 1);
        assert!((result[0].1 - 1.0).abs() < 1e-5, "got {}", result[0].1);
    }

    #[test]
    fn ranking_is_exact_cosine_descending() {
        // Three candidates at angles 0, π/4, π/2 from the query (angle 0).
        // Cosine similarities: 1.0, ~0.707, 0.0. After re-rank, the
        // order should be (mid 1, 1.0), (mid 2, 0.707), (mid 3, 0.0).
        let q = unit_at_angle(0.0);
        let arena_data: HashMap<MemoryId, [f32; VECTOR_DIM]> = [
            (mid(3), unit_at_angle(std::f32::consts::FRAC_PI_2)),
            (mid(1), unit_at_angle(0.0)),
            (mid(2), unit_at_angle(std::f32::consts::FRAC_PI_4)),
        ]
        .into_iter()
        .collect();

        // Note: the ADC distances are intentionally WRONG ordering
        // — re-rank must not preserve input order, it must compute
        // exact cosine.
        let candidates = vec![
            (mid(3), 0.01), // misleadingly small ADC distance
            (mid(1), 5.0),
            (mid(2), 2.0),
        ];

        let result = rerank(&candidates, &q, 3, |id| arena_data.get(&id).copied());
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].0, mid(1));
        assert_eq!(result[1].0, mid(2));
        assert_eq!(result[2].0, mid(3));

        assert!((result[0].1 - 1.0).abs() < 1e-5);
        assert!((result[1].1 - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-5);
        assert!(result[2].1.abs() < 1e-5);
    }

    #[test]
    fn missing_arena_slots_silently_dropped() {
        // 5 candidates; arena returns None for two of them. Output
        // should have 3 entries, not 5.
        let q = unit_at_angle(0.0);
        let candidates: Vec<_> = (1..=5).map(|i| (mid(i), 1.0)).collect();
        let arena = |id: MemoryId| {
            let slot = id.slot() as u8;
            if slot == 2 || slot == 4 {
                None
            } else {
                Some(unit_at_angle(0.0))
            }
        };
        let result = rerank(&candidates, &q, 5, arena);
        assert_eq!(result.len(), 3);
        for (id, _) in &result {
            let slot = id.slot() as u8;
            assert!(slot == 1 || slot == 3 || slot == 5);
        }
    }

    #[test]
    fn truncation_to_k_keeps_top_k_only() {
        // 10 candidates, k=3. Output is the 3 highest-similarity
        // entries. Build them with descending dot products against
        // the query and verify the top 3 are returned.
        let q = unit_at_angle(0.0);
        let arena_data: HashMap<MemoryId, [f32; VECTOR_DIM]> = (1..=10)
            .map(|i| {
                // Angles 0, π/10, 2π/10, ... π
                let angle = (i - 1) as f32 * std::f32::consts::PI / 10.0;
                (mid(i as u8), unit_at_angle(angle))
            })
            .collect();
        let candidates: Vec<_> = (1..=10).map(|i| (mid(i as u8), i as f32)).collect();

        let result = rerank(&candidates, &q, 3, |id| arena_data.get(&id).copied());
        assert_eq!(result.len(), 3);
        // Top 3 by cosine similarity = smallest angles = mid(1), mid(2), mid(3).
        assert_eq!(result[0].0, mid(1));
        assert_eq!(result[1].0, mid(2));
        assert_eq!(result[2].0, mid(3));
    }

    #[test]
    fn rerank_corrects_pq_misranking() {
        // The whole point of re-rank: PQ approximation can put the
        // wrong candidate first, and re-rank fixes it. Set up the
        // candidate list with id=3 first (worst angle) but arena says
        // id=1 should win.
        let q = unit_at_angle(0.0);
        let arena_data: HashMap<MemoryId, [f32; VECTOR_DIM]> = [
            (mid(1), unit_at_angle(0.0)),
            (mid(2), unit_at_angle(std::f32::consts::FRAC_PI_4)),
            (mid(3), unit_at_angle(std::f32::consts::FRAC_PI_2)),
        ]
        .into_iter()
        .collect();

        // PQ-distance ordering says (3, 1, 2) — adversarial.
        let candidates = vec![(mid(3), 0.0), (mid(1), 0.5), (mid(2), 0.4)];

        let result = rerank(&candidates, &q, 3, |id| arena_data.get(&id).copied());
        assert_eq!(result.len(), 3);
        // After re-rank: cosine ordering wins.
        assert_eq!(result[0].0, mid(1));
        assert_eq!(result[1].0, mid(2));
        assert_eq!(result[2].0, mid(3));
    }
}
