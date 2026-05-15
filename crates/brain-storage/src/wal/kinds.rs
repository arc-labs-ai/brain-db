//! WAL record kind discriminator.
//!
//! This module defines the `record_type` byte from `spec/05_storage_arena_wal/
//! 05_wal_records.md` §3 and the knowledge-layer extensions from
//! `spec/26_knowledge_storage/00_purpose.md`.
//!
//! ## Discriminant ranges
//!
//! - **1..=15**  — substrate kinds (per `spec/05/05_wal_records.md` §3).
//! - **16..=80** — knowledge-layer kinds (per `spec/26` "WAL frame types"),
//!   with reserved gaps inside the block for future grouping.
//! - **81..=127** — reserved for v1 minor versions.
//! - **128..**   — reserved for v2+ (incompatible format).
//!
//! Sub-task 15.2 introduced the knowledge-layer block. Their bodies are
//! treated as opaque `Vec<u8>` payloads by the framing layer; the typed
//! body schemas land in phases 16 (entities), 17 (statements), 18
//! (relations), 19 (schema DSL), 20+ (audit).

/// One variant per spec'd `record_type` byte.
///
/// The discriminant matches the spec table exactly so casts to/from `u8`
/// are the on-disk encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum WalRecordKind {
    // ---- Substrate (spec §05/05 §3) ----
    Encode = 1,
    Forget = 2,
    Link = 3,
    Unlink = 4,
    UpdateSalience = 5,
    Reclaim = 6,
    Consolidate = 7,
    UpdateKind = 8,
    UpdateContext = 9,
    CheckpointBegin = 10,
    CheckpointEnd = 11,
    TxnBegin = 12,
    TxnCommit = 13,
    TxnAbort = 14,
    MigrateEmbedding = 15,

    // ---- Knowledge layer (spec §26 "WAL frame types") ----
    /// 0x10 — entity creation.
    EntityCreate = 0x10,
    /// 0x11 — entity attribute / alias update.
    EntityUpdate = 0x11,
    /// 0x12 — merge of one entity into another.
    EntityMerge = 0x12,
    /// 0x13 — entity tombstoned.
    EntityTombstone = 0x13,
    /// 0x20 — statement creation.
    StatementCreate = 0x20,
    /// 0x21 — supersession of an existing statement.
    StatementSupersede = 0x21,
    /// 0x22 — statement tombstoned.
    StatementTombstone = 0x22,
    /// 0x30 — relation creation.
    RelationCreate = 0x30,
    /// 0x31 — supersession of an existing relation.
    RelationSupersede = 0x31,
    /// 0x32 — relation tombstoned.
    RelationTombstone = 0x32,
    /// 0x40 — schema document uploaded.
    SchemaUpdate = 0x40,
    /// 0x50 — extractor / resolution audit entry.
    Audit = 0x50,
}

impl WalRecordKind {
    /// Inverse of the `#[repr(u8)]` cast. Returns `None` for `0`
    /// (reserved per spec) and any value not in a defined slot of the
    /// substrate (1..=15) or knowledge-layer (per spec §26) tables.
    pub const fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            // Substrate (spec §05/05).
            1 => Self::Encode,
            2 => Self::Forget,
            3 => Self::Link,
            4 => Self::Unlink,
            5 => Self::UpdateSalience,
            6 => Self::Reclaim,
            7 => Self::Consolidate,
            8 => Self::UpdateKind,
            9 => Self::UpdateContext,
            10 => Self::CheckpointBegin,
            11 => Self::CheckpointEnd,
            12 => Self::TxnBegin,
            13 => Self::TxnCommit,
            14 => Self::TxnAbort,
            15 => Self::MigrateEmbedding,
            // Knowledge layer (spec §26).
            0x10 => Self::EntityCreate,
            0x11 => Self::EntityUpdate,
            0x12 => Self::EntityMerge,
            0x13 => Self::EntityTombstone,
            0x20 => Self::StatementCreate,
            0x21 => Self::StatementSupersede,
            0x22 => Self::StatementTombstone,
            0x30 => Self::RelationCreate,
            0x31 => Self::RelationSupersede,
            0x32 => Self::RelationTombstone,
            0x40 => Self::SchemaUpdate,
            0x50 => Self::Audit,
            _ => return None,
        })
    }

    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// `true` for knowledge-layer kinds (discriminant `0x10..=0x50`).
    /// The substrate WAL apply-paths ignore these; knowledge-layer
    /// hydration is performed by later phases via their own sinks.
    #[must_use]
    pub const fn is_knowledge(self) -> bool {
        let d = self as u8;
        d >= 0x10 && d <= 0x50
    }
}

/// Every spec'd kind, in declaration order. Useful for exhaustive tests.
pub const ALL_KINDS: &[WalRecordKind] = &[
    // Substrate.
    WalRecordKind::Encode,
    WalRecordKind::Forget,
    WalRecordKind::Link,
    WalRecordKind::Unlink,
    WalRecordKind::UpdateSalience,
    WalRecordKind::Reclaim,
    WalRecordKind::Consolidate,
    WalRecordKind::UpdateKind,
    WalRecordKind::UpdateContext,
    WalRecordKind::CheckpointBegin,
    WalRecordKind::CheckpointEnd,
    WalRecordKind::TxnBegin,
    WalRecordKind::TxnCommit,
    WalRecordKind::TxnAbort,
    WalRecordKind::MigrateEmbedding,
    // Knowledge layer.
    WalRecordKind::EntityCreate,
    WalRecordKind::EntityUpdate,
    WalRecordKind::EntityMerge,
    WalRecordKind::EntityTombstone,
    WalRecordKind::StatementCreate,
    WalRecordKind::StatementSupersede,
    WalRecordKind::StatementTombstone,
    WalRecordKind::RelationCreate,
    WalRecordKind::RelationSupersede,
    WalRecordKind::RelationTombstone,
    WalRecordKind::SchemaUpdate,
    WalRecordKind::Audit,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discriminants_match_spec_table() {
        // Substrate (spec/05_storage_arena_wal/05_wal_records.md §3).
        assert_eq!(WalRecordKind::Encode.as_u8(), 1);
        assert_eq!(WalRecordKind::Forget.as_u8(), 2);
        assert_eq!(WalRecordKind::Reclaim.as_u8(), 6);
        assert_eq!(WalRecordKind::CheckpointEnd.as_u8(), 11);
        assert_eq!(WalRecordKind::MigrateEmbedding.as_u8(), 15);

        // Knowledge layer (spec/26_knowledge_storage/00_purpose.md).
        assert_eq!(WalRecordKind::EntityCreate.as_u8(), 0x10);
        assert_eq!(WalRecordKind::EntityTombstone.as_u8(), 0x13);
        assert_eq!(WalRecordKind::StatementCreate.as_u8(), 0x20);
        assert_eq!(WalRecordKind::RelationCreate.as_u8(), 0x30);
        assert_eq!(WalRecordKind::SchemaUpdate.as_u8(), 0x40);
        assert_eq!(WalRecordKind::Audit.as_u8(), 0x50);
    }

    #[test]
    fn from_u8_round_trips_every_kind() {
        for &k in ALL_KINDS {
            assert_eq!(WalRecordKind::from_u8(k.as_u8()), Some(k));
        }
    }

    #[test]
    fn from_u8_rejects_reserved_and_unknown() {
        assert_eq!(WalRecordKind::from_u8(0), None); // reserved per spec
        // Gaps inside the substrate block — none, 1..=15 are all populated.
        // Gaps inside the knowledge-layer block (entity 0x14..=0x1F, etc.).
        assert_eq!(WalRecordKind::from_u8(0x14), None);
        assert_eq!(WalRecordKind::from_u8(0x23), None);
        assert_eq!(WalRecordKind::from_u8(0x60), None); // beyond 0x50 audit
        assert_eq!(WalRecordKind::from_u8(96), None); // 0x60 in decimal
        assert_eq!(WalRecordKind::from_u8(128), None); // reserved for v2+
        assert_eq!(WalRecordKind::from_u8(255), None);
    }

    #[test]
    fn all_kinds_covers_every_variant() {
        // If a new variant is added without updating ALL_KINDS, this
        // catches it via the byte set.
        let seen: std::collections::HashSet<u8> = ALL_KINDS.iter().map(|k| k.as_u8()).collect();
        assert_eq!(seen.len(), 27, "15 substrate + 12 knowledge = 27 kinds");
        for v in 1..=15u8 {
            assert!(seen.contains(&v), "substrate kind {v} missing from ALL_KINDS");
        }
        for v in [0x10, 0x11, 0x12, 0x13, 0x20, 0x21, 0x22, 0x30, 0x31, 0x32, 0x40, 0x50] {
            assert!(seen.contains(&v), "knowledge kind 0x{v:02X} missing from ALL_KINDS");
        }
    }

    #[test]
    fn is_knowledge_partition() {
        // Substrate kinds are NOT knowledge.
        for k in [
            WalRecordKind::Encode,
            WalRecordKind::Forget,
            WalRecordKind::MigrateEmbedding,
        ] {
            assert!(!k.is_knowledge(), "{k:?} should not be knowledge");
        }
        // Knowledge kinds ARE knowledge.
        for k in [
            WalRecordKind::EntityCreate,
            WalRecordKind::StatementSupersede,
            WalRecordKind::Audit,
        ] {
            assert!(k.is_knowledge(), "{k:?} should be knowledge");
        }
    }
}
