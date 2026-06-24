//! Codebook of role + filler vectors and cleanup-by-nearest.
//!
//! HRR retrieval is lossy: `unbind` produces a noisy approximation of
//! the original filler, and we need an anchor vocabulary to snap that
//! noise back to a canonical label. The `Codebook` is that vocabulary.

use std::collections::HashMap;

use super::errors::VsaError;
use super::ops::{cosine, random_vec, VsaVec};

/// Names get hashed into 64-bit seeds so the same label always produces
/// the same vector across processes. We mix the codebook seed in so
/// two codebooks with different seeds give different vocabularies.
fn seed_for(codebook_seed: u64, name: &str) -> u64 {
    // FNV-1a 64-bit + SplitMix64-style mixing with the codebook seed.
    let mut h: u64 = 0xCBF2_9CE4_8422_2325;
    for byte in name.as_bytes() {
        h ^= u64::from(*byte);
        h = h.wrapping_mul(0x100_0000_01B3);
    }
    let mut mixed = h ^ codebook_seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    mixed ^ (mixed >> 31)
}

/// A reproducible vocabulary of role + filler vectors plus nearest-
/// neighbor cleanup. Roles and fillers are kept in separate maps so
/// callers can iterate "all known fillers" for cleanup without
/// matching against role vectors.
pub struct Codebook {
    roles: HashMap<String, VsaVec>,
    fillers: HashMap<String, VsaVec>,
    seed: u64,
}

impl Codebook {
    /// Create an empty codebook. Roles/fillers are populated lazily by
    /// `get_or_create_role` / `get_or_create_filler` so callers don't
    /// need to enumerate the vocabulary up front.
    pub fn new(seed: u64) -> Self {
        Self {
            roles: HashMap::new(),
            fillers: HashMap::new(),
            seed,
        }
    }

    /// Returns the role vector for `name`, generating it on first
    /// access. Same `(seed, name)` always yields the same vector.
    pub fn get_or_create_role(&mut self, name: &str) -> &VsaVec {
        if !self.roles.contains_key(name) {
            let s = seed_for(self.seed ^ 0xA5A5_A5A5_A5A5_A5A5, name);
            self.roles.insert(name.to_string(), random_vec(s));
        }
        &self.roles[name]
    }

    /// Returns the filler vector for `name`, generating it on first
    /// access. Fillers are namespaced separately from roles so a name
    /// reused in both buckets gets distinct vectors.
    pub fn get_or_create_filler(&mut self, name: &str) -> &VsaVec {
        if !self.fillers.contains_key(name) {
            let s = seed_for(self.seed ^ 0x5A5A_5A5A_5A5A_5A5A, name);
            self.fillers.insert(name.to_string(), random_vec(s));
        }
        &self.fillers[name]
    }

    /// Convenience: returns a snapshot of every filler currently in the
    /// vocabulary. Used by `cleanup` and is also the right shape for
    /// tests that want to assert "the codebook knows X".
    pub fn fillers(&self) -> &HashMap<String, VsaVec> {
        &self.fillers
    }

    /// Argmax cosine-similarity over the filler vocabulary. Returns
    /// `None` if the vocabulary is empty.
    pub fn cleanup(&self, noisy: &VsaVec) -> Option<(String, f32)> {
        self.cleanup_among(noisy, self.fillers.keys().map(String::as_str))
    }

    /// Cleanup restricted to a candidate subset of filler names — used
    /// when the caller knows the answer comes from (say) a list of
    /// company names rather than the entire vocabulary.
    pub fn cleanup_among<'a, I>(&self, noisy: &VsaVec, candidates: I) -> Option<(String, f32)>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut best: Option<(String, f32)> = None;
        for name in candidates {
            let Some(v) = self.fillers.get(name) else {
                continue;
            };
            let cos = cosine(noisy, v);
            match &best {
                Some((_, b)) if cos <= *b => {}
                _ => best = Some((name.to_string(), cos)),
            }
        }
        best
    }

    /// Eager population — fluent helper for tests and demos. Calling
    /// `with_fillers(["Alice", "Bob"])` reserves those vectors so
    /// `cleanup` can find them without a prior bind/encode pass.
    pub fn with_fillers<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for n in names {
            let _ = self.get_or_create_filler(n.as_ref());
        }
        self
    }

    /// Validates internal invariants (correct dim on every vector).
    /// Cheap; used by debug assertions in callers.
    pub fn validate(&self) -> Result<(), VsaError> {
        for (_, v) in self.roles.iter().chain(self.fillers.iter()) {
            if v.len() != super::ops::VSA_DIM {
                return Err(VsaError::DimensionMismatch {
                    expected: super::ops::VSA_DIM,
                    lhs_len: v.len(),
                    rhs_len: super::ops::VSA_DIM,
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_finds_exact_filler() {
        let mut cb = Codebook::new(99).with_fillers(["Alice", "Bob", "Carol"]);
        let target = cb.get_or_create_filler("Bob").clone();
        let (name, cos) = cb.cleanup(&target).expect("non-empty codebook");
        assert_eq!(name, "Bob");
        assert!(cos > 0.999, "cos={cos}");
    }
}
