//! Audit tables: `extractor_audit` + `entity_resolution_audit` +
//! the three secondary indexes over the extractor audit table.
//!
//! Audit entries are append-only and time-ordered via the `AuditId`
//! UUIDv7 key. The row carries versions, timestamps, outputs, cost and
//! input hash, indexed three ways (by-memory / by-extractor / by-time).

use crate::impl_redb_rkyv_value;
use brain_core::{AuditId, EntityId, MemoryId};
use redb::TableDefinition;

// ---------------------------------------------------------------------------
// Stable byte tables.
// ---------------------------------------------------------------------------

/// `ExtractionAudit::status` byte values. Stable; never reassigned.
/// Discriminants match `brain_extractors::ExtractionStatus::as_u8()`
/// byte-for-byte so the extractors crate and metadata crate share
/// the same wire-equivalent enum.
pub mod extraction_status {
    pub const SUCCESS: u8 = 1;
    pub const FAILURE: u8 = 2;
    pub const SKIPPED_BUDGET: u8 = 3;
    pub const SKIPPED_FILTER: u8 = 4;
    pub const SKIPPED_DUPLICATE: u8 = 5;
    pub const SKIPPED_DISABLED: u8 = 6;
}

/// `OutputRef::kind` byte values. Stable.
pub mod output_kind {
    pub const ENTITY: u8 = 1;
    pub const STATEMENT: u8 = 2;
    pub const RELATION: u8 = 3;
    pub const ENTITY_MENTION: u8 = 4;
}

/// Per-row cap on `outputs.len` Q9 —
/// overflow handling (follow-on rows) is post-v1.
pub const OUTPUTS_CAP: usize = 64;

// ---------------------------------------------------------------------------
// extractor_audit
// ---------------------------------------------------------------------------

pub const EXTRACTOR_AUDIT_TABLE: TableDefinition<'static, [u8; 16], ExtractionAudit> =
    TableDefinition::new("extractor_audit");

/// `(memory_id_bytes, audit_id_bytes) → ()`. Iterating
/// `(mem_id, [0;16])..=(mem_id, [0xff;16])` yields all audits for
/// one memory in audit-id order.
pub const EXTRACTOR_AUDIT_BY_MEMORY_TABLE: TableDefinition<'static, ([u8; 16], [u8; 16]), ()> =
    TableDefinition::new("extractor_audit_by_memory");

/// `(extractor_id, audit_id_bytes) → ()`. Per-extractor history.
pub const EXTRACTOR_AUDIT_BY_EXTRACTOR_TABLE: TableDefinition<'static, (u32, [u8; 16]), ()> =
    TableDefinition::new("extractor_audit_by_extractor");

/// `(started_at_unix_nanos, audit_id_bytes) → ()`. Global
/// time-window scans (e.g., "recent failures").
pub const EXTRACTOR_AUDIT_BY_TIME_TABLE: TableDefinition<'static, (u64, [u8; 16]), ()> =
    TableDefinition::new("extractor_audit_by_time");

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct ExtractionAudit {
    pub audit_id_bytes: [u8; 16],
    pub memory_id_bytes: [u8; 16],
    pub extractor_id: u32,
    pub extractor_version: u32,
    pub schema_version: u32,
    pub started_at_unix_nanos: u64,
    pub completed_at_unix_nanos: u64,
    /// One of `extraction_status::*` byte values.
    pub status: u8,
    /// Free-form reason (empty on Success).
    pub status_reason: String,
    /// Produced outputs. Capped at `OUTPUTS_CAP`.
    pub outputs: Vec<OutputRef>,
    /// Estimated cost in dollar micro-units (1e-6 USD). 0 for the
    /// pattern + classifier tiers; the LLM tier fills this.
    pub cost_micro_usd: u64,
    /// rkyv-archived `ModelMetadata` blob — empty for non-LLM tiers.
    pub model_metadata: Vec<u8>,
    /// BLAKE3 of `memory.text` (32 bytes). The idempotency probe uses
    /// this to detect text edits.
    pub input_hash: [u8; 32],
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[archive(check_bytes)]
pub struct OutputRef {
    /// One of `output_kind::*` byte values.
    pub kind: u8,
    /// `EntityId` / `StatementId` / `RelationId` bytes.
    /// `EntityMention` outputs are transient (not persisted) and
    /// carry the all-zero id.
    pub id: [u8; 16],
}

impl ExtractionAudit {
    /// Build an audit row for a `Success` extraction. `outputs`
    /// supplied by the caller; `status_reason` is empty by
    /// convention.
    // Argument list mirrors the on-disk row's required fields — a
    // builder struct would be the same field set with extra indirection.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn success(
        audit_id: AuditId,
        memory_id: MemoryId,
        extractor_id: u32,
        extractor_version: u32,
        schema_version: u32,
        started_at_unix_nanos: u64,
        completed_at_unix_nanos: u64,
        outputs: Vec<OutputRef>,
        input_hash: [u8; 32],
    ) -> Self {
        Self {
            audit_id_bytes: audit_id.to_bytes(),
            memory_id_bytes: memory_id.to_be_bytes(),
            extractor_id,
            extractor_version,
            schema_version,
            started_at_unix_nanos,
            completed_at_unix_nanos,
            status: extraction_status::SUCCESS,
            status_reason: String::new(),
            outputs,
            cost_micro_usd: 0,
            model_metadata: Vec::new(),
            input_hash,
        }
    }

    /// Build a non-Success audit row. `status` MUST be one of the
    /// `extraction_status::*` constants other than SUCCESS.
    // See [`Self::success`] re: arg count.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn non_success(
        audit_id: AuditId,
        memory_id: MemoryId,
        extractor_id: u32,
        extractor_version: u32,
        schema_version: u32,
        started_at_unix_nanos: u64,
        completed_at_unix_nanos: u64,
        status: u8,
        status_reason: String,
        input_hash: [u8; 32],
    ) -> Self {
        Self {
            audit_id_bytes: audit_id.to_bytes(),
            memory_id_bytes: memory_id.to_be_bytes(),
            extractor_id,
            extractor_version,
            schema_version,
            started_at_unix_nanos,
            completed_at_unix_nanos,
            status,
            status_reason,
            outputs: Vec::new(),
            cost_micro_usd: 0,
            model_metadata: Vec::new(),
            input_hash,
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

    /// True iff `status == SUCCESS`.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.status == extraction_status::SUCCESS
    }

    /// True iff `status == FAILURE`.
    #[must_use]
    pub fn is_failure(&self) -> bool {
        self.status == extraction_status::FAILURE
    }
}

impl_redb_rkyv_value!(ExtractionAudit, "brain_metadata::ExtractionAudit");
impl_redb_rkyv_value!(OutputRef, "brain_metadata::OutputRef");

// ---------------------------------------------------------------------------
// entity_resolution_audit
// ---------------------------------------------------------------------------

pub const ENTITY_RESOLUTION_AUDIT_TABLE: TableDefinition<'static, [u8; 16], ResolutionAudit> =
    TableDefinition::new("entity_resolution_audit");

/// `ResolutionAudit::outcome` byte values. Mirrors the resolver tiers.
/// Tier 5 (Created) is a side-effect, not a tier; included here for
/// completeness.
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

impl_redb_rkyv_value!(ResolutionAudit, "brain_metadata::ResolutionAudit");

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::fresh_db;
    use brain_core::MemoryId;
    use redb::ReadableDatabase;

    #[test]
    fn extraction_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let id = AuditId::new();
        let memory = MemoryId::pack(1, 42, 1);
        let row = ExtractionAudit::success(
            id,
            memory,
            7,
            1,
            3,
            1_700_000_000_000_000_000,
            1_700_000_000_000_000_500,
            vec![OutputRef {
                kind: output_kind::ENTITY,
                id: [9u8; 16],
            }],
            [0x42u8; 32],
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
        assert!(got.is_success());
        assert_eq!(got.outputs.len(), 1);
        assert_eq!(got.outputs[0].kind, output_kind::ENTITY);
    }

    #[test]
    fn extraction_non_success_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let id = AuditId::new();
        let memory = MemoryId::pack(1, 42, 1);
        let row = ExtractionAudit::non_success(
            id,
            memory,
            7,
            1,
            3,
            1_700_000_000_000_000_000,
            1_700_000_000_000_000_500,
            extraction_status::FAILURE,
            "inference oom".into(),
            [0u8; 32],
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
        assert!(got.is_failure());
        assert!(!got.is_success());
        assert_eq!(got.status_reason, "inference oom");
        assert!(got.outputs.is_empty());
    }

    #[test]
    fn discriminant_bytes_are_stable() {
        // These bytes are part of the on-disk format. Never renumber.
        assert_eq!(extraction_status::SUCCESS, 1);
        assert_eq!(extraction_status::FAILURE, 2);
        assert_eq!(extraction_status::SKIPPED_BUDGET, 3);
        assert_eq!(extraction_status::SKIPPED_FILTER, 4);
        assert_eq!(extraction_status::SKIPPED_DUPLICATE, 5);
        assert_eq!(extraction_status::SKIPPED_DISABLED, 6);

        assert_eq!(output_kind::ENTITY, 1);
        assert_eq!(output_kind::STATEMENT, 2);
        assert_eq!(output_kind::RELATION, 3);
        assert_eq!(output_kind::ENTITY_MENTION, 4);
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
