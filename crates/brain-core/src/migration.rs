//! Schema migration types.
//!
//! Pure value types shared across the SCHEMA_UPLOAD response,
//! brain-workers' migration worker, and operator-facing CLIs.

use uuid::Uuid;

use crate::ids::ExtractorId;
use crate::MemoryId;

/// UUIDv7 identifier for a single schema-migration request.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MigrationId(pub Uuid);

impl MigrationId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
    #[must_use]
    pub const fn to_bytes(self) -> [u8; 16] {
        *self.0.as_bytes()
    }
}

impl Default for MigrationId {
    fn default() -> Self {
        Self::new()
    }
}

/// Why a migration item exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MigrationReason {
    ExtractorVersionBump,
    SchemaVersionBump,
    NewExtractor,
}

/// One (memory, extractor) pair to re-extract under the new
/// schema version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MigrationItem {
    pub memory_id: MemoryId,
    pub extractor_id: ExtractorId,
    pub reason: MigrationReason,
}

/// Full migration plan emitted by the schema-upload handler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationPlan {
    pub request_id: MigrationId,
    pub from_version: u32,
    pub to_version: u32,
    pub namespace: String,
    pub items: Vec<MigrationItem>,
}

impl MigrationPlan {
    #[must_use]
    pub fn summary(&self) -> MigrationSummary {
        let mut by_reason = MigrationByReason::default();
        for item in &self.items {
            match item.reason {
                MigrationReason::ExtractorVersionBump => by_reason.extractor_version_bump += 1,
                MigrationReason::SchemaVersionBump => by_reason.schema_version_bump += 1,
                MigrationReason::NewExtractor => by_reason.new_extractor += 1,
            }
        }
        MigrationSummary {
            total_items: self.items.len() as u32,
            by_reason,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MigrationByReason {
    pub extractor_version_bump: u32,
    pub schema_version_bump: u32,
    pub new_extractor: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MigrationSummary {
    pub total_items: u32,
    pub by_reason: MigrationByReason,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_counts_by_reason() {
        let plan = MigrationPlan {
            request_id: MigrationId::new(),
            from_version: 1,
            to_version: 2,
            namespace: "acme".into(),
            items: vec![
                MigrationItem {
                    memory_id: MemoryId::from_raw(1),
                    extractor_id: ExtractorId(7),
                    reason: MigrationReason::ExtractorVersionBump,
                },
                MigrationItem {
                    memory_id: MemoryId::from_raw(2),
                    extractor_id: ExtractorId(7),
                    reason: MigrationReason::NewExtractor,
                },
                MigrationItem {
                    memory_id: MemoryId::from_raw(3),
                    extractor_id: ExtractorId(7),
                    reason: MigrationReason::NewExtractor,
                },
            ],
        };
        let s = plan.summary();
        assert_eq!(s.total_items, 3);
        assert_eq!(s.by_reason.extractor_version_bump, 1);
        assert_eq!(s.by_reason.new_extractor, 2);
        assert_eq!(s.by_reason.schema_version_bump, 0);
    }
}
