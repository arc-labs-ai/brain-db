//! Entity attribution for recall — deciding whether a memory is *about*
//! the entity a cue named, as opposed to merely adjacent to it.
//!
//! Retrieval answers "what is topically near this cue". Attribution answers
//! "whose fact is this". Those come apart whenever two subjects share a
//! topic, and when they do, a memory store that only ranks will hand back
//! one person's fact under another person's name.
//!
//! Brain already records the binding twice over. A statement carries a
//! typed `subject` / `object`, and the extractor writes one `Mentions` edge
//! per resolved entity. Neither was consulted when shaping the episodic
//! membership set, which is the set a caller reads.
//!
//! # Mentions do not establish aboutness
//!
//! The obvious predicate — "keep it if the entity is mentioned" — does not
//! work, and the corpus says so. Across the evidence turns behind 404
//! twin-pair questions (one asking about the real subject, one re-attributing
//! that same fact to the other speaker), **30.2% also name the other party**.
//! They are vocatives and asides, not statements of aboutness:
//!
//! > *"Yep, Melanie! I've got some other stuff with sentimental value, like
//! > my hand-painted bowl."*
//!
//! Naming Melanie does not make the bowl hers. So when typed roles exist,
//! the roles decide and a bare mention cannot overrule them; a genuine
//! participant is carried by an *object* role, which the role test already
//! covers. The mention set is consulted only when no typed role exists at
//! all, where it is the sole evidence available.
//!
//! # The predicate fails open
//!
//! Extraction is asynchronous. A memory recalled before its extractor pass
//! has no statements and no edges, and a schemaless deployment never mints
//! entities at all. Absence of binding is not evidence of mis-binding, so
//! an unbound memory is admitted. The cost of that choice is a missed
//! refusal; the cost of the opposite is an empty recall on a healthy store.

use std::collections::HashSet;

use brain_core::nodes::statement::{Statement, StatementObject, SubjectRef};
use brain_core::{EdgeKindRef, EntityId, MemoryId, NodeRef, StatementId};

use crate::statement::statement_get;
use crate::tables::edge::walk_outgoing;
use crate::tables::statement::STATEMENTS_BY_EVIDENCE_TABLE;
use crate::RowScope;

/// What a memory's stored rows say about which entities it concerns.
///
/// `had_statements` / `had_mentions` are kept separately from the sets
/// because "no rows were written yet" and "rows were written and name
/// nobody relevant" are different facts that the predicate treats
/// differently.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryBinding {
    /// Entities appearing as the `subject` of a live statement.
    pub subject_roles: HashSet<EntityId>,
    /// Entities appearing as the `object` of a live statement.
    pub object_roles: HashSet<EntityId>,
    /// Entities the extractor resolved anywhere in the text.
    pub mentions: HashSet<EntityId>,
    /// At least one live statement cites this memory as evidence.
    pub had_statements: bool,
    /// At least one `Mentions` edge leaves this memory.
    pub had_mentions: bool,
}

/// Why a memory was admitted or refused for a given anchor set.
///
/// Carried rather than reduced to a bool so a trace can say which rule
/// fired — `UnknownBinding` and `BoundToAnchor` both admit, but only one
/// of them means the store actually knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttributionVerdict {
    /// The cue named nobody; attribution does not apply.
    NoAnchor,
    /// A role or, absent roles, a mention ties this memory to an anchor.
    BoundToAnchor,
    /// Nothing is recorded yet. Admitted, because silence is not denial.
    UnknownBinding,
    /// The store knows whom this is about, and it is not the anchor.
    MisAttributed,
}

impl AttributionVerdict {
    /// Whether the memory survives the gate.
    #[must_use]
    pub const fn admits(self) -> bool {
        !matches!(self, Self::MisAttributed)
    }
}

/// Decide whether `binding` may be returned for a cue naming `anchors`.
///
/// Pure: no database, no transaction, so the rule is testable on its own.
#[must_use]
pub fn attribution_admits(binding: &MemoryBinding, anchors: &HashSet<EntityId>) -> AttributionVerdict {
    // An unanchored cue is the common case ("when was the trip"). The gate
    // must be a no-op there or every such recall returns nothing.
    if anchors.is_empty() {
        return AttributionVerdict::NoAnchor;
    }

    // Typed roles are the strong binding: the extractor committed to whom
    // this is about. They are decisive in both directions.
    let has_roles = !binding.subject_roles.is_empty() || !binding.object_roles.is_empty();
    if has_roles {
        let bound = binding.subject_roles.iter().any(|e| anchors.contains(e))
            || binding.object_roles.iter().any(|e| anchors.contains(e));
        return if bound {
            AttributionVerdict::BoundToAnchor
        } else {
            AttributionVerdict::MisAttributed
        };
    }

    // No roles. If nothing was written at all, extraction has not run and
    // the memory is admitted unjudged.
    if !binding.had_mentions {
        return AttributionVerdict::UnknownBinding;
    }

    // Mentions are the only evidence left, so they decide.
    if binding.mentions.iter().any(|e| anchors.contains(e)) {
        AttributionVerdict::BoundToAnchor
    } else {
        AttributionVerdict::MisAttributed
    }
}

/// Whether a statement's own subject or object lies in `anchors`.
///
/// Widens the subject-only check the grounded path uses, so that
/// "what did Melanie tell Caroline" is not refused under Caroline.
#[must_use]
pub fn statement_admits(stmt: &Statement, anchors: &HashSet<EntityId>) -> bool {
    if anchors.is_empty() {
        return true;
    }
    if stmt.subject.as_entity().is_some_and(|id| anchors.contains(&id)) {
        return true;
    }
    matches!(stmt.object, StatementObject::Entity(id) if anchors.contains(&id))
}

/// `SubjectRef` helper kept beside the predicate it serves.
#[must_use]
pub fn subject_is_entity(subject: SubjectRef) -> bool {
    subject.as_entity().is_some()
}

/// Read what the store records about whom `memory` is about.
///
/// Two sources, both already on disk — nothing new is written or indexed
/// for attribution:
///
///   * **typed roles** — the live statements that cite `memory` as
///     evidence, via the scoped `STATEMENTS_BY_EVIDENCE_TABLE` reverse
///     index. A statement that is tombstoned or superseded is not current
///     truth, so it carries no binding.
///   * **mentions** — the `Mentions` edges the extractor wrote from the
///     memory node to each entity it resolved in the text.
///
/// # Fails soft, by construction
///
/// Every fallible step degrades to "no binding found" rather than
/// propagating: a missing table, an unreadable range, a decode failure.
/// An empty [`MemoryBinding`] makes [`attribution_admits`] return
/// `UnknownBinding`, which **admits**. This is the only acceptable
/// failure direction — a transient read error must never be the reason a
/// caller is told a fact does not exist, so this function returns no
/// `Result` at all and there is nothing for a caller to mishandle.
#[must_use]
pub fn memory_binding(
    rtxn: &redb::ReadTransaction,
    scope: RowScope,
    memory: MemoryId,
) -> MemoryBinding {
    let mut binding = MemoryBinding::default();

    // (a) Typed roles from the live statements citing this memory.
    if let Ok(table) = rtxn.open_table(STATEMENTS_BY_EVIDENCE_TABLE) {
        let mid = memory.to_be_bytes();
        let lo = (scope.namespace_id, scope.space_id_bytes, mid, [0u8; 16]);
        let hi = (scope.namespace_id, scope.space_id_bytes, mid, [0xFFu8; 16]);
        if let Ok(range) = table.range(lo..=hi) {
            for entry in range {
                let Ok((k, _)) = entry else { continue };
                let (_, _, _, sid) = k.value();
                // The index row itself is the evidence link: the memory HAS
                // extracted statements even if this particular row is stale
                // or undecodable, so `had_statements` is set from the range,
                // not from the surviving rows.
                binding.had_statements = true;
                let Ok(Some(stmt)) = statement_get(rtxn, StatementId::from_bytes(sid)) else {
                    continue;
                };
                // A superseded row is a prior version of a claim the store
                // has since replaced; believing its roles would let a
                // corrected attribution keep voting.
                if stmt.tombstoned || stmt.superseded_by.is_some() {
                    continue;
                }
                if let Some(subject) = stmt.subject.as_entity() {
                    binding.subject_roles.insert(subject);
                }
                if let StatementObject::Entity(id) = stmt.object {
                    binding.object_roles.insert(id);
                }
            }
        }
    }

    // (b) Mentions edges, Memory → Entity.
    if let Ok(rows) = walk_outgoing(
        rtxn,
        NodeRef::Memory(memory),
        Some(EdgeKindRef::Mentions),
    ) {
        binding.had_mentions = !rows.is_empty();
        for (_, to, _, _) in rows {
            if let NodeRef::Entity(id) = to {
                binding.mentions.insert(id);
            }
        }
    }

    binding
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchors(ids: &[EntityId]) -> HashSet<EntityId> {
        ids.iter().copied().collect()
    }

    fn bound(subject: &[EntityId], object: &[EntityId], mentions: &[EntityId]) -> MemoryBinding {
        MemoryBinding {
            subject_roles: subject.iter().copied().collect(),
            object_roles: object.iter().copied().collect(),
            mentions: mentions.iter().copied().collect(),
            had_statements: !subject.is_empty() || !object.is_empty(),
            had_mentions: !mentions.is_empty(),
        }
    }

    /// The headline case. One memory, two cues, opposite verdicts — the
    /// asymmetry IS the assertion, so both halves live in one test.
    #[test]
    fn twin_pair_admits_the_subject_and_refuses_the_other() {
        let melanie = EntityId::new();
        let caroline = EntityId::new();
        let bowl = bound(&[melanie], &[], &[melanie]);

        assert_eq!(
            attribution_admits(&bowl, &anchors(&[melanie])),
            AttributionVerdict::BoundToAnchor
        );
        assert_eq!(
            attribution_admits(&bowl, &anchors(&[caroline])),
            AttributionVerdict::MisAttributed
        );
    }

    /// Most cues name nobody. The gate must not touch them.
    #[test]
    fn an_unanchored_cue_admits_everything() {
        let someone = EntityId::new();
        let b = bound(&[someone], &[], &[someone]);
        assert_eq!(
            attribution_admits(&b, &HashSet::new()),
            AttributionVerdict::NoAnchor
        );
    }

    /// Extraction is asynchronous, so "nothing written yet" is routine.
    /// Silence is not denial.
    #[test]
    fn a_memory_with_no_binding_is_admitted_unjudged() {
        let caroline = EntityId::new();
        assert_eq!(
            attribution_admits(&MemoryBinding::default(), &anchors(&[caroline])),
            AttributionVerdict::UnknownBinding
        );
    }

    /// "What did Melanie tell Caroline" must not lose its own evidence.
    #[test]
    fn an_object_role_counts_as_involvement() {
        let melanie = EntityId::new();
        let caroline = EntityId::new();
        let told = bound(&[melanie], &[caroline], &[]);
        assert_eq!(
            attribution_admits(&told, &anchors(&[caroline])),
            AttributionVerdict::BoundToAnchor
        );
    }

    /// Measured: 30.2% of twin-pair evidence turns also name the other
    /// party, as vocatives — "Yep, Melanie! I've got my hand-painted bowl".
    /// Naming someone is not a claim about them, so a mention must not
    /// overrule a typed role that says otherwise.
    #[test]
    fn a_vocative_mention_does_not_overrule_a_typed_role() {
        let melanie = EntityId::new();
        let caroline = EntityId::new();
        // Caroline speaks, names Melanie, and the bowl is Caroline's.
        let bowl = bound(&[caroline], &[], &[caroline, melanie]);
        assert_eq!(
            attribution_admits(&bowl, &anchors(&[melanie])),
            AttributionVerdict::MisAttributed
        );
    }

    /// With no typed roles, mentions are the only evidence, so they decide.
    #[test]
    fn a_mention_only_binding_still_discriminates() {
        let melanie = EntityId::new();
        let caroline = EntityId::new();
        let b = bound(&[], &[], &[melanie]);
        assert_eq!(
            attribution_admits(&b, &anchors(&[melanie])),
            AttributionVerdict::BoundToAnchor
        );
        assert_eq!(
            attribution_admits(&b, &anchors(&[caroline])),
            AttributionVerdict::MisAttributed
        );
    }

    /// A cue can name two people; matching either is enough.
    #[test]
    fn a_two_name_cue_admits_either_subject() {
        let melanie = EntityId::new();
        let caroline = EntityId::new();
        let b = bound(&[caroline], &[], &[caroline]);
        assert_eq!(
            attribution_admits(&b, &anchors(&[melanie, caroline])),
            AttributionVerdict::BoundToAnchor
        );
    }

    #[test]
    fn admits_is_false_only_for_misattribution() {
        assert!(AttributionVerdict::NoAnchor.admits());
        assert!(AttributionVerdict::BoundToAnchor.admits());
        assert!(AttributionVerdict::UnknownBinding.admits());
        assert!(!AttributionVerdict::MisAttributed.admits());
    }
}
