//! `schema_versions` table — schema upload history.
//!
//! See `spec/21_schema_dsl/00_purpose.md` (versioning + migration).
//! Each `SCHEMA_UPLOAD` increments the version monotonically; this
//! table is the authoritative history. `migration_plan_blob` is the
//! pre-computed migration plan (phase 19); empty for the first upload.

use crate::impl_redb_rkyv_value;
use redb::TableDefinition;

pub const SCHEMA_VERSIONS_TABLE: TableDefinition<'static, u32, SchemaDocument> =
    TableDefinition::new("schema_versions");

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct SchemaDocument {
    pub version: u32,
    pub document_text: String,
    pub uploaded_at_unix_nanos: u64,
    /// `None` for the initial upload; `Some(...)` once a migration is
    /// computed against the previous version.
    pub migration_plan_blob: Option<Vec<u8>>,
}

impl SchemaDocument {
    #[must_use]
    pub fn new(version: u32, document_text: String, uploaded_at_unix_nanos: u64) -> Self {
        Self {
            version,
            document_text,
            uploaded_at_unix_nanos,
            migration_plan_blob: None,
        }
    }
}

impl_redb_rkyv_value!(SchemaDocument, "brain_metadata::SchemaDocument::v1");

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::knowledge::fresh_db;
    use redb::ReadableDatabase;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let mut doc =
            SchemaDocument::new(1, "namespace acme\n".into(), 1_700_000_000_000_000_000);
        doc.migration_plan_blob = Some(vec![9, 8, 7]);

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(SCHEMA_VERSIONS_TABLE).unwrap();
            t.insert(&doc.version, &doc).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(SCHEMA_VERSIONS_TABLE).unwrap();
        let got = t.get(&doc.version).unwrap().unwrap().value();
        assert_eq!(got, doc);
    }
}
