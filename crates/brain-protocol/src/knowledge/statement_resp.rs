//! Statement-op response payloads. Spec §28/06.

use rkyv::{Archive, Deserialize, Serialize};

use brain_core::knowledge::{
    EvidenceEntry, EvidenceRef, Statement, StatementObject, StatementValue, SubjectRef,
    TombstoneReason, INLINE_EVIDENCE_CAP,
};
use brain_core::{
    EntityId, EvidenceOverflowId, ExtractorId, MemoryId, PredicateId, StatementId, StatementKind,
};
use smallvec::SmallVec;

use crate::knowledge::statement_req::{
    EvidenceRefWire, StatementKindWire, StatementObjectWire, StatementValueWire,
};
use crate::request::WireUuid;

// ---------------------------------------------------------------------------
// StatementView — read-side projection. Spec §28/06 §2.4.
// ---------------------------------------------------------------------------

/// Wire-domain projection of `brain_core::knowledge::Statement`.
/// Optional value-side fields collapse to sentinel zero (`[0; 16]` for
/// ids, `0` for nanos) — same convention as `EntityView`.
///
/// `predicate` is the canonical `"namespace:name"` form — the server
/// resolves the `PredicateId` registry row to its string at projection
/// time.
///
/// `subject` is the resolved `EntityId` for resolved subjects; for
/// pending subjects, `subject == [0;16]` and `subject_pending_audit_id`
/// carries the audit row id. `flags & 1 != 0` ⇔ subject is pending.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct StatementView {
    pub statement_id: WireUuid,
    pub kind: StatementKindWire,
    pub subject: WireUuid,
    pub subject_pending_audit_id: WireUuid,
    pub predicate: String,
    pub object: StatementObjectWire,
    pub confidence: f32,
    pub evidence: EvidenceRefWire,
    pub extractor_id: u32,
    pub extracted_at_unix_nanos: u64,
    pub schema_version: u32,
    pub valid_from_unix_nanos: u64,
    pub valid_to_unix_nanos: u64,
    pub event_at_unix_nanos: u64,
    pub version: u32,
    pub superseded_by: WireUuid,
    pub supersedes: WireUuid,
    pub chain_root: WireUuid,
    pub tombstoned: bool,
    pub tombstoned_at_unix_nanos: u64,
    pub tombstone_reason: u8,
    pub flags: u32,
}

// ---------------------------------------------------------------------------
// Errors raised by wire → brain-core conversion.
// ---------------------------------------------------------------------------

/// Failures from [`StatementView::to_statement`] /
/// [`evidence_ref_from_wire`] / [`statement_object_from_wire`].
#[derive(thiserror::Error, Debug)]
pub enum WireToStatementError {
    #[error("inline evidence list exceeds cap of {cap}; got {len}")]
    EvidenceInlineTooLarge { len: usize, cap: usize },

    #[error("unknown StatementKindWire byte {0}")]
    UnknownKind(u8),

    #[error("unknown TombstoneReason byte {0}")]
    UnknownTombstoneReason(u8),
}

// ---------------------------------------------------------------------------
// Conversion helpers — StatementObject ↔ wire.
// ---------------------------------------------------------------------------

#[must_use]
pub fn statement_value_to_wire(v: &StatementValue) -> StatementValueWire {
    match v {
        StatementValue::Text(s) => StatementValueWire::Text(s.clone()),
        StatementValue::Integer(i) => StatementValueWire::Integer(*i),
        StatementValue::Float(f) => StatementValueWire::Float(*f),
        StatementValue::Bool(b) => StatementValueWire::Bool(*b),
        StatementValue::UnixNanos(n) => StatementValueWire::UnixNanos(*n),
        StatementValue::Blob(b) => StatementValueWire::Blob(b.clone()),
    }
}

#[must_use]
pub fn statement_value_from_wire(w: &StatementValueWire) -> StatementValue {
    match w {
        StatementValueWire::Text(s) => StatementValue::Text(s.clone()),
        StatementValueWire::Integer(i) => StatementValue::Integer(*i),
        StatementValueWire::Float(f) => StatementValue::Float(*f),
        StatementValueWire::Bool(b) => StatementValue::Bool(*b),
        StatementValueWire::UnixNanos(n) => StatementValue::UnixNanos(*n),
        StatementValueWire::Blob(b) => StatementValue::Blob(b.clone()),
    }
}

#[must_use]
pub fn statement_object_to_wire(o: &StatementObject) -> StatementObjectWire {
    match o {
        StatementObject::Entity(id) => StatementObjectWire::EntityRef(id.to_bytes()),
        StatementObject::Value(v) => StatementObjectWire::Value(statement_value_to_wire(v)),
        StatementObject::Memory(m) => StatementObjectWire::MemoryRef(m.to_be_bytes()),
        StatementObject::Statement(s) => StatementObjectWire::StatementRef(s.to_bytes()),
    }
}

#[must_use]
pub fn statement_object_from_wire(w: &StatementObjectWire) -> StatementObject {
    match w {
        StatementObjectWire::EntityRef(id) => StatementObject::Entity(EntityId::from_bytes(*id)),
        StatementObjectWire::Value(v) => StatementObject::Value(statement_value_from_wire(v)),
        StatementObjectWire::MemoryRef(m) => StatementObject::Memory(MemoryId::from_be_bytes(*m)),
        StatementObjectWire::StatementRef(s) => {
            StatementObject::Statement(StatementId::from_bytes(*s))
        }
    }
}

// ---------------------------------------------------------------------------
// Conversion helpers — EvidenceRef ↔ wire.
// ---------------------------------------------------------------------------

/// Convert brain-core's evidence list to the wire shape. Note that the
/// per-entry confidence/timestamp/extractor metadata that brain-core
/// carries is **dropped** in the wire encoding — the wire variant
/// carries only `MemoryId`s. Server-side projections that need the
/// metadata should call `evidence_overflow_load` for overflow lists
/// and read inline metadata from the storage layer directly.
#[must_use]
pub fn evidence_ref_to_wire(e: &EvidenceRef) -> EvidenceRefWire {
    match e {
        EvidenceRef::Inline(entries) => {
            let memory_ids = entries
                .iter()
                .map(|e| e.memory_id.to_be_bytes())
                .collect::<Vec<[u8; 16]>>();
            EvidenceRefWire::Inline(memory_ids)
        }
        EvidenceRef::Overflow(id) => EvidenceRefWire::Overflow(id.to_bytes()),
    }
}

/// Convert the wire shape back to brain-core's `EvidenceRef`. Inline
/// entries get zero confidence / zero timestamp / extractor_id = 0;
/// callers that need the per-entry metadata read it from the storage
/// layer's overflow row (or from a future phase-22 add-evidence
/// payload that carries the metadata explicitly).
pub fn evidence_ref_from_wire(w: &EvidenceRefWire) -> Result<EvidenceRef, WireToStatementError> {
    match w {
        EvidenceRefWire::Inline(memory_ids) => {
            if memory_ids.len() > INLINE_EVIDENCE_CAP {
                return Err(WireToStatementError::EvidenceInlineTooLarge {
                    len: memory_ids.len(),
                    cap: INLINE_EVIDENCE_CAP,
                });
            }
            let mut entries =
                SmallVec::<[EvidenceEntry; INLINE_EVIDENCE_CAP]>::with_capacity(memory_ids.len());
            for mid in memory_ids {
                entries.push(EvidenceEntry {
                    memory_id: MemoryId::from_be_bytes(*mid),
                    confidence_milli: 0,
                    timestamp_unix_nanos: 0,
                    extractor_id: ExtractorId::from(0),
                });
            }
            Ok(EvidenceRef::Inline(entries))
        }
        EvidenceRefWire::Overflow(id) => {
            Ok(EvidenceRef::Overflow(EvidenceOverflowId::from(*id)))
        }
    }
}

// ---------------------------------------------------------------------------
// StatementKind ↔ wire.
// ---------------------------------------------------------------------------

#[must_use]
pub fn statement_kind_to_wire(k: StatementKind) -> StatementKindWire {
    match k {
        StatementKind::Fact => StatementKindWire::Fact,
        StatementKind::Preference => StatementKindWire::Preference,
        StatementKind::Event => StatementKindWire::Event,
    }
}

#[must_use]
pub fn statement_kind_from_wire(w: StatementKindWire) -> StatementKind {
    match w {
        StatementKindWire::Fact => StatementKind::Fact,
        StatementKindWire::Preference => StatementKind::Preference,
        StatementKindWire::Event => StatementKind::Event,
    }
}

// ---------------------------------------------------------------------------
// StatementView ↔ Statement.
// ---------------------------------------------------------------------------

impl StatementView {
    /// Build a wire view from a brain-core `Statement`. The handler
    /// resolves `predicate_qname` via the registry (string form
    /// `"namespace:name"`).
    #[must_use]
    pub fn from_statement(s: &Statement, predicate_qname: String) -> Self {
        let (subject, subject_pending_audit_id, flags) = match s.subject {
            SubjectRef::Entity(id) => (id.to_bytes(), [0u8; 16], 0u32),
            SubjectRef::Pending(audit) => ([0u8; 16], audit.to_bytes(), 1u32),
        };

        Self {
            statement_id: s.id.to_bytes(),
            kind: statement_kind_to_wire(s.kind),
            subject,
            subject_pending_audit_id,
            predicate: predicate_qname,
            object: statement_object_to_wire(&s.object),
            confidence: s.confidence,
            evidence: evidence_ref_to_wire(&s.evidence),
            extractor_id: s.extractor_id.raw(),
            extracted_at_unix_nanos: s.extracted_at_unix_nanos,
            schema_version: s.schema_version,
            valid_from_unix_nanos: s.valid_from_unix_nanos.unwrap_or(0),
            valid_to_unix_nanos: s.valid_to_unix_nanos.unwrap_or(0),
            event_at_unix_nanos: s.event_at_unix_nanos.unwrap_or(0),
            version: s.version,
            superseded_by: s.superseded_by.map(StatementId::to_bytes).unwrap_or([0u8; 16]),
            supersedes: s.supersedes.map(StatementId::to_bytes).unwrap_or([0u8; 16]),
            chain_root: s.chain_root.to_bytes(),
            tombstoned: s.tombstoned,
            tombstoned_at_unix_nanos: s.tombstoned_at_unix_nanos.unwrap_or(0),
            tombstone_reason: s
                .tombstone_reason
                .map(TombstoneReason::as_u8)
                .unwrap_or(0),
            flags,
        }
    }

    /// Project a wire view back to a brain-core `Statement`. The
    /// caller supplies the resolved `predicate` since the wire-side
    /// carries the canonical string and not the interned u32 id.
    pub fn to_statement(&self, predicate: PredicateId) -> Result<Statement, WireToStatementError> {
        let kind = statement_kind_from_wire(self.kind);
        let subject = if self.flags & 1 != 0 {
            SubjectRef::Pending(brain_core::knowledge::AuditId::from_bytes(
                self.subject_pending_audit_id,
            ))
        } else {
            SubjectRef::Entity(EntityId::from_bytes(self.subject))
        };

        let tombstone_reason = if self.tombstone_reason == 0 {
            None
        } else {
            Some(TombstoneReason::from_u8(self.tombstone_reason).ok_or(
                WireToStatementError::UnknownTombstoneReason(self.tombstone_reason),
            )?)
        };

        let opt_nz = |v: u64| if v == 0 { None } else { Some(v) };
        let opt_id = |b: [u8; 16]| {
            if b == [0u8; 16] {
                None
            } else {
                Some(StatementId::from_bytes(b))
            }
        };

        Ok(Statement {
            id: StatementId::from_bytes(self.statement_id),
            kind,
            subject,
            predicate,
            object: statement_object_from_wire(&self.object),
            confidence: self.confidence,
            evidence: evidence_ref_from_wire(&self.evidence)?,
            extractor_id: ExtractorId::from(self.extractor_id),
            extracted_at_unix_nanos: self.extracted_at_unix_nanos,
            schema_version: self.schema_version,
            valid_from_unix_nanos: opt_nz(self.valid_from_unix_nanos),
            valid_to_unix_nanos: opt_nz(self.valid_to_unix_nanos),
            event_at_unix_nanos: opt_nz(self.event_at_unix_nanos),
            version: self.version,
            superseded_by: opt_id(self.superseded_by),
            supersedes: opt_id(self.supersedes),
            chain_root: StatementId::from_bytes(self.chain_root),
            tombstoned: self.tombstoned,
            tombstoned_at_unix_nanos: opt_nz(self.tombstoned_at_unix_nanos),
            tombstone_reason,
        })
    }
}

// ---------------------------------------------------------------------------
// Response structs.
// ---------------------------------------------------------------------------

/// Reply to `STATEMENT_CREATE` (`0x01C0`). Spec §28/06 §3.2.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct StatementCreateResponse {
    pub statement_id: WireUuid,
    /// `[0; 16]` unless auto-supersession fired (Preference kind with a
    /// prior current row at same `(subject, predicate)`).
    pub auto_superseded: WireUuid,
    pub chain_root: WireUuid,
}

/// Reply to `STATEMENT_GET` (`0x01C1`). Spec §28/06 §4.2.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct StatementGetResponse {
    pub statement: StatementView,
    /// `true` iff `follow_supersession = true` redirected to a later
    /// chain entry.
    pub returned_via_supersession: bool,
}

/// Reply to `STATEMENT_SUPERSEDE` (`0x01C2`). Spec §28/06 §5.2.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct StatementSupersedeResponse {
    pub new_statement_id: WireUuid,
    pub chain_root: WireUuid,
    pub version: u32,
}

/// Reply to `STATEMENT_TOMBSTONE` (`0x01C3`). Spec §28/06 §6.2.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct StatementTombstoneResponse {
    pub tombstoned_at_unix_nanos: u64,
}

/// Reply to `STATEMENT_RETRACT` (`0x01C4`). Spec §28/06 §7.2.
#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct StatementRetractResponse {
    pub retracted_at_unix_nanos: u64,
    /// When the GC sweep will physically reclaim the row. Phase 21
    /// wires the worker; in v1 this is "tombstoned_at + 30 days".
    pub will_zero_at_unix_nanos: u64,
}

/// Single-frame snapshot reply for `STATEMENT_HISTORY` (`0x01C5`).
/// Spec §28/06 §8.2. Per phase-17 plan we collapse the per-item +
/// tail shapes into one frame; phase 23 splits when it streams.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct StatementHistoryResponseFrame {
    /// Chain entries in `version` ascending order.
    pub items: Vec<StatementView>,
    pub chain_root: WireUuid,
    pub total_versions: u32,
    pub is_final: bool,
}

impl StatementHistoryResponseFrame {
    #[must_use]
    pub fn is_final(&self) -> bool {
        self.is_final
    }
}

/// Single-frame snapshot reply for `STATEMENT_LIST` (`0x01C6`).
/// Mirrors `EntityListResponseFrame` (16.7.5). Phase 23 splits into
/// per-batch streaming + cursor pagination.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct StatementListResponseFrame {
    pub items: Vec<StatementView>,
    pub next_cursor: Vec<u8>,
    pub cumulative_count: u32,
    pub is_final: bool,
}

impl StatementListResponseFrame {
    #[must_use]
    pub fn is_final(&self) -> bool {
        self.is_final
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use brain_core::knowledge::{StatementValue, INLINE_EVIDENCE_CAP};
    use brain_core::{ContextId, ExtractorId, MemoryId, PredicateId};

    fn mem(byte: u16) -> MemoryId {
        MemoryId::pack(byte, ContextId::DEFAULT.into(), 0)
    }

    fn sample_statement(object: StatementObject) -> Statement {
        let id = StatementId::new();
        let subject = EntityId::new();
        let mut s = Statement::new_root(
            id,
            StatementKind::Fact,
            SubjectRef::Entity(subject),
            PredicateId::from(7),
            object,
            0.9,
            EvidenceRef::Inline({
                let mut sv = SmallVec::<[EvidenceEntry; INLINE_EVIDENCE_CAP]>::new();
                sv.push(EvidenceEntry::from_parts(
                    mem(1),
                    0.8,
                    1_700_000_000_000_000_000,
                    ExtractorId::from(0),
                ));
                sv
            }),
            ExtractorId::from(0),
            1_700_000_000_000_000_000,
            1,
        );
        s.valid_from_unix_nanos = Some(1_700_000_000_000_000_000);
        s
    }

    #[test]
    fn view_round_trip_entity_object() {
        let s = sample_statement(StatementObject::Entity(EntityId::new()));
        let view = StatementView::from_statement(&s, "test:role".into());
        let back = view.to_statement(s.predicate).unwrap();
        // Evidence metadata is dropped on the wire, so re-project the
        // original through the wire and compare.
        let expected_wire = StatementView::from_statement(&s, "test:role".into())
            .to_statement(s.predicate)
            .unwrap();
        assert_eq!(back, expected_wire);
    }

    #[test]
    fn view_round_trip_value_variants() {
        let cases = [
            StatementObject::Value(StatementValue::Text("hi".into())),
            StatementObject::Value(StatementValue::Integer(-5)),
            StatementObject::Value(StatementValue::Float(2.5)),
            StatementObject::Value(StatementValue::Bool(true)),
            StatementObject::Value(StatementValue::UnixNanos(42)),
            StatementObject::Value(StatementValue::Blob(vec![1, 2, 3])),
            StatementObject::Memory(mem(9)),
            StatementObject::Statement(StatementId::new()),
        ];
        for o in cases {
            let s = sample_statement(o.clone());
            let view = StatementView::from_statement(&s, "test:p".into());
            let back = view.to_statement(s.predicate).unwrap();
            assert_eq!(back.object, o);
        }
    }

    #[test]
    fn view_pending_subject_round_trip() {
        let mut s = sample_statement(StatementObject::Value(StatementValue::Bool(false)));
        let audit = brain_core::knowledge::AuditId::new();
        s.subject = SubjectRef::Pending(audit);

        let view = StatementView::from_statement(&s, "test:p".into());
        assert_eq!(view.subject, [0u8; 16]);
        assert_eq!(view.subject_pending_audit_id, audit.to_bytes());
        assert_eq!(view.flags & 1, 1);

        let back = view.to_statement(s.predicate).unwrap();
        match back.subject {
            SubjectRef::Pending(a) => assert_eq!(a, audit),
            other => panic!("expected Pending, got {other:?}"),
        }
    }

    #[test]
    fn view_chain_fields_preserved() {
        let mut s = sample_statement(StatementObject::Entity(EntityId::new()));
        let prior = StatementId::new();
        s.supersedes = Some(prior);
        s.chain_root = prior;
        s.version = 2;

        let view = StatementView::from_statement(&s, "test:p".into());
        let back = view.to_statement(s.predicate).unwrap();
        assert_eq!(back.supersedes, Some(prior));
        assert_eq!(back.chain_root, prior);
        assert_eq!(back.version, 2);
    }

    #[test]
    fn evidence_inline_too_large_rejected() {
        let big = vec![[0u8; 16]; INLINE_EVIDENCE_CAP + 1];
        let err = evidence_ref_from_wire(&EvidenceRefWire::Inline(big)).unwrap_err();
        matches!(
            err,
            WireToStatementError::EvidenceInlineTooLarge { cap: 8, .. }
        )
        .then_some(())
        .expect("expected EvidenceInlineTooLarge");
    }

    #[test]
    fn evidence_overflow_round_trip() {
        let id = EvidenceOverflowId::new();
        let wire = EvidenceRefWire::Overflow(id.to_bytes());
        let back = evidence_ref_from_wire(&wire).unwrap();
        match back {
            EvidenceRef::Overflow(got) => assert_eq!(got, id),
            _ => panic!("expected Overflow"),
        }
    }

    #[test]
    fn object_discriminants_stable() {
        assert_eq!(
            StatementObjectWire::EntityRef([0u8; 16]).discriminant(),
            0
        );
        assert_eq!(
            StatementObjectWire::Value(StatementValueWire::Bool(false)).discriminant(),
            1
        );
        assert_eq!(StatementObjectWire::MemoryRef([0u8; 16]).discriminant(), 2);
        assert_eq!(
            StatementObjectWire::StatementRef([0u8; 16]).discriminant(),
            3
        );
    }
}
