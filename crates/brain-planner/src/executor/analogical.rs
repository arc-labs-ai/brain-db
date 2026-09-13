//! Bridges REASON evidence items to the typed-graph statement they
//! were extracted into, and computes a bounded VSA structural-fit
//! nudge from the resulting `(subject, predicate, object)` triples.
//!
//! No new extraction logic and no new index: extraction is always-on
//! (C0), so most non-trivial evidence memories already have a
//! statement row pointing back at them via `STATEMENTS_BY_EVIDENCE_TABLE`
//! — the exact reverse lookup RECALL's `include_graph` enrichment
//! already uses to bridge a memory id to its sourced statements
//! (`brain-ops::handlers::recall::fetch_enrichment_for`). This module
//! reuses that lookup pattern rather than inventing a new one.

use brain_core::{MemoryId, Statement, StatementId, StatementObject, StatementValue, SubjectRef};
use brain_metadata::entity::ops::entity_get;
use brain_metadata::schema::predicate::predicate_get;
use brain_metadata::statement::statement_get;
use brain_metadata::tables::statement::STATEMENTS_BY_EVIDENCE_TABLE;
use brain_metadata::RowScope;

use crate::vsa::{analogy_query, encode_triple, Codebook, ROLE_OBJECT};

/// Deterministic seed for the per-`execute_reason`-call analogical
/// codebook. Fixed (not random) so repeated calls over the same
/// corpus produce reproducible fits. A fresh [`Codebook`] is built
/// once per call and dropped at the end of it — never a shared/global
/// mutable singleton — which keeps the executor's lock-free-reads
/// discipline intact (no cross-request mutable state).
pub const ANALOGICAL_CODEBOOK_SEED: u64 = 0xA1A0_B1EA_5EED_0001;

/// Lower bound of the analogical-fit multiplier: −20 % at worst.
pub const ANALOGICAL_FIT_MIN: f32 = 0.8;
/// Upper bound of the analogical-fit multiplier: +20 % at best.
///
/// `analogy_query` returns a cosine in `[-1, 1]` (empirically the
/// `vsa::analogy` smoke test sees ~0.4-0.9 for a cleanly bundled
/// triple with no interference). We clamp that cosine to `[0, 1]` and
/// remap linearly into `[ANALOGICAL_FIT_MIN, ANALOGICAL_FIT_MAX]` so
/// the *worst* case (cosine 0, or no resolvable triple at all) is a
/// neutral-to-mild damp and the *best* case is a mild boost — ±20 % is
/// far below anything that could flip a `confidence_threshold` cut on
/// its own, and item inclusion is decided strictly *before* this
/// factor is ever computed (see `executor::reason::execute_reason`,
/// which applies the nudge only to items that already survived
/// `filter_and_trim`). This is the "bounded re-rank nudge, never a
/// hard filter" contract from the analogical-inference design.
pub const ANALOGICAL_FIT_MAX: f32 = 1.2;

/// A resolved `(subject, predicate, object)` triple, rendered to
/// display strings suitable as VSA codebook filler names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceTriple {
    pub subject: String,
    pub predicate: String,
    pub object: String,
}

/// Resolve the highest-confidence, non-tombstoned statement for which
/// `memory_id` is evidence, and render it as an [`EvidenceTriple`].
///
/// Best-effort: returns `None` when the memory was never run through
/// the extractor pipeline (no `STATEMENTS_BY_EVIDENCE_TABLE` rows),
/// every candidate statement is tombstoned, the subject isn't a
/// concrete entity (`SubjectRef::Memory` / `Pending`), or the
/// predicate row is missing. None of these are errors — an item with
/// no resolvable triple is exactly the "no penalty, no exclusion,
/// neutral multiplier" case the caller falls back to.
#[must_use]
pub fn resolve_statement_triple(
    rtxn: &redb::ReadTransaction,
    scope: RowScope,
    memory_id: MemoryId,
) -> Option<EvidenceTriple> {
    let evidence_table = rtxn.open_table(STATEMENTS_BY_EVIDENCE_TABLE).ok()?;
    let mid = memory_id.to_be_bytes();
    // STATEMENTS_BY_EVIDENCE_TABLE keys are `(namespace_id,
    // space_id_bytes, MemoryId, StatementId)` — same scoped range
    // shape as `fetch_enrichment_for`'s evidence-table scan.
    let lo = (scope.namespace_id, scope.space_id_bytes, mid, [0u8; 16]);
    let hi = (scope.namespace_id, scope.space_id_bytes, mid, [0xFFu8; 16]);
    let Ok(range) = evidence_table.range(lo..=hi) else {
        return None;
    };

    let mut best: Option<Statement> = None;
    for entry in range {
        let Ok((k, _v)) = entry else { continue };
        let (_ns, _space, _mem, sid_bytes) = k.value();
        let sid = StatementId::from_bytes(sid_bytes);
        let Ok(Some(stmt)) = statement_get(rtxn, sid) else {
            continue;
        };
        if stmt.tombstoned {
            continue;
        }
        match &best {
            Some(b) if b.confidence >= stmt.confidence => {}
            _ => best = Some(stmt),
        }
    }
    let stmt = best?;
    let subject = render_subject(rtxn, &stmt.subject)?;
    let predicate = predicate_get(rtxn, stmt.predicate)
        .ok()
        .flatten()?
        .canonical();
    let object = render_object(rtxn, &stmt.object)?;
    Some(EvidenceTriple {
        subject,
        predicate,
        object,
    })
}

fn render_subject(rtxn: &redb::ReadTransaction, subject: &SubjectRef) -> Option<String> {
    match subject {
        SubjectRef::Entity(eid) => entity_get(rtxn, *eid)
            .ok()
            .flatten()
            .map(|e| e.canonical_name),
        // No stable label for a memory-as-subject or a pending
        // resolver audit; the triple stays unresolved for these.
        SubjectRef::Memory(_) | SubjectRef::Pending(_) => None,
    }
}

fn render_object(rtxn: &redb::ReadTransaction, object: &StatementObject) -> Option<String> {
    match object {
        StatementObject::Entity(eid) => entity_get(rtxn, *eid)
            .ok()
            .flatten()
            .map(|e| e.canonical_name),
        StatementObject::Value(v) => Some(render_value(v)),
        StatementObject::Memory(mid) => Some(format!("memory:{:x?}", mid.to_be_bytes())),
        StatementObject::Statement(sid) => Some(format!("statement:{:x?}", sid.to_bytes())),
    }
}

fn render_value(v: &StatementValue) -> String {
    match v {
        StatementValue::Text(s) => s.clone(),
        StatementValue::Integer(n) => n.to_string(),
        StatementValue::Float(f) => f.to_string(),
        StatementValue::Bool(b) => b.to_string(),
        StatementValue::UnixNanos(t) => t.to_string(),
        StatementValue::Blob(b) => format!("<{} bytes>", b.len()),
    }
}

/// Bounded structural-fit multiplier for `candidate` against
/// `observation`, using `analogy::analogy_query` over the shared
/// per-call `codebook`.
///
/// Returns `1.0` (neutral — no boost, no damp) unless BOTH triples
/// resolve *and* share the same predicate: the "X works_at Acme" / "Y
/// works_at ?" analogy shape `analogy_query`'s own smoke test
/// exercises. A different relation has no structural axis to compare
/// against, so it stays neutral rather than penalized — matching the
/// "never a hard filter/gate" contract: an item with an unrelated
/// predicate is scored exactly as if it had no triple at all.
#[must_use]
pub fn analogical_fit(
    observation: Option<&EvidenceTriple>,
    candidate: Option<&EvidenceTriple>,
    codebook: &mut Codebook,
) -> f32 {
    let (Some(obs), Some(cand)) = (observation, candidate) else {
        return 1.0;
    };
    if obs.predicate != cand.predicate {
        return 1.0;
    }
    let Ok(obs_vec) = encode_triple(codebook, &obs.subject, &obs.predicate, &obs.object) else {
        return 1.0;
    };
    let Ok(cand_vec) = encode_triple(codebook, &cand.subject, &cand.predicate, &cand.object) else {
        return 1.0;
    };
    let Ok(Some((_, cos))) = analogy_query(codebook, &obs_vec, &cand_vec, ROLE_OBJECT) else {
        return 1.0;
    };
    let cos = cos.clamp(0.0, 1.0);
    ANALOGICAL_FIT_MIN + cos * (ANALOGICAL_FIT_MAX - ANALOGICAL_FIT_MIN)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn triple(subject: &str, predicate: &str, object: &str) -> EvidenceTriple {
        EvidenceTriple {
            subject: subject.to_string(),
            predicate: predicate.to_string(),
            object: object.to_string(),
        }
    }

    #[test]
    fn neutral_when_either_side_missing() {
        let mut cb = Codebook::new(1);
        let t = triple("Alice", "works_at", "Acme");
        assert_eq!(analogical_fit(None, Some(&t), &mut cb), 1.0);
        assert_eq!(analogical_fit(Some(&t), None, &mut cb), 1.0);
        assert_eq!(analogical_fit(None, None, &mut cb), 1.0);
    }

    #[test]
    fn neutral_when_predicates_differ() {
        let mut cb = Codebook::new(2);
        let obs = triple("Alice", "works_at", "Acme");
        let cand = triple("Bob", "lives_in", "Berlin");
        assert_eq!(analogical_fit(Some(&obs), Some(&cand), &mut cb), 1.0);
    }

    #[test]
    fn same_predicate_produces_bounded_nudge() {
        let mut cb = Codebook::new(3);
        let obs = triple("Alice", "works_at", "Acme");
        let cand = triple("Bob", "works_at", "Stripe");
        let fit = analogical_fit(Some(&obs), Some(&cand), &mut cb);
        assert!(
            (ANALOGICAL_FIT_MIN..=ANALOGICAL_FIT_MAX).contains(&fit),
            "fit={fit} must stay within [{ANALOGICAL_FIT_MIN}, {ANALOGICAL_FIT_MAX}]",
        );
        // A clean self-consistent triple recovers its own object with
        // high cosine, so the nudge should land at or near the boost
        // end rather than the neutral/damp end.
        assert!(fit > 1.0, "fit={fit} should boost a clean analogy match");
    }

    #[test]
    fn bound_holds_across_many_seeds() {
        // The clamp is a pure function of the cosine, but assert the
        // bound holds across a spread of codebook seeds/vocab so the
        // guarantee isn't an artifact of one lucky seed.
        for seed in 0..20u64 {
            let mut cb = Codebook::new(seed);
            let obs = triple("Alice", "works_at", "Acme");
            let cand = triple("Bob", "works_at", "Stripe");
            let fit = analogical_fit(Some(&obs), Some(&cand), &mut cb);
            assert!(
                (ANALOGICAL_FIT_MIN..=ANALOGICAL_FIT_MAX).contains(&fit),
                "seed={seed} fit={fit} out of bound",
            );
        }
    }
}
