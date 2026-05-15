//! Statement family — 8 tables.
//!
//! See `spec/19_statements/` (record + supersession rules) and
//! `spec/26_knowledge_storage/00_purpose.md` (table catalog).
//!
//! - [`STATEMENTS_TABLE`]                  — primary `StatementId → StatementMetadata`.
//! - [`STATEMENTS_BY_SUBJECT_TABLE`]       — subject-anchored secondary.
//! - [`STATEMENTS_BY_PREDICATE_TABLE`]     — predicate-anchored secondary.
//! - [`STATEMENTS_BY_OBJECT_ENTITY_TABLE`] — object-side reverse index.
//! - [`STATEMENTS_BY_EVENT_TIME_TABLE`]    — time-range Event queries.
//! - [`STATEMENTS_BY_EVIDENCE_TABLE`]      — reverse: which statements derive from memory M.
//! - [`STATEMENT_CHAIN_TABLE`]             — supersession-chain traversal.
//! - [`EVIDENCE_OVERFLOW_TABLE`]           — long evidence lists that don't fit inline.

use crate::impl_redb_rkyv_value;
use brain_core::{EvidenceOverflowId, StatementId, StatementKind};
use redb::TableDefinition;

// ---------------------------------------------------------------------------
// Tables.
// ---------------------------------------------------------------------------

pub const STATEMENTS_TABLE: TableDefinition<'static, [u8; 16], StatementMetadata> =
    TableDefinition::new("statements");

/// `(EntityId, kind, predicate_id, is_current)` → `StatementId.to_bytes()`.
pub const STATEMENTS_BY_SUBJECT_TABLE: TableDefinition<
    'static,
    ([u8; 16], u8, u32, u8),
    [u8; 16],
> = TableDefinition::new("statements_by_subject");

/// `(predicate_id, kind, confidence_bucket)` → `StatementId.to_bytes()`.
/// `confidence_bucket` is `floor(confidence * 10)` clamped to `0..=10`.
pub const STATEMENTS_BY_PREDICATE_TABLE: TableDefinition<
    'static,
    (u32, u8, u8),
    [u8; 16],
> = TableDefinition::new("statements_by_predicate");

/// `(EntityId, kind)` → `StatementId.to_bytes()`. Walk this when
/// answering "what statements have X as object?".
pub const STATEMENTS_BY_OBJECT_ENTITY_TABLE: TableDefinition<
    'static,
    ([u8; 16], u8),
    [u8; 16],
> = TableDefinition::new("statements_by_object_entity");

/// `(event_at_unix_nanos, subject_entity_bytes)` → `StatementId.to_bytes()`.
/// Time-range queries scan a prefix; the EntityId disambiguates same-time
/// events for the same subject.
pub const STATEMENTS_BY_EVENT_TIME_TABLE: TableDefinition<
    'static,
    (u64, [u8; 16]),
    [u8; 16],
> = TableDefinition::new("statements_by_event_time");

/// `(MemoryId, StatementId)` → `()`. Reverse index for FORGET cascade.
pub const STATEMENTS_BY_EVIDENCE_TABLE: TableDefinition<
    'static,
    ([u8; 16], [u8; 16]),
    (),
> = TableDefinition::new("statements_by_evidence");

/// `(chain_root, version)` → `StatementId.to_bytes()`. Walk this to
/// reconstruct the supersession chain of a statement.
pub const STATEMENT_CHAIN_TABLE: TableDefinition<'static, ([u8; 16], u32), [u8; 16]> =
    TableDefinition::new("statement_chain");

pub const EVIDENCE_OVERFLOW_TABLE: TableDefinition<'static, [u8; 16], EvidenceOverflow> =
    TableDefinition::new("evidence_overflow");

// ---------------------------------------------------------------------------
// Tombstone-reason discriminant.
// ---------------------------------------------------------------------------

/// `StatementMetadata::tombstone_reason` byte values (spec §19).
pub mod tombstone_reason {
    pub const NOT_TOMBSTONED: u8 = 0;
    pub const SOURCE_MEMORY_FORGOTTEN: u8 = 1;
    pub const USER_REQUEST: u8 = 2;
    pub const SCHEMA_INVALIDATION: u8 = 3;
    pub const EXTRACTOR_RETRACTION: u8 = 4;
}

// ---------------------------------------------------------------------------
// Value structs.
// ---------------------------------------------------------------------------

/// Primary statement record. Free-form fields (`object_blob`) hold rkyv
/// blobs in 15.1; phase 17 (statement layer) defines the typed
/// `StatementObject` union.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct StatementMetadata {
    pub statement_id_bytes: [u8; 16],
    pub chain_root_bytes: [u8; 16],
    pub version: u32,
    /// Fact=0 / Preference=1 / Event=2 per `brain_core::StatementKind`.
    pub kind: u8,
    pub subject_entity_bytes: [u8; 16],
    pub predicate_id: u32,
    /// rkyv-encoded `StatementObject` union (typed shape in phase 17).
    pub object_blob: Vec<u8>,
    pub confidence: f32,
    pub extractor_id: u32,
    pub extractor_version: u32,
    pub schema_version: u32,
    pub extracted_at_unix_nanos: u64,
    pub valid_from_unix_nanos: Option<u64>,
    pub valid_to_unix_nanos: Option<u64>,
    /// Required for Event kind; `None` otherwise.
    pub event_at_unix_nanos: Option<u64>,
    pub superseded_by_bytes: Option<[u8; 16]>,
    pub supersedes_bytes: Option<[u8; 16]>,
    /// Inline evidence list. Bounded length (default 8 — spec §19);
    /// overflow spills into `evidence_overflow`.
    pub evidence_inline: Vec<[u8; 16]>,
    pub evidence_overflow_id_bytes: Option<[u8; 16]>,
    pub tombstoned: u8,
    pub tombstoned_at_unix_nanos: Option<u64>,
    pub tombstone_reason: u8,
    pub is_current: u8,
}

impl StatementMetadata {
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        statement_id: StatementId,
        kind: StatementKind,
        subject_entity_bytes: [u8; 16],
        predicate_id: u32,
        extractor_id: u32,
        extractor_version: u32,
        schema_version: u32,
        extracted_at_unix_nanos: u64,
        confidence: f32,
    ) -> Self {
        let bytes = statement_id.to_bytes();
        Self {
            statement_id_bytes: bytes,
            chain_root_bytes: bytes, // self-rooted until superseded
            version: 1,
            kind: kind.as_u8(),
            subject_entity_bytes,
            predicate_id,
            object_blob: Vec::new(),
            confidence,
            extractor_id,
            extractor_version,
            schema_version,
            extracted_at_unix_nanos,
            valid_from_unix_nanos: None,
            valid_to_unix_nanos: None,
            event_at_unix_nanos: None,
            superseded_by_bytes: None,
            supersedes_bytes: None,
            evidence_inline: Vec::new(),
            evidence_overflow_id_bytes: None,
            tombstoned: 0,
            tombstoned_at_unix_nanos: None,
            tombstone_reason: tombstone_reason::NOT_TOMBSTONED,
            is_current: 1,
        }
    }

    #[must_use]
    pub fn statement_id(&self) -> StatementId {
        StatementId::from(self.statement_id_bytes)
    }

    #[must_use]
    pub fn chain_root(&self) -> StatementId {
        StatementId::from(self.chain_root_bytes)
    }

    pub fn kind(&self) -> Option<StatementKind> {
        StatementKind::from_u8(self.kind)
    }

    #[must_use]
    pub fn is_tombstoned(&self) -> bool {
        self.tombstoned != 0
    }

    #[must_use]
    pub fn is_current(&self) -> bool {
        self.is_current != 0
    }
}

impl_redb_rkyv_value!(StatementMetadata, "brain_metadata::StatementMetadata::v1");

/// Overflow row for statements whose inline evidence list overflowed.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct EvidenceOverflow {
    pub overflow_id_bytes: [u8; 16],
    pub memory_ids: Vec<[u8; 16]>,
    pub created_at_unix_nanos: u64,
}

impl EvidenceOverflow {
    #[must_use]
    pub fn new(
        overflow_id: EvidenceOverflowId,
        memory_ids: Vec<[u8; 16]>,
        created_at_unix_nanos: u64,
    ) -> Self {
        Self {
            overflow_id_bytes: overflow_id.to_bytes(),
            memory_ids,
            created_at_unix_nanos,
        }
    }

    #[must_use]
    pub fn overflow_id(&self) -> EvidenceOverflowId {
        EvidenceOverflowId::from(self.overflow_id_bytes)
    }
}

impl_redb_rkyv_value!(EvidenceOverflow, "brain_metadata::EvidenceOverflow::v1");

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::knowledge::fresh_db;
    use brain_core::EntityId;
    use redb::ReadableDatabase;

    #[test]
    fn statements_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let id = StatementId::new();
        let subject = EntityId::new();
        let s = StatementMetadata::new(
            id,
            StatementKind::Fact,
            subject.to_bytes(),
            7,
            11,
            1,
            1,
            1_700_000_000_000_000_000,
            0.91,
        );
        let key = s.statement_id_bytes;

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(STATEMENTS_TABLE).unwrap();
            t.insert(&key, &s).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(STATEMENTS_TABLE).unwrap();
        let got = t.get(&key).unwrap().unwrap().value();
        assert_eq!(got, s);
        assert_eq!(got.statement_id(), id);
        assert_eq!(got.kind(), Some(StatementKind::Fact));
        assert!(got.is_current());
        assert!(!got.is_tombstoned());
    }

    #[test]
    fn by_subject_index_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let subject = EntityId::new();
        let stmt = StatementId::new();
        let key = (subject.to_bytes(), StatementKind::Preference.as_u8(), 3u32, 1u8);

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE).unwrap();
            t.insert(&key, &stmt.to_bytes()).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(STATEMENTS_BY_SUBJECT_TABLE).unwrap();
        let got = t.get(&key).unwrap().unwrap().value();
        assert_eq!(StatementId::from(got), stmt);
    }

    #[test]
    fn by_predicate_index_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let stmt = StatementId::new();
        let key = (3u32, StatementKind::Fact.as_u8(), 9u8);

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(STATEMENTS_BY_PREDICATE_TABLE).unwrap();
            t.insert(&key, &stmt.to_bytes()).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(STATEMENTS_BY_PREDICATE_TABLE).unwrap();
        assert!(t.get(&key).unwrap().is_some());
    }

    #[test]
    fn by_object_entity_index_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let entity = EntityId::new();
        let stmt = StatementId::new();
        let key = (entity.to_bytes(), StatementKind::Fact.as_u8());

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(STATEMENTS_BY_OBJECT_ENTITY_TABLE).unwrap();
            t.insert(&key, &stmt.to_bytes()).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(STATEMENTS_BY_OBJECT_ENTITY_TABLE).unwrap();
        assert!(t.get(&key).unwrap().is_some());
    }

    #[test]
    fn by_event_time_index_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let entity = EntityId::new();
        let stmt = StatementId::new();
        let key = (1_700_000_000_000_000_000u64, entity.to_bytes());

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(STATEMENTS_BY_EVENT_TIME_TABLE).unwrap();
            t.insert(&key, &stmt.to_bytes()).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(STATEMENTS_BY_EVENT_TIME_TABLE).unwrap();
        assert!(t.get(&key).unwrap().is_some());
    }

    #[test]
    fn by_evidence_and_chain_indexes_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let mem = [9u8; 16];
        let stmt = StatementId::new();
        let chain_root = StatementId::new();

        let wtxn = db.begin_write().unwrap();
        {
            let mut e = wtxn.open_table(STATEMENTS_BY_EVIDENCE_TABLE).unwrap();
            e.insert(&(mem, stmt.to_bytes()), &()).unwrap();
            let mut c = wtxn.open_table(STATEMENT_CHAIN_TABLE).unwrap();
            c.insert(&(chain_root.to_bytes(), 1u32), &stmt.to_bytes()).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let e = rtxn.open_table(STATEMENTS_BY_EVIDENCE_TABLE).unwrap();
        assert!(e.get(&(mem, stmt.to_bytes())).unwrap().is_some());
        let c = rtxn.open_table(STATEMENT_CHAIN_TABLE).unwrap();
        let got = c.get(&(chain_root.to_bytes(), 1u32)).unwrap().unwrap().value();
        assert_eq!(StatementId::from(got), stmt);
    }

    #[test]
    fn evidence_overflow_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let id = EvidenceOverflowId::new();
        let row = EvidenceOverflow::new(
            id,
            vec![[1u8; 16], [2u8; 16], [3u8; 16]],
            1_700_000_000_000_000_000,
        );
        let key = row.overflow_id_bytes;

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(EVIDENCE_OVERFLOW_TABLE).unwrap();
            t.insert(&key, &row).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(EVIDENCE_OVERFLOW_TABLE).unwrap();
        let got = t.get(&key).unwrap().unwrap().value();
        assert_eq!(got, row);
        assert_eq!(got.memory_ids.len(), 3);
    }
}
