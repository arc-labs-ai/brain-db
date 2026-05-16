//! Relation family — 4 tables.
//!
//! See `spec/20_relations/` and `spec/26_knowledge_storage/00_purpose.md`.
//!
//! - [`RELATIONS_TABLE`]              — primary `RelationId → RelationMetadata`.
//! - [`RELATIONS_BY_FROM_TABLE`]      — outgoing index keyed by `(from, type, is_current)`.
//! - [`RELATIONS_BY_TO_TABLE`]        — incoming index keyed by `(to, type, is_current)`.
//! - [`RELATIONS_BY_EVIDENCE_TABLE`]  — reverse: which relations derive from memory M.
//!
//! Phase 15.1 — types only. Phase 18.4 widens `RelationMetadata` with
//! `chain_root_bytes` (rkyv archive id bumped to v2; pre-v1.0, no
//! migration) and adds projection helpers between this row shape and
//! `brain_core::knowledge::Relation`.

use crate::impl_redb_rkyv_value;
use brain_core::knowledge::Relation;
use brain_core::{EntityId, ExtractorId, MemoryId, RelationId, RelationTypeId};
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
    pub chain_root_bytes: [u8; 16],
    pub relation_type_id: u32,
    pub from_entity_bytes: [u8; 16],
    pub to_entity_bytes: [u8; 16],
    /// Phase 19 (schema DSL) defines the typed shape; for now opaque.
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
    pub fn relation_id(&self) -> RelationId {
        RelationId::from(self.relation_id_bytes)
    }

    #[must_use]
    pub fn chain_root(&self) -> RelationId {
        RelationId::from(self.chain_root_bytes)
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

impl_redb_rkyv_value!(RelationMetadata, "brain_metadata::RelationMetadata::v2");

// ---------------------------------------------------------------------------
// Projections — Relation (brain-core) ↔ RelationMetadata (rkyv row).
// ---------------------------------------------------------------------------

/// `Relation → RelationMetadata`. Derives the `is_current` byte from
/// `superseded_by / tombstoned` only — validity-window timing is left
/// to query-time per spec §20/03 §1.2.
#[must_use]
pub fn metadata_from_relation(r: &Relation) -> RelationMetadata {
    let is_current = u8::from(!r.tombstoned && r.superseded_by.is_none());
    let evidence_inline: Vec<[u8; 16]> = r.evidence.iter().map(|m| m.to_be_bytes()).collect();

    RelationMetadata {
        relation_id_bytes: r.id.to_bytes(),
        chain_root_bytes: r.chain_root.to_bytes(),
        relation_type_id: r.relation_type.raw(),
        from_entity_bytes: r.from_entity.to_bytes(),
        to_entity_bytes: r.to_entity.to_bytes(),
        properties_blob: r.properties_blob.clone(),
        version: r.version,
        confidence: r.confidence,
        extractor_id: r.extractor_id.raw(),
        extracted_at_unix_nanos: r.extracted_at_unix_nanos,
        valid_from_unix_nanos: r.valid_from_unix_nanos,
        valid_to_unix_nanos: r.valid_to_unix_nanos,
        superseded_by_bytes: r.superseded_by.map(RelationId::to_bytes),
        supersedes_bytes: r.supersedes.map(RelationId::to_bytes),
        evidence_inline,
        tombstoned: u8::from(r.tombstoned),
        tombstoned_at_unix_nanos: r.tombstoned_at_unix_nanos,
        is_current,
        is_symmetric: u8::from(r.is_symmetric),
    }
}

/// `RelationMetadata → Relation`. Projects the rkyv row back to the
/// brain-core value type.
#[must_use]
pub fn relation_from_metadata(m: &RelationMetadata) -> Relation {
    let evidence: Vec<MemoryId> = m
        .evidence_inline
        .iter()
        .map(|b| MemoryId::from_be_bytes(*b))
        .collect();
    Relation {
        id: m.relation_id(),
        relation_type: RelationTypeId::from(m.relation_type_id),
        from_entity: m.from_entity(),
        to_entity: m.to_entity(),
        properties_blob: m.properties_blob.clone(),
        confidence: m.confidence,
        evidence,
        extractor_id: ExtractorId::from(m.extractor_id),
        extracted_at_unix_nanos: m.extracted_at_unix_nanos,
        valid_from_unix_nanos: m.valid_from_unix_nanos,
        valid_to_unix_nanos: m.valid_to_unix_nanos,
        version: m.version,
        superseded_by: m.superseded_by_bytes.map(RelationId::from),
        supersedes: m.supersedes_bytes.map(RelationId::from),
        chain_root: m.chain_root(),
        tombstoned: m.is_tombstoned(),
        tombstoned_at_unix_nanos: m.tombstoned_at_unix_nanos,
        is_symmetric: m.is_symmetric(),
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::knowledge::fresh_db;
    use brain_core::knowledge::Relation;
    use redb::ReadableDatabase;

    fn sample_relation() -> Relation {
        let id = RelationId::new();
        Relation::new_root(
            id,
            RelationTypeId::from(3),
            EntityId::new(),
            EntityId::new(),
            0.9,
            vec![],
            ExtractorId::from(0),
            1_700_000_000_000_000_000,
            false,
        )
    }

    #[test]
    fn relations_round_trip_through_projection() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let r = sample_relation();
        let row = metadata_from_relation(&r);
        let key = row.relation_id_bytes;

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(RELATIONS_TABLE).unwrap();
            t.insert(&key, &row).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(RELATIONS_TABLE).unwrap();
        let got = t.get(&key).unwrap().unwrap().value();
        assert_eq!(got, row);
        assert_eq!(got.relation_id(), r.id);
        assert_eq!(got.chain_root(), r.id);
        assert!(got.is_current());
        assert!(!got.is_symmetric());

        let back = relation_from_metadata(&got);
        assert_eq!(back, r);
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
