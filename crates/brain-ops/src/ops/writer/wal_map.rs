//! `Phase → WalPayload` mapping for the unified write path.
//!
//! This is the first slice of P3b (WAL framing for submit(Write)).
//! Single-phase writes whose phase maps to an existing typed
//! [`WalPayload`] variant get WAL durability automatically.
//!
//! ## Scope
//!
//! Covered (substrate phases with direct typed-payload counterparts):
//! - Link, Unlink, Tombstone(Memory)
//! - UpdateSalience, UpdateKind, UpdateContext
//!
//! Deferred (later P3b slices):
//! - UpsertMemory → WalPayload::Encode. EncodePayload carries
//!   request_hash, response_payload, deduplicate fields that aren't
//!   natively on Phase::UpsertMemory. Adding them needs a Phase shape
//!   extension; lands once handler migration drives the requirement.
//! - UpsertEntity / UpsertStatement / UpsertRelation / Supersede /
//!   UpsertSchema / SetExtractorEnabled / MergeEntities → these need
//!   the `WalPayload::Knowledge` variant with rkyv-encoded bodies; the
//!   body schemas land in a knowledge_bodies.rs follow-up.
//! - Multi-phase writes (wrap in TxnBegin/TxnCommit) — its own slice.
//!
//! Phases without a wire-side WAL event (UpdateEmbedding before
//! arena-write wiring, Resolve, StampAudit, ReclaimSlots, UpdateAttribute):
//! return `None`. The caller (submit()) skips WAL append for them.

use brain_core::EdgeOrigin;
#[cfg(test)]
use brain_core::NodeRef;
use brain_storage::wal::payload::{
    ForgetPayload, ForgetReason, LinkPayload, SalienceReason, SalienceUpdate, UnlinkPayload,
    UpdateContextPayload, UpdateKindPayload, UpdateSaliencePayload, WalPayload,
};

use crate::write::{Phase, TombstoneTarget, Write};

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
            target: TombstoneTarget::Memory { id, mode },
            ..
        } => Some(WalPayload::Forget(ForgetPayload {
            memory_id: *id,
            // The legacy ForgetPayload carries the wire request_id for
            // idempotency replay. The unified path uses WriteId as the
            // analog. They share the UUIDv7 byte layout so we project
            // directly — recovery rebuilds the same idempotency entry
            // regardless of which path wrote it.
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

        // Phases without a WAL mapping in this slice — see the module
        // doc comment for the breakdown. submit() skips WAL append for
        // these (no durability gap because they're either re-derivable
        // from redb state, or because their phases haven't reached the
        // unified path yet).
        _ => None,
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
    use brain_core::{AgentId, ContextId, EdgeKind, EdgeKindRef, MemoryId, MemoryKind, Salience};
    use brain_metadata::tables::edge::zero_disambiguator;

    use crate::write::{Phase, Write, WriteId};

    fn write_for(phase: Phase) -> Write {
        Write::single(WriteId::new(), AgentId::default(), phase)
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
        // The write_id projects into the legacy request_id slot.
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
    fn unsupported_phases_return_none() {
        // UpsertMemory needs EncodePayload extras (request_hash,
        // response_payload, deduplicate) that aren't on the Phase yet
        // — handler-migration slice adds them. Until then: None.
        let phase = Phase::UpsertMemory {
            id: MemoryId::pack(0, 1, 0),
            text: "x".into(),
            vector: Box::new([0.0_f32; brain_embed::VECTOR_DIM]),
            kind: MemoryKind::Episodic,
            salience: Salience::default(),
            context: ContextId(0),
            created_at_unix_nanos: 0,
            arena_slot: 1,
            fingerprint: [0; 16],
        };
        let w = write_for(phase.clone());
        assert!(phase_to_wal_payload(&phase, &w).is_none());

        // ReclaimSlots — derivable from MEMORIES_TABLE state on
        // recovery; never WAL'd.
        let phase = Phase::ReclaimSlots { slots: vec![1, 2] };
        let w = write_for(phase.clone());
        assert!(phase_to_wal_payload(&phase, &w).is_none());
    }
}
