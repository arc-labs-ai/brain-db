//! Relation sidecar metadata + evidence reverse index.
//!
//! The `(from, to, type)` pair lives in the unified
//! [`crate::tables::edge::EDGES_TABLE`]; this module owns the
//! per-relation fields that have no substrate analog — confidence,
//! validity window, supersession chain, evidence pointers,
//! tombstone state, and the per-row property blob.
//!
//! ## Tables
//!
//! - [`RELATION_METADATA_TABLE`] keyed by `RelationId.to_bytes()` →
//!   [`RelationMetadata`]. The sidecar carries `from: NodeRef`,
//!   `to: NodeRef`, and `relation_type_id` so `relation_get(id)`
//!   reconstructs a [`Relation`] without a back-scan over the edge
//!   table.
//! - [`RELATION_BY_EVIDENCE_TABLE`] keyed by `(MemoryId.to_be_bytes(),
//!   RelationId.to_bytes())` → `()`. FORGET cascade reads this to
//!   discover which relations cite a forgotten memory.
//!
//! The old `RELATIONS_TABLE` / `RELATIONS_BY_FROM_TABLE` /
//! `RELATIONS_BY_TO_TABLE` from v1 are gone — the unified edge table
//! is the directional index.

use crate::impl_redb_rkyv_value;
use brain_core::Relation;
use brain_core::{EntityId, ExtractorId, MemoryId, NodeRef, RelationId, RelationTypeId};
use redb::TableDefinition;

// ---------------------------------------------------------------------------
// Tables.
// ---------------------------------------------------------------------------

pub const RELATION_METADATA_TABLE: TableDefinition<'static, [u8; 16], RelationMetadata> =
    TableDefinition::new("relation_metadata");

/// `(MemoryId.to_be_bytes(), RelationId.to_bytes())` → `()`. FORGET
/// cascade lookup index.
pub const RELATION_BY_EVIDENCE_TABLE: TableDefinition<'static, ([u8; 16], [u8; 16]), ()> =
    TableDefinition::new("relation_by_evidence");

// ---------------------------------------------------------------------------
// Sidecar value type.
// ---------------------------------------------------------------------------

/// Per-relation metadata that doesn't fit in the unified edge row's
/// `EdgeData`. The `from` / `to` endpoints are duplicated here so a
/// `RelationId`-keyed lookup can rebuild the full `Relation` without
/// scanning the edge table.
///
/// `from_tag` / `to_tag` are the 1-byte `NodeRef` discriminator
/// (`0` = Memory, `1` = Entity); paired with `from_bytes` / `to_bytes`
/// they encode the full `NodeRef`. The split is a side-effect of
/// rkyv's `check_bytes` mode rejecting nested enums on the bytes-of
/// fast path.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct RelationMetadata {
    pub from_tag: u8,
    pub from_bytes: [u8; 16],
    pub to_tag: u8,
    pub to_bytes: [u8; 16],
    pub relation_type_id: u32,
    pub chain_root_bytes: [u8; 16],
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
    /// Bit-flag scratch space for schema-evolution markers
    /// (`OUTSIDE_ACTIVE_SCHEMA`, `IMPLICIT_PREDICATE`, …). Bits are
    /// reserved as they're claimed.
    pub flags: u32,
}

impl RelationMetadata {
    /// Decode the source endpoint of this relation row.
    ///
    /// # Errors
    /// Returns [`NodeRefError`] if the stored discriminant is not a
    /// known [`NodeRef`] variant — only possible on corrupted rows.
    ///
    /// [`NodeRefError`]: brain_core::NodeRefError
    pub fn source_node(&self) -> Result<NodeRef, brain_core::NodeRefError> {
        let mut bytes = [0u8; 17];
        bytes[0] = self.from_tag;
        bytes[1..].copy_from_slice(&self.from_bytes);
        NodeRef::from_bytes(bytes)
    }

    /// Decode the destination endpoint of this relation row.
    ///
    /// # Errors
    /// Returns [`NodeRefError`] if the stored discriminant is not a
    /// known [`NodeRef`] variant — only possible on corrupted rows.
    ///
    /// [`NodeRefError`]: brain_core::NodeRefError
    pub fn target_node(&self) -> Result<NodeRef, brain_core::NodeRefError> {
        let mut bytes = [0u8; 17];
        bytes[0] = self.to_tag;
        bytes[1..].copy_from_slice(&self.to_bytes);
        NodeRef::from_bytes(bytes)
    }

    #[must_use]
    pub fn chain_root(&self) -> RelationId {
        RelationId::from(self.chain_root_bytes)
    }

    /// Project the `(from, to)` pair as [`EntityId`]s. Returns `None`
    /// if either endpoint is not an `Entity` — typed-graph
    /// relations canonically have entity endpoints; a Memory endpoint
    /// indicates a future mention-style typed relation.
    #[must_use]
    pub fn entity_endpoints(&self) -> Option<(EntityId, EntityId)> {
        match (self.source_node().ok()?, self.target_node().ok()?) {
            (NodeRef::Entity(a), NodeRef::Entity(b)) => Some((a, b)),
            _ => None,
        }
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

impl_redb_rkyv_value!(RelationMetadata, "brain_metadata::RelationMetadata");

// ---------------------------------------------------------------------------
// Projections — Relation (brain-core) ↔ RelationMetadata (rkyv row).
// ---------------------------------------------------------------------------

/// `Relation → RelationMetadata`. Derives the `is_current` byte from
/// `superseded_by / tombstoned` only — validity-window timing is left
/// to query-time.
#[must_use]
pub fn metadata_from_relation(r: &Relation) -> RelationMetadata {
    let is_current = u8::from(!r.tombstoned && r.superseded_by.is_none());
    let evidence_inline: Vec<[u8; 16]> = r.evidence.iter().map(|m| m.to_be_bytes()).collect();

    RelationMetadata {
        from_tag: NodeRef::Entity(r.from_entity).tag(),
        from_bytes: r.from_entity.to_bytes(),
        to_tag: NodeRef::Entity(r.to_entity).tag(),
        to_bytes: r.to_entity.to_bytes(),
        relation_type_id: r.relation_type.raw(),
        chain_root_bytes: r.chain_root.to_bytes(),
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
        flags: 0,
    }
}

/// `(RelationId, RelationMetadata) → Relation`. The `id` is supplied
/// separately because the sidecar table key carries the
/// authoritative `RelationId`.
#[must_use]
pub fn relation_from_metadata(id: RelationId, m: &RelationMetadata) -> Relation {
    let (from_entity, to_entity) = m
        .entity_endpoints()
        .unwrap_or_else(|| (EntityId::from_bytes([0; 16]), EntityId::from_bytes([0; 16])));
    let evidence: Vec<MemoryId> = m
        .evidence_inline
        .iter()
        .map(|b| MemoryId::from_be_bytes(*b))
        .collect();
    Relation {
        id,
        relation_type: RelationTypeId::from(m.relation_type_id),
        from_entity,
        to_entity,
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
    use crate::tables::fresh_db;
    use brain_core::Relation;
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
    fn sidecar_metadata_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let r = sample_relation();
        let row = metadata_from_relation(&r);
        let key = r.id.to_bytes();

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(RELATION_METADATA_TABLE).unwrap();
            t.insert(&key, &row).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(RELATION_METADATA_TABLE).unwrap();
        let got = t.get(&key).unwrap().unwrap().value();
        assert_eq!(got, row);
        assert!(got.is_current());
        assert!(!got.is_symmetric());

        let back = relation_from_metadata(r.id, &got);
        assert_eq!(back, r);
    }

    #[test]
    fn endpoint_projection_recovers_entity_pair() {
        let r = sample_relation();
        let row = metadata_from_relation(&r);
        let (a, b) = row.entity_endpoints().unwrap();
        assert_eq!(a, r.from_entity);
        assert_eq!(b, r.to_entity);
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
            let mut t = wtxn.open_table(RELATION_BY_EVIDENCE_TABLE).unwrap();
            t.insert(&key, &()).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(RELATION_BY_EVIDENCE_TABLE).unwrap();
        assert!(t.get(&key).unwrap().is_some());
    }
}
