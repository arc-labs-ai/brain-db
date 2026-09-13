//! Consensus-direction vector for a set of memory embeddings.
//!
//! Bundles a set of memory embeddings into a single "centroid" — the
//! L2-normalized sum. The algebra mirrors HRR bundling
//! ([`super::ops::bundle`]) — sum + normalize — but operates in the
//! 384-dim BGE-small embedding space rather than the VSA's 512-dim
//! HRR space, so it can compare directly against memory arena vectors
//! without an intermediate projection.
//!
//! Used by the PLAN executor (goal-direction heuristic for BFS
//! expansion) and the REASON executor (topic alignment damper on
//! evidence scoring). The HRR-space analogy machinery in
//! [`super::analogy`] is the next integration step — a typed-graph
//! follow-up — and is intentionally separate from this helper.

use super::errors::VsaError;

/// L2-normalized sum of a non-empty slice of D-dim embeddings.
///
/// Empty input returns [`VsaError::EmptyBundle`]. A zero-magnitude
/// sum (e.g. two opposing unit vectors) returns
/// [`VsaError::DegenerateNorm`] — the caller has no consensus
/// direction in that case.
pub fn semantic_centroid<const D: usize>(vectors: &[&[f32; D]]) -> Result<[f32; D], VsaError> {
    if vectors.is_empty() {
        return Err(VsaError::EmptyBundle);
    }

    let mut sum = [0.0_f32; D];
    for v in vectors {
        for (s, x) in sum.iter_mut().zip(v.iter()) {
            *s += *x;
        }
    }

    let norm_sq: f32 = sum.iter().map(|x| x * x).sum();
    let norm = norm_sq.sqrt();
    if !norm.is_finite() || norm < 1e-12 {
        return Err(VsaError::DegenerateNorm { norm });
    }
    let inv = norm.recip();
    for x in sum.iter_mut() {
        *x *= inv;
    }
    Ok(sum)
}

/// Cosine similarity between an embedding and a centroid. Both
/// inputs are expected to be L2-normalized — the cosine reduces to
/// a dot product.
#[must_use]
pub fn cosine_to_centroid<const D: usize>(v: &[f32; D], centroid: &[f32; D]) -> f32 {
    v.iter().zip(centroid).map(|(a, b)| a * b).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit_x() -> [f32; 4] {
        [1.0, 0.0, 0.0, 0.0]
    }
    fn unit_y() -> [f32; 4] {
        [0.0, 1.0, 0.0, 0.0]
    }
    fn neg_unit_x() -> [f32; 4] {
        [-1.0, 0.0, 0.0, 0.0]
    }

    #[test]
    fn single_vector_centroid_is_identity() {
        let x = unit_x();
        let c = semantic_centroid::<4>(&[&x]).unwrap();
        for (a, b) in c.iter().zip(x.iter()) {
            assert!((a - b).abs() < 1e-6, "got={a} want={b}");
        }
    }

    #[test]
    fn two_vector_centroid_lies_between_inputs() {
        let x = unit_x();
        let y = unit_y();
        let c = semantic_centroid::<4>(&[&x, &y]).unwrap();
        // Should be (1/sqrt(2), 1/sqrt(2), 0, 0).
        let expected = 1.0_f32 / 2.0_f32.sqrt();
        assert!((c[0] - expected).abs() < 1e-5, "c[0]={}", c[0]);
        assert!((c[1] - expected).abs() < 1e-5, "c[1]={}", c[1]);
        assert!(c[2].abs() < 1e-6 && c[3].abs() < 1e-6);
        // Cosine to either input should be 1/sqrt(2) ≈ 0.707.
        let cos_x = cosine_to_centroid(&x, &c);
        let cos_y = cosine_to_centroid(&y, &c);
        assert!((cos_x - expected).abs() < 1e-5);
        assert!((cos_y - expected).abs() < 1e-5);
    }

    #[test]
    fn opposing_vectors_have_no_consensus_direction() {
        let x = unit_x();
        let neg = neg_unit_x();
        let err = semantic_centroid::<4>(&[&x, &neg]).unwrap_err();
        assert!(
            matches!(err, VsaError::DegenerateNorm { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn cosine_to_centroid_is_bounded_in_range() {
        // For arbitrary unit-norm inputs the cosine must sit in [-1, 1].
        let inputs: [[f32; 4]; 4] = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.5, 0.5, 0.5, 0.5],
        ];
        let refs: Vec<&[f32; 4]> = inputs.iter().take(3).collect();
        let c = semantic_centroid::<4>(&refs).unwrap();
        for v in &inputs {
            let cos = cosine_to_centroid(v, &c);
            assert!((-1.0..=1.0).contains(&cos), "cos out of range: {cos}",);
        }
        // Opposite of the centroid should yield cosine ≈ -1.
        let mut antipode = c;
        for x in antipode.iter_mut() {
            *x = -*x;
        }
        let cos = cosine_to_centroid(&antipode, &c);
        assert!((cos + 1.0).abs() < 1e-5, "antipode cos={cos}");
    }
}
