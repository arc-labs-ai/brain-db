//! SUBSCRIBE / UNSUBSCRIBE plus filter sub-structs.

use rkyv::{Archive, Deserialize, Serialize};

use crate::shared::primitives::MemoryKindWire;
use crate::envelope::request::{WireContextId, WireMemoryId, WireUuid};

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct SubscribeRequest {
    pub filter: SubscriptionFilter,
    pub include_history: bool,
    pub from_lsn: Option<u64>,
    pub max_inflight: u32,
}

#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct SubscriptionFilter {
    pub contexts: Option<Vec<WireContextId>>,
    pub kinds: Option<Vec<MemoryKindWire>>,
    pub similar_to: Option<SimilarityFilter>,
    /// Subset of agent ids whose events the subscriber wants. `None`
    /// or empty = all agents (server-wide / shard-wide). The single
    /// most useful filter on a multi-tenant shard — without it, a
    /// subscriber sees every other agent's events that happen to
    /// route to the same shard. Server-side matching is a
    /// `HashSet::contains` per event.
    pub agents: Option<Vec<WireUuid>>,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct SimilarityFilter {
    pub reference_memory_id: WireMemoryId,
    pub threshold: f32,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct UnsubscribeRequest {
    pub target_stream_id: u32,
}

// ============================================================
// Response payloads
// ============================================================


use crate::shared::enums::{EventType, StageKind, StageOutcome, StagePayload};

/// Push event for a subscription.
///
/// Body carries `knowledge_payload`, an optional typed sidecar with
/// typed-graph event data. For cognitive events (`Encoded`,
/// `Forgotten`, `Reclaimed`, `KindChanged`) the field is `None`. For
/// typed-graph events the cognitive fields (`memory_id`, `context_id`,
/// `kind`, `salience`, `text`) are zero-filled and `knowledge_payload`
/// carries the data.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct SubscriptionEvent {
    pub event_type: EventType,
    pub memory_id: WireMemoryId,
    pub context_id: WireContextId,
    pub text: String,
    pub kind: MemoryKindWire,
    pub salience: f32,
    pub timestamp_unix_nanos: u64,
    pub lsn: u64,
    /// `None` for cognitive events; `Some(_)` for typed-graph events.
    pub knowledge_payload: Option<KnowledgeEventPayload>,
    /// `Some(_)` when `event_type` is `EdgeAdded`, `EdgeRemoved` or
    /// `EdgeSuperseded` — unified-edge change-feed events. LINK /
    /// UNLINK, typed-relation create / supersede / tombstone all
    /// surface here. `None` for every other event.
    pub edge_payload: Option<EdgeEventPayload>,
    /// `Some(_)` when `event_type == StageCompleted` — one background
    /// stage of a write's pipeline finished. The triple
    /// `(memory_id, stage_kind, outcome)` is the wait-helper's
    /// match-key; `payload` carries the per-stage detail. `None` for
    /// every other event.
    pub stage_kind: Option<StageKind>,
    pub stage_outcome: Option<StageOutcome>,
    pub stage_payload: Option<StagePayload>,
}

/// Side-channel payload carried on an `EdgeAdded` / `EdgeRemoved` /
/// `EdgeSuperseded` subscription event. The same shape covers
/// memory-graph edges and typed-graph relations — kind discriminator
/// and optional `relation_id` distinguish them.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct EdgeEventPayload {
    /// `0` = Memory, `1` = Entity — matches the `NodeRef::tag()` byte.
    pub from_kind: u8,
    pub from_id: WireUuid,
    pub to_kind: u8,
    pub to_id: WireUuid,
    /// `0` = Builtin memory-graph kind, `1` = Mentions, `2` = Typed
    /// relation. Matches `EdgeKindRef` discriminator.
    pub edge_kind_tag: u8,
    /// Discriminator-specific payload byte:
    /// - `Builtin(EdgeKind)` → the memory-graph `EdgeKind` u8.
    /// - `Mentions` → 0.
    /// - `Typed(RelationTypeId)` → low byte; full id in
    ///   `relation_type_id`.
    pub edge_kind_byte: u8,
    /// `Some(_)` for typed-relation events (`Typed(RelationTypeId)`).
    /// `None` for memory-graph / mentions edges.
    pub relation_type_id: Option<u32>,
    /// Per-edge weight from `EdgeData`. Typed-relation rows write
    /// `1.0` (sidecar carries `confidence`).
    pub weight: f32,
    /// `Some(_)` for typed-relation events — the per-relation
    /// disambiguator id. `None` for memory-graph / mentions edges.
    pub relation_id: Option<WireUuid>,
    /// Only populated for `EdgeSuperseded` — the prior relation that
    /// got replaced.
    pub superseded_relation_id: Option<WireUuid>,
    /// Origin discriminator copied from
    /// `brain_metadata::tables::edge::origin::*`:
    /// `0` = `EXPLICIT` (LINK / RELATION_LINK / WAL replay of either),
    /// `1` = `AUTO_DERIVED` (worker-inferred, e.g. AutoEdgeWorker's
    /// `SimilarTo`).
    /// Agents driving on the change feed filter by this so they can
    /// distinguish edges they wrote from edges the server inferred.
    pub origin: u8,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct UnsubscribeResponse {
    pub target_stream_id: u32,
    pub final_lsn: u64,
}

// ============================================================
// Event payloads
// ============================================================

// ---------------------------------------------------------------------------
// Top-level union.
// ---------------------------------------------------------------------------

/// Typed payload for a typed-graph SUBSCRIBE event. Discriminated by
/// the parent [`crate::ops::subscribe::SubscriptionEvent::event_type`].
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
    use crate::codec::rkyv::{from_rkyv_bytes, to_rkyv_bytes};

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
