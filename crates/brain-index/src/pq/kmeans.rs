//! Deterministic k-means trainer for PQ codebooks.
//!
//! Per subspace, runs k-means++
//! seeding followed by Lloyd iterations, with a collapsed-cell re-seed
//! that picks the point furthest from any existing centroid when a
//! centroid receives zero assignments. The trainer is deterministic
//! given the same `(sample, seed)` so two shards observing the same
//! input produce byte-identical codebooks.

use thiserror::Error;

use super::codebook::{Codebook, CodebookError};
use super::params::{PqParams, MIN_TRAINING_SAMPLE, PQ_CENTROIDS_PER_SUBSPACE};
use crate::params::VECTOR_DIM;

/// Train a codebook from a `[f32; D]` sample. Returns a fully-validated
/// [`Codebook`] ready for encoding.
///
/// `M` must match `params.m`. The function panics in debug (`assert`)
/// rather than returning an error because mismatched const generics
/// represent a programming bug, not a runtime condition — the only
/// caller is the index activation path which has just consulted the
/// same `params`.
///
/// # Errors
/// - [`KmeansError::SampleTooSmall`] if the sample is below
///   [`MIN_TRAINING_SAMPLE`].
/// - [`KmeansError::NonFiniteInput`] if any sample component is NaN /
///   infinity.
/// - [`KmeansError::CodebookBuild`] on a structural failure shaping the
///   trained centroids into a [`Codebook`] (should never fire — the
///   trainer guarantees the layout — but surfaces if invariants drift).
pub fn train<const M: usize>(
    sample: &[[f32; VECTOR_DIM]],
    params: &PqParams,
    seed: u64,
) -> Result<Codebook<M>, KmeansError> {
    debug_assert_eq!(M, params.m, "const generic M must match PqParams.m");

    if sample.len() < MIN_TRAINING_SAMPLE {
        return Err(KmeansError::SampleTooSmall {
            got: sample.len(),
            required: MIN_TRAINING_SAMPLE,
        });
    }
    if sample.iter().any(|v| v.iter().any(|&x| !x.is_finite())) {
        return Err(KmeansError::NonFiniteInput);
    }

    let sub_dim = params.subspace_dim();
    let k = PQ_CENTROIDS_PER_SUBSPACE;
    let n = sample.len();

    // One trainer seed → m distinct subspace seeds. Mixing the subspace
    // index in (rather than threading the rng across subspaces in
    // sequence) keeps each subspace's training independently reproducible
    // and parallelisable.
    let mut centroids = vec![0.0_f32; M * k * sub_dim];

    for s in 0..M {
        let mut rng =
            SplitMix64::new(seed.wrapping_add((s as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)));
        train_subspace(
            sample,
            s,
            sub_dim,
            n,
            params.kmeans_iters,
            &mut rng,
            &mut centroids[s * k * sub_dim..(s + 1) * k * sub_dim],
        );
    }

    Codebook::<M>::from_trained(centroids, sub_dim).map_err(KmeansError::CodebookBuild)
}

#[derive(Debug, Error, Clone, PartialEq)]
pub enum KmeansError {
    #[error("training sample has {got} vectors; need at least {required}")]
    SampleTooSmall { got: usize, required: usize },

    #[error("training sample contains NaN or infinity")]
    NonFiniteInput,

    #[error(transparent)]
    CodebookBuild(#[from] CodebookError),
}

// ---------------------------------------------------------------------------
// Single-subspace k-means
// ---------------------------------------------------------------------------

/// Train one subspace into `out` (length `K * sub_dim`).
///
/// `s` is the subspace index used to slice each sample vector. The
/// trainer reads `sample[i][s*sub_dim..(s+1)*sub_dim]` for every `i`.
fn train_subspace(
    sample: &[[f32; VECTOR_DIM]],
    s: usize,
    sub_dim: usize,
    n: usize,
    iters: u8,
    rng: &mut SplitMix64,
    out: &mut [f32],
) {
    let k = PQ_CENTROIDS_PER_SUBSPACE;
    debug_assert_eq!(out.len(), k * sub_dim);

    // Helper closures over the subspace slice.
    let chunk_of = |i: usize| -> &[f32] { &sample[i][s * sub_dim..(s + 1) * sub_dim] };

    // ----- k-means++ seeding ----------------------------------------
    // First centroid: uniformly random sample point.
    let first_idx = (rng.next_u64() as usize) % n;
    out[0..sub_dim].copy_from_slice(chunk_of(first_idx));

    // Subsequent centroids: probability ∝ D²(x, nearest existing
    // centroid). Maintain a running min-distance² per sample point so
    // the probability table can be rebuilt in O(N) per pick instead of
    // O(N·k).
    let mut nearest_d2: Vec<f32> = (0..n)
        .map(|i| squared_distance(chunk_of(i), &out[0..sub_dim]))
        .collect();

    for centroid_idx in 1..k {
        let pick = weighted_pick(&nearest_d2, rng);
        let centroid_start = centroid_idx * sub_dim;
        out[centroid_start..centroid_start + sub_dim].copy_from_slice(chunk_of(pick));
        // Refresh the running minimum: each point now compares against
        // the new centroid too.
        for (i, nearest) in nearest_d2.iter_mut().enumerate() {
            let d2 = squared_distance(chunk_of(i), &out[centroid_start..centroid_start + sub_dim]);
            if d2 < *nearest {
                *nearest = d2;
            }
        }
    }

    // ----- Lloyd iterations -----------------------------------------
    let mut assignments = vec![0u16; n];
    let mut new_centroids = vec![0.0_f32; k * sub_dim];
    let mut counts = vec![0u32; k];

    for _iter in 0..iters {
        // Assignment step.
        for (i, assignment) in assignments.iter_mut().enumerate() {
            *assignment = argmin_centroid(chunk_of(i), out, sub_dim, k);
        }

        // Update step.
        new_centroids.iter_mut().for_each(|c| *c = 0.0);
        counts.iter_mut().for_each(|c| *c = 0);
        for (i, &assignment) in assignments.iter().enumerate() {
            let cluster = assignment as usize;
            let dst = &mut new_centroids[cluster * sub_dim..(cluster + 1) * sub_dim];
            let src = chunk_of(i);
            for j in 0..sub_dim {
                dst[j] += src[j];
            }
            counts[cluster] += 1;
        }
        for cluster in 0..k {
            if counts[cluster] == 0 {
                // Collapsed cell: re-seed from the point furthest from
                // any existing centroid. The next iteration will assign
                // points to it normally.
                let reseed = furthest_point(sample, s, sub_dim, n, out, k);
                let dst = &mut new_centroids[cluster * sub_dim..(cluster + 1) * sub_dim];
                dst.copy_from_slice(chunk_of(reseed));
            } else {
                let inv = 1.0 / counts[cluster] as f32;
                let dst = &mut new_centroids[cluster * sub_dim..(cluster + 1) * sub_dim];
                for c in dst.iter_mut() {
                    *c *= inv;
                }
            }
        }
        out.copy_from_slice(&new_centroids);
    }
}

// ---------------------------------------------------------------------------
// Hot-loop helpers
// ---------------------------------------------------------------------------

/// Euclidean distance squared between two equal-length slices. The
/// `inline` keeps the per-point cost down inside the assignment loop.
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

/// Argmin over centroid distances. Inlined into the assignment loop.
#[inline]
fn argmin_centroid(point: &[f32], centroids: &[f32], sub_dim: usize, k: usize) -> u16 {
    let mut best_idx = 0u16;
    let mut best_d2 = f32::INFINITY;
    for c in 0..k {
        let start = c * sub_dim;
        let d2 = squared_distance(point, &centroids[start..start + sub_dim]);
        if d2 < best_d2 {
            best_d2 = d2;
            best_idx = c as u16;
        }
    }
    best_idx
}

/// Probability-weighted pick over `weights`. Treats negative weights as
/// zero. Caller guarantees the total weight is positive (k-means++
/// after the first pick has at least one non-zero distance).
fn weighted_pick(weights: &[f32], rng: &mut SplitMix64) -> usize {
    let total: f32 = weights.iter().map(|&w| w.max(0.0)).sum();
    if total <= 0.0 {
        // Degenerate: every point already matches a centroid exactly.
        // Pick deterministically rather than NaN-walking.
        return (rng.next_u64() as usize) % weights.len();
    }
    let target = (rng.next_f32_unit()) * total;
    let mut running = 0.0_f32;
    for (i, &w) in weights.iter().enumerate() {
        running += w.max(0.0);
        if running >= target {
            return i;
        }
    }
    weights.len() - 1
}

/// Index of the sample point with the largest min-distance to any
/// existing centroid in subspace `s`. Used by the collapsed-cell
/// re-seeder.
fn furthest_point(
    sample: &[[f32; VECTOR_DIM]],
    s: usize,
    sub_dim: usize,
    n: usize,
    centroids: &[f32],
    k: usize,
) -> usize {
    let mut best_idx = 0;
    let mut best_d2 = -1.0_f32;
    for (i, sample_i) in sample.iter().enumerate().take(n) {
        let chunk = &sample_i[s * sub_dim..(s + 1) * sub_dim];
        let mut min_d2 = f32::INFINITY;
        for c in 0..k {
            let cs = c * sub_dim;
            let d2 = squared_distance(chunk, &centroids[cs..cs + sub_dim]);
            if d2 < min_d2 {
                min_d2 = d2;
            }
        }
        if min_d2 > best_d2 {
            best_d2 = min_d2;
            best_idx = i;
        }
    }
    best_idx
}

// ---------------------------------------------------------------------------
// Deterministic PRNG: SplitMix64
// ---------------------------------------------------------------------------

/// Tiny deterministic PRNG. SplitMix64 (Vigna, 2014). 64-bit state, no
/// external dep. Fine for k-means seeding — the trainer's correctness
/// does not depend on cryptographic randomness, only on reproducibility
/// across runs with the same seed.
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform `f32` in `[0, 1)`. Uses 24 random bits — the float
    /// mantissa width.
    fn next_f32_unit(&mut self) -> f32 {
        ((self.next_u64() >> 40) as f32) / ((1u64 << 24) as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic sample with `clusters` ground-truth centres
    /// embedded in subspace 0; the rest of the dims are noise. Useful
    /// for "does k-means actually cluster" smoke tests.
    fn synthetic_sample(clusters: usize, per_cluster: usize, seed: u64) -> Vec<[f32; VECTOR_DIM]> {
        let mut rng = SplitMix64::new(seed);
        let mut out = Vec::with_capacity(clusters * per_cluster);
        for c in 0..clusters {
            // Centre this cluster well-separated in subspace 0.
            let centre = c as f32 * 100.0;
            for _ in 0..per_cluster {
                let mut v = [0.0_f32; VECTOR_DIM];
                // Subspace 0: centre + small jitter.
                for v_d in v.iter_mut().take(48) {
                    *v_d = centre + (rng.next_f32_unit() - 0.5) * 0.5;
                }
                // Rest: low-amplitude noise.
                for v_d in v.iter_mut().skip(48) {
                    *v_d = (rng.next_f32_unit() - 0.5) * 0.1;
                }
                out.push(v);
            }
        }
        out
    }

    #[test]
    fn rejects_too_small_sample() {
        let sample: Vec<[f32; VECTOR_DIM]> = (0..100).map(|_| [0.1; VECTOR_DIM]).collect();
        let err = train::<8>(&sample, &PqParams::default_v1(), 0).unwrap_err();
        assert!(matches!(err, KmeansError::SampleTooSmall { .. }));
    }

    #[test]
    fn rejects_nan_input() {
        let mut sample: Vec<[f32; VECTOR_DIM]> = (0..MIN_TRAINING_SAMPLE)
            .map(|i| [i as f32 * 1e-4; VECTOR_DIM])
            .collect();
        sample[1234][7] = f32::NAN;
        let err = train::<8>(&sample, &PqParams::default_v1(), 0).unwrap_err();
        assert_eq!(err, KmeansError::NonFiniteInput);
    }

    #[test]
    #[ignore = "heavy PQ k-means training (v1.x feature, not wired into the v1 live path); slow in debug builds. Run with --run-ignored."]
    fn determinism_same_seed_byte_identical() {
        let sample = synthetic_sample(16, 300, 0xDEAD_BEEF); // 4800 vectors
                                                             // Bump to MIN_TRAINING_SAMPLE by padding with noise.
        let pad_count = MIN_TRAINING_SAMPLE.saturating_sub(sample.len());
        let pad: Vec<[f32; VECTOR_DIM]> = (0..pad_count)
            .map(|i| {
                let mut v = [0.0_f32; VECTOR_DIM];
                v[0] = (i as f32) * 1e-4;
                v
            })
            .collect();
        let full: Vec<[f32; VECTOR_DIM]> = sample.into_iter().chain(pad).collect();

        let params = PqParams::default_v1();
        let cb_a = train::<8>(&full, &params, 0xABCD).unwrap();
        let cb_b = train::<8>(&full, &params, 0xABCD).unwrap();
        assert_eq!(cb_a.as_flat(), cb_b.as_flat());
    }

    #[test]
    #[ignore = "heavy PQ k-means training (v1.x feature, not wired into the v1 live path); slow in debug builds. Run with --run-ignored."]
    fn determinism_different_seed_different_codebook() {
        let sample = synthetic_sample(8, 600, 0x1234);
        let pad_count = MIN_TRAINING_SAMPLE.saturating_sub(sample.len());
        let pad: Vec<[f32; VECTOR_DIM]> = (0..pad_count)
            .map(|i| {
                let mut v = [0.0_f32; VECTOR_DIM];
                v[0] = (i as f32) * 1e-4;
                v
            })
            .collect();
        let full: Vec<[f32; VECTOR_DIM]> = sample.into_iter().chain(pad).collect();
        let params = PqParams::default_v1();
        let cb_a = train::<8>(&full, &params, 1).unwrap();
        let cb_b = train::<8>(&full, &params, 2).unwrap();
        assert_ne!(cb_a.as_flat(), cb_b.as_flat());
    }

    #[test]
    #[ignore = "heavy PQ k-means training (v1.x feature, not wired into the v1 live path); slow in debug builds. Run with --run-ignored."]
    fn assignment_cost_monotone_nonincreasing() {
        // Property: each Lloyd iteration cannot raise the total
        // assignment cost. We can't easily probe per-iter costs from
        // the public API, so we check the end-state inertia is no
        // worse than a single-iter run on the same seed.
        let sample = synthetic_sample(8, 600, 0xCAFE);
        let pad_count = MIN_TRAINING_SAMPLE.saturating_sub(sample.len());
        let pad: Vec<[f32; VECTOR_DIM]> = (0..pad_count)
            .map(|i| {
                let mut v = [0.0_f32; VECTOR_DIM];
                v[0] = (i as f32) * 1e-4;
                v
            })
            .collect();
        let full: Vec<[f32; VECTOR_DIM]> = sample.into_iter().chain(pad).collect();

        let mut params_short = PqParams::default_v1();
        params_short.kmeans_iters = 5;
        let mut params_long = params_short;
        params_long.kmeans_iters = 25;

        let cb_short = train::<8>(&full, &params_short, 0xBEEF).unwrap();
        let cb_long = train::<8>(&full, &params_long, 0xBEEF).unwrap();

        let cost_short = total_inertia(&full, &cb_short);
        let cost_long = total_inertia(&full, &cb_long);
        assert!(
            cost_long <= cost_short * 1.01,
            "long run inertia {cost_long} > short run inertia {cost_short}"
        );
    }

    #[test]
    #[ignore = "heavy PQ k-means training (v1.x feature, not wired into the v1 live path); slow in debug builds. Run with --run-ignored."]
    fn centroids_recover_subspace_0_clusters() {
        // 16 well-separated clusters in subspace 0, 300 points each.
        // Subspace-0 centroids should land near the cluster centres
        // {0, 100, 200, ..., 1500}.
        let sample = synthetic_sample(16, 300, 0xFEED);
        let pad_count = MIN_TRAINING_SAMPLE.saturating_sub(sample.len());
        let pad: Vec<[f32; VECTOR_DIM]> = (0..pad_count)
            .map(|i| {
                let mut v = [0.0_f32; VECTOR_DIM];
                v[0] = (i as f32) * 1e-4;
                v
            })
            .collect();
        let full: Vec<[f32; VECTOR_DIM]> = sample.into_iter().chain(pad).collect();
        let cb = train::<8>(&full, &PqParams::default_v1(), 0xACE).unwrap();

        // Look at the first-component of each subspace-0 centroid.
        // At least 8 of them (half the 16 true centres; padding
        // dominates the rest) should land within ±2 of a multiple
        // of 100 in [0, 1500].
        let mut hits = 0;
        for c in 0..PQ_CENTROIDS_PER_SUBSPACE {
            let centroid = cb.centroid(0, c);
            let v0 = centroid[0];
            for target in 0..16 {
                if (v0 - (target as f32 * 100.0)).abs() <= 2.0 {
                    hits += 1;
                    break;
                }
            }
        }
        assert!(
            hits >= 8,
            "centroids landed on only {hits}/16 of the planted clusters"
        );
    }

    /// Sum-of-squared-distances to nearest centroid across the whole
    /// sample. Lower is better; convergence is monotone non-increasing
    /// in Lloyd iteration count.
    fn total_inertia<const M: usize>(sample: &[[f32; VECTOR_DIM]], cb: &Codebook<M>) -> f64 {
        let mut total = 0.0_f64;
        for v in sample {
            for s in 0..M {
                let chunk = &v[s * cb.sub_dim()..(s + 1) * cb.sub_dim()];
                let mut min_d2 = f32::INFINITY;
                for k in 0..PQ_CENTROIDS_PER_SUBSPACE {
                    let centroid = cb.centroid(s, k);
                    let mut d2 = 0.0_f32;
                    for j in 0..cb.sub_dim() {
                        let d = chunk[j] - centroid[j];
                        d2 += d * d;
                    }
                    if d2 < min_d2 {
                        min_d2 = d2;
                    }
                }
                total += min_d2 as f64;
            }
        }
        total
    }
}
