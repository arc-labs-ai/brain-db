//! HNSW parameters with Brain's defaults.
//!
//! These are the HNSW knobs; the shipped defaults are
//! `M=16, ef_construction=200, ef_search=64, ef_search_max=500`. Indexes run
//! at full precision — product quantization lives in [`crate::pq`] and is not
//! driven from these parameters.

use thiserror::Error;

/// The vector dimension used throughout v1: BGE-small-en-v1.5 produces
/// 384-dim L2-normalised vectors.
pub const VECTOR_DIM: usize = 384;

/// Maximum graph layer count passed to `hnsw_rs::Hnsw::new`. HNSW theory
/// says `max_layer ≥ log_M(N)`. At M=16 and N=10M (the per-shard
/// ceiling), log_16(10M) ≈ 5.6 — `16` is a comfortable upper bound
/// recommended by the original HNSW paper and matched by hnsw_rs's own
/// defaults.
pub const MAX_LAYER: usize = 16;

/// Initial `max_elements` hint to pass to `hnsw_rs::Hnsw::new`. The crate
/// uses this only to pre-size internal tables; it does not cap insert
/// count, so undersizing is a perf hint rather than a correctness limit.
/// `1024` keeps the small-test footprint tiny; production callers
/// override via a `with_capacity_hint` builder.
pub const DEFAULT_CAPACITY_HINT: usize = 1024;

/// HNSW knobs.
///
/// Defaults from [`Self::default_v1`]:
/// `M=16, ef_construction=200, ef_search=64, ef_search_max=500`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexParams {
    /// Max edges per non-bottom-layer node. Range: 4..=64.
    pub m: usize,
    /// Search width during insertion. Range: 50..=500.
    pub ef_construction: usize,
    /// Default search width per query. Range: 10..=500.
    /// Per-query overrides are clamped to `[k, ef_search_max]`.
    pub ef_search: usize,
    /// Cap on per-query `ef_search` overrides.
    pub ef_search_max: usize,
    /// Multiplier used to derive `ef` from the requested `k` when no per-query
    /// `ef` is given. Range: 1..=16.
    ///
    /// HNSW recall depends on the beam being meaningfully WIDER than the
    /// result set. At Brain's shipped defaults `ef_search` and `DEFAULT_TOP_K`
    /// are both 64, so `ef == k` exactly — the degenerate configuration where
    /// the search barely explores past the results it is about to return.
    /// Weaviate derives `ef = k * 8` for the same query, clamped to the same
    /// 500 ceiling we already use.
    ///
    /// **The default is 1, so `ef` is derived as `k * 1`: the beam equals the
    /// requested `k` and incurs no extra read cost.** It is a knob first and a
    /// default change second: the factor raises read cost in proportion to `k`,
    /// so the recall it buys must be measured before the default moves. That
    /// measurement needs a retrieval-only recall evaluation, which does not
    /// exist yet.
    ///
    /// # Most of the accepted range does nothing at the shipped defaults
    ///
    /// The derived beam is clamped to [`EF_SEARCH_MAX`][crate::semantic_retriever::EF_SEARCH_MAX]
    /// (500), so the factor stops having any effect once `k * ef_factor`
    /// crosses it. At the shipped `DEFAULT_TOP_K` of 64 that happens at **8**:
    ///
    /// ```text
    ///   k=64, ef_factor=7  ->  448   distinct
    ///   k=64, ef_factor=8  ->  512 -> clamped to 500
    ///   k=64, ef_factor=16 ->       clamped to 500   (identical to 8)
    /// ```
    ///
    /// [`validate`][Self::validate] accepts `1..=16` because the useful range
    /// depends on `k`, which is a per-query value it cannot see — at `k=10`
    /// every factor up to 16 is still distinct. But at the default `k` that
    /// means **nine of the sixteen accepted values are the same setting**, and
    /// raising the factor from 8 to 16 reads as a tuning change while
    /// changing nothing.
    ///
    /// Written down rather than narrowed: a smaller validated range would be
    /// wrong for small-`k` callers, and silently flattening half the range is
    /// the part worth knowing.
    pub ef_factor: usize,
}

impl IndexParams {
    /// Brain's v1 defaults.
    #[must_use]
    pub const fn default_v1() -> Self {
        Self {
            m: 16,
            ef_construction: 200,
            ef_search: 64,
            ef_search_max: 500,
            // 1 = reproduce prior behaviour. See the field's own note.
            ef_factor: 1,
        }
    }

    /// Validate fields lie in the accepted ranges. The validation runs once
    /// at [`HnswIndex::new`][crate::hnsw::HnswIndex::new]; downstream code
    /// can rely on the invariants.
    pub fn validate(&self) -> Result<(), IndexParamsError> {
        if !(4..=64).contains(&self.m) {
            return Err(IndexParamsError::MOutOfRange(self.m));
        }
        if !(50..=500).contains(&self.ef_construction) {
            return Err(IndexParamsError::EfConstructionOutOfRange(
                self.ef_construction,
            ));
        }
        if !(10..=500).contains(&self.ef_search) {
            return Err(IndexParamsError::EfSearchOutOfRange(self.ef_search));
        }
        if !(1..=16).contains(&self.ef_factor) {
            return Err(IndexParamsError::EfFactorOutOfRange(self.ef_factor));
        }
        if self.ef_search_max < self.ef_search {
            return Err(IndexParamsError::EfSearchMaxBelowDefault {
                ef_search: self.ef_search,
                ef_search_max: self.ef_search_max,
            });
        }
        Ok(())
    }
}

impl Default for IndexParams {
    fn default() -> Self {
        Self::default_v1()
    }
}

/// Failure to validate [`IndexParams`].
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum IndexParamsError {
    #[error("ef_factor={0} is outside the range 1..=16")]
    EfFactorOutOfRange(usize),
    #[error("M={0} is outside the spec range 4..=64")]
    MOutOfRange(usize),

    #[error("ef_construction={0} is outside the spec range 50..=500")]
    EfConstructionOutOfRange(usize),

    #[error("ef_search={0} is outside the spec range 10..=500")]
    EfSearchOutOfRange(usize),

    #[error("ef_search_max={ef_search_max} is below ef_search={ef_search}")]
    EfSearchMaxBelowDefault {
        ef_search: usize,
        ef_search_max: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_v1_matches_spec() {
        let p = IndexParams::default_v1();
        assert_eq!(p.m, 16);
        assert_eq!(p.ef_construction, 200);
        assert_eq!(p.ef_search, 64);
        assert_eq!(p.ef_search_max, 500);
    }

    #[test]
    fn validate_accepts_spec_defaults() {
        IndexParams::default_v1().validate().unwrap();
    }

    #[test]
    fn validate_rejects_out_of_range() {
        // M too small.
        let mut p = IndexParams::default_v1();
        p.m = 3;
        assert!(matches!(
            p.validate(),
            Err(IndexParamsError::MOutOfRange(3))
        ));

        // M too large.
        p = IndexParams::default_v1();
        p.m = 65;
        assert!(matches!(
            p.validate(),
            Err(IndexParamsError::MOutOfRange(65))
        ));

        // ef_construction below range.
        p = IndexParams::default_v1();
        p.ef_construction = 49;
        assert!(matches!(
            p.validate(),
            Err(IndexParamsError::EfConstructionOutOfRange(49))
        ));

        // ef_search above range.
        p = IndexParams::default_v1();
        p.ef_search = 501;
        assert!(matches!(
            p.validate(),
            Err(IndexParamsError::EfSearchOutOfRange(501))
        ));
    }

    #[test]
    fn validate_rejects_ef_search_max_below_ef_search() {
        let p = IndexParams {
            m: 16,
            ef_construction: 200,
            ef_search: 100,
            ef_search_max: 50, // < ef_search
            ef_factor: 1,
        };
        assert!(matches!(
            p.validate(),
            Err(IndexParamsError::EfSearchMaxBelowDefault {
                ef_search: 100,
                ef_search_max: 50,
            })
        ));
    }
}
