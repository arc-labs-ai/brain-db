//! `extractors` table — interned extractor registry.
//!
//! Row follows the canonical pattern (namespace + name + qname index)
//! matching [`crate::tables::predicate::PredicateDefinition`] /
//! [`crate::tables::relation_type::RelationTypeDefinition`].

use crate::impl_redb_rkyv_value;
use brain_core::{ExtractorId, ExtractorKind};
use redb::TableDefinition;

/// `extractors` table. Key is `ExtractorId.raw()` (u32); value is
/// [`ExtractorDefinition`].
pub const EXTRACTORS_TABLE: TableDefinition<'static, u32, ExtractorDefinition> =
    TableDefinition::new("extractors");

/// `extractors_by_qname` — secondary index for `"namespace:name"
/// → ExtractorId`. Mirrors `predicates_by_qname`.
pub const EXTRACTORS_BY_QNAME_TABLE: TableDefinition<'static, &str, u32> =
    TableDefinition::new("extractors_by_qname");

/// A registered extractor. `(namespace, name)` is unique within a
/// deployment; uniqueness is enforced by
/// [`EXTRACTORS_BY_QNAME_TABLE`] writes inside `extractor_intern`.
///
/// `kind`: `0` pattern, `1` classifier, `2` llm — matches
/// [`brain_core::ExtractorKind::as_u8`].
///
/// `definition_blob`: `serde_json::to_vec(&ExtractorDef)` where
/// `ExtractorDef` is the schema-DSL AST. Opaque to brain-metadata;
/// brain-extractors decodes it when materialising the runtime
/// extractor at MetadataDb::open / `SCHEMA_UPLOAD` time.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct ExtractorDefinition {
    pub extractor_id: u32,
    pub namespace: String,
    pub name: String,
    pub kind: u8,
    pub enabled: u8,
    pub schema_version: u32,
    pub definition_blob: Vec<u8>,
    pub created_at_unix_nanos: u64,
}

impl ExtractorDefinition {
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: ExtractorId,
        namespace: String,
        name: String,
        kind: ExtractorKind,
        enabled: bool,
        schema_version: u32,
        definition_blob: Vec<u8>,
        created_at_unix_nanos: u64,
    ) -> Self {
        Self {
            extractor_id: id.raw(),
            namespace,
            name,
            kind: kind.as_u8(),
            enabled: u8::from(enabled),
            schema_version,
            definition_blob,
            created_at_unix_nanos,
        }
    }

    #[must_use]
    pub fn id(&self) -> ExtractorId {
        ExtractorId::from(self.extractor_id)
    }

    #[must_use]
    pub fn kind(&self) -> Option<ExtractorKind> {
        ExtractorKind::from_u8(self.kind)
    }

    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled != 0
    }

    /// Canonical `"namespace:name"` qname.
    #[must_use]
    pub fn qname(&self) -> String {
        format!("{}:{}", self.namespace, self.name)
    }
}

impl_redb_rkyv_value!(ExtractorDefinition, "brain_metadata::ExtractorDefinition");

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::fresh_db;
    use redb::ReadableDatabase;

    #[test]
    fn extractor_definition_round_trips_through_redb_preserving_fields() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let ex = ExtractorDefinition::new(
            ExtractorId::from(11),
            "acme".into(),
            "person_mentions".into(),
            ExtractorKind::Pattern,
            true,
            1,
            vec![1, 2, 3, 4],
            1_700_000_000_000_000_000,
        );

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(EXTRACTORS_TABLE).unwrap();
            t.insert(&ex.extractor_id, &ex).unwrap();
        }
        {
            let mut t = wtxn.open_table(EXTRACTORS_BY_QNAME_TABLE).unwrap();
            t.insert(&ex.qname().as_str(), &ex.extractor_id).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(EXTRACTORS_TABLE).unwrap();
        let got = t.get(&ex.extractor_id).unwrap().unwrap().value();
        assert_eq!(got, ex);
        assert_eq!(got.kind(), Some(ExtractorKind::Pattern));
        assert!(got.is_enabled());
        assert_eq!(got.qname(), "acme:person_mentions");

        let idx = rtxn.open_table(EXTRACTORS_BY_QNAME_TABLE).unwrap();
        let id_from_idx = idx.get(&"acme:person_mentions").unwrap().unwrap().value();
        assert_eq!(id_from_idx, ex.extractor_id);
    }
}
