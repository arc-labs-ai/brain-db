//! Trained PQ codebook — the immutable artefact produced by k-means
//! training and consumed by every later operation.
//!
//! Layout: `m` subspaces, each
//! with `K = 2^bits` centroids of `D/m` `f32` components. v1 fixes
//! `K = 256`. Centroids are stored contiguously in a single flat
//! `Vec<f32>` so SIMD-friendly access patterns work without per-subspace
//! pointer chasing.

use thiserror::Error;

use super::params::{PqParams, PQ_CENTROIDS_PER_SUBSPACE};
use crate::params::VECTOR_DIM;

/// A trained quantiser ready for encoding and distance computation.
///
/// `M` is the number of subquantisers (compile-time const so the
/// distance kernels can const-unroll). The number of centroids per
/// subspace is fixed at [`PQ_CENTROIDS_PER_SUBSPACE`] (= 256) in v1.
///
/// Layout (`m × K × sub_dim` floats, row-major over `(subspace,
/// centroid, component)`):
///
/// ```text
///   centroids[subspace=s][centroid=k][component=c]
/// = data[(s * K + k) * sub_dim + c]
/// ```
#[derive(Debug, Clone)]
pub struct Codebook<const M: usize> {
    /// Sub-vector dimension (`D / m`). Stored for runtime checks; should
    /// equal [`VECTOR_DIM`] / `M`.
    sub_dim: usize,
    /// Flat centroid storage. Length is `m × K × sub_dim`.
    data: Vec<f32>,
}

impl<const M: usize> Codebook<M> {
    /// Build a codebook from already-trained centroids. The k-means
    /// trainer ([`super::kmeans::train`]) is the only intended caller;
    /// tests can use this constructor to feed a known codebook.
    ///
    /// # Errors
    /// - [`CodebookError::DimMismatch`] if `M * sub_dim != VECTOR_DIM`.
    /// - [`CodebookError::CentroidsWrongSize`] if `centroids.len() != M *
    ///   K * sub_dim`.
    /// - [`CodebookError::NonFinite`] if any centroid component is NaN
    ///   or infinity (would propagate into distance lookups and silently
    ///   break ranking).
    pub fn from_trained(centroids: Vec<f32>, sub_dim: usize) -> Result<Self, CodebookError> {
        if sub_dim == 0 || sub_dim * M != VECTOR_DIM {
            return Err(CodebookError::DimMismatch {
                m: M,
                sub_dim,
                d: VECTOR_DIM,
            });
        }
        let expected_len = M * PQ_CENTROIDS_PER_SUBSPACE * sub_dim;
        if centroids.len() != expected_len {
            return Err(CodebookError::CentroidsWrongSize {
                got: centroids.len(),
                expected: expected_len,
            });
        }
        if centroids.iter().any(|&c| !c.is_finite()) {
            return Err(CodebookError::NonFinite);
        }
        Ok(Self {
            sub_dim,
            data: centroids,
        })
    }

    /// Sub-vector dimension. Equal to [`VECTOR_DIM`] / `M`.
    #[must_use]
    pub const fn sub_dim(&self) -> usize {
        self.sub_dim
    }

    /// Number of subquantisers.
    #[must_use]
    pub const fn m(&self) -> usize {
        M
    }

    /// Number of centroids per subspace ([`PQ_CENTROIDS_PER_SUBSPACE`]).
    #[must_use]
    pub const fn k(&self) -> usize {
        PQ_CENTROIDS_PER_SUBSPACE
    }

    /// View the `k`-th centroid of subspace `s`. Panics on out-of-bounds
    /// indices — callers inside the hot loops guarantee the bounds via
    /// loop ranges over `0..M` and `0..K`.
    #[inline]
    #[must_use]
    pub fn centroid(&self, subspace: usize, centroid: usize) -> &[f32] {
        debug_assert!(subspace < M);
        debug_assert!(centroid < PQ_CENTROIDS_PER_SUBSPACE);
        let start = (subspace * PQ_CENTROIDS_PER_SUBSPACE + centroid) * self.sub_dim;
        &self.data[start..start + self.sub_dim]
    }

    /// View every centroid of one subspace as a flat slice (centroid
    /// `k`'s components start at offset `k * sub_dim`). Used by the
    /// per-subspace inner loops in encoding and LUT construction.
    #[inline]
    #[must_use]
    pub fn subspace(&self, subspace: usize) -> &[f32] {
        debug_assert!(subspace < M);
        let start = subspace * PQ_CENTROIDS_PER_SUBSPACE * self.sub_dim;
        let len = PQ_CENTROIDS_PER_SUBSPACE * self.sub_dim;
        &self.data[start..start + len]
    }

    /// Borrow the raw centroid buffer. Used by persistence (snapshot
    /// writes the bytes wholesale; loader runs them back through
    /// [`Self::from_trained`] to re-validate).
    #[must_use]
    pub fn as_flat(&self) -> &[f32] {
        &self.data
    }

    /// Serialize the codebook into a self-describing byte image. Layout:
    /// `magic(4) | version(4) | M(4) | sub_dim(4) | data_len(4) | data: f32 LE`.
    /// The wrapper carries an independent BLAKE3 over the bytes for
    /// integrity; this format only needs to round-trip the centroids
    /// and the shape parameters it was trained for.
    #[must_use]
    pub fn serialize(&self) -> Vec<u8> {
        let header_len = CODEBOOK_HEADER_LEN;
        let mut out = Vec::with_capacity(header_len + self.data.len() * 4);
        out.extend_from_slice(&CODEBOOK_MAGIC);
        out.extend_from_slice(&CODEBOOK_FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&(M as u32).to_le_bytes());
        out.extend_from_slice(&(self.sub_dim as u32).to_le_bytes());
        out.extend_from_slice(&(self.data.len() as u32).to_le_bytes());
        for f in &self.data {
            out.extend_from_slice(&f.to_le_bytes());
        }
        out
    }

    /// Inverse of [`Self::serialize`]. Validates magic + version, then
    /// hands the centroids to [`Self::from_trained`] for the standard
    /// shape + non-finite checks.
    pub fn deserialize(bytes: &[u8]) -> Result<Self, CodebookError> {
        if bytes.len() < CODEBOOK_HEADER_LEN {
            return Err(CodebookError::Truncated);
        }
        let magic: [u8; 4] = bytes[0..4]
            .try_into()
            .expect("invariant: header length checked above guarantees 4 magic bytes");
        if magic != CODEBOOK_MAGIC {
            return Err(CodebookError::BadMagic);
        }
        let version = u32::from_le_bytes(
            bytes[4..8]
                .try_into()
                .expect("invariant: header length checked above guarantees these 4 bytes"),
        );
        if version != CODEBOOK_FORMAT_VERSION {
            return Err(CodebookError::UnsupportedVersion(version));
        }
        let m = u32::from_le_bytes(
            bytes[8..12]
                .try_into()
                .expect("invariant: header length checked above guarantees these 4 bytes"),
        ) as usize;
        if m != M {
            return Err(CodebookError::ConfigMismatch {
                params_m: m,
                codebook_m: M,
            });
        }
        let sub_dim = u32::from_le_bytes(
            bytes[12..16]
                .try_into()
                .expect("invariant: header length checked above guarantees these 4 bytes"),
        ) as usize;
        let data_len = u32::from_le_bytes(
            bytes[16..20]
                .try_into()
                .expect("invariant: header length checked above guarantees these 4 bytes"),
        ) as usize;
        let expected_bytes = CODEBOOK_HEADER_LEN + data_len * 4;
        if bytes.len() != expected_bytes {
            return Err(CodebookError::Truncated);
        }
        let mut data = Vec::with_capacity(data_len);
        for i in 0..data_len {
            let off = CODEBOOK_HEADER_LEN + i * 4;
            let chunk: [u8; 4] = bytes[off..off + 4]
                .try_into()
                .expect("invariant: total length checked above guarantees this 4-byte chunk");
            data.push(f32::from_le_bytes(chunk));
        }
        Self::from_trained(data, sub_dim)
    }

    /// Confirm the codebook matches a [`PqParams`] config — `m` matches,
    /// `bits` matches (centroid count). Run at index-load time to fail
    /// loudly on a config/snapshot drift instead of silently producing
    /// garbage distances.
    pub fn matches_params(&self, params: &PqParams) -> Result<(), CodebookError> {
        if params.m != M {
            return Err(CodebookError::ConfigMismatch {
                params_m: params.m,
                codebook_m: M,
            });
        }
        if params.centroids_per_subspace() != PQ_CENTROIDS_PER_SUBSPACE {
            return Err(CodebookError::ConfigCentroidsMismatch {
                params_k: params.centroids_per_subspace(),
                codebook_k: PQ_CENTROIDS_PER_SUBSPACE,
            });
        }
        Ok(())
    }
}

/// `b"BCB0"` — Brain Codebook v0 magic.
const CODEBOOK_MAGIC: [u8; 4] = *b"BCB0";
const CODEBOOK_FORMAT_VERSION: u32 = 1;
const CODEBOOK_HEADER_LEN: usize = 20;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CodebookError {
    #[error("M={m} × sub_dim={sub_dim} ≠ VECTOR_DIM={d}")]
    DimMismatch { m: usize, sub_dim: usize, d: usize },

    #[error("centroid buffer has {got} elements, expected {expected}")]
    CentroidsWrongSize { got: usize, expected: usize },

    #[error("centroid buffer contains NaN or infinity")]
    NonFinite,

    #[error("params.m={params_m} does not match codebook M={codebook_m}")]
    ConfigMismatch { params_m: usize, codebook_m: usize },

    #[error("params.K={params_k} does not match codebook K={codebook_k}")]
    ConfigCentroidsMismatch { params_k: usize, codebook_k: usize },

    #[error("codebook serialized bytes are truncated or malformed length")]
    Truncated,

    #[error("codebook bytes have wrong magic prefix")]
    BadMagic,

    #[error("codebook bytes have unsupported format version: {0}")]
    UnsupportedVersion(u32),
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUB_DIM_M8: usize = VECTOR_DIM / 8;

    fn fake_centroids<const M: usize>() -> Vec<f32> {
        let len = M * PQ_CENTROIDS_PER_SUBSPACE * (VECTOR_DIM / M);
        (0..len).map(|i| i as f32 * 1e-3).collect()
    }

    #[test]
    fn from_trained_accepts_well_shaped_input() {
        let data = fake_centroids::<8>();
        let cb = Codebook::<8>::from_trained(data, SUB_DIM_M8).unwrap();
        assert_eq!(cb.sub_dim(), SUB_DIM_M8);
        assert_eq!(cb.m(), 8);
        assert_eq!(cb.k(), 256);
    }

    #[test]
    fn from_trained_rejects_dim_mismatch() {
        let data = vec![0.0_f32; 8 * 256 * 10]; // sub_dim=10 doesn't fit 384/8
        let err = Codebook::<8>::from_trained(data, 10).unwrap_err();
        assert!(matches!(err, CodebookError::DimMismatch { .. }));
    }

    #[test]
    fn from_trained_rejects_wrong_length() {
        let mut data = fake_centroids::<8>();
        data.pop(); // off by one
        let err = Codebook::<8>::from_trained(data, SUB_DIM_M8).unwrap_err();
        assert!(matches!(err, CodebookError::CentroidsWrongSize { .. }));
    }

    #[test]
    fn from_trained_rejects_nan() {
        let mut data = fake_centroids::<8>();
        data[42] = f32::NAN;
        let err = Codebook::<8>::from_trained(data, SUB_DIM_M8).unwrap_err();
        assert_eq!(err, CodebookError::NonFinite);
    }

    #[test]
    fn centroid_indexing_lines_up_with_subspace_view() {
        let data = fake_centroids::<8>();
        let cb = Codebook::<8>::from_trained(data, SUB_DIM_M8).unwrap();
        for s in 0..8 {
            let subspace_slice = cb.subspace(s);
            for k in 0..PQ_CENTROIDS_PER_SUBSPACE {
                let direct = cb.centroid(s, k);
                let start = k * SUB_DIM_M8;
                assert_eq!(direct, &subspace_slice[start..start + SUB_DIM_M8]);
            }
        }
    }

    #[test]
    fn matches_params_accepts_default_config() {
        let cb = Codebook::<8>::from_trained(fake_centroids::<8>(), SUB_DIM_M8).unwrap();
        cb.matches_params(&PqParams::default_v1()).unwrap();
    }

    #[test]
    fn matches_params_rejects_m_drift() {
        let cb = Codebook::<8>::from_trained(fake_centroids::<8>(), SUB_DIM_M8).unwrap();
        let mut params = PqParams::default_v1();
        params.m = 16;
        assert!(matches!(
            cb.matches_params(&params),
            Err(CodebookError::ConfigMismatch {
                params_m: 16,
                codebook_m: 8
            })
        ));
    }

    #[test]
    fn serialize_round_trip_is_bit_exact() {
        let cb = Codebook::<8>::from_trained(fake_centroids::<8>(), SUB_DIM_M8).unwrap();
        let bytes = cb.serialize();
        let cb2 = Codebook::<8>::deserialize(&bytes).unwrap();
        assert_eq!(cb2.sub_dim(), cb.sub_dim());
        assert_eq!(cb2.m(), cb.m());
        // Bit-exact centroid round-trip via the little-endian f32 image.
        let a = cb.as_flat();
        let b = cb2.as_flat();
        assert_eq!(a.len(), b.len());
        for i in 0..a.len() {
            assert_eq!(a[i].to_bits(), b[i].to_bits(), "centroid {i} drifted");
        }
    }

    #[test]
    fn deserialize_rejects_wrong_magic() {
        let cb = Codebook::<8>::from_trained(fake_centroids::<8>(), SUB_DIM_M8).unwrap();
        let mut bytes = cb.serialize();
        bytes[0] = b'X';
        assert!(matches!(
            Codebook::<8>::deserialize(&bytes),
            Err(CodebookError::BadMagic)
        ));
    }

    #[test]
    fn deserialize_rejects_truncation() {
        let cb = Codebook::<8>::from_trained(fake_centroids::<8>(), SUB_DIM_M8).unwrap();
        let mut bytes = cb.serialize();
        bytes.truncate(bytes.len() - 4);
        assert!(matches!(
            Codebook::<8>::deserialize(&bytes),
            Err(CodebookError::Truncated)
        ));
    }
}
