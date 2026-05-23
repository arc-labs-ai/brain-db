//! Typed-graph SUBSCRIBE event payloads.
//!
//! Carried on [`crate::responses::SubscriptionEvent`] via its
//! `knowledge_payload: Option<KnowledgeEventPayload>` field. The
//! variants cover entity, statement, relation, and schema events.

use rkyv::{Archive, Deserialize, Serialize};

use crate::request::WireUuid;

// ---------------------------------------------------------------------------
// Top-level union.
// ---------------------------------------------------------------------------

/// Typed payload for a typed-graph SUBSCRIBE event. Discriminated by
/// the parent [`crate::responses::SubscriptionEvent::event_type`].
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub enum KnowledgeEventPayload {
    // Entity events.
    EntityCreated(EntityCreatedEvent),
    EntityUpdated(EntityUpdatedEvent),
    EntityRenamed(EntityRenamedEvent),
    EntityMerged(EntityMergedEvent),
    EntityUnmerged(EntityUnmergedEvent),
    EntityTombstoned(EntityTombstonedEvent),

    // Statement events.
    StatementCreated(StatementCreatedEvent),
    StatementSuperseded(StatementSupersededEvent),
    StatementTombstoned(StatementTombstonedEvent),

    // Relation events.
    RelationCreated(RelationCreatedEvent),
    RelationSuperseded(RelationSupersededEvent),
    RelationTombstoned(RelationTombstonedEvent),

    // Schema events.
    SchemaUpdated(SchemaUpdatedEvent),
}

// ---------------------------------------------------------------------------
// Entity events.
// ---------------------------------------------------------------------------

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct EntityCreatedEvent {
    pub entity_id: WireUuid,
    pub entity_type_id: u32,
    pub canonical_name: String,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct EntityUpdatedEvent {
    pub entity_id: WireUuid,
    pub entity_type_id: u32,
    pub canonical_name: String,
    pub embedding_version_changed: bool,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct EntityRenamedEvent {
    pub entity_id: WireUuid,
    pub old_canonical_name: String,
    pub new_canonical_name: String,
    pub old_moved_to_alias: bool,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct EntityMergedEvent {
    pub survivor: WireUuid,
    pub merged: WireUuid,
    pub audit_id: WireUuid,
    pub confidence: f32,
    pub statements_rerouted: u32,
    pub relations_rerouted: u32,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct EntityUnmergedEvent {
    pub restored_entity_id: WireUuid,
    pub from_survivor: WireUuid,
    pub audit_id: WireUuid,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct EntityTombstonedEvent {
    pub entity_id: WireUuid,
    pub reason: String,
}

// ---------------------------------------------------------------------------
// Statement events.
// ---------------------------------------------------------------------------

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct StatementCreatedEvent {
    pub statement_id: WireUuid,
    /// 1=Fact, 2=Preference, 3=Event.
    pub kind: u8,
    pub subject: WireUuid,
    pub predicate: String,
    pub confidence: f32,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct StatementSupersededEvent {
    pub old_statement_id: WireUuid,
    pub new_statement_id: WireUuid,
    pub chain_root: WireUuid,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct StatementTombstonedEvent {
    pub statement_id: WireUuid,
    pub reason: String,
}

// ---------------------------------------------------------------------------
// Relation events.
// ---------------------------------------------------------------------------

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct RelationCreatedEvent {
    pub relation_id: WireUuid,
    pub relation_type: String,
    pub from: WireUuid,
    pub to: WireUuid,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct RelationSupersededEvent {
    pub old_relation_id: WireUuid,
    pub new_relation_id: WireUuid,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct RelationTombstonedEvent {
    pub relation_id: WireUuid,
    pub reason: String,
}

// ---------------------------------------------------------------------------
// Schema events.
// ---------------------------------------------------------------------------

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct SchemaUpdatedEvent {
    /// Namespace the new version belongs to.
    pub namespace: String,
    pub from_version: u32,
    pub to_version: u32,
    /// Always `true` in v1 — no diff computed.
    pub backward_compatible: bool,
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rkyv_codec::{from_rkyv_bytes, to_rkyv_bytes};

    fn uuid(seed: u8) -> WireUuid {
        let mut u = [0u8; 16];
        for (i, b) in u.iter_mut().enumerate() {
            *b = seed.wrapping_add(i as u8);
        }
        u
    }

    fn roundtrip(payload: KnowledgeEventPayload) {
        let bytes = to_rkyv_bytes(&payload);
        let decoded: KnowledgeEventPayload = from_rkyv_bytes(&bytes).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn entity_created_round_trip() {
        roundtrip(KnowledgeEventPayload::EntityCreated(EntityCreatedEvent {
            entity_id: uuid(1),
            entity_type_id: 1,
            canonical_name: "Alice".into(),
        }));
    }

    #[test]
    fn entity_merged_round_trip() {
        roundtrip(KnowledgeEventPayload::EntityMerged(EntityMergedEvent {
            survivor: uuid(2),
            merged: uuid(3),
            audit_id: uuid(4),
            confidence: 0.93,
            statements_rerouted: 0,
            relations_rerouted: 0,
        }));
    }

    #[test]
    fn entity_unmerged_round_trip() {
        roundtrip(KnowledgeEventPayload::EntityUnmerged(EntityUnmergedEvent {
            restored_entity_id: uuid(5),
            from_survivor: uuid(6),
            audit_id: uuid(7),
        }));
    }

    #[test]
    fn entity_tombstoned_round_trip() {
        roundtrip(KnowledgeEventPayload::EntityTombstoned(
            EntityTombstonedEvent {
                entity_id: uuid(8),
                reason: "obsolete".into(),
            },
        ));
    }

    #[test]
    fn statement_event_round_trips() {
        roundtrip(KnowledgeEventPayload::StatementCreated(
            StatementCreatedEvent {
                statement_id: uuid(10),
                kind: 1,
                subject: uuid(11),
                predicate: "brain:has_role".into(),
                confidence: 0.85,
            },
        ));
    }

    #[test]
    fn relation_event_round_trips() {
        roundtrip(KnowledgeEventPayload::RelationCreated(
            RelationCreatedEvent {
                relation_id: uuid(20),
                relation_type: "brain:manages".into(),
                from: uuid(21),
                to: uuid(22),
            },
        ));
    }

    #[test]
    fn schema_event_round_trips() {
        roundtrip(KnowledgeEventPayload::SchemaUpdated(SchemaUpdatedEvent {
            namespace: "acme".into(),
            from_version: 1,
            to_version: 2,
            backward_compatible: true,
        }));
    }
}
