//! Extractor output types.
//!
//! `ExtractedItem` is what `Extractor::run` emits before resolver
//! and persistence. Mentions carry qnames (not interned ids) so
//! the resolver can fail gracefully if the registry doesn't have
//! the target type yet.

use serde::{Deserialize, Serialize};

/// Sum type covering all per-mention output kinds.
// The `Mention` suffix is the domain noun — every payload here is a
// span-level "mention" (vs. a resolved entity / statement / relation).
// Stripping the suffix would conflate the variants with the underlying
// resolved-form types of the same names elsewhere in `brain-core`.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ExtractedItem {
    EntityMention(EntityMention),
    StatementMention(StatementMention),
    RelationMention(RelationMention),
}

impl ExtractedItem {
    #[must_use]
    pub fn confidence(&self) -> f32 {
        match self {
            Self::EntityMention(m) => m.confidence,
            Self::StatementMention(m) => m.confidence,
            Self::RelationMention(m) => m.confidence,
        }
    }

    #[must_use]
    pub fn extractor_id(&self) -> u32 {
        match self {
            Self::EntityMention(m) => m.extractor_id,
            Self::StatementMention(m) => m.extractor_id,
            Self::RelationMention(m) => m.extractor_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntityMention {
    /// Canonical type qname e.g. `"brain:Person"`. Resolver
    /// converts to `EntityTypeId` at persistence time.
    pub entity_type_qname: String,
    /// The matched text. UTF-8 slice of `memory.text[start..end]`.
    pub text: String,
    /// Byte-offset range within `memory.text`. UTF-8-safe; both
    /// ends fall on character boundaries.
    pub start: usize,
    pub end: usize,
    pub confidence: f32,
    pub extractor_id: u32,
    pub extractor_version: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatementMention {
    /// `StatementKind` discriminant. 1=Fact, 2=Preference,
    /// 3=Event.
    pub kind: u8,
    /// Optional — extractor may not always extract subject inline.
    pub subject_text: Option<String>,
    /// Canonical predicate qname e.g. `"brain:prefers"`.
    pub predicate_qname: String,
    /// Optional — Memory / Statement object kinds carry no inline
    /// text.
    pub object_text: Option<String>,
    pub confidence: f32,
    pub extractor_id: u32,
    pub extractor_version: u32,
    /// LLM's per-extraction statefulness signal. The extractor pipeline
    /// uses this verbatim for `brain:fact` wildcard-sink rows; for
    /// schema-declared predicates the registry's
    /// `PredicateDefinition.is_stateful` wins.
    #[serde(default)]
    pub is_stateful: bool,
    /// When true, the statement's subject is the *source memory itself*
    /// (`SubjectRef::Memory`), not an entity — `subject_text` is ignored.
    /// Set by the temporal-expressions extractor for memory-anchored
    /// Events ("this memory's content occurred at T"). Default false
    /// keeps every existing extractor's entity-subject behaviour.
    #[serde(default)]
    pub subject_is_memory: bool,
    /// When true, `object_text` names an entity (e.g. "Acme Corp", "Paris")
    /// and the apply pass resolves/mints it as an entity-object node; when
    /// false it's a literal value kept as text ("blue", "200"). The LLM tier
    /// sets this; a declared predicate's `object_type_constraint` overrides it,
    /// and an object already surfaced as an entity this cycle links regardless.
    /// Default false preserves the prior value-by-default behaviour.
    #[serde(default)]
    pub object_is_entity: bool,
    /// Resolved occurrence time (unix-nanos) for an Event-kind statement. The
    /// LLM tier emits an ISO date the projection parses into this; the
    /// temporal-expressions extractor sets it directly. An entity-subject Event
    /// with a time persists as an Event; without one it downgrades to Fact (no
    /// precise time to anchor). `None` for non-Event statements.
    #[serde(default)]
    pub event_at_unix_nanos: Option<u64>,
    /// When true, the subject is the *writing agent itself* — a first-person
    /// statement ("I prefer dark roast", "yo soy vegetariano", "私は…"). The LLM
    /// tier sets this from meaning, NOT a pronoun list, so it is language-neutral.
    /// The apply pass routes such statements to the agent's self-entity
    /// (`EntityId::from(agent_id)`) instead of dropping the first-person surface
    /// as non-referential. Default false preserves entity-subject behaviour.
    #[serde(default)]
    pub subject_is_self: bool,
    /// When true, the source text RETRACTS this prior fact ("not at Google
    /// anymore", "no longer drinks coffee"): the apply pass tombstones the
    /// matching current statement(s) for (subject, predicate, object) instead
    /// of creating a new row. The LLM sets this from meaning, not a keyword
    /// list. Default false = a normal positive assertion.
    #[serde(default)]
    pub retract: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelationMention {
    /// Canonical relation-type qname e.g. `"brain:reports_to"`.
    pub relation_type_qname: String,
    /// Required: relation mentions always carry both endpoints.
    pub subject_text: String,
    pub object_text: String,
    pub confidence: f32,
    pub extractor_id: u32,
    pub extractor_version: u32,
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn em() -> EntityMention {
        EntityMention {
            entity_type_qname: "brain:Person".into(),
            text: "Alice".into(),
            start: 0,
            end: 5,
            confidence: 0.7,
            extractor_id: 1,
            extractor_version: 1,
        }
    }

    #[test]
    fn entity_mention_round_trips_serde_json() {
        let m = em();
        let s = serde_json::to_string(&ExtractedItem::EntityMention(m.clone())).unwrap();
        let back: ExtractedItem = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ExtractedItem::EntityMention(m));
    }

    #[test]
    fn statement_mention_includes_optional_subject_object() {
        let m = StatementMention {
            kind: 2, // Preference
            subject_text: Some("Alice".into()),
            subject_is_memory: false,
            predicate_qname: "brain:prefers".into(),
            object_text: Some("async meetings".into()),
            confidence: 0.85,
            extractor_id: 2,
            extractor_version: 1,
            is_stateful: true,
            object_is_entity: false,
            event_at_unix_nanos: None,
            subject_is_self: false,
            retract: false,
        };
        let s = serde_json::to_string(&ExtractedItem::StatementMention(m.clone())).unwrap();
        assert!(s.contains("\"subject_text\":\"Alice\""));
        assert!(s.contains("\"predicate_qname\":\"brain:prefers\""));
        let back: ExtractedItem = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ExtractedItem::StatementMention(m));
    }

    #[test]
    fn relation_mention_requires_subject_and_object() {
        let m = RelationMention {
            relation_type_qname: "brain:reports_to".into(),
            subject_text: "Bob".into(),
            object_text: "Priya".into(),
            confidence: 0.9,
            extractor_id: 3,
            extractor_version: 1,
        };
        let s = serde_json::to_string(&ExtractedItem::RelationMention(m.clone())).unwrap();
        let back: ExtractedItem = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ExtractedItem::RelationMention(m));
    }

    #[test]
    fn extracted_item_confidence_helper() {
        let item = ExtractedItem::EntityMention(em());
        assert!((item.confidence() - 0.7).abs() < 1e-6);
        assert_eq!(item.extractor_id(), 1);
    }
}
