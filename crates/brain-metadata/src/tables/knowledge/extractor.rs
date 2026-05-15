//! `extractors` table — active extractor declarations.
//!
//! See `spec/22_extractors/00_purpose.md` (three-tier model:
//! pattern → classifier → LLM) and `spec/21_schema_dsl/00_purpose.md`
//! (extractor declaration syntax). The declaration body is an opaque
//! blob in 15.1; phase 19 (schema DSL) defines the typed shape.

use crate::impl_redb_rkyv_value;
use brain_core::{ExtractorId, ExtractorKind};
use redb::TableDefinition;

pub const EXTRACTORS_TABLE: TableDefinition<'static, u32, ExtractorDefinition> =
    TableDefinition::new("extractors");

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct ExtractorDefinition {
    pub extractor_id: u32,
    pub name: String,
    pub version: u32,
    /// Pattern / Classifier / Llm — see `brain_core::ExtractorKind`.
    pub kind: u8,
    pub enabled: u8,
    pub definition_blob: Vec<u8>,
    pub created_at_unix_nanos: u64,
}

impl ExtractorDefinition {
    #[must_use]
    pub fn new(
        id: ExtractorId,
        name: String,
        version: u32,
        kind: ExtractorKind,
        enabled: bool,
        definition_blob: Vec<u8>,
        created_at_unix_nanos: u64,
    ) -> Self {
        Self {
            extractor_id: id.raw(),
            name,
            version,
            kind: kind.as_u8(),
            enabled: u8::from(enabled),
            definition_blob,
            created_at_unix_nanos,
        }
    }

    #[must_use]
    pub fn id(&self) -> ExtractorId {
        ExtractorId::from(self.extractor_id)
    }

    pub fn kind(&self) -> Option<ExtractorKind> {
        ExtractorKind::from_u8(self.kind)
    }

    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled != 0
    }
}

impl_redb_rkyv_value!(ExtractorDefinition, "brain_metadata::ExtractorDefinition::v1");

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::knowledge::fresh_db;
    use redb::ReadableDatabase;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let ex = ExtractorDefinition::new(
            ExtractorId::from(11),
            "brain.entity_mentions".into(),
            1,
            ExtractorKind::Pattern,
            true,
            vec![],
            1_700_000_000_000_000_000,
        );

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(EXTRACTORS_TABLE).unwrap();
            t.insert(&ex.extractor_id, &ex).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(EXTRACTORS_TABLE).unwrap();
        let got = t.get(&ex.extractor_id).unwrap().unwrap().value();
        assert_eq!(got, ex);
        assert_eq!(got.kind(), Some(ExtractorKind::Pattern));
        assert!(got.is_enabled());
    }
}
