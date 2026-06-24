//! Distance kernels for PQ codes.
//!
//! Two modes:
//!
//! - **Asymmetric Distance Computation (ADC).** Used at search time —
//!   the query is full-precision `f32`, the target is a PQ code.
//!   Higher recall than SDC because no quantisation noise on the query
//!   side. Pre-computation: one `M × K` lookup table per query; per-
//!   candidate cost is `M` table reads + `M` adds.
//!
//! - **Symmetric Distance Computation (SDC).** Used at HNSW
//!   construction time — both endpoints are PQ codes. Pre-computation:
//!   one `M × K × K` table per codebook; per-candidate cost is `M`
//!   double-indexed reads + `M` adds.
//!
//! The HNSW crate's [`Distance`] trait is uniform — both args are
//! `&[u8]`. [`PqDist`] dispatches between modes via a thread-local
//! [`SEARCH_LUT`] slot that the search wrapper installs before
//! invoking the HNSW traversal. With the slot set, [`PqDist::eval`]
//! treats the second argument as the candidate code and runs ADC.
//! With the slot empty, both arguments are PQ codes from the graph
//! and SDC is the right answer.

use std::cell::RefCell;
use std::sync::Arc;

use hnsw_rs::anndists::dist::distances::Distance;

use super::codebook::Codebook;
use super::params::PQ_CENTROIDS_PER_SUBSPACE;
use crate::params::VECTOR_DIM;

// ---------------------------------------------------------------------------
// Lookup tables
// ---------------------------------------------------------------------------

/// Query-conditioned lookup table for ADC. `data[s * K + k]` is the
/// squared distance from the query's `s`-th subspace chunk to the
/// `k`-th centroid of that subspace.
///
/// Built once per query at search time, lives only for the duration of
/// the HNSW traversal. ~`M * K * 4` bytes (8 KiB at v1 defaults — fits
/// in L1).
#[derive(Debug, Clone)]
pub struct Lut<const M: usize> {
    data: Box<[f32]>,
}

impl<const M: usize> Lut<M> {
    /// Build a LUT for `query` against `codebook`. Both must agree on
    /// `M`; checked at compile time.
    #[must_use]
    pub fn build(query: &[f32; VECTOR_DIM], codebook: &Codebook<M>) -> Self {
        let sub_dim = codebook.sub_dim();
        let mut data = vec![0.0_f32; M * PQ_CENTROIDS_PER_SUBSPACE].into_boxed_slice();
        for s in 0..M {
            let chunk = &query[s * sub_dim..(s + 1) * sub_dim];
            let subspace_centroids = codebook.subspace(s);
            let row = &mut data[s * PQ_CENTROIDS_PER_SUBSPACE..(s + 1) * PQ_CENTROIDS_PER_SUBSPACE];
            for k in 0..PQ_CENTROIDS_PER_SUBSPACE {
                let centroid = &subspace_centroids[k * sub_dim..(k + 1) * sub_dim];
                row[k] = squared_distance(chunk, centroid);
            }
        }
        Self { data }
    }

    /// Read-only view of the flat table. Used by [`adc`].
    #[inline]
    #[must_use]
    pub fn as_flat(&self) -> &[f32] {
        &self.data
    }
}

/// Symmetric distance table for SDC. `data[s * K * K + i * K + j]` is
/// the squared distance between centroid `i` and centroid `j` of
/// subspace `s`.
///
/// Built once per codebook at index activation, lives for the lifetime
/// of the HNSW epoch. ~`M * K² * 4` bytes (2 MiB at v1 defaults — fits
/// in L2 on every target CPU).
#[derive(Debug, Clone)]
pub struct SdcTable<const M: usize> {
    data: Box<[f32]>,
}

impl<const M: usize> SdcTable<M> {
    /// Pre-compute the table from `codebook`. Symmetric and
    /// zero-diagonal by construction; we only fill the upper triangle
    /// and mirror, halving the centroid-pair work.
    #[must_use]
    pub fn build(codebook: &Codebook<M>) -> Self {
        let k = PQ_CENTROIDS_PER_SUBSPACE;
        let sub_dim = codebook.sub_dim();
        let mut data = vec![0.0_f32; M * k * k].into_boxed_slice();
        for s in 0..M {
            let subspace_centroids = codebook.subspace(s);
            let base = s * k * k;
            for i in 0..k {
                let centroid_i = &subspace_centroids[i * sub_dim..(i + 1) * sub_dim];
                for j in i + 1..k {
                    let centroid_j = &subspace_centroids[j * sub_dim..(j + 1) * sub_dim];
                    let d2 = squared_distance(centroid_i, centroid_j);
                    data[base + i * k + j] = d2;
                    data[base + j * k + i] = d2;
                }
            }
        }
        Self { data }
    }

    /// Read-only view of the flat table. Used by [`sdc`].
    #[inline]
    #[must_use]
    pub fn as_flat(&self) -> &[f32] {
        &self.data
    }
}

// ---------------------------------------------------------------------------
// Per-candidate distance kernels
// ---------------------------------------------------------------------------

/// Asymmetric distance: sum of LUT lookups, one per subspace, using
/// the candidate code as the centroid index.
///
/// `code.len()` must equal `M` — the compile-time array shape enforces
/// it.
#[inline]
#[must_use]
pub fn adc<const M: usize>(lut: &Lut<M>, code: &[u8; M]) -> f32 {
    let data = lut.as_flat();
    let mut sum = 0.0_f32;
    for s in 0..M {
        sum += data[s * PQ_CENTROIDS_PER_SUBSPACE + code[s] as usize];
    }
    sum
}

/// Symmetric distance: sum of SDC table lookups, one per subspace.
/// Both arguments are PQ codes.
#[inline]
#[must_use]
pub fn sdc<const M: usize>(table: &SdcTable<M>, code_a: &[u8; M], code_b: &[u8; M]) -> f32 {
    let data = table.as_flat();
    let k = PQ_CENTROIDS_PER_SUBSPACE;
    let mut sum = 0.0_f32;
    for s in 0..M {
        let i = code_a[s] as usize;
        let j = code_b[s] as usize;
        sum += data[s * k * k + i * k + j];
    }
    sum
}

// ---------------------------------------------------------------------------
// PqDist — the hnsw_rs Distance impl
// ---------------------------------------------------------------------------

thread_local! {
    /// Slot for the per-search [`Lut`]. The search wrapper installs an
    /// `Arc<Lut<M>>` here for the duration of `Hnsw::search` and
    /// clears it on exit. With the slot set, [`PqDist::eval`]
    /// interprets `(va, vb)` as `(query_code, candidate_code)` and runs
    /// ADC; with the slot empty, both are graph-resident codes and SDC
    /// is the right answer.
    ///
    /// A type-erased `Arc<dyn Any>` lets one thread-local serve every
    /// `M`. Recoverable into the concrete `Arc<Lut<M>>` via downcasting.
    static SEARCH_LUT: RefCell<Option<Arc<dyn std::any::Any + Send + Sync>>> =
        const { RefCell::new(None) };
}

/// Install a LUT in the thread-local for the duration of a HNSW
/// search. Returns a guard that clears the slot on drop, so RAII
/// covers early-return / panic paths inside the search.
///
/// Nested guards (rare — search inside search) restore the outer
/// LUT on drop, so the outer search still uses its own LUT.
#[must_use]
pub fn install_search_lut<const M: usize>(lut: Arc<Lut<M>>) -> LutGuard {
    let previous = SEARCH_LUT.with(|cell| {
        let prev = cell.borrow_mut().take();
        *cell.borrow_mut() = Some(lut);
        prev
    });
    LutGuard { previous }
}

/// Guard returned by [`install_search_lut`]. Drop restores the slot to
/// whatever it held before the install.
pub struct LutGuard {
    previous: Option<Arc<dyn std::any::Any + Send + Sync>>,
}

impl Drop for LutGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        SEARCH_LUT.with(|cell| {
            *cell.borrow_mut() = previous;
        });
    }
}

/// `hnsw_rs::Distance<u8>` impl that dispatches between ADC (when a
/// search LUT is installed) and SDC (otherwise).
///
/// Always cheap to clone — the SDC table lives behind an `Arc`.
#[derive(Debug, Clone)]
pub struct PqDist<const M: usize> {
    sdc_table: Arc<SdcTable<M>>,
}

impl<const M: usize> PqDist<M> {
    /// Build a `PqDist` from a trained codebook. Caches the SDC table
    /// inside an `Arc` so cloning is reference-bumping.
    #[must_use]
    pub fn new(codebook: &Codebook<M>) -> Self {
        Self {
            sdc_table: Arc::new(SdcTable::build(codebook)),
        }
    }

    /// Borrow the cached SDC table. Useful for tests that want to
    /// validate the table directly.
    #[must_use]
    pub fn sdc_table(&self) -> &SdcTable<M> {
        &self.sdc_table
    }
}

impl<const M: usize> Distance<u8> for PqDist<M> {
    fn eval(&self, va: &[u8], vb: &[u8]) -> f32 {
        debug_assert_eq!(va.len(), M, "PQ code length must equal M");
        debug_assert_eq!(vb.len(), M, "PQ code length must equal M");
        // SAFETY: debug_assert above; production callers guarantee.
        let code_a: &[u8; M] = va
            .try_into()
            .expect("invariant: PQ codes are always M bytes (asserted above)");
        let code_b: &[u8; M] = vb
            .try_into()
            .expect("invariant: PQ codes are always M bytes (asserted above)");

        // ADC if a search LUT is installed (hnsw_rs calls
        // eval(query, candidate)).
        let adc_distance = SEARCH_LUT.with(|cell| {
            cell.borrow().as_ref().and_then(|any_arc| {
                any_arc
                    .clone()
                    .downcast::<Lut<M>>()
                    .ok()
                    .map(|lut| adc::<M>(&lut, code_b))
            })
        });
        if let Some(d) = adc_distance {
            return d;
        }

        // No LUT installed → graph construction call.
        sdc::<M>(&self.sdc_table, code_a, code_b)
    }
}

// ---------------------------------------------------------------------------
// Shared helper
// ---------------------------------------------------------------------------

/// Sum of squared component-wise differences. Mirrors the per-module
/// helpers in `kmeans.rs` and `encode.rs` — kept private so each
/// module can specialise (e.g., add SIMD) independently.
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
    use crate::pq::encode::encode;

    /// Build a codebook whose subspace-`s` centroid `k` is
    /// `[k, 0, 0, ...]` in subspace `s`. This makes hand-checking
    /// distances trivial: `||centroid_i - centroid_j||² = (i - j)²` per
    /// subspace.
    fn arithmetic_codebook<const M: usize>() -> Codebook<M> {
        let sub_dim = VECTOR_DIM / M;
        let mut centroids = vec![0.0_f32; M * PQ_CENTROIDS_PER_SUBSPACE * sub_dim];
        for s in 0..M {
            for k in 0..PQ_CENTROIDS_PER_SUBSPACE {
                let offset = (s * PQ_CENTROIDS_PER_SUBSPACE + k) * sub_dim;
                centroids[offset] = k as f32;
            }
        }
        Codebook::<M>::from_trained(centroids, sub_dim).unwrap()
    }

    #[test]
    fn lut_first_centroid_distance_matches_hand_calc() {
        // Query: all zeros. Subspace-0 LUT[k] should equal k² (the
        // centroid `[k, 0, 0, ...]` is k units away from origin in the
        // first component).
        let cb = arithmetic_codebook::<8>();
        let q = [0.0_f32; VECTOR_DIM];
        let lut = Lut::<8>::build(&q, &cb);
        for k in 0..PQ_CENTROIDS_PER_SUBSPACE {
            let expected = (k as f32) * (k as f32);
            let actual = lut.as_flat()[k];
            assert!(
                (expected - actual).abs() < 1e-3,
                "subspace 0 centroid {k}: expected {expected}, got {actual}"
            );
        }
    }

    #[test]
    fn sdc_table_zero_diagonal_and_symmetric() {
        let cb = arithmetic_codebook::<8>();
        let table = SdcTable::<8>::build(&cb);
        let k = PQ_CENTROIDS_PER_SUBSPACE;
        for s in 0..8 {
            let base = s * k * k;
            for i in 0..k {
                assert_eq!(
                    table.as_flat()[base + i * k + i],
                    0.0,
                    "diagonal non-zero at s={s}, i={i}"
                );
                for j in 0..k {
                    let upper = table.as_flat()[base + i * k + j];
                    let lower = table.as_flat()[base + j * k + i];
                    assert_eq!(upper, lower, "asymmetric at s={s}, ({i},{j})");
                }
            }
        }
    }

    #[test]
    fn sdc_distance_zero_for_identical_codes() {
        let cb = arithmetic_codebook::<8>();
        let table = SdcTable::<8>::build(&cb);
        let code = [17u8; 8];
        assert_eq!(sdc::<8>(&table, &code, &code), 0.0);
    }

    #[test]
    fn sdc_distance_matches_hand_calc() {
        // Codes [3, 3, 3, ...] vs [5, 5, 5, ...]: per subspace, the
        // squared distance is (5 - 3)² = 4. Eight subspaces → total 32.
        let cb = arithmetic_codebook::<8>();
        let table = SdcTable::<8>::build(&cb);
        let a = [3u8; 8];
        let b = [5u8; 8];
        let d = sdc::<8>(&table, &a, &b);
        assert!((d - 32.0).abs() < 1e-3, "expected 32, got {d}");
    }

    #[test]
    fn adc_distance_matches_hand_calc() {
        // Query = all zeros. Candidate code = [7, 7, ..., 7]. Per
        // subspace, distance = ||0 - [7,0,0,...]||² = 49. Eight
        // subspaces → 392.
        let cb = arithmetic_codebook::<8>();
        let q = [0.0_f32; VECTOR_DIM];
        let lut = Lut::<8>::build(&q, &cb);
        let code = [7u8; 8];
        let d = adc::<8>(&lut, &code);
        assert!((d - 392.0).abs() < 1e-3, "expected 392, got {d}");
    }

    #[test]
    fn pqdist_uses_sdc_when_no_lut_installed() {
        let cb = arithmetic_codebook::<8>();
        let dist = PqDist::<8>::new(&cb);
        let a = [3u8; 8];
        let b = [5u8; 8];
        let d = dist.eval(&a, &b);
        assert!((d - 32.0).abs() < 1e-3, "expected SDC=32, got {d}");
    }

    #[test]
    fn pqdist_uses_adc_when_lut_installed() {
        let cb = arithmetic_codebook::<8>();
        let dist = PqDist::<8>::new(&cb);
        let q = [0.0_f32; VECTOR_DIM];
        let lut = Arc::new(Lut::<8>::build(&q, &cb));

        let _guard = install_search_lut::<8>(lut);
        let candidate = [7u8; 8];
        // First arg is "query side" — its value does not affect the
        // returned ADC distance (which only reads the LUT).
        let d = dist.eval(&[0u8; 8], &candidate);
        assert!((d - 392.0).abs() < 1e-3, "expected ADC=392, got {d}");
    }

    #[test]
    fn install_search_lut_guard_clears_on_drop() {
        let cb = arithmetic_codebook::<8>();
        let dist = PqDist::<8>::new(&cb);
        let q = [0.0_f32; VECTOR_DIM];
        let lut = Arc::new(Lut::<8>::build(&q, &cb));

        {
            let _guard = install_search_lut::<8>(lut);
            // Inside scope: ADC.
            let d = dist.eval(&[0u8; 8], &[7u8; 8]);
            assert!((d - 392.0).abs() < 1e-3);
        }
        // After scope: SDC again. With a=[0;8], b=[7;8],
        // SDC = sum over subspaces of (7-0)² = 49 × 8 = 392.
        // Pick codes whose SDC differs from ADC to verify the dispatch.
        let d = dist.eval(&[3u8; 8], &[5u8; 8]);
        assert!(
            (d - 32.0).abs() < 1e-3,
            "after guard drop, expected SDC=32, got {d}"
        );
    }

    /// Real-codebook smoke test: train against a synthetic sample,
    /// encode a known point, then assert ADC against the encoded code
    /// is at most the recoverable lower bound (very close to zero for
    /// a point that lies on a centroid).
    #[test]
    #[ignore = "heavy PQ k-means training (v1.x feature, not wired into the v1 live path); slow in debug builds. Run with --run-ignored."]
    fn adc_against_encoded_point_is_near_zero() {
        use crate::pq::params::MIN_TRAINING_SAMPLE;
        use crate::pq::{kmeans, PqParams};

        // 16 well-separated clusters in subspace 0; pad to minimum
        // sample size.
        let mut sample: Vec<[f32; VECTOR_DIM]> = Vec::with_capacity(MIN_TRAINING_SAMPLE);
        for c in 0..16 {
            for _ in 0..300 {
                let mut v = [0.0_f32; VECTOR_DIM];
                v[0] = c as f32 * 100.0;
                sample.push(v);
            }
        }
        while sample.len() < MIN_TRAINING_SAMPLE {
            sample.push([0.0_f32; VECTOR_DIM]);
        }
        let cb = kmeans::train::<8>(&sample, &PqParams::default_v1(), 0xCAFE).unwrap();

        // Pick a sample point and encode it. ADC(query=point) should
        // be very small (the point lies on or near a centroid in
        // subspace 0 and at the zero centroid elsewhere).
        let point = sample[5 * 300]; // cluster 5
        let code = encode(&point, &cb).unwrap();
        let lut = Lut::<8>::build(&point, &cb);
        let d = adc::<8>(&lut, &code);
        // Encoded centroid for a real point is the nearest centroid;
        // distance is bounded by intra-cluster spread. The synthetic
        // sample is exact-on-centroid → distance should be effectively
        // zero in subspace 0 plus zero elsewhere.
        assert!(d < 1.0, "ADC against encoded point should be tiny, got {d}");
    }
}
