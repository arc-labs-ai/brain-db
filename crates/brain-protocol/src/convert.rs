//! Conversions between `brain_core` enum types and `brain_protocol`
//! wire-mirror enums.
//!
//! The primitive-aliased conversions (`MemoryId ⇄ WireMemoryId`,
//! `ContextId ⇄ WireContextId`, `AgentId / RequestId / TxnId ⇄ WireUuid`)
//! live in `brain_core` rather than here — the wire-domain aliases are
//! type-aliases for primitives, so the From impls must live where the
//! domain types are local (orphan rules).
//!
//! What stays here are the enum mirrors that exist only in this crate
//! because rkyv's closed-world derive needs concrete enums:
//! `MemoryKindWire`, `EdgeKindWire`. Those map back to `brain_core`'s
//! `MemoryKind` / `EdgeKind` via the impls below.

use brain_core::{EdgeKind, MemoryKind};

use crate::request::{EdgeKindWire, MemoryKindWire};

// ---------------------------------------------------------------------------
// MemoryKind  ⇄  MemoryKindWire
// ---------------------------------------------------------------------------

impl From<MemoryKind> for MemoryKindWire {
    #[inline]
    fn from(k: MemoryKind) -> Self {
        match k {
            MemoryKind::Episodic => Self::Episodic,
            MemoryKind::Semantic => Self::Semantic,
            MemoryKind::Consolidated => Self::Consolidated,
        }
    }
}

impl From<MemoryKindWire> for MemoryKind {
    #[inline]
    fn from(k: MemoryKindWire) -> Self {
        match k {
            MemoryKindWire::Episodic => Self::Episodic,
            MemoryKindWire::Semantic => Self::Semantic,
            MemoryKindWire::Consolidated => Self::Consolidated,
        }
    }
}

// ---------------------------------------------------------------------------
// EdgeKind  ⇄  EdgeKindWire
// ---------------------------------------------------------------------------

impl From<EdgeKind> for EdgeKindWire {
    #[inline]
    fn from(k: EdgeKind) -> Self {
        match k {
            EdgeKind::Caused => Self::Caused,
            EdgeKind::FollowedBy => Self::FollowedBy,
            EdgeKind::DerivedFrom => Self::DerivedFrom,
            EdgeKind::SimilarTo => Self::SimilarTo,
            EdgeKind::Contradicts => Self::Contradicts,
            EdgeKind::Supports => Self::Supports,
            EdgeKind::References => Self::References,
            EdgeKind::PartOf => Self::PartOf,
        }
    }
}

impl From<EdgeKindWire> for EdgeKind {
    #[inline]
    fn from(k: EdgeKindWire) -> Self {
        match k {
            EdgeKindWire::Caused => Self::Caused,
            EdgeKindWire::FollowedBy => Self::FollowedBy,
            EdgeKindWire::DerivedFrom => Self::DerivedFrom,
            EdgeKindWire::SimilarTo => Self::SimilarTo,
            EdgeKindWire::Contradicts => Self::Contradicts,
            EdgeKindWire::Supports => Self::Supports,
            EdgeKindWire::References => Self::References,
            EdgeKindWire::PartOf => Self::PartOf,
        }
    }
}

#[cfg(test)]
mod tests {
    use brain_core::{AgentId, ContextId, EdgeKind, MemoryId, MemoryKind, RequestId, TxnId};

    use super::*;
    use crate::request::{WireContextId, WireMemoryId, WireUuid};

    #[test]
    fn memory_id_round_trips_via_wire() {
        let id = MemoryId::pack(7, 0x1234_5678, 42);
        let wire: WireMemoryId = id.into();
        let back: MemoryId = wire.into();
        assert_eq!(back, id);
        assert_eq!(back.shard(), 7);
        assert_eq!(back.slot(), 0x1234_5678);
        assert_eq!(back.version(), 42);
    }

    #[test]
    fn context_id_round_trips_via_wire() {
        let id = ContextId(0x0123_4567_89AB_CDEF);
        let wire: WireContextId = id.into();
        let back: ContextId = wire.into();
        assert_eq!(back, id);
    }

    #[test]
    fn agent_request_txn_round_trip_via_wire() {
        let agent = AgentId::new();
        let request = RequestId::new();
        let txn = TxnId::new();

        let agent_wire: WireUuid = agent.into();
        let request_wire: WireUuid = request.into();
        let txn_wire: WireUuid = txn.into();

        assert_eq!(AgentId::from(agent_wire), agent);
        assert_eq!(RequestId::from(request_wire), request);
        assert_eq!(TxnId::from(txn_wire), txn);
    }

    #[test]
    fn memory_kind_round_trips_each_variant() {
        for k in [
            MemoryKind::Episodic,
            MemoryKind::Semantic,
            MemoryKind::Consolidated,
        ] {
            let wire: MemoryKindWire = k.into();
            let back: MemoryKind = wire.into();
            assert_eq!(back, k);
        }
    }

    #[test]
    fn edge_kind_round_trips_each_variant() {
        for k in [
            EdgeKind::Caused,
            EdgeKind::FollowedBy,
            EdgeKind::DerivedFrom,
            EdgeKind::SimilarTo,
            EdgeKind::Contradicts,
            EdgeKind::Supports,
            EdgeKind::References,
            EdgeKind::PartOf,
        ] {
            let wire: EdgeKindWire = k.into();
            let back: EdgeKind = wire.into();
            assert_eq!(back, k);
        }
    }
}
