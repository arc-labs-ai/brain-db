//! `Phase → WalPayload` mapping for the unified write path.
//!
//! WAL framing for submit(Write).
//! Single-phase writes whose phase maps to an existing typed
//! [`WalPayload`] variant get WAL durability automatically.
//!
//! ## Scope
//!
//! Covered (substrate phases with direct typed-payload counterparts):
//! - UpsertMemory → WalPayload::Encode
//! - Tombstone(Memory) → WalPayload::Forget
//! - Link / Unlink → WalPayload::Link / Unlink
//! - UpdateSalience / UpdateKind / UpdateContext → matching payloads
//!
//! Multi-phase wrapping in TxnBegin/TxnCommit is handled by the
//! caller (`submit::wal_append_for_write`) — this module just maps
//! each phase to its payload.
//!
//! Deferred:
//! - UpsertEntity / UpsertStatement / UpsertRelation / Supersede /
//!   UpsertSchema / SetExtractorEnabled / MergeEntities — these need
//!   the `WalPayload::PhaseBody` variant with rkyv-encoded bodies; the
//!   body schemas land in a phase_bodies.rs follow-up.
//!
//! Phases without a wire-side WAL event (UpdateEmbedding before
//! arena-write wiring, ReclaimSlots, UpdateEntity, RenameEntity,
//! UnmergeEntities): return `None`. The caller (submit()) skips WAL
//! append for them — these phases mirror the pre-migration handler
//! behavior of not WAL-logging.

use brain_core::{EdgeOrigin, NodeRef};
use brain_metadata::recovery::phase_bodies::{
    encode_entity_create, encode_entity_merge, encode_entity_rename, encode_entity_tombstone,
    encode_entity_unmerge, encode_entity_update, encode_extractor_toggle, encode_schema_update,
    encode_statement_create, encode_statement_supersede, encode_statement_tombstone,
    EntityMergeBody, EntityRenameBody, EntityTombstoneBody, EntityUnmergeBody, EntityUpdateBody,
    ExtractorToggleBody, SchemaUpdateBody, StatementCreateBody, StatementSupersedeBody,
    StatementTombstoneBody,
};
use brain_metadata::tables::entity::EntityMetadata;
use brain_metadata::tables::statement::metadata_from_statement;
use brain_storage::wal::kinds::WalRecordKind;
use brain_storage::wal::payload::{
    EncodePayload, ForgetPayload, ForgetReason, PhaseBodyRecord, LinkPayload, RelationLinkPayload,
    RelationSupersedePayload, RelationTombstonePayload, SalienceReason, SalienceUpdate,
    UnlinkPayload, UpdateContextPayload, UpdateKindPayload, UpdateSaliencePayload, WalPayload,
};

use crate::apply::entity::entity_from_upsert_phase;
use crate::apply::statement::statement_from_upsert_phase;
use crate::write::{Phase, SupersedeReplacement, SupersedeTarget, TombstoneTarget, Write};

/// Map a phase to its WAL payload, if one exists.
///
/// `None` means "this phase is not WAL-logged by the unified path
/// today" — either because the payload type doesn't exist yet, or
/// because the phase has no wire-replay semantic (auto-derived edges,
/// audit stamping, slot reclamation).
///
/// Returns owned `WalPayload` — the caller wraps in a `WalRecord` for
/// the WAL sink.
#[must_use]
pub fn phase_to_wal_payload(phase: &Phase, write: &Write) -> Option<WalPayload> {
    match phase {
        // content_hash isn't an EncodePayload field — the WAL doesn't
        // ship it inline; recovery reconstructs the FINGERPRINTS_TABLE
        // row from MEMORIES_TABLE.content_hash which apply_upsert_memory
        // wrote durably alongside the metadata row. So Phase::UpsertMemory's
        // content_hash flows through redb, not the WAL.
        Phase::UpsertMemory {
            id,
            text,
            vector,
            kind,
            salience,
            context,
            embedding_model_fp,
            content_hash: _,
            deduplicate,
            ..
        } => Some(WalPayload::Encode(EncodePayload {
            memory_id: *id,
            // The WAL's request_id field carries the WriteId (both are
            // UUIDv7, 16 bytes). Recovery keys the idempotency cache off
            // this field.
            request_id: brain_core::RequestId(write.write_id.as_uuid()),
            agent_id: write.agent_id,
            context_id: *context,
            kind: *kind,
            salience_initial: salience.raw(),
            embedding_model_fp: *embedding_model_fp,
            text: text.clone(),
            vector: vector.to_vec(),
            // Inline edges aren't part of Phase::UpsertMemory — they
            // ride as separate Phase::Link records in the same write
            // (wrapped in TxnBegin/Commit).
            edges: Vec::new(),
            // request_hash + response_payload are unused by the write
            // path; durability rides on WriteIdempotencyCache.
            request_hash: [0; 32],
            response_payload: Vec::new(),
            deduplicate: *deduplicate,
        })),

        Phase::Link {
            from,
            to,
            kind,
            weight,
            origin,
            ..
        } => Some(WalPayload::Link(LinkPayload {
            source: *from,
            target: *to,
            edge_kind: *kind,
            weight: *weight,
            origin: edge_origin_from_byte(*origin),
        })),

        Phase::Unlink { from, to, kind, .. } => Some(WalPayload::Unlink(UnlinkPayload {
            source: *from,
            target: *to,
            edge_kind: *kind,
            // Substrate UNLINK predates the per-edge sequence number;
            // the field is reserved (zero means "any matching edge").
            edge_seq: 0,
        })),

        Phase::Tombstone {
            target,
            reason,
            at_unix_nanos,
        } => match target {
            TombstoneTarget::Memory { id, mode } => Some(WalPayload::Forget(ForgetPayload {
                memory_id: *id,
                // ForgetPayload.request_id carries the WriteId for
                // idempotency replay (both share the UUIDv7 16-byte
                // layout).
                request_id: brain_core::RequestId(write.write_id.as_uuid()),
                agent_id: write.agent_id,
                mode: match mode {
                    crate::write::phase::TombstoneMode::Soft => {
                        brain_storage::wal::payload::ForgetMode::Soft
                    }
                    crate::write::phase::TombstoneMode::Hard => {
                        brain_storage::wal::payload::ForgetMode::Hard
                    }
                },
                // Phase::Tombstone has a generic reason byte; only the
                // memory-tombstone path uses the wire ForgetReason enum.
                // Treat all Phase-driven tombstones as ClientRequest —
                // eviction-driven tombstones go through the worker path
                // with their own WAL record.
                reason: ForgetReason::ClientRequest,
            })),
            // Entity tombstone rides the PhaseBody envelope: recovery
            // replays it through `entity_tombstone`, the same helper the
            // live apply path calls.
            TombstoneTarget::Entity(id) => {
                let body = encode_entity_tombstone(&EntityTombstoneBody {
                    id: id.to_bytes(),
                    at_unix_nanos: *at_unix_nanos,
                });
                Some(WalPayload::PhaseBody(PhaseBodyRecord::new(
                    WalRecordKind::EntityTombstone,
                    write.agent_id,
                    body,
                )))
            }
            // Statement tombstone rides the PhaseBody envelope: recovery
            // replays it through `statement_tombstone`.
            TombstoneTarget::Statement(id) => {
                let body = encode_statement_tombstone(&StatementTombstoneBody {
                    id: id.to_bytes(),
                    reason: *reason,
                    at_unix_nanos: *at_unix_nanos,
                });
                Some(WalPayload::PhaseBody(PhaseBodyRecord::new(
                    WalRecordKind::StatementTombstone,
                    write.agent_id,
                    body,
                )))
            }
            // Relation tombstone rides the first-class RelationTombstone
            // payload. The reason byte isn't carried — neither the live
            // apply nor recovery uses a relation tombstone reason today.
            TombstoneTarget::Relation(id) => {
                Some(WalPayload::RelationTombstone(RelationTombstonePayload {
                    relation_id: *id,
                    reason: String::new(),
                    at_unix_nanos: *at_unix_nanos,
                    agent_id: write.agent_id,
                }))
            }
        },

        Phase::UpdateSalience { id, new_salience } => {
            Some(WalPayload::UpdateSalience(UpdateSaliencePayload {
                updates: vec![SalienceUpdate {
                    memory_id: *id,
                    new_salience: new_salience.raw(),
                    // The wire path differentiates Access / Decay / Explicit;
                    // Phase::UpdateSalience predates that classification —
                    // treat the phase form as Explicit (the catch-all).
                    reason: SalienceReason::Explicit,
                }],
            }))
        }

        Phase::UpdateKind { id, new_kind } => Some(WalPayload::UpdateKind(UpdateKindPayload {
            memory_id: *id,
            new_kind: *new_kind,
        })),

        Phase::UpdateContext { id, new_context } => {
            Some(WalPayload::UpdateContext(UpdateContextPayload {
                memory_id: *id,
                new_context_id: *new_context,
            }))
        }

        // Opaque-body phases — durability rides on the redb commit; wire
        // replay flows through the post-commit typed-graph-event burst.
        // No standalone WAL body today.
        // Entity create rides the PhaseBody envelope: the body is the
        // full entity row, replayed via `entity_put` (the same helper the
        // live apply path calls). Built through `entity_from_upsert_phase`
        // so the WAL row matches what apply persists.
        Phase::UpsertEntity { .. } => {
            let e = entity_from_upsert_phase(phase)?;
            let meta = EntityMetadata::from(&e);
            let body = encode_entity_create(&meta);
            Some(WalPayload::PhaseBody(PhaseBodyRecord::new(
                WalRecordKind::EntityCreate,
                write.agent_id,
                body,
            )))
        }

        // Statement create rides the PhaseBody envelope. The body carries
        // the statement row built from the phase's predicate plus the
        // schemaless intern hint; recovery re-resolves the predicate when
        // the hint is present (see phase_bodies::StatementCreateBody).
        Phase::UpsertStatement {
            predicate,
            predicate_intern_hint,
            ..
        } => {
            let s = statement_from_upsert_phase(phase, *predicate)?;
            let body = encode_statement_create(&StatementCreateBody {
                meta: metadata_from_statement(&s),
                predicate_intern_hint: predicate_intern_hint.clone(),
            });
            Some(WalPayload::PhaseBody(PhaseBodyRecord::new(
                WalRecordKind::StatementCreate,
                write.agent_id,
                body,
            )))
        }

        // Statement supersession rides the PhaseBody envelope; the new
        // statement is fully built (predicate resolved) so its row is
        // carried inline. Relation supersession falls through to the
        // None group below — not WAL-mapped yet.
        Phase::Supersede {
            target: SupersedeTarget::Statement(old_id),
            replacement: SupersedeReplacement::Statement(new_statement),
            at_unix_nanos,
        } => {
            let body = encode_statement_supersede(&StatementSupersedeBody {
                old_id: old_id.to_bytes(),
                new: metadata_from_statement(new_statement.as_ref()),
                at_unix_nanos: *at_unix_nanos,
            });
            Some(WalPayload::PhaseBody(PhaseBodyRecord::new(
                WalRecordKind::StatementSupersede,
                write.agent_id,
                body,
            )))
        }

        // Relation supersession rides the first-class RelationSupersede
        // payload; the new relation is fully built (type resolved) so its
        // row is carried inline (no intern hint). The supersession
        // timestamp comes from the WAL record, not the payload.
        Phase::Supersede {
            target: SupersedeTarget::Relation(old_id),
            replacement: SupersedeReplacement::Relation(new_rel),
            ..
        } => Some(WalPayload::RelationSupersede(RelationSupersedePayload {
            old_relation_id: *old_id,
            new: RelationLinkPayload {
                relation_id: new_rel.id,
                from: NodeRef::Entity(new_rel.from_entity),
                to: NodeRef::Entity(new_rel.to_entity),
                relation_type_id: new_rel.relation_type,
                chain_root: new_rel.chain_root,
                confidence: new_rel.confidence,
                valid_from_unix_nanos: new_rel.valid_from_unix_nanos,
                valid_to_unix_nanos: new_rel.valid_to_unix_nanos,
                supersedes: new_rel.supersedes,
                evidence: new_rel.evidence.clone(),
                extractor_id: new_rel.extractor_id.raw(),
                is_symmetric: new_rel.is_symmetric,
                properties_blob: new_rel.properties_blob.clone(),
                agent_id: write.agent_id,
                relation_type_intern_hint: None,
            },
        })),

        // Relation create rides the first-class RelationLink payload (the
        // edge row + sidecar + evidence index rebuild atomically on
        // recovery). `relation_type_id` is the placeholder on the
        // schemaless path; recovery re-resolves it via the intern hint.
        Phase::UpsertRelation {
            id,
            ty,
            from,
            to,
            confidence,
            evidence_memories,
            is_symmetric,
            extractor,
            properties_blob,
            valid_from_unix_nanos,
            valid_to_unix_nanos,
            relation_type_intern_hint,
            ..
        } => Some(WalPayload::RelationLink(RelationLinkPayload {
            relation_id: *id,
            from: NodeRef::Entity(*from),
            to: NodeRef::Entity(*to),
            relation_type_id: *ty,
            chain_root: *id,
            confidence: *confidence,
            valid_from_unix_nanos: *valid_from_unix_nanos,
            valid_to_unix_nanos: *valid_to_unix_nanos,
            supersedes: None,
            evidence: evidence_memories.clone(),
            extractor_id: extractor.raw(),
            is_symmetric: *is_symmetric,
            properties_blob: properties_blob.clone(),
            agent_id: write.agent_id,
            relation_type_intern_hint: relation_type_intern_hint.clone(),
        })),

        // Entity full-row update rides the PhaseBody envelope; recovery
        // re-reads the current row and applies the new canonical / aliases
        // / attributes via `entity_update`.
        Phase::UpdateEntity {
            id,
            canonical_name,
            aliases,
            attributes_blob,
            at_unix_nanos,
        } => {
            let body = encode_entity_update(&EntityUpdateBody {
                id: id.to_bytes(),
                canonical_name: canonical_name.clone(),
                aliases: aliases.clone(),
                attributes_blob: attributes_blob.clone(),
                at_unix_nanos: *at_unix_nanos,
            });
            Some(WalPayload::PhaseBody(PhaseBodyRecord::new(
                WalRecordKind::EntityUpdate,
                write.agent_id,
                body,
            )))
        }

        // Entity rename rides the PhaseBody envelope; recovery applies it
        // via `entity_rename` (which moves the old canonical into aliases).
        Phase::RenameEntity {
            id,
            new_canonical_name,
            at_unix_nanos,
        } => {
            let body = encode_entity_rename(&EntityRenameBody {
                id: id.to_bytes(),
                new_canonical_name: new_canonical_name.clone(),
                at_unix_nanos: *at_unix_nanos,
            });
            Some(WalPayload::PhaseBody(PhaseBodyRecord::new(
                WalRecordKind::EntityRename,
                write.agent_id,
                body,
            )))
        }

        // Entity merge rides the PhaseBody envelope; recovery replays it
        // via `merge_entity` (guarded by `merged_into` for re-replay).
        Phase::MergeEntities {
            source,
            target,
            retain_aliases,
            retain_attributes,
            at_unix_nanos,
            confidence,
            reason,
            actor,
            grace_seconds,
        } => {
            let (actor_kind, actor_agent) = match actor {
                brain_metadata::entity::merge::MergeActor::System => (0u8, [0u8; 16]),
                brain_metadata::entity::merge::MergeActor::Agent(bytes) => (1u8, *bytes),
            };
            let body = encode_entity_merge(&EntityMergeBody {
                source: source.to_bytes(),
                target: target.to_bytes(),
                retain_aliases: *retain_aliases,
                retain_attributes: *retain_attributes,
                at_unix_nanos: *at_unix_nanos,
                confidence: *confidence,
                reason: reason.clone(),
                actor_kind,
                actor_agent,
                grace_seconds: *grace_seconds,
            });
            Some(WalPayload::PhaseBody(PhaseBodyRecord::new(
                WalRecordKind::EntityMerge,
                write.agent_id,
                body,
            )))
        }

        // Schema upload rides the PhaseBody envelope; recovery re-parses
        // the DSL blob + re-uploads. namespace+version are carried so
        // recovery can skip an already-applied version on re-replay.
        Phase::UpsertSchema {
            namespace,
            version,
            blob,
            created_at_unix_nanos,
            ..
        } => {
            let body = encode_schema_update(&SchemaUpdateBody {
                namespace: namespace.clone(),
                version: *version,
                blob: blob.clone(),
                created_at_unix_nanos: *created_at_unix_nanos,
            });
            Some(WalPayload::PhaseBody(PhaseBodyRecord::new(
                WalRecordKind::SchemaUpdate,
                write.agent_id,
                body,
            )))
        }

        // Entity unmerge rides the PhaseBody envelope; recovery reverses
        // the merge via `unmerge_entity` (guarded by `merged_into`).
        Phase::UnmergeEntities {
            merged,
            actor,
            at_unix_nanos,
        } => {
            let (actor_kind, actor_agent) = match actor {
                brain_metadata::entity::merge::MergeActor::System => (0u8, [0u8; 16]),
                brain_metadata::entity::merge::MergeActor::Agent(bytes) => (1u8, *bytes),
            };
            let body = encode_entity_unmerge(&EntityUnmergeBody {
                merged: merged.to_bytes(),
                actor_kind,
                actor_agent,
                at_unix_nanos: *at_unix_nanos,
            });
            Some(WalPayload::PhaseBody(PhaseBodyRecord::new(
                WalRecordKind::EntityUnmerge,
                write.agent_id,
                body,
            )))
        }

        // Extractor enable/disable rides the PhaseBody envelope.
        Phase::SetExtractorEnabled { id, enabled } => {
            let body = encode_extractor_toggle(&ExtractorToggleBody {
                id: id.raw(),
                enabled: *enabled,
            });
            Some(WalPayload::PhaseBody(PhaseBodyRecord::new(
                WalRecordKind::ExtractorToggle,
                write.agent_id,
                body,
            )))
        }

        // ApproveMerge / RejectMerge resolve a merge proposal at apply
        // time, so they need a handler-side pre-resolution before they can
        // be WAL-mapped — durability still rides the redb commit for now.
        Phase::Supersede { .. }
        | Phase::ApproveMerge { .. }
        | Phase::RejectMerge { .. } => None,

        // No wire-replay semantic — UpdateEmbedding rewrites a vector
        // the HNSW already absorbed pre-commit; ReclaimSlots is derivable
        // from MEMORIES_TABLE state on recovery.
        Phase::UpdateEmbedding { .. } | Phase::ReclaimSlots { .. } => None,
    }
}

/// Map the byte form of `Phase::Link.origin` to the WAL's typed
/// `EdgeOrigin`. The byte values match
/// `brain_metadata::tables::edge::origin::{EXPLICIT, AUTO_DERIVED}`.
fn edge_origin_from_byte(byte: u8) -> EdgeOrigin {
    use brain_metadata::tables::edge::origin;
    match byte {
        origin::AUTO_DERIVED => EdgeOrigin::AutoDerived,
        _ => EdgeOrigin::Explicit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_core::{
        AgentId, ContextId, EdgeKind, EdgeKindRef, EntityAttributes, EntityId, EntityTypeId,
        MemoryId, MemoryKind, Salience,
    };
    use brain_metadata::tables::edge::zero_disambiguator;

    use crate::write::{Phase, Write, WriteId};

    fn write_for(phase: Phase) -> Write {
        Write::single(WriteId::new(), AgentId::default(), phase)
    }

    #[test]
    fn upsert_entity_maps_to_graph_entity_create() {
        let phase = Phase::UpsertEntity {
            id: EntityId::new(),
            ty: EntityTypeId::from(1),
            canonical: "Priya Patel".into(),
            normalized: "priya patel".into(),
            aliases: vec!["priya".into()],
            attributes: EntityAttributes::default(),
            created_at_unix_nanos: 1_700_000_000_000,
        };
        let w = write_for(phase.clone());
        let WalPayload::PhaseBody(rec) = phase_to_wal_payload(&phase, &w).expect("should map") else {
            panic!("expected PhaseBody payload")
        };
        assert_eq!(rec.kind, WalRecordKind::EntityCreate);
        assert!(!rec.body.is_empty());
    }

    #[test]
    fn tombstone_entity_maps_to_graph_entity_tombstone() {
        let phase = Phase::Tombstone {
            target: TombstoneTarget::Entity(EntityId::new()),
            reason: 0,
            at_unix_nanos: 1_700_000_000_000,
        };
        let w = write_for(phase.clone());
        let WalPayload::PhaseBody(rec) = phase_to_wal_payload(&phase, &w).expect("should map") else {
            panic!("expected PhaseBody payload")
        };
        assert_eq!(rec.kind, WalRecordKind::EntityTombstone);
    }

    #[test]
    fn update_entity_maps_to_graph_entity_update() {
        let phase = Phase::UpdateEntity {
            id: EntityId::new(),
            canonical_name: "New Name".into(),
            aliases: vec![],
            attributes_blob: vec![],
            at_unix_nanos: 1_700_000_000_000,
        };
        let w = write_for(phase.clone());
        let WalPayload::PhaseBody(rec) = phase_to_wal_payload(&phase, &w).expect("should map") else {
            panic!("expected PhaseBody payload")
        };
        assert_eq!(rec.kind, WalRecordKind::EntityUpdate);
    }

    #[test]
    fn rename_entity_maps_to_graph_entity_rename() {
        let phase = Phase::RenameEntity {
            id: EntityId::new(),
            new_canonical_name: "New Canonical".into(),
            at_unix_nanos: 1_700_000_000_000,
        };
        let w = write_for(phase.clone());
        let WalPayload::PhaseBody(rec) = phase_to_wal_payload(&phase, &w).expect("should map") else {
            panic!("expected PhaseBody payload")
        };
        assert_eq!(rec.kind, WalRecordKind::EntityRename);
    }

    #[test]
    fn merge_entities_maps_to_graph_entity_merge() {
        let phase = Phase::MergeEntities {
            source: EntityId::new(),
            target: EntityId::new(),
            retain_aliases: true,
            retain_attributes: true,
            at_unix_nanos: 1_700_000_000_000,
            confidence: 0.9,
            reason: "dup".into(),
            actor: brain_metadata::entity::merge::MergeActor::System,
            grace_seconds: 0,
        };
        let w = write_for(phase.clone());
        let WalPayload::PhaseBody(rec) = phase_to_wal_payload(&phase, &w).expect("should map") else {
            panic!("expected PhaseBody payload")
        };
        assert_eq!(rec.kind, WalRecordKind::EntityMerge);
    }

    #[test]
    fn upsert_schema_maps_to_graph_schema_update() {
        let phase = Phase::UpsertSchema {
            namespace: "acme".into(),
            version: 1,
            blob: b"namespace acme".to_vec(),
            declared_predicates: vec![],
            declared_relation_types: vec![],
            declared_entity_types: vec![],
            created_at_unix_nanos: 1_700_000_000_000,
        };
        let w = write_for(phase.clone());
        let WalPayload::PhaseBody(rec) = phase_to_wal_payload(&phase, &w).expect("should map") else {
            panic!("expected PhaseBody payload")
        };
        assert_eq!(rec.kind, WalRecordKind::SchemaUpdate);
    }

    #[test]
    fn unmerge_entities_maps_to_graph_entity_unmerge() {
        let phase = Phase::UnmergeEntities {
            merged: EntityId::new(),
            actor: brain_metadata::entity::merge::MergeActor::System,
            at_unix_nanos: 1_700_000_000_000,
        };
        let w = write_for(phase.clone());
        let WalPayload::PhaseBody(rec) = phase_to_wal_payload(&phase, &w).expect("should map") else {
            panic!("expected PhaseBody payload")
        };
        assert_eq!(rec.kind, WalRecordKind::EntityUnmerge);
    }

    #[test]
    fn set_extractor_enabled_maps_to_graph_extractor_toggle() {
        let phase = Phase::SetExtractorEnabled {
            id: brain_core::ExtractorId::from(3),
            enabled: false,
        };
        let w = write_for(phase.clone());
        let WalPayload::PhaseBody(rec) = phase_to_wal_payload(&phase, &w).expect("should map") else {
            panic!("expected PhaseBody payload")
        };
        assert_eq!(rec.kind, WalRecordKind::ExtractorToggle);
    }

    #[test]
    fn upsert_statement_maps_to_graph_statement_create() {
        use brain_core::{
            ExtractorId, PredicateId, StatementId, StatementKind, StatementObject, StatementValue,
            SubjectRef,
        };
        let phase = Phase::UpsertStatement {
            id: StatementId::new(),
            kind: StatementKind::Fact,
            subject: SubjectRef::Entity(EntityId::new()),
            predicate: PredicateId::from(0),
            object: StatementObject::Value(StatementValue::Text("blue".into())),
            confidence: 0.9,
            evidence: crate::write::EvidenceRefPhase::Inline(vec![]),
            valid_from_unix_nanos: None,
            extractor: ExtractorId::from(0),
            extracted_at_unix_nanos: 1_700_000_000_000,
            schema_version: 1,
            predicate_intern_hint: Some(("brain".into(), "likes".into())),
        };
        let w = write_for(phase.clone());
        let WalPayload::PhaseBody(rec) = phase_to_wal_payload(&phase, &w).expect("should map") else {
            panic!("expected PhaseBody payload")
        };
        assert_eq!(rec.kind, WalRecordKind::StatementCreate);
        assert!(!rec.body.is_empty());
    }

    #[test]
    fn tombstone_statement_maps_to_graph_statement_tombstone() {
        let phase = Phase::Tombstone {
            target: TombstoneTarget::Statement(brain_core::StatementId::new()),
            reason: 0,
            at_unix_nanos: 1_700_000_000_000,
        };
        let w = write_for(phase.clone());
        let WalPayload::PhaseBody(rec) = phase_to_wal_payload(&phase, &w).expect("should map") else {
            panic!("expected PhaseBody payload")
        };
        assert_eq!(rec.kind, WalRecordKind::StatementTombstone);
    }

    #[test]
    fn upsert_relation_maps_to_relation_link_with_hint() {
        use brain_core::{ExtractorId, RelationId, RelationTypeId};
        let id = RelationId::new();
        let from = EntityId::new();
        let phase = Phase::UpsertRelation {
            id,
            ty: RelationTypeId::from(0),
            from,
            to: EntityId::new(),
            confidence: 0.9,
            evidence_memories: vec![],
            is_symmetric: true,
            extractor: ExtractorId::from(0),
            extracted_at_unix_nanos: 1_700_000_000_000,
            properties_blob: vec![],
            valid_from_unix_nanos: None,
            valid_to_unix_nanos: None,
            relation_type_intern_hint: Some(("app".into(), "works_with".into())),
        };
        let w = write_for(phase.clone());
        let WalPayload::RelationLink(rl) = phase_to_wal_payload(&phase, &w).expect("should map")
        else {
            panic!("expected RelationLink payload")
        };
        assert_eq!(rl.relation_id, id);
        assert_eq!(rl.from, NodeRef::Entity(from));
        assert!(rl.is_symmetric);
        assert!(rl.relation_type_intern_hint.is_some());
    }

    #[test]
    fn tombstone_relation_maps_to_relation_tombstone() {
        let phase = Phase::Tombstone {
            target: TombstoneTarget::Relation(brain_core::RelationId::new()),
            reason: 0,
            at_unix_nanos: 1_700_000_000_000,
        };
        let w = write_for(phase.clone());
        let WalPayload::RelationTombstone(rt) = phase_to_wal_payload(&phase, &w).expect("should map")
        else {
            panic!("expected RelationTombstone payload")
        };
        assert_eq!(rt.at_unix_nanos, 1_700_000_000_000);
    }

    #[test]
    fn supersede_relation_maps_to_relation_supersede() {
        use brain_core::{ExtractorId, Relation, RelationId, RelationTypeId};
        let old_id = RelationId::new();
        let new_id = RelationId::new();
        let new_rel = Relation {
            id: new_id,
            relation_type: RelationTypeId::from(7),
            from_entity: EntityId::new(),
            to_entity: EntityId::new(),
            properties_blob: vec![],
            confidence: 0.9,
            evidence: vec![],
            extractor_id: ExtractorId::from(0),
            extracted_at_unix_nanos: 1_700_000_000_000,
            valid_from_unix_nanos: None,
            valid_to_unix_nanos: None,
            version: 2,
            superseded_by: None,
            supersedes: Some(old_id),
            chain_root: old_id,
            tombstoned: false,
            tombstoned_at_unix_nanos: None,
            is_symmetric: false,
        };
        let phase = Phase::Supersede {
            target: SupersedeTarget::Relation(old_id),
            replacement: SupersedeReplacement::Relation(Box::new(new_rel)),
            at_unix_nanos: 1_700_000_000_001,
        };
        let w = write_for(phase.clone());
        let WalPayload::RelationSupersede(rs) = phase_to_wal_payload(&phase, &w).expect("should map")
        else {
            panic!("expected RelationSupersede payload")
        };
        assert_eq!(rs.old_relation_id, old_id);
        assert_eq!(rs.new.relation_id, new_id);
    }

    #[test]
    fn link_maps_to_link_payload() {
        let phase = Phase::Link {
            from: NodeRef::Memory(MemoryId::pack(0, 1, 0)),
            to: NodeRef::Memory(MemoryId::pack(0, 2, 0)),
            kind: EdgeKindRef::Builtin(EdgeKind::SimilarTo),
            weight: 0.42,
            origin: 0,
            derived_by: 0,
            disambiguator: zero_disambiguator(),
            created_at_unix_nanos: 1_700_000_000_000,
        };
        let w = write_for(phase.clone());
        let payload = phase_to_wal_payload(&phase, &w).expect("link should map");
        let WalPayload::Link(lp) = payload else {
            panic!("expected Link payload")
        };
        assert_eq!(lp.source, NodeRef::Memory(MemoryId::pack(0, 1, 0)));
        assert_eq!(lp.target, NodeRef::Memory(MemoryId::pack(0, 2, 0)));
        assert_eq!(lp.edge_kind, EdgeKindRef::Builtin(EdgeKind::SimilarTo));
        assert_eq!(lp.weight, 0.42);
        assert_eq!(lp.origin, EdgeOrigin::Explicit);
    }

    #[test]
    fn link_auto_derived_origin_propagates() {
        use brain_metadata::tables::edge::origin;
        let phase = Phase::Link {
            from: NodeRef::Memory(MemoryId::pack(0, 1, 0)),
            to: NodeRef::Memory(MemoryId::pack(0, 2, 0)),
            kind: EdgeKindRef::Builtin(EdgeKind::SimilarTo),
            weight: 0.5,
            origin: origin::AUTO_DERIVED,
            derived_by: 0,
            disambiguator: zero_disambiguator(),
            created_at_unix_nanos: 0,
        };
        let w = write_for(phase.clone());
        let WalPayload::Link(lp) = phase_to_wal_payload(&phase, &w).unwrap() else {
            panic!()
        };
        assert_eq!(lp.origin, EdgeOrigin::AutoDerived);
    }

    #[test]
    fn unlink_maps_to_unlink_payload() {
        let phase = Phase::Unlink {
            from: NodeRef::Memory(MemoryId::pack(0, 1, 0)),
            to: NodeRef::Memory(MemoryId::pack(0, 2, 0)),
            kind: EdgeKindRef::Builtin(EdgeKind::SimilarTo),
            disambiguator: zero_disambiguator(),
        };
        let w = write_for(phase.clone());
        let WalPayload::Unlink(up) = phase_to_wal_payload(&phase, &w).unwrap() else {
            panic!()
        };
        assert_eq!(up.source, NodeRef::Memory(MemoryId::pack(0, 1, 0)));
        assert_eq!(up.target, NodeRef::Memory(MemoryId::pack(0, 2, 0)));
    }

    #[test]
    fn tombstone_memory_maps_to_forget_with_write_id() {
        let id = MemoryId::pack(0, 1, 0);
        let phase = Phase::Tombstone {
            target: TombstoneTarget::Memory {
                id,
                mode: crate::write::phase::TombstoneMode::Soft,
            },
            reason: 1,
            at_unix_nanos: 1_700_000_000_000,
        };
        let write_id = WriteId::new();
        let w = Write::single(write_id, AgentId::default(), phase.clone());
        let WalPayload::Forget(fp) = phase_to_wal_payload(&phase, &w).unwrap() else {
            panic!()
        };
        assert_eq!(fp.memory_id, id);
        // The WAL's request_id field carries the WriteId.
        assert_eq!(fp.request_id.0, write_id.as_uuid());
        assert_eq!(fp.mode, brain_storage::wal::payload::ForgetMode::Soft);
    }

    #[test]
    fn update_salience_maps_to_one_entry() {
        let phase = Phase::UpdateSalience {
            id: MemoryId::pack(0, 1, 0),
            new_salience: Salience::new(0.75),
        };
        let w = write_for(phase.clone());
        let WalPayload::UpdateSalience(p) = phase_to_wal_payload(&phase, &w).unwrap() else {
            panic!()
        };
        assert_eq!(p.updates.len(), 1);
        assert_eq!(p.updates[0].memory_id, MemoryId::pack(0, 1, 0));
        assert!((p.updates[0].new_salience - 0.75).abs() < 1e-6);
    }

    #[test]
    fn update_kind_maps_through() {
        let phase = Phase::UpdateKind {
            id: MemoryId::pack(0, 1, 0),
            new_kind: MemoryKind::Semantic,
        };
        let w = write_for(phase.clone());
        let WalPayload::UpdateKind(p) = phase_to_wal_payload(&phase, &w).unwrap() else {
            panic!()
        };
        assert_eq!(p.memory_id, MemoryId::pack(0, 1, 0));
        assert_eq!(p.new_kind, MemoryKind::Semantic);
    }

    #[test]
    fn update_context_maps_through() {
        let phase = Phase::UpdateContext {
            id: MemoryId::pack(0, 1, 0),
            new_context: ContextId(42),
        };
        let w = write_for(phase.clone());
        let WalPayload::UpdateContext(p) = phase_to_wal_payload(&phase, &w).unwrap() else {
            panic!()
        };
        assert_eq!(p.memory_id, MemoryId::pack(0, 1, 0));
        assert_eq!(p.new_context_id, ContextId(42));
    }

    #[test]
    fn upsert_memory_maps_to_encode_payload() {
        let id = MemoryId::pack(0, 1, 0);
        let phase = Phase::UpsertMemory {
            id,
            text: "hello".into(),
            vector: Box::new([0.5_f32; brain_embed::VECTOR_DIM]),
            kind: MemoryKind::Episodic,
            salience: Salience::new(0.7),
            context: ContextId(3),
            created_at_unix_nanos: 1_700_000_000_000,
            arena_slot: 7,
            embedding_model_fp: [0xCC; 16],
            content_hash: Some([0xDD; 32]),
            deduplicate: true,
        };
        let w = write_for(phase.clone());
        let WalPayload::Encode(ep) = phase_to_wal_payload(&phase, &w).unwrap() else {
            panic!("expected Encode payload")
        };
        assert_eq!(ep.memory_id, id);
        assert_eq!(ep.context_id, ContextId(3));
        assert_eq!(ep.kind, MemoryKind::Episodic);
        assert!((ep.salience_initial - 0.7).abs() < 1e-6);
        assert_eq!(ep.embedding_model_fp, [0xCC; 16]);
        assert_eq!(ep.text, "hello");
        assert_eq!(ep.vector.len(), brain_embed::VECTOR_DIM);
        assert!(
            ep.edges.is_empty(),
            "unified path puts edges in their own phases"
        );
        assert!(ep.deduplicate);
    }

    #[test]
    fn phases_without_mapping_return_none() {
        // ReclaimSlots — derivable from MEMORIES_TABLE state on
        // recovery; never WAL'd.
        let phase = Phase::ReclaimSlots { slots: vec![1, 2] };
        let w = write_for(phase.clone());
        assert!(phase_to_wal_payload(&phase, &w).is_none());

        // RejectMerge — resolves a merge proposal at apply time; needs a
        // handler-side pre-resolution before it can be WAL-mapped.
        let phase = Phase::RejectMerge {
            proposal_id: brain_core::MergeId::new(),
            at_unix_nanos: 1_700_000_000_000,
        };
        let w = write_for(phase.clone());
        assert!(phase_to_wal_payload(&phase, &w).is_none());
    }
}
