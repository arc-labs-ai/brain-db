//! `forget_undo_log` table: the additive undo journal that makes a
//! **soft** FORGET cascade reversible within the tombstone-grace window.
//!
//! The forward cascade ([`crate::cascade::cascade_forget_to_statements`]
//! / [`crate::cascade::cascade_forget_to_edges`]) strips a forgotten
//! memory from every dependent statement / relation: it drops the
//! evidence entry, removes the `STATEMENTS_BY_EVIDENCE` /
//! `RELATION_BY_EVIDENCE` reverse-index row, recomputes confidence, and
//! — when the memory was the sole evidence — tombstones the row with
//! reason `SourceMemoryForgotten`. All of that is lossy: nothing else
//! records which rows cited the memory or at what confidence.
//!
//! For a soft FORGET the cascade therefore writes one
//! [`ForgetUndoRecord`] per mutated dependent row *before/as* it mutates
//! it, capturing exactly what is needed to reverse the change. The
//! [`crate::cascade::cascade_revert_forget`] executor replays these
//! records to restore the pre-FORGET state, then deletes each consumed
//! row so a second revert run is a structural no-op.
//!
//! A **hard** FORGET is irreversible by design (the privacy escape
//! hatch), so it writes no undo records. Once the tombstone-grace window
//! passes and slot reclamation runs, the undo rows for that memory are
//! deleted too — after grace the forget can no longer be undone.
//!
//! ## Additive, not a schema migration
//!
//! This table is created lazily by [`crate::tables::materialize_all_tables`]
//! on open — redb materializes tables on demand. It adds no column to any
//! existing row and requires no `CURRENT_SCHEMA_VERSION` bump.

use redb::TableDefinition;

/// `(forgotten_memory_id_bytes, dependent_record_id_bytes)` →
/// [`ForgetUndoRecord`].
///
/// The leading `forgotten_memory_id` makes every dependent of one
/// forgotten memory a contiguous key range, so
/// [`crate::cascade::cascade_revert_forget`] can prefix-scan them
/// cheaply. The trailing `dependent_record_id` is the `StatementId` /
/// `RelationId` (16-byte); [`ForgetUndoRecord::record_kind`]
/// disambiguates which table it addresses.
pub const FORGET_UNDO_LOG_TABLE: TableDefinition<'static, ([u8; 16], [u8; 16]), ForgetUndoRecord> =
    TableDefinition::new("forget_undo_log");

/// [`ForgetUndoRecord::record_kind`] byte values.
pub mod record_kind {
    /// The dependent row lives in `STATEMENTS_TABLE`.
    pub const STATEMENT: u8 = 1;
    /// The dependent row lives in `RELATION_METADATA_TABLE`.
    pub const RELATION: u8 = 2;
}

/// [`ForgetUndoRecord::outcome`] byte values — what the forward cascade
/// did to the dependent row, so the revert executor knows how to invert
/// it.
pub mod outcome {
    /// The row kept other evidence; the cascade only dropped the
    /// forgotten memory's entry and recomputed confidence.
    pub const EVIDENCE_DROPPED: u8 = 1;
    /// The row's evidence list emptied but the row was kept (confidence
    /// threshold not crossed) with stale-evidence semantics.
    pub const KEPT_STALE: u8 = 2;
    /// The row's evidence list emptied and the row was tombstoned with
    /// reason `SourceMemoryForgotten`.
    pub const TOMBSTONED: u8 = 3;
}

/// One reversible unit of a soft FORGET cascade: everything needed to
/// re-attach the forgotten memory to one dependent statement / relation.
///
/// The dropped-evidence fields mirror
/// [`crate::tables::statement::EvidenceEntryRow`]; for a relation only
/// `dropped_memory_id_bytes` is meaningful (relation evidence carries no
/// per-entry confidence / extractor), and the other three are zero.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct ForgetUndoRecord {
    /// [`record_kind`] discriminant — statement vs relation.
    pub record_kind: u8,
    /// The forgotten memory's id — the evidence entry the cascade
    /// stripped and the revert must re-attach.
    pub dropped_memory_id_bytes: [u8; 16],
    /// Dropped-entry confidence (milli). `0` for relation evidence.
    pub dropped_confidence_milli: u16,
    /// Dropped-entry timestamp. `0` for relation evidence.
    pub dropped_timestamp_unix_nanos: u64,
    /// Dropped-entry extractor id. `0` for relation evidence.
    pub dropped_extractor_id: u32,
    /// The dependent row's confidence before the cascade mutated it.
    pub prior_confidence: f32,
    /// The dependent row's `is_current` byte before the cascade.
    pub prior_is_current: u8,
    /// The dependent row's `tombstone_reason` byte before the cascade
    /// (statements only; `0` for relations, which carry no reason).
    pub prior_tombstone_reason: u8,
    /// The dependent statement's evidence-overflow id before the cascade,
    /// if it owned one (statements only; always `None` for relations).
    pub prior_overflow_id_bytes: Option<[u8; 16]>,
    /// [`outcome`] discriminant — what the cascade did to this row.
    pub outcome: u8,
    /// Wall-clock (unix nanos) after which this undo row is no longer
    /// valid: the source memory's tombstone-grace expiry. Slot
    /// reclamation deletes undo rows past this instant so a post-grace
    /// forget stays irreversible.
    pub grace_expiry_unix_nanos: u64,
}

crate::impl_redb_rkyv_value!(ForgetUndoRecord, "brain_metadata::ForgetUndoRecord");

impl ForgetUndoRecord {
    /// `true` when this record addresses a statement row.
    #[must_use]
    pub fn is_statement(&self) -> bool {
        self.record_kind == record_kind::STATEMENT
    }

    /// `true` when this record addresses a relation row.
    #[must_use]
    pub fn is_relation(&self) -> bool {
        self.record_kind == record_kind::RELATION
    }
}
