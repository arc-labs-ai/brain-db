//! Process-global bootstrap codebook.
//!
//! PQ is core: every Brain
//! corpus is PQ-compressed from byte zero. Callers don't construct
//! codebooks — they reach for [`bootstrap_codebook`] which trains
//! once per process, lazily, against a deterministic synthetic
//! sample.
//!
//! Once a corpus accumulates `MIN_TRAINING_SAMPLE` real vectors, the
//! per-shard maintenance worker retrains against that corpus's actual
//! distribution and swaps in the refined codebook. The bootstrap
//! codebook is only the cold-start fallback.
//!
//! Why deterministic-synthetic instead of an embedded blob:
//!
//! - The bootstrap codebook quality only matters during the cold-start
//!   window (first ~4096 inserts). After that the maintenance retrain
//!   takes over.
//! - Embedding a binary blob in the crate adds build-time generation
//!   complexity (build.rs, generated rs file, target-specific paths)
//!   for a one-time cost we can amortise across the process lifetime
//!   instead.
//! - Determinism (fixed PRNG seed) guarantees every shard observes
//!   the same default codebook, so cross-shard recall behaviour is
//!   reproducible.

use std::sync::{Arc, OnceLock};

use super::codebook::Codebook;
use super::kmeans;
use super::params::{PqParams, MIN_TRAINING_SAMPLE};
use crate::params::VECTOR_DIM;

/// Default `M` for the bootstrap codebook. Matches
/// [`PqParams::default_v1`]'s `m`.
pub const BOOTSTRAP_M: usize = 8;

/// Fixed seed for the synthetic training sample. Locks reproducibility
/// across processes.
const SYNTHETIC_SEED: u64 = 0x4252_4149_4e2d_5051; // "BRAIN-PQ"

/// Fixed seed for the k-means trainer over the synthetic sample.
const TRAINER_SEED: u64 = 0x4453_4554_3030_3030; // "DSET0000"

/// Lazy global. First reader pays the ~one-time training cost
/// (~150 ms with the synthetic sample at the minimum sample size); every
/// subsequent reader gets the cached `Arc` for free.
static BOOTSTRAP: OnceLock<Arc<Codebook<BOOTSTRAP_M>>> = OnceLock::new();

/// Borrow the process-wide default codebook. First call trains; later
/// calls are an `Arc::clone`.
///
/// The training sample is a deterministic synthetic distribution
/// (Gaussian → L2-normalised, matching BGE-small's output shape).
/// Recall against real BGE-small embeddings is poor until the
/// maintenance worker retrains — the re-rank pass against the
/// full-precision arena recovers correctness in the meantime.
#[must_use]
pub fn bootstrap_codebook() -> Arc<Codebook<BOOTSTRAP_M>> {
    BOOTSTRAP
        .get_or_init(|| {
            let sample = synthetic_training_sample();
            let cb = kmeans::train::<BOOTSTRAP_M>(&sample, &PqParams::default_v1(), TRAINER_SEED)
                .expect("invariant: locally generated synthetic sample is always well-shaped");
            Arc::new(cb)
        })
        .clone()
}

/// Generate a deterministic training sample of L2-normalised vectors
/// drawn from a per-dimension Gaussian. Mimics BGE-small's output
/// distribution well enough for bootstrap PQ — real embeddings have
/// thicker tails but the centroid layout is in the right
/// neighbourhood.
fn synthetic_training_sample() -> Vec<[f32; VECTOR_DIM]> {
    let mut rng = SplitMix64::new(SYNTHETIC_SEED);
    let mut out = Vec::with_capacity(MIN_TRAINING_SAMPLE);
    for _ in 0..MIN_TRAINING_SAMPLE {
        let mut v = [0.0_f32; VECTOR_DIM];
        let mut sum_sq = 0.0_f32;
        for x in v.iter_mut() {
            // Box-Muller from two uniforms → standard normal.
            let u1 = rng.next_f32_unit().max(1e-7);
            let u2 = rng.next_f32_unit();
            let r = (-2.0 * u1.ln()).sqrt();
            let theta = 2.0 * std::f32::consts::PI * u2;
            *x = r * theta.cos();
            sum_sq += *x * *x;
        }
        // L2-normalise so the sample matches BGE-small's output norm.
        let inv_norm = 1.0 / sum_sq.sqrt();
        for x in v.iter_mut() {
            *x *= inv_norm;
        }
        out.push(v);
    }
    out
}

// ---------------------------------------------------------------------------
// SplitMix64 — duplicated from kmeans.rs so this module is fully
// self-contained. The two RNGs run on independent seeds and don't
// share state; keeping the implementation local avoids a `pub(super)`
// surface on a primitive that should stay private.
// ---------------------------------------------------------------------------

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

    fn next_f32_unit(&mut self) -> f32 {
        ((self.next_u64() >> 40) as f32) / ((1u64 << 24) as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "trains the 65k-vector bootstrap codebook (v1.x feature, not wired into the v1 live path); slow in debug builds. Run with --run-ignored."]
    fn bootstrap_codebook_is_stable_across_calls() {
        let cb_a = bootstrap_codebook();
        let cb_b = bootstrap_codebook();
        assert!(Arc::ptr_eq(&cb_a, &cb_b), "second call should re-share");
    }

    #[test]
    #[ignore = "trains the 65k-vector bootstrap codebook (v1.x feature, not wired into the v1 live path); slow in debug builds. Run with --run-ignored."]
    fn bootstrap_codebook_has_correct_shape() {
        let cb = bootstrap_codebook();
        assert_eq!(cb.m(), BOOTSTRAP_M);
        assert_eq!(cb.k(), super::super::PQ_CENTROIDS_PER_SUBSPACE);
        assert_eq!(cb.sub_dim(), VECTOR_DIM / BOOTSTRAP_M);
    }

    #[test]
    fn synthetic_sample_is_unit_norm() {
        // Spot-check the first 10 vectors: each should have L2 norm
        // very close to 1.0 (Gaussian + normalise).
        let sample = synthetic_training_sample();
        for v in sample.iter().take(10) {
            let sum_sq: f32 = v.iter().map(|x| x * x).sum();
            let norm = sum_sq.sqrt();
            assert!((norm - 1.0).abs() < 1e-5, "expected unit norm, got {norm}",);
        }
    }

    #[test]
    fn synthetic_sample_size_is_minimum() {
        let sample = synthetic_training_sample();
        assert_eq!(sample.len(), MIN_TRAINING_SAMPLE);
    }
}
