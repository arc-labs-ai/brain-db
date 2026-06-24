//! Product-Quantization configuration knobs.
//!
//! The default profile matches
//! `PqParams::default_v1`: `m=8, bits=8, training_sample=65_536,
//! kmeans_iters=25, rerank_factor=4`. Disabled by default at the
//! [`crate::IndexParams`] level (`pq: None`).

use thiserror::Error;

use crate::params::VECTOR_DIM;

/// v1 fixes `bits=8` (256 centroids per subspace, one byte per code).
/// `bits=4` (16 centroids, packed half-byte codes) is reserved for a
/// future shape but rejected at validation time today.
pub const PQ_BITS_V1: u8 = 8;

/// Derived from `PQ_BITS_V1`. Hardcoded throughout the trainer + codebook
/// so the inner loops can const-unroll and avoid heap juggling at
/// per-vector frequency.
pub const PQ_CENTROIDS_PER_SUBSPACE: usize = 1 << (PQ_BITS_V1 as usize);

/// Minimum sample size accepted by the trainer. Smaller samples cannot
/// fill 256 centroids per subspace without severe under-training; the
/// activation is rejected with `PqError::InsufficientSample`
/// rather than silently degrading recall.
pub const MIN_TRAINING_SAMPLE: usize = 4_096;

/// Maximum sample size accepted by the trainer. Past this the marginal
/// recall gain is negligible and training latency dominates.
pub const MAX_TRAINING_SAMPLE: usize = 1_048_576;

/// Per-corpus PQ knobs. Always nested inside `crate::IndexParams::pq`
/// — a `None` there means pure HNSW.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PqParams {
    /// Number of subquantisers. Must divide [`VECTOR_DIM`] evenly so
    /// every subspace has the same width. `D / m` is the subspace
    /// dimension fed into k-means.
    pub m: usize,
    /// Bits per code. v1 accepts only [`PQ_BITS_V1`].
    pub bits: u8,
    /// Vectors drawn from the corpus to train the codebook. Bounded
    /// by [`MIN_TRAINING_SAMPLE`] and [`MAX_TRAINING_SAMPLE`].
    pub training_sample: usize,
    /// k-means Lloyd iterations during training. The collapsed-cell
    /// re-seed runs inside each iteration; the bound is on total
    /// passes, not converged passes.
    pub kmeans_iters: u8,
    /// Search-time over-fetch factor. The HNSW returns `rerank_factor *
    /// K` PQ-approximate candidates; the re-rank pass against the
    /// full-precision arena vectors picks the actual top-K.
    pub rerank_factor: u8,
}

impl PqParams {
    /// The v1 profile.
    #[must_use]
    pub const fn default_v1() -> Self {
        Self {
            m: 8,
            bits: PQ_BITS_V1,
            training_sample: 65_536,
            kmeans_iters: 25,
            rerank_factor: 4,
        }
    }

    /// Width (in `f32` components) of a single subspace. `D / m` —
    /// always an integer because [`Self::validate`] rejects non-dividing
    /// `m`.
    #[must_use]
    pub const fn subspace_dim(&self) -> usize {
        VECTOR_DIM / self.m
    }

    /// Number of centroids per subspace. Fixed at [`PQ_CENTROIDS_PER_SUBSPACE`]
    /// in v1; the method is here so future `bits=4` work has a single
    /// call site to update.
    #[must_use]
    pub const fn centroids_per_subspace(&self) -> usize {
        PQ_CENTROIDS_PER_SUBSPACE
    }

    /// Validate every field is in range. Runs once at index
    /// construction; the inner loops then assume the invariants hold.
    pub fn validate(&self) -> Result<(), PqParamsError> {
        if self.m == 0 {
            return Err(PqParamsError::MZero);
        }
        if !VECTOR_DIM.is_multiple_of(self.m) {
            return Err(PqParamsError::MDoesNotDivideDim {
                m: self.m,
                d: VECTOR_DIM,
            });
        }
        if self.bits != PQ_BITS_V1 {
            return Err(PqParamsError::BitsUnsupported(self.bits));
        }
        if !(MIN_TRAINING_SAMPLE..=MAX_TRAINING_SAMPLE).contains(&self.training_sample) {
            return Err(PqParamsError::TrainingSampleOutOfRange(
                self.training_sample,
            ));
        }
        if !(5..=100).contains(&self.kmeans_iters) {
            return Err(PqParamsError::KmeansItersOutOfRange(self.kmeans_iters));
        }
        if !(1..=16).contains(&self.rerank_factor) {
            return Err(PqParamsError::RerankFactorOutOfRange(self.rerank_factor));
        }
        Ok(())
    }
}

impl Default for PqParams {
    fn default() -> Self {
        Self::default_v1()
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum PqParamsError {
    #[error("m must be at least 1")]
    MZero,

    #[error("m={m} does not divide VECTOR_DIM={d} evenly")]
    MDoesNotDivideDim { m: usize, d: usize },

    #[error("bits={0} is not supported (v1 accepts only bits=8)")]
    BitsUnsupported(u8),

    #[error(
        "training_sample={0} is outside the supported range {}..={}",
        MIN_TRAINING_SAMPLE,
        MAX_TRAINING_SAMPLE
    )]
    TrainingSampleOutOfRange(usize),

    #[error("kmeans_iters={0} is outside the supported range 5..=100")]
    KmeansItersOutOfRange(u8),

    #[error("rerank_factor={0} is outside the supported range 1..=16")]
    RerankFactorOutOfRange(u8),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_v1_validates() {
        PqParams::default_v1().validate().unwrap();
    }

    #[test]
    fn default_v1_matches_spec_3_1() {
        let p = PqParams::default_v1();
        assert_eq!(p.m, 8);
        assert_eq!(p.bits, 8);
        assert_eq!(p.training_sample, 65_536);
        assert_eq!(p.kmeans_iters, 25);
        assert_eq!(p.rerank_factor, 4);
    }

    #[test]
    fn subspace_dim_divides_cleanly_at_defaults() {
        let p = PqParams::default_v1();
        assert_eq!(p.subspace_dim(), 48);
        assert_eq!(p.subspace_dim() * p.m, VECTOR_DIM);
    }

    #[test]
    fn rejects_non_dividing_m() {
        let mut p = PqParams::default_v1();
        p.m = 7; // 384 / 7 = 54.857...
        assert!(matches!(
            p.validate(),
            Err(PqParamsError::MDoesNotDivideDim {
                m: 7,
                d: VECTOR_DIM
            })
        ));
    }

    #[test]
    fn rejects_unsupported_bits() {
        let mut p = PqParams::default_v1();
        p.bits = 4;
        assert!(matches!(
            p.validate(),
            Err(PqParamsError::BitsUnsupported(4))
        ));
    }

    #[test]
    fn rejects_too_small_sample() {
        let mut p = PqParams::default_v1();
        p.training_sample = 1_000; // below MIN_TRAINING_SAMPLE
        assert!(matches!(
            p.validate(),
            Err(PqParamsError::TrainingSampleOutOfRange(1_000))
        ));
    }

    #[test]
    fn rejects_extreme_iter_count() {
        let mut p = PqParams::default_v1();
        p.kmeans_iters = 200;
        assert!(matches!(
            p.validate(),
            Err(PqParamsError::KmeansItersOutOfRange(200))
        ));
    }

    #[test]
    fn rejects_zero_rerank_factor() {
        let mut p = PqParams::default_v1();
        p.rerank_factor = 0;
        assert!(matches!(
            p.validate(),
            Err(PqParamsError::RerankFactorOutOfRange(0))
        ));
    }
}
