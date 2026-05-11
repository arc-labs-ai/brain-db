//! HNSW parameters with Brain's spec defaults.
//!
//! See `spec/06_ann_index/02_parameters.md` (M=16, ef_construction=200,
//! ef_search=64, ef_search_max=500).

use thiserror::Error;

/// The vector dimension used throughout v1: BGE-small-en-v1.5 produces
/// 384-dim L2-normalised vectors. Spec `§04/03 §1`.
pub const VECTOR_DIM: usize = 384;

/// Maximum graph layer count passed to `hnsw_rs::Hnsw::new`. Spec doesn't
/// pin this directly; HNSW theory says `max_layer ≥ log_M(N)`. At M=16
/// and N=10M (the spec's per-shard ceiling), log_16(10M) ≈ 5.6 — `16` is
/// a comfortable upper bound recommended by the original HNSW paper and
/// matched by hnsw_rs's own defaults.
pub const MAX_LAYER: usize = 16;

/// Initial `max_elements` hint to pass to `hnsw_rs::Hnsw::new`. The crate
/// uses this only to pre-size internal tables; it does not cap insert
/// count, so undersizing is a perf hint rather than a correctness limit.
/// `1024` keeps the small-test footprint tiny; production callers
/// override via [`IndexParams::with_capacity_hint`] (added in 4.6 when
/// rebuild needs it).
pub const DEFAULT_CAPACITY_HINT: usize = 1024;

/// HNSW knobs.
///
/// Defaults from [`Self::default_v1`] match `spec/06_ann_index/02_parameters.md`:
/// `M=16, ef_construction=200, ef_search=64, ef_search_max=500`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexParams {
    /// Max edges per non-bottom-layer node. Spec `§02 §1` range: 4..=64.
    pub m: usize,
    /// Search width during insertion. Spec `§02 §1` range: 50..=500.
    pub ef_construction: usize,
    /// Default search width per query. Spec `§02 §1` range: 10..=500.
    /// Per-query overrides are clamped to `[k, ef_search_max]`.
    pub ef_search: usize,
    /// Cap on per-query `ef_search` overrides. Spec `§02 §8` config key.
    pub ef_search_max: usize,
}

impl IndexParams {
    /// Brain's v1 defaults per `spec/06_ann_index/02_parameters.md`.
    #[must_use]
    pub const fn default_v1() -> Self {
        Self {
            m: 16,
            ef_construction: 200,
            ef_search: 64,
            ef_search_max: 500,
        }
    }

    /// Validate fields lie in the spec's ranges. The validation runs once
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
