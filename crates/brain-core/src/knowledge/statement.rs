//! `Statement` value types — Layer 3 of the knowledge graph.
//!
//! Pure value types — no I/O, no async, no rkyv. The rkyv-archived
//! storage shape lives in `brain-metadata::tables::knowledge::statement`
//! and the wire-archived shape lives in
//! `brain-protocol::knowledge::statement_*`. Conversion between this
//! brain-core value type and those layers is the respective layer's
//! responsibility (via `From` impls).
//!
//! See `spec/19_statements/00_purpose.md` for the canonical schema and
//! `spec/19_statements/{01_supersession, 02_contradiction, 04_confidence, 05_evidence}.md`
//! for the kind-specific contracts.

use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::knowledge::ids::{
    EntityId, EvidenceOverflowId, ExtractorId, PredicateId, StatementId,
};
use crate::knowledge::kinds::StatementKind;
use crate::knowledge::AuditId;
use crate::MemoryId;

// ---------------------------------------------------------------------------
// Constants.
// ---------------------------------------------------------------------------

/// Inline-evidence cap per spec §19/05 §2. Statements with more than
/// this many evidence entries spill to [`EvidenceRef::Overflow`].
pub const INLINE_EVIDENCE_CAP: usize = 8;

// ---------------------------------------------------------------------------
// SubjectRef.
// ---------------------------------------------------------------------------

/// The subject of a statement — either a resolved entity or a pending
/// resolution audit (when the resolver returns `Ambiguous`).
///
/// Spec `§19/00` §"Schema".
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum SubjectRef {
    Entity(EntityId),
    /// The resolver returned `Ambiguous`; the bound audit row records
    /// the candidate set. Query paths excluding pending subjects skip
    /// these.
    Pending(AuditId),
}

impl SubjectRef {
    /// Returns the resolved entity id if this is `Entity`, else `None`.
    #[must_use]
    pub fn as_entity(&self) -> Option<EntityId> {
        match self {
            Self::Entity(id) => Some(*id),
            Self::Pending(_) => None,
        }
    }

    /// True iff this subject is bound to an audit row pending review.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        matches!(self, Self::Pending(_))
    }
}

// ---------------------------------------------------------------------------
// StatementValue (literal scalar).
// ---------------------------------------------------------------------------

/// Typed literal value used in [`StatementObject::Value`].
///
/// Spec `§19/00` lists "typed literal" without specifying the variants
/// — these match the value types the wire layer encodes in
/// `brain-protocol::knowledge::statement_resp::StatementValueWire`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum StatementValue {
    Text(String),
    Integer(i64),
    Float(f64),
    Bool(bool),
    UnixNanos(u64),
    /// Opaque bytes (caps at 64 KiB at the wire layer per
    /// `spec/28_knowledge_wire_protocol/04_validation.md`).
    Blob(Vec<u8>),
}

impl Eq for StatementValue {}

impl StatementValue {
    /// True if `other` represents the same value. Useful for
    /// contradiction detection — different `object` variants always
    /// differ, same variant compares structurally.
    #[must_use]
    pub fn matches(&self, other: &StatementValue) -> bool {
        self == other
    }
}

// ---------------------------------------------------------------------------
// StatementObject (tagged union).
// ---------------------------------------------------------------------------

/// The object of a statement. Spec §19/00 §"Schema":
///
/// ```text
/// enum StatementObject {
///     Entity(EntityId),
///     Value(typed_literal),
///     Memory(MemoryId),
///     Statement(StatementId),   // meta-statement
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum StatementObject {
    Entity(EntityId),
    Value(StatementValue),
    Memory(MemoryId),
    /// Meta-statement — this object is itself a statement. Used for
    /// claims about claims (provenance, retraction reasons, etc.).
    Statement(StatementId),
}

impl Eq for StatementObject {}

impl StatementObject {
    /// Discriminant byte. Stable across versions; used in audit and
    /// for contradiction-comparison fast paths.
    #[must_use]
    pub const fn discriminant(&self) -> u8 {
        match self {
            Self::Entity(_) => 0,
            Self::Value(_) => 1,
            Self::Memory(_) => 2,
            Self::Statement(_) => 3,
        }
    }

    /// Resolved entity id if this object is an `Entity(_)`, else
    /// `None`. Convenience for graph queries.
    #[must_use]
    pub fn as_entity(&self) -> Option<EntityId> {
        match self {
            Self::Entity(id) => Some(*id),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// EvidenceEntry + EvidenceRef.
// ---------------------------------------------------------------------------

/// One piece of evidence backing a statement. Spec `§19/05 §1`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct EvidenceEntry {
    pub memory_id: MemoryId,
    /// Per-source confidence in `[0, 1]`. Aggregated into the
    /// statement's `confidence` via the noisy-OR in `§19/04`.
    pub confidence_milli: u16,
    /// When the evidence was first observed. Drives decay in
    /// `§19/04 §3`.
    pub timestamp_unix_nanos: u64,
    /// Which extractor (`0` = user-authored).
    pub extractor_id: ExtractorId,
}

impl EvidenceEntry {
    /// Returns `confidence_milli` as a `f32` in `[0, 1]`. Stored as
    /// `u16 / 1000` so the value-type is `Copy` + `Hash` (which f32
    /// is not).
    #[must_use]
    pub fn confidence(&self) -> f32 {
        (self.confidence_milli as f32) / 1000.0
    }

    /// Build from a float `[0, 1]` confidence.
    ///
    /// # Panics
    /// Panics if `confidence` is not in `[0, 1]` or is NaN.
    #[must_use]
    pub fn from_parts(
        memory_id: MemoryId,
        confidence: f32,
        timestamp_unix_nanos: u64,
        extractor_id: ExtractorId,
    ) -> Self {
        assert!(
            (0.0..=1.0).contains(&confidence) && !confidence.is_nan(),
            "invariant: confidence must be in [0, 1]"
        );
        let confidence_milli = (confidence * 1000.0).round() as u16;
        Self {
            memory_id,
            confidence_milli: confidence_milli.min(1000),
            timestamp_unix_nanos,
            extractor_id,
        }
    }
}

/// Evidence pointer — inline (up to `INLINE_EVIDENCE_CAP`) or
/// overflow row pointer. Spec `§19/05`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvidenceRef {
    Inline(SmallVec<[EvidenceEntry; INLINE_EVIDENCE_CAP]>),
    /// Pointer to a row in `EVIDENCE_OVERFLOW_TABLE`. Caller resolves
    /// against `brain-metadata::statement_ops::evidence_overflow_load`.
    Overflow(EvidenceOverflowId),
}

impl Default for EvidenceRef {
    fn default() -> Self {
        Self::Inline(SmallVec::new())
    }
}

impl EvidenceRef {
    /// `true` if no evidence backs this statement. Used by the
    /// FORGET cascade in `§19/05 §6` to decide auto-tombstone.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Inline(v) => v.is_empty(),
            // Overflow is non-empty by construction (we don't allocate
            // an overflow row for zero entries).
            Self::Overflow(_) => false,
        }
    }

    /// `Some(n)` if inline; `None` if overflow (caller must load the
    /// row).
    #[must_use]
    pub fn inline_len(&self) -> Option<usize> {
        match self {
            Self::Inline(v) => Some(v.len()),
            Self::Overflow(_) => None,
        }
    }
}

// ---------------------------------------------------------------------------
// TombstoneReason.
// ---------------------------------------------------------------------------

/// Why a statement was tombstoned. Mirrors the byte discriminants in
/// `brain-metadata::tables::knowledge::statement::tombstone_reason`.
///
/// Spec `§19/00` §"Schema".
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub enum TombstoneReason {
    SourceMemoryForgotten = 1,
    UserRequest = 2,
    SchemaInvalidation = 3,
    ExtractorRetraction = 4,
}

impl TombstoneReason {
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    #[must_use]
    pub const fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            1 => Self::SourceMemoryForgotten,
            2 => Self::UserRequest,
            3 => Self::SchemaInvalidation,
            4 => Self::ExtractorRetraction,
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// Statement (the value type).
// ---------------------------------------------------------------------------

/// A typed claim about an entity. Spec `§19/00`.
///
/// Pure value type. The brain-metadata storage layer holds the rkyv-
/// archived form ([`brain_metadata::tables::knowledge::statement::StatementMetadata`]);
/// the wire layer holds the rkyv-archived view
/// ([`brain_protocol::knowledge::statement_resp::StatementView`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Statement {
    pub id: StatementId,
    pub kind: StatementKind,
    pub subject: SubjectRef,
    pub predicate: PredicateId,
    pub object: StatementObject,

    pub confidence: f32,
    pub evidence: EvidenceRef,
    pub extractor_id: ExtractorId,
    pub extracted_at_unix_nanos: u64,
    pub schema_version: u32,

    /// Open-ended if `None`.
    pub valid_from_unix_nanos: Option<u64>,
    /// Open-ended if `None`. Set on supersession (`§19/01 §3.2`).
    pub valid_to_unix_nanos: Option<u64>,
    /// Required for `Event` kind; `None` for `Fact` / `Preference`.
    pub event_at_unix_nanos: Option<u64>,

    pub version: u32,
    pub superseded_by: Option<StatementId>,
    pub supersedes: Option<StatementId>,
    /// Spec `§19/01` — id of the first statement in this chain.
    /// Self-referential for un-superseded statements.
    pub chain_root: StatementId,

    pub tombstoned: bool,
    pub tombstoned_at_unix_nanos: Option<u64>,
    pub tombstone_reason: Option<TombstoneReason>,
}

impl Statement {
    /// Build a fresh, never-superseded statement. Sets:
    /// - `chain_root = id`
    /// - `version = 1`
    /// - `superseded_by = None`, `supersedes = None`
    /// - `tombstoned = false`
    /// - `valid_from / valid_to / event_at = None` (caller may override)
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new_root(
        id: StatementId,
        kind: StatementKind,
        subject: SubjectRef,
        predicate: PredicateId,
        object: StatementObject,
        confidence: f32,
        evidence: EvidenceRef,
        extractor_id: ExtractorId,
        extracted_at_unix_nanos: u64,
        schema_version: u32,
    ) -> Self {
        Self {
            id,
            kind,
            subject,
            predicate,
            object,
            confidence,
            evidence,
            extractor_id,
            extracted_at_unix_nanos,
            schema_version,
            valid_from_unix_nanos: None,
            valid_to_unix_nanos: None,
            event_at_unix_nanos: None,
            version: 1,
            superseded_by: None,
            supersedes: None,
            chain_root: id,
            tombstoned: false,
            tombstoned_at_unix_nanos: None,
            tombstone_reason: None,
        }
    }

    /// `true` iff the statement is the current entry in its chain:
    /// not superseded, not tombstoned, and (if validity-bounded) the
    /// `now` value falls within `[valid_from, valid_to)`.
    ///
    /// Spec `§19/01 §3.1` `is_current` bit definition.
    #[must_use]
    pub fn is_current(&self, now_unix_nanos: u64) -> bool {
        if self.tombstoned || self.superseded_by.is_some() {
            return false;
        }
        if let Some(start) = self.valid_from_unix_nanos {
            if now_unix_nanos < start {
                return false;
            }
        }
        if let Some(end) = self.valid_to_unix_nanos {
            if now_unix_nanos >= end {
                return false;
            }
        }
        true
    }

    /// `true` iff this statement is the chain root (was never
    /// superseded).
    #[must_use]
    pub fn is_chain_root(&self) -> bool {
        self.supersedes.is_none() && self.chain_root == self.id
    }

    /// `true` iff `kind == Event`. Event-specific rules (no
    /// supersession, `event_at` required) gate on this.
    #[must_use]
    pub fn is_event(&self) -> bool {
        matches!(self.kind, StatementKind::Event)
    }
}

// ---------------------------------------------------------------------------
// Predicate (the registry value type).
// ---------------------------------------------------------------------------

/// A registered predicate. Spec `§19/00 §"Predicate vocabulary"`.
///
/// The id is a u32 (`PredicateId`), interned at first use in the
/// `predicates` redb table. `kind_constraint` / `object_type_constraint`
/// gate the validator in `statement_ops::statement_create`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Predicate {
    pub id: PredicateId,
    pub namespace: String,
    pub name: String,
    /// `None` means any kind is allowed for this predicate.
    pub kind_constraint: Option<StatementKind>,
    /// Per-predicate object-type constraint (spec `§21_schema_dsl`).
    /// Phase 17 uses a coarse byte; phase 19's schema DSL replaces
    /// this with a richer typed constraint.
    pub object_type_constraint_byte: u8,
    pub schema_version: u32,
    pub description: String,
}

impl Predicate {
    /// Canonical wire form: `"namespace:name"`.
    #[must_use]
    pub fn canonical(&self) -> String {
        format!("{}:{}", self.namespace, self.name)
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ContextId;

    fn sample_subject() -> SubjectRef {
        SubjectRef::Entity(EntityId::new())
    }

    fn sample_evidence() -> EvidenceRef {
        let entry = EvidenceEntry::from_parts(
            MemoryId::pack(1, ContextId::DEFAULT.into(), 0),
            0.9,
            1_700_000_000_000_000_000,
            ExtractorId::default(),
        );
        EvidenceRef::Inline(SmallVec::from_buf_and_len([entry; INLINE_EVIDENCE_CAP], 1))
    }

    fn sample_statement(kind: StatementKind) -> Statement {
        let id = StatementId::new();
        Statement::new_root(
            id,
            kind,
            sample_subject(),
            PredicateId::default(),
            StatementObject::Value(StatementValue::Text("test".into())),
            0.9,
            sample_evidence(),
            ExtractorId::default(),
            1_700_000_000_000_000_000,
            1,
        )
    }

    // ---- SubjectRef ----

    #[test]
    fn subject_entity_as_entity_returns_id() {
        let id = EntityId::new();
        let s = SubjectRef::Entity(id);
        assert_eq!(s.as_entity(), Some(id));
        assert!(!s.is_pending());
    }

    #[test]
    fn subject_pending_marker() {
        let s = SubjectRef::Pending(AuditId::new());
        assert!(s.is_pending());
        assert_eq!(s.as_entity(), None);
    }

    // ---- StatementValue ----

    #[test]
    fn statement_value_matches_same_variant_same_inner() {
        let a = StatementValue::Text("hi".into());
        let b = StatementValue::Text("hi".into());
        assert!(a.matches(&b));
    }

    #[test]
    fn statement_value_differs_across_variants() {
        let a = StatementValue::Text("42".into());
        let b = StatementValue::Integer(42);
        assert!(!a.matches(&b));
    }

    // ---- StatementObject ----

    #[test]
    fn statement_object_discriminants_are_stable() {
        let e = StatementObject::Entity(EntityId::new());
        let v = StatementObject::Value(StatementValue::Bool(true));
        let m = StatementObject::Memory(MemoryId::pack(0, 0, 0));
        let s = StatementObject::Statement(StatementId::new());
        assert_eq!(e.discriminant(), 0);
        assert_eq!(v.discriminant(), 1);
        assert_eq!(m.discriminant(), 2);
        assert_eq!(s.discriminant(), 3);
    }

    #[test]
    fn statement_object_as_entity() {
        let id = EntityId::new();
        assert_eq!(StatementObject::Entity(id).as_entity(), Some(id));
        assert_eq!(
            StatementObject::Value(StatementValue::Integer(1)).as_entity(),
            None
        );
    }

    // ---- EvidenceEntry / EvidenceRef ----

    #[test]
    fn evidence_entry_round_trips_confidence() {
        let entry = EvidenceEntry::from_parts(
            MemoryId::pack(1, 0, 0),
            0.873,
            1_700_000_000_000_000_000,
            ExtractorId::default(),
        );
        // u16 milli-precision rounds to nearest; 0.873 → 873.
        assert_eq!(entry.confidence_milli, 873);
        assert!((entry.confidence() - 0.873).abs() < 1e-6);
    }

    #[test]
    fn evidence_entry_clamps_at_one() {
        let entry =
            EvidenceEntry::from_parts(MemoryId::pack(1, 0, 0), 1.0, 0, ExtractorId::default());
        assert_eq!(entry.confidence_milli, 1000);
        assert!((entry.confidence() - 1.0).abs() < 1e-6);
    }

    #[test]
    #[should_panic(expected = "invariant: confidence must be in [0, 1]")]
    fn evidence_entry_rejects_out_of_range() {
        let _ = EvidenceEntry::from_parts(
            MemoryId::pack(1, 0, 0),
            1.5,
            0,
            ExtractorId::default(),
        );
    }

    #[test]
    fn evidence_ref_inline_empty_reports_empty() {
        let r: EvidenceRef = EvidenceRef::default();
        assert!(r.is_empty());
        assert_eq!(r.inline_len(), Some(0));
    }

    #[test]
    fn evidence_ref_overflow_is_not_empty() {
        let r = EvidenceRef::Overflow(EvidenceOverflowId::new());
        assert!(!r.is_empty());
        assert_eq!(r.inline_len(), None);
    }

    // ---- TombstoneReason ----

    #[test]
    fn tombstone_reason_round_trips() {
        for r in [
            TombstoneReason::SourceMemoryForgotten,
            TombstoneReason::UserRequest,
            TombstoneReason::SchemaInvalidation,
            TombstoneReason::ExtractorRetraction,
        ] {
            assert_eq!(TombstoneReason::from_u8(r.as_u8()), Some(r));
        }
        assert_eq!(TombstoneReason::from_u8(0), None);
        assert_eq!(TombstoneReason::from_u8(255), None);
    }

    // ---- Statement ----

    #[test]
    fn new_root_self_chains() {
        let s = sample_statement(StatementKind::Fact);
        assert_eq!(s.chain_root, s.id);
        assert_eq!(s.version, 1);
        assert!(s.is_chain_root());
        assert_eq!(s.superseded_by, None);
        assert_eq!(s.supersedes, None);
    }

    #[test]
    fn is_current_for_fresh_statement() {
        let s = sample_statement(StatementKind::Fact);
        assert!(s.is_current(s.extracted_at_unix_nanos));
        assert!(s.is_current(s.extracted_at_unix_nanos + 60_000_000_000));
    }

    #[test]
    fn is_current_false_when_superseded() {
        let mut s = sample_statement(StatementKind::Preference);
        s.superseded_by = Some(StatementId::new());
        assert!(!s.is_current(s.extracted_at_unix_nanos));
    }

    #[test]
    fn is_current_false_when_tombstoned() {
        let mut s = sample_statement(StatementKind::Fact);
        s.tombstoned = true;
        assert!(!s.is_current(s.extracted_at_unix_nanos));
    }

    #[test]
    fn is_current_respects_valid_from() {
        let mut s = sample_statement(StatementKind::Fact);
        s.valid_from_unix_nanos = Some(s.extracted_at_unix_nanos + 60_000_000_000);
        assert!(!s.is_current(s.extracted_at_unix_nanos));
        assert!(s.is_current(s.extracted_at_unix_nanos + 60_000_000_000));
    }

    #[test]
    fn is_current_respects_valid_to() {
        let mut s = sample_statement(StatementKind::Fact);
        s.valid_to_unix_nanos = Some(s.extracted_at_unix_nanos + 60_000_000_000);
        assert!(s.is_current(s.extracted_at_unix_nanos));
        // At valid_to exactly: half-open interval excludes.
        assert!(!s.is_current(s.extracted_at_unix_nanos + 60_000_000_000));
    }

    #[test]
    fn is_event_predicate() {
        let s = sample_statement(StatementKind::Event);
        assert!(s.is_event());
        let s = sample_statement(StatementKind::Fact);
        assert!(!s.is_event());
    }

    // ---- Predicate ----

    #[test]
    fn predicate_canonical_form() {
        let p = Predicate {
            id: PredicateId::from(7),
            namespace: "brain".into(),
            name: "is_a".into(),
            kind_constraint: Some(StatementKind::Fact),
            object_type_constraint_byte: 0,
            schema_version: 1,
            description: "entity type assertion".into(),
        };
        assert_eq!(p.canonical(), "brain:is_a");
    }
}
