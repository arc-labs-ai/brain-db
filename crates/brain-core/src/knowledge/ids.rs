//! Identifier types for the knowledge layer.
//!
//! Two flavors:
//!
//! - **UUIDv7** (16 bytes) for first-class records that need globally
//!   unique IDs and time-ordering: `EntityId`, `StatementId`,
//!   `RelationId`, `AuditId`, `MergeId`, `EvidenceOverflowId`.
//! - **u32 interned** for registry entries that are user-declared and
//!   table-local: `EntityTypeId`, `RelationTypeId`, `PredicateId`,
//!   `ExtractorId`. These are small integers because typical
//!   deployments will have tens to hundreds of each — not millions —
//!   and small keys keep secondary indexes compact.
//!
//! See `spec/17_knowledge_model/00_purpose.md` (identity scheme) and
//! `spec/26_knowledge_storage/00_purpose.md` (table catalog).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// UUIDv7-backed identifiers.
// ---------------------------------------------------------------------------

macro_rules! uuid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
        )]
        pub struct $name(pub Uuid);

        impl $name {
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            #[must_use]
            pub const fn from_uuid(u: Uuid) -> Self {
                Self(u)
            }

            #[must_use]
            pub const fn to_bytes(self) -> [u8; 16] {
                *self.0.as_bytes()
            }

            #[must_use]
            pub const fn from_bytes(b: [u8; 16]) -> Self {
                Self(Uuid::from_bytes(b))
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl From<$name> for [u8; 16] {
            #[inline]
            fn from(id: $name) -> Self {
                id.to_bytes()
            }
        }

        impl From<[u8; 16]> for $name {
            #[inline]
            fn from(b: [u8; 16]) -> Self {
                Self::from_bytes(b)
            }
        }
    };
}

uuid_id! {
    /// Canonical entity identifier (spec §18). UUIDv7; immutable across
    /// renames and attribute updates.
    EntityId
}

uuid_id! {
    /// Statement identifier (spec §19). UUIDv7. A new `StatementId` is
    /// minted on every supersession; the chain is traversed via
    /// `chain_root` in `statement_chain`.
    StatementId
}

uuid_id! {
    /// Relation identifier (spec §20). UUIDv7. A new `RelationId` is
    /// minted on every supersession.
    RelationId
}

uuid_id! {
    /// Audit record identifier (spec §25). UUIDv7 because audits are
    /// append-only and time-ordered traversal is the dominant query
    /// shape.
    AuditId
}

uuid_id! {
    /// Entity-merge record identifier (spec §18 — merge log).
    MergeId
}

uuid_id! {
    /// Evidence overflow row identifier (spec §19 / §26). Points to a
    /// `Vec<MemoryId>` blob when a statement's inline evidence list
    /// outgrows the inline cap (8 by default).
    EvidenceOverflowId
}

// ---------------------------------------------------------------------------
// u32-interned identifiers.
// ---------------------------------------------------------------------------

macro_rules! u32_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Clone,
            Copy,
            Debug,
            Default,
            Eq,
            Hash,
            Ord,
            PartialEq,
            PartialOrd,
            Serialize,
            Deserialize,
        )]
        pub struct $name(pub u32);

        impl $name {
            #[must_use]
            pub const fn raw(self) -> u32 {
                self.0
            }
        }

        impl From<$name> for u32 {
            #[inline]
            fn from(id: $name) -> Self {
                id.0
            }
        }

        impl From<u32> for $name {
            #[inline]
            fn from(raw: u32) -> Self {
                Self(raw)
            }
        }
    };
}

u32_id! {
    /// Interned entity-type identifier (spec §18). Stable within a
    /// deployment; assigned at schema upload (spec §19 / §21).
    EntityTypeId
}

u32_id! {
    /// Interned relation-type identifier (spec §20).
    RelationTypeId
}

u32_id! {
    /// Interned predicate identifier (spec §19). A predicate is a
    /// namespaced string (e.g. `acme:reports_to`); the namespace+name
    /// pair is the primary key in the `predicates` table.
    PredicateId
}

u32_id! {
    /// Extractor identifier (spec §22). Assigned at schema upload.
    ExtractorId
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_round_trip_through_bytes() {
        let id = EntityId::new();
        let bytes = id.to_bytes();
        let back = EntityId::from_bytes(bytes);
        assert_eq!(id, back);
    }

    #[test]
    fn u32_round_trip() {
        let id = EntityTypeId::from(42);
        assert_eq!(id.raw(), 42);
        let back: u32 = id.into();
        assert_eq!(back, 42);
    }

    #[test]
    fn default_uuid_ids_are_unique() {
        let a = StatementId::new();
        let b = StatementId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn default_u32_ids_are_zero() {
        assert_eq!(PredicateId::default().raw(), 0);
        assert_eq!(ExtractorId::default().raw(), 0);
    }
}
