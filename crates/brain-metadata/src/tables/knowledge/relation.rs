//! Relation family — 4 tables.
//!
//! See `spec/20_relations/` and `spec/26_knowledge_storage/00_purpose.md`.
//!
//! - [`RELATIONS_TABLE`]              — primary `RelationId → RelationMetadata`.
//! - [`RELATIONS_BY_FROM_TABLE`]      — outgoing index keyed by `(from, type, is_current)`.
//! - [`RELATIONS_BY_TO_TABLE`]        — incoming index keyed by `(to, type, is_current)`.
//! - [`RELATIONS_BY_EVIDENCE_TABLE`]  — reverse: which relations derive from memory M.
//!
//! Phase 15.1 — types only. Phase 18 wires the typed CRUD, cardinality
//! enforcement, symmetry, and traversal.

use crate::impl_redb_rkyv_value;
use brain_core::{EntityId, RelationId, RelationTypeId};
use redb::TableDefinition;

// ---------------------------------------------------------------------------
// Tables.
// ---------------------------------------------------------------------------

pub const RELATIONS_TABLE: TableDefinition<'static, [u8; 16], RelationMetadata> =
    TableDefinition::new("relations");

/// `(from_entity_bytes, relation_type_id, is_current)` → `RelationId.to_bytes()`.
pub const RELATIONS_BY_FROM_TABLE: TableDefinition<
    'static,
    ([u8; 16], u32, u8),
    [u8; 16],
> = TableDefinition::new("relations_by_from");

/// `(to_entity_bytes, relation_type_id, is_current)` → `RelationId.to_bytes()`.
pub const RELATIONS_BY_TO_TABLE: TableDefinition<
    'static,
    ([u8; 16], u32, u8),
    [u8; 16],
> = TableDefinition::new("relations_by_to");

/// `(MemoryId.to_be_bytes(), RelationId.to_bytes())` → `()`.
pub const RELATIONS_BY_EVIDENCE_TABLE: TableDefinition<
    'static,
    ([u8; 16], [u8; 16]),
    (),
> = TableDefinition::new("relations_by_evidence");

// ---------------------------------------------------------------------------
// Value struct.
// ---------------------------------------------------------------------------

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct RelationMetadata {
    pub relation_id_bytes: [u8; 16],
    pub relation_type_id: u32,
    pub from_entity_bytes: [u8; 16],
    pub to_entity_bytes: [u8; 16],
    /// rkyv-encoded properties map. Phase 19 (schema DSL) defines the
    /// typed shape; for now opaque.
    pub properties_blob: Vec<u8>,
    pub version: u32,
    pub confidence: f32,
    pub extractor_id: u32,
    pub extracted_at_unix_nanos: u64,
    pub valid_from_unix_nanos: Option<u64>,
    pub valid_to_unix_nanos: Option<u64>,
    pub superseded_by_bytes: Option<[u8; 16]>,
    pub supersedes_bytes: Option<[u8; 16]>,
    pub evidence_inline: Vec<[u8; 16]>,
    pub tombstoned: u8,
    pub tombstoned_at_unix_nanos: Option<u64>,
    pub is_current: u8,
    pub is_symmetric: u8,
}

impl RelationMetadata {
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        relation_id: RelationId,
        relation_type_id: RelationTypeId,
        from_entity: EntityId,
        to_entity: EntityId,
        extractor_id: u32,
        extracted_at_unix_nanos: u64,
        confidence: f32,
        is_symmetric: bool,
    ) -> Self {
        Self {
            relation_id_bytes: relation_id.to_bytes(),
            relation_type_id: relation_type_id.raw(),
            from_entity_bytes: from_entity.to_bytes(),
            to_entity_bytes: to_entity.to_bytes(),
            properties_blob: Vec::new(),
            version: 1,
            confidence,
            extractor_id,
            extracted_at_unix_nanos,
            valid_from_unix_nanos: None,
            valid_to_unix_nanos: None,
            superseded_by_bytes: None,
            supersedes_bytes: None,
            evidence_inline: Vec::new(),
            tombstoned: 0,
            tombstoned_at_unix_nanos: None,
            is_current: 1,
            is_symmetric: u8::from(is_symmetric),
        }
    }

    #[must_use]
    pub fn relation_id(&self) -> RelationId {
        RelationId::from(self.relation_id_bytes)
    }

    #[must_use]
    pub fn from_entity(&self) -> EntityId {
        EntityId::from(self.from_entity_bytes)
    }

    #[must_use]
    pub fn to_entity(&self) -> EntityId {
        EntityId::from(self.to_entity_bytes)
    }

    #[must_use]
    pub fn is_current(&self) -> bool {
        self.is_current != 0
    }

    #[must_use]
    pub fn is_symmetric(&self) -> bool {
        self.is_symmetric != 0
    }

    #[must_use]
    pub fn is_tombstoned(&self) -> bool {
        self.tombstoned != 0
    }
}

impl_redb_rkyv_value!(RelationMetadata, "brain_metadata::RelationMetadata::v1");

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::knowledge::fresh_db;
    use redb::ReadableDatabase;

    #[test]
    fn relations_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let rel_id = RelationId::new();
        let from = EntityId::new();
        let to = EntityId::new();
        let r = RelationMetadata::new(
            rel_id,
            RelationTypeId::from(3),
            from,
            to,
            11,
            1_700_000_000_000_000_000,
            0.9,
            false,
        );
        let key = r.relation_id_bytes;

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(RELATIONS_TABLE).unwrap();
            t.insert(&key, &r).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(RELATIONS_TABLE).unwrap();
        let got = t.get(&key).unwrap().unwrap().value();
        assert_eq!(got, r);
        assert_eq!(got.relation_id(), rel_id);
        assert_eq!(got.from_entity(), from);
        assert_eq!(got.to_entity(), to);
        assert!(got.is_current());
        assert!(!got.is_symmetric());
    }

    #[test]
    fn direction_indexes_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let rel_id = RelationId::new();
        let from = EntityId::new();
        let to = EntityId::new();
        let k_from = (from.to_bytes(), 3u32, 1u8);
        let k_to = (to.to_bytes(), 3u32, 1u8);

        let wtxn = db.begin_write().unwrap();
        {
            let mut f = wtxn.open_table(RELATIONS_BY_FROM_TABLE).unwrap();
            f.insert(&k_from, &rel_id.to_bytes()).unwrap();
            let mut t = wtxn.open_table(RELATIONS_BY_TO_TABLE).unwrap();
            t.insert(&k_to, &rel_id.to_bytes()).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let f = rtxn.open_table(RELATIONS_BY_FROM_TABLE).unwrap();
        assert_eq!(
            RelationId::from(f.get(&k_from).unwrap().unwrap().value()),
            rel_id,
        );
        let t = rtxn.open_table(RELATIONS_BY_TO_TABLE).unwrap();
        assert_eq!(
            RelationId::from(t.get(&k_to).unwrap().unwrap().value()),
            rel_id,
        );
    }

    #[test]
    fn evidence_index_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let rel_id = RelationId::new();
        let mem = [7u8; 16];
        let key = (mem, rel_id.to_bytes());

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(RELATIONS_BY_EVIDENCE_TABLE).unwrap();
            t.insert(&key, &()).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(RELATIONS_BY_EVIDENCE_TABLE).unwrap();
        assert!(t.get(&key).unwrap().is_some());
    }
}
