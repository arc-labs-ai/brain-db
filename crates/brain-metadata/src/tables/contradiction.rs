//! `statement_contradiction_audit` — durable record of Fact-vs-Fact
//! contradictions detected at `statement_create` time.
//!
//! When a new Fact disagrees with an already-active Fact on the same
//! `(subject, predicate)` (different object, overlapping validity), the
//! insert still proceeds — coexisting Facts are allowed — but the
//! conflict is recorded here so an operator can find and reconcile it
//! via `ADMIN_LIST_PENDING_CONTRADICTIONS`.
//!
//! The row is keyed by `(subject, predicate_id)`: at most one open
//! contradiction per subject+predicate, so re-detection updates the same
//! row rather than fanning out duplicates. The row is **self-contained**
//! — it stores the contradicting statement ids — so liveness is
//! re-checked directly against `statements` at list time (the
//! `statements_by_subject` index is single-value and cannot enumerate
//! coexisting Facts).

use crate::impl_redb_rkyv_value;
use brain_core::AuditId;
use redb::TableDefinition;

/// `ContradictionAudit::outcome` byte values. Stable; never reassigned.
pub mod contradiction_outcome {
    /// The contradiction is still live (≥2 coexisting current Facts with
    /// distinct objects). Surfaced by `ADMIN_LIST_PENDING_CONTRADICTIONS`.
    pub const PENDING: u8 = 0;
    /// The contradiction no longer holds — one side was superseded,
    /// retracted, or tombstoned, leaving ≤1 distinct object.
    pub const RESOLVED: u8 = 1;
}

/// `(subject_bytes, predicate_id) → ContradictionAudit`. One open row per
/// subject+predicate.
pub const STATEMENT_CONTRADICTION_AUDIT_TABLE: TableDefinition<
    'static,
    ([u8; 16], u32),
    ContradictionAudit,
> = TableDefinition::new("statement_contradiction_audit");

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct ContradictionAudit {
    /// Stable UUIDv7, allocated at first detection and preserved across
    /// updates. Lets operators reference a specific contradiction.
    pub audit_id_bytes: [u8; 16],
    pub subject_bytes: [u8; 16],
    pub predicate_id: u32,
    /// The disagreeing statement ids. Pruned to the still-live subset
    /// whenever the row is re-checked.
    pub contradicting_statement_ids: Vec<[u8; 16]>,
    pub detected_at_unix_nanos: u64,
    /// Wall-clock at which the contradiction was found resolved, or
    /// `None` while still pending.
    pub resolved_at_unix_nanos: Option<u64>,
    /// [`contradiction_outcome`] discriminant.
    pub outcome: u8,
}

impl ContradictionAudit {
    #[must_use]
    pub fn audit_id(&self) -> AuditId {
        AuditId::from(self.audit_id_bytes)
    }
}

impl_redb_rkyv_value!(ContradictionAudit, "brain_metadata::ContradictionAudit");
