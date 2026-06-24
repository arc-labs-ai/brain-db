//! Smoke-test analogy demo for the HRR algebra.
//!
//! Encodes triples `(subject, predicate, object)` into a single bound
//! vector and shows that `unbind(triple, role)` followed by codebook
//! cleanup recovers the filler bound to that role.

use super::codebook::Codebook;
use super::errors::VsaError;
use super::ops::{bind, bundle, unbind, VsaVec};

/// Canonical role names. Kept as constants so producers and consumers
/// of triples don't drift on a typo (e.g. "subjet" vs "subject").
pub const ROLE_SUBJECT: &str = "subject";
pub const ROLE_PREDICATE: &str = "predicate";
pub const ROLE_OBJECT: &str = "object";

/// Encode a `(subject, predicate, object)` triple into a single HRR
/// vector. The result is bundle(role⊛subject, role⊛predicate,
/// role⊛object) — all three pieces are recoverable via `unbind` over
/// the matching role.
pub fn encode_triple(
    codebook: &mut Codebook,
    subject: &str,
    predicate: &str,
    object: &str,
) -> Result<VsaVec, VsaError> {
    // Clone the role/filler vectors out of the codebook so we don't
    // hold immutable borrows across the second `get_or_create_*` call.
    let r_subj = codebook.get_or_create_role(ROLE_SUBJECT).clone();
    let f_subj = codebook.get_or_create_filler(subject).clone();
    let r_pred = codebook.get_or_create_role(ROLE_PREDICATE).clone();
    let f_pred = codebook.get_or_create_filler(predicate).clone();
    let r_obj = codebook.get_or_create_role(ROLE_OBJECT).clone();
    let f_obj = codebook.get_or_create_filler(object).clone();

    let s = bind(&r_subj, &f_subj)?;
    let p = bind(&r_pred, &f_pred)?;
    let o = bind(&r_obj, &f_obj)?;
    bundle(&[&s, &p, &o])
}

/// Unbind a role from a triple and snap the noisy result to the
/// nearest filler in the codebook. Returns `(name, cosine)` or `None`
/// if the codebook has no fillers.
pub fn query_role(
    codebook: &mut Codebook,
    triple: &VsaVec,
    role: &str,
) -> Result<Option<(String, f32)>, VsaError> {
    let r = codebook.get_or_create_role(role).clone();
    let noisy = unbind(triple, &r)?;
    Ok(codebook.cleanup(&noisy))
}

/// "A is to B as C is to ?" — solves the analogy by reading the filler
/// bound to `role_to_extract` out of `triple_c`.
///
/// `triple_a` is included so callers can chain it with `triple_c` for
/// more elaborate analogy forms in future versions. Today the answer
/// is determined purely by `triple_c` and `role_to_extract`; we keep
/// the slot to lock the public signature for the v1.1 wiring.
pub fn analogy_query(
    codebook: &mut Codebook,
    _triple_a: &VsaVec,
    triple_c: &VsaVec,
    role_to_extract: &str,
) -> Result<Option<(String, f32)>, VsaError> {
    query_role(codebook, triple_c, role_to_extract)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analogy_alice_works_at_acme_then_bob_works_at_what() {
        // Triples share the predicate "works_at"; "?" is recovered by
        // unbinding `object` from triple_2 and snapping to the
        // codebook. Acme/Stripe both live in the vocabulary.
        let mut cb = Codebook::new(42).with_fillers([
            "Alice", "Bob", "Carol", "Acme", "Stripe", "Globex", "works_at", "lives_in",
        ]);

        let triple_1 = encode_triple(&mut cb, "Alice", "works_at", "Acme").unwrap();
        let triple_2 = encode_triple(&mut cb, "Bob", "works_at", "Stripe").unwrap();

        let (name, cos) = analogy_query(&mut cb, &triple_1, &triple_2, ROLE_OBJECT)
            .unwrap()
            .expect("non-empty codebook");
        assert_eq!(name, "Stripe", "recovered={name} cos={cos}");
        assert!(cos > 0.4, "cos={cos}");
    }
}
