//! Extractor output types. Spec §22/01 §4 + §22/04.
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
    /// `StatementKind` discriminant per spec §17. 1=Fact, 2=Preference,
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
            predicate_qname: "brain:prefers".into(),
            object_text: Some("async meetings".into()),
            confidence: 0.85,
            extractor_id: 2,
            extractor_version: 1,
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
