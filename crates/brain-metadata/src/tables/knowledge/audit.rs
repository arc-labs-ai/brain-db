//! Audit tables: `extractor_audit` + `entity_resolution_audit`.
//!
//! See `spec/25_provenance_versioning/00_purpose.md`. Audit entries
//! are append-only and time-ordered via the `AuditId` UUIDv7 key.
//! Two separate value structs (per spec) — extraction and resolution
//! capture different evidence shapes.

use crate::impl_redb_rkyv_value;
use brain_core::{AuditId, EntityId, MemoryId};
use redb::TableDefinition;

// ---------------------------------------------------------------------------
// extractor_audit
// ---------------------------------------------------------------------------

pub const EXTRACTOR_AUDIT_TABLE: TableDefinition<'static, [u8; 16], ExtractionAudit> =
    TableDefinition::new("extractor_audit");

/// `ExtractionAudit::outcome` byte values.
pub mod extraction_outcome {
    pub const SUCCESS: u8 = 0;
    pub const FAILURE: u8 = 1;
    pub const SKIPPED: u8 = 2;
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct ExtractionAudit {
    pub audit_id_bytes: [u8; 16],
    pub extractor_id: u32,
    pub extractor_version: u32,
    pub memory_id_bytes: [u8; 16],
    pub created_at_unix_nanos: u64,
    pub outcome: u8,
    pub items_produced: u32,
    pub error_message: Option<String>,
    pub payload_blob: Vec<u8>,
}

impl ExtractionAudit {
    #[must_use]
    pub fn new(
        audit_id: AuditId,
        extractor_id: u32,
        extractor_version: u32,
        memory_id: MemoryId,
        created_at_unix_nanos: u64,
        outcome: u8,
        items_produced: u32,
    ) -> Self {
        Self {
            audit_id_bytes: audit_id.to_bytes(),
            extractor_id,
            extractor_version,
            memory_id_bytes: memory_id.to_be_bytes(),
            created_at_unix_nanos,
            outcome,
            items_produced,
            error_message: None,
            payload_blob: Vec::new(),
        }
    }

    #[must_use]
    pub fn audit_id(&self) -> AuditId {
        AuditId::from(self.audit_id_bytes)
    }

    #[must_use]
    pub fn memory_id(&self) -> MemoryId {
        MemoryId::from_be_bytes(self.memory_id_bytes)
    }
}

impl_redb_rkyv_value!(ExtractionAudit, "brain_metadata::ExtractionAudit::v1");

// ---------------------------------------------------------------------------
// entity_resolution_audit
// ---------------------------------------------------------------------------

pub const ENTITY_RESOLUTION_AUDIT_TABLE: TableDefinition<'static, [u8; 16], ResolutionAudit> =
    TableDefinition::new("entity_resolution_audit");

/// `ResolutionAudit::outcome` byte values. Mirrors the resolver tier
/// (spec §18). Tier 5 (Created) is a side-effect, not a tier; included
/// here for completeness.
pub mod resolution_outcome {
    pub const TIER_1_EXACT: u8 = 0;
    pub const TIER_2_FUZZY: u8 = 1;
    pub const TIER_3_EMBEDDING: u8 = 2;
    pub const TIER_4_LLM: u8 = 3;
    pub const CREATED: u8 = 4;
    pub const AMBIGUOUS: u8 = 5;
    pub const NOT_RESOLVED: u8 = 6;
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct ResolutionAudit {
    pub audit_id_bytes: [u8; 16],
    pub candidate_name: String,
    pub entity_type_id: u32,
    pub resolved_entity_bytes: Option<[u8; 16]>,
    pub outcome: u8,
    pub confidence: f32,
    pub created_at_unix_nanos: u64,
    /// Other entities the resolver considered. Empty for tier 1
    /// exact-match wins.
    pub candidates_blob: Vec<u8>,
}

impl ResolutionAudit {
    #[must_use]
    pub fn new(
        audit_id: AuditId,
        candidate_name: String,
        entity_type_id: u32,
        outcome: u8,
        confidence: f32,
        created_at_unix_nanos: u64,
    ) -> Self {
        Self {
            audit_id_bytes: audit_id.to_bytes(),
            candidate_name,
            entity_type_id,
            resolved_entity_bytes: None,
            outcome,
            confidence,
            created_at_unix_nanos,
            candidates_blob: Vec::new(),
        }
    }

    #[must_use]
    pub fn audit_id(&self) -> AuditId {
        AuditId::from(self.audit_id_bytes)
    }

    #[must_use]
    pub fn resolved_entity(&self) -> Option<EntityId> {
        self.resolved_entity_bytes.map(EntityId::from)
    }
}

impl_redb_rkyv_value!(ResolutionAudit, "brain_metadata::ResolutionAudit::v1");

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::knowledge::fresh_db;
    use brain_core::MemoryId;
    use redb::ReadableDatabase;

    #[test]
    fn extraction_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let id = AuditId::new();
        let memory = MemoryId::pack(1, 42, 1);
        let row = ExtractionAudit::new(
            id,
            7,
            1,
            memory,
            1_700_000_000_000_000_000,
            extraction_outcome::SUCCESS,
            3,
        );
        let key = row.audit_id_bytes;

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(EXTRACTOR_AUDIT_TABLE).unwrap();
            t.insert(&key, &row).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(EXTRACTOR_AUDIT_TABLE).unwrap();
        let got = t.get(&key).unwrap().unwrap().value();
        assert_eq!(got, row);
        assert_eq!(got.audit_id(), id);
        assert_eq!(got.memory_id(), memory);
    }

    #[test]
    fn resolution_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let id = AuditId::new();
        let mut row = ResolutionAudit::new(
            id,
            "Priya".into(),
            1,
            resolution_outcome::TIER_2_FUZZY,
            0.81,
            1_700_000_000_000_000_000,
        );
        let resolved = EntityId::new();
        row.resolved_entity_bytes = Some(resolved.to_bytes());
        let key = row.audit_id_bytes;

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(ENTITY_RESOLUTION_AUDIT_TABLE).unwrap();
            t.insert(&key, &row).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(ENTITY_RESOLUTION_AUDIT_TABLE).unwrap();
        let got = t.get(&key).unwrap().unwrap().value();
        assert_eq!(got, row);
        assert_eq!(got.resolved_entity(), Some(resolved));
    }
}
