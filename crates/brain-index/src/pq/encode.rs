//! PQ encoder: full-precision `[f32; D]` → compressed `[u8; M]`.
//!
//! Per subspace, picks the
//! centroid index that minimises squared distance to the corresponding
//! chunk of the input vector. The output code is the concatenation of
//! the per-subspace centroid indices, one byte each.

use thiserror::Error;

use super::codebook::Codebook;
use super::params::PQ_CENTROIDS_PER_SUBSPACE;
use crate::params::VECTOR_DIM;

/// Compute the PQ code for one vector. Returns `[u8; M]` — one byte per
/// subspace, each byte indexing into the corresponding subspace
/// codebook.
///
/// # Errors
/// - [`EncodeError::NotANumber`] if `vector` contains NaN or infinity.
///   A corrupt input would otherwise propagate as a centroid index with
///   undefined semantics and silently break ranking.
pub fn encode<const M: usize>(
    vector: &[f32; VECTOR_DIM],
    codebook: &Codebook<M>,
) -> Result<[u8; M], EncodeError> {
    if vector.iter().any(|&x| !x.is_finite()) {
        return Err(EncodeError::NotANumber);
    }
    debug_assert_eq!(codebook.sub_dim() * M, VECTOR_DIM);

    let sub_dim = codebook.sub_dim();
    let mut code = [0u8; M];
    for s in 0..M {
        let chunk = &vector[s * sub_dim..(s + 1) * sub_dim];
        code[s] = nearest_centroid(chunk, codebook.subspace(s), sub_dim);
    }
    Ok(code)
}

/// Bulk encode a slice of vectors into the caller-owned output buffer.
/// `out.len()` must equal `vectors.len()` — checked at the top so the
/// inner loop can skip bounds.
///
/// # Errors
/// - [`EncodeError::OutputSizeMismatch`] if `out.len() != vectors.len()`.
/// - [`EncodeError::NotANumber`] if any vector contains NaN/infinity;
///   `out` is left in a partial state (callers should discard on
///   error).
pub fn encode_batch<const M: usize>(
    vectors: &[[f32; VECTOR_DIM]],
    codebook: &Codebook<M>,
    out: &mut [[u8; M]],
) -> Result<(), EncodeError> {
    if vectors.len() != out.len() {
        return Err(EncodeError::OutputSizeMismatch {
            vectors: vectors.len(),
            out: out.len(),
        });
    }
    for (i, v) in vectors.iter().enumerate() {
        out[i] = encode(v, codebook)?;
    }
    Ok(())
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    #[error("vector contains NaN or infinity; cannot quantise")]
    NotANumber,

    #[error("output length {out} does not match vector count {vectors}")]
    OutputSizeMismatch { vectors: usize, out: usize },
}

// ---------------------------------------------------------------------------
// Inner kernel
// ---------------------------------------------------------------------------

/// Argmin over centroid distances for a single subspace chunk. The
/// inline keeps the per-vector overhead at the loop overhead level —
/// the cost is dominated by the squared-distance compute, not the call
/// shape.
///
/// `subspace_centroids.len()` must equal `PQ_CENTROIDS_PER_SUBSPACE *
/// sub_dim` — guaranteed by [`Codebook::subspace`].
#[inline]
fn nearest_centroid(chunk: &[f32], subspace_centroids: &[f32], sub_dim: usize) -> u8 {
    debug_assert_eq!(chunk.len(), sub_dim);
    debug_assert_eq!(
        subspace_centroids.len(),
        PQ_CENTROIDS_PER_SUBSPACE * sub_dim
    );

    let mut best_idx = 0u8;
    let mut best_d2 = f32::INFINITY;
    for k in 0..PQ_CENTROIDS_PER_SUBSPACE {
        let centroid = &subspace_centroids[k * sub_dim..(k + 1) * sub_dim];
        let d2 = squared_distance(chunk, centroid);
        if d2 < best_d2 {
            best_d2 = d2;
            #[allow(clippy::cast_possible_truncation)] // K = 256 fits u8
            {
                best_idx = k as u8;
            }
        }
    }
    best_idx
}

/// Sum of squared component-wise differences. Mirror of the helper in
/// `kmeans.rs` — kept private here so the inlining is local to the
/// encoder and the kmeans helper can evolve independently (e.g., grow
/// SIMD specialisation) without coupling.
#[inline]
fn squared_distance(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let mut sum = 0.0_f32;
    for i in 0..a.len() {
        let d = a[i] - b[i];
        sum += d * d;
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pq::kmeans;
    use crate::pq::params::{PqParams, MIN_TRAINING_SAMPLE};

    /// Build a codebook whose subspace-`s` centroid `k` has its first
    /// component set to `k * 10.0 + s` and zeros elsewhere. Encoding a
    /// vector with first component `target * 10.0 + s` per subspace
    /// then yields code `[target, target, target, ...]`.
    fn synthetic_codebook<const M: usize>() -> Codebook<M> {
        let sub_dim = VECTOR_DIM / M;
        let mut centroids = vec![0.0_f32; M * PQ_CENTROIDS_PER_SUBSPACE * sub_dim];
        for s in 0..M {
            for k in 0..PQ_CENTROIDS_PER_SUBSPACE {
                let offset = (s * PQ_CENTROIDS_PER_SUBSPACE + k) * sub_dim;
                centroids[offset] = (k as f32) * 10.0 + (s as f32);
            }
        }
        Codebook::<M>::from_trained(centroids, sub_dim).unwrap()
    }

    #[test]
    fn encode_centroid_recovers_index() {
        // A vector that lies exactly on centroid `42` in every subspace
        // should encode to all-42 bytes.
        let cb = synthetic_codebook::<8>();
        let sub_dim = cb.sub_dim();
        let mut v = [0.0_f32; VECTOR_DIM];
        for s in 0..8 {
            v[s * sub_dim] = 42.0 * 10.0 + (s as f32);
        }
        let code = encode(&v, &cb).unwrap();
        assert_eq!(code, [42u8; 8]);
    }

    #[test]
    fn encode_rejects_nan() {
        let cb = synthetic_codebook::<8>();
        let mut v = [0.0_f32; VECTOR_DIM];
        v[17] = f32::NAN;
        assert_eq!(encode(&v, &cb).unwrap_err(), EncodeError::NotANumber);
    }

    #[test]
    fn encode_rejects_infinity() {
        let cb = synthetic_codebook::<8>();
        let mut v = [0.0_f32; VECTOR_DIM];
        v[200] = f32::INFINITY;
        assert_eq!(encode(&v, &cb).unwrap_err(), EncodeError::NotANumber);
    }

    #[test]
    fn encode_picks_closest_when_between_centroids() {
        // Vector first-component falls between centroid 7 (= 70.0) and
        // centroid 8 (= 80.0) for subspace 0; should encode to whichever
        // is closer. At 72.0 → 7; at 78.0 → 8.
        let cb = synthetic_codebook::<8>();
        let mut v = [0.0_f32; VECTOR_DIM];

        v[0] = 72.0;
        for s in 1..8 {
            v[s * cb.sub_dim()] = s as f32; // matches centroid 0
        }
        assert_eq!(encode(&v, &cb).unwrap()[0], 7);

        v[0] = 78.0;
        assert_eq!(encode(&v, &cb).unwrap()[0], 8);
    }

    #[test]
    fn encode_batch_matches_per_vector_encode() {
        let cb = synthetic_codebook::<8>();
        let sub_dim = cb.sub_dim();
        let mut vectors = vec![[0.0_f32; VECTOR_DIM]; 100];
        for (i, v) in vectors.iter_mut().enumerate() {
            let target = (i % PQ_CENTROIDS_PER_SUBSPACE) as f32;
            for s in 0..8 {
                v[s * sub_dim] = target * 10.0 + (s as f32);
            }
        }

        let mut batch_out = vec![[0u8; 8]; 100];
        encode_batch(&vectors, &cb, &mut batch_out).unwrap();

        for (i, v) in vectors.iter().enumerate() {
            let solo = encode(v, &cb).unwrap();
            assert_eq!(batch_out[i], solo, "mismatch at index {i}");
        }
    }

    #[test]
    fn encode_batch_rejects_size_mismatch() {
        let cb = synthetic_codebook::<8>();
        let vectors = vec![[0.0_f32; VECTOR_DIM]; 5];
        let mut out = vec![[0u8; 8]; 6];
        assert!(matches!(
            encode_batch(&vectors, &cb, &mut out),
            Err(EncodeError::OutputSizeMismatch { vectors: 5, out: 6 })
        ));
    }

    /// End-to-end: train a codebook on a synthetic well-separated
    /// sample, then encode points back. Most points should encode to
    /// the centroid nearest their planted cluster.
    #[test]
    #[ignore = "heavy PQ k-means training (v1.x feature, not wired into the v1 live path); slow in debug builds. Run with --run-ignored."]
    fn trained_codebook_encodes_planted_clusters_consistently() {
        // 16 clusters in subspace 0 at multiples of 100.
        let mut sample = Vec::with_capacity(MIN_TRAINING_SAMPLE);
        for c in 0..16 {
            for _ in 0..300 {
                let mut v = [0.0_f32; VECTOR_DIM];
                v[0] = c as f32 * 100.0;
                sample.push(v);
            }
        }
        // Pad to MIN_TRAINING_SAMPLE with zero vectors.
        while sample.len() < MIN_TRAINING_SAMPLE {
            sample.push([0.0_f32; VECTOR_DIM]);
        }
        let cb = kmeans::train::<8>(&sample, &PqParams::default_v1(), 0x1234).unwrap();

        // Pick three sample points from cluster 5; they should all
        // encode to the same subspace-0 centroid.
        let p1 = sample[5 * 300];
        let p2 = sample[5 * 300 + 100];
        let p3 = sample[5 * 300 + 200];
        let c1 = encode(&p1, &cb).unwrap();
        let c2 = encode(&p2, &cb).unwrap();
        let c3 = encode(&p3, &cb).unwrap();
        assert_eq!(
            c1[0], c2[0],
            "two cluster-5 points encoded to different subspace-0 centroids"
        );
        assert_eq!(c2[0], c3[0]);
    }
}
