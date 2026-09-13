//! Schema-version tables.
//!
//! - [`SCHEMA_VERSIONS_TABLE`] — `(namespace, version) -> SchemaVersionRow`.
//!   The authoritative per-namespace history.
//! - [`SCHEMA_ACTIVE_VERSIONS_TABLE`] — `namespace -> version`. Pointer
//!   to the currently-active version for each known namespace.
//!
//! `SchemaVersionRow.source` is the parsed AST encoded as
//! `serde_json::to_vec(&Schema)`. Per the AST is
//! value-typed (no rkyv); JSON is the canonical encoding here. The
//! field is opaque bytes to redb — only the schema_store needs to
//! decode it.

use crate::impl_redb_rkyv_value;
use redb::TableDefinition;

pub const SCHEMA_VERSIONS_TABLE: TableDefinition<'static, (&str, u32), SchemaVersionRow> =
    TableDefinition::new("schema_versions");

pub const SCHEMA_ACTIVE_VERSIONS_TABLE: TableDefinition<'static, &str, u32> =
    TableDefinition::new("schema_active_versions");

/// Current shape version of validator output. Bump when the
/// validator's rules change in a way that requires re-running over
/// stored sources. v1.
pub const VALIDATOR_VERSION: u32 = 1;

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct SchemaVersionRow {
    pub namespace: String,
    pub version: u32,
    pub uploaded_at_unix_nanos: u64,
    /// `serde_json::to_vec(&Schema)` — see module docs.
    pub source: Vec<u8>,
    /// Verbatim DSL source if uploaded as text; `None` for
    /// programmatic `SchemaBuilder` uploads.
    pub source_text: Option<String>,
    pub validator_version: u32,
}

impl_redb_rkyv_value!(SchemaVersionRow, "brain_metadata::SchemaVersionRow");

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::fresh_db;
    use redb::ReadableDatabase;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let row = SchemaVersionRow {
            namespace: "acme".into(),
            version: 1,
            uploaded_at_unix_nanos: 1_700_000_000_000_000_000,
            source: b"{\"namespace\":\"acme\",\"items\":[]}".to_vec(),
            source_text: Some("namespace acme\n".into()),
            validator_version: VALIDATOR_VERSION,
        };

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(SCHEMA_VERSIONS_TABLE).unwrap();
            t.insert(&(row.namespace.as_str(), row.version), &row)
                .unwrap();
        }
        {
            let mut t = wtxn.open_table(SCHEMA_ACTIVE_VERSIONS_TABLE).unwrap();
            t.insert(&row.namespace.as_str(), &row.version).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(SCHEMA_VERSIONS_TABLE).unwrap();
        let got = t.get(&("acme", 1u32)).unwrap().unwrap().value();
        assert_eq!(got, row);
        let active = rtxn.open_table(SCHEMA_ACTIVE_VERSIONS_TABLE).unwrap();
        let v = active.get(&"acme").unwrap().unwrap().value();
        assert_eq!(v, 1);
    }
}
