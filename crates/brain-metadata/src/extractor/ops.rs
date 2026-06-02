//! Typed CRUD + interning over the extractor registry.
//!
//! Mirrors [`crate::schema::predicate`] / [`crate::relation::types`]
//! patterns: qname-keyed uniqueness, idempotent intern,
//! `AlreadyExists` on diverging redefinition.

use brain_core::{ExtractorId, ExtractorKind};
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

use crate::tables::extractor::{ExtractorDefinition, EXTRACTORS_BY_QNAME_TABLE, EXTRACTORS_TABLE};

#[derive(thiserror::Error, Debug)]
pub enum ExtractorOpError {
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),

    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),

    #[error("invalid extractor identifier: {reason}")]
    InvalidIdentifier { reason: &'static str },

    #[error(
        "extractor {qname:?} already exists with id {existing_id:?} but kind / definition differ"
    )]
    AlreadyExists {
        qname: String,
        existing_id: ExtractorId,
    },

    #[error("extractor not found: id {id:?}")]
    NotFound { id: ExtractorId },
}

// ---------------------------------------------------------------------------
// Writes.
// ---------------------------------------------------------------------------

/// Intern (or look up) an extractor by its `(namespace, name)`
/// qname.
///
/// - No prior row: allocate fresh id, write row + qname index,
///   `enabled = 1`.
/// - Prior row with identical kind + schema_version +
///   definition_blob: return the existing id (idempotent).
/// - Prior row with diverging kind / definition_blob /
///   schema_version: `AlreadyExists` (caller decides whether to
///   evict + re-intern).
#[allow(clippy::too_many_arguments)]
pub fn extractor_intern(
    wtxn: &WriteTransaction,
    namespace: &str,
    name: &str,
    kind: ExtractorKind,
    schema_version: u32,
    definition_blob: Vec<u8>,
    now_unix_nanos: u64,
) -> Result<ExtractorId, ExtractorOpError> {
    validate_namespace(namespace)?;
    validate_name(name)?;

    let q = qname(namespace, name);

    // Idempotency probe.
    let existing_id: Option<u32> = {
        let idx = wtxn.open_table(EXTRACTORS_BY_QNAME_TABLE)?;
        let guard = idx.get(q.as_str())?;
        guard.map(|g| g.value())
    };
    if let Some(id_raw) = existing_id {
        let row = {
            let t = wtxn.open_table(EXTRACTORS_TABLE)?;
            let guard = t.get(&id_raw)?;
            guard
                .map(|g| g.value())
                .expect("qname index points to a missing row — file is corrupt")
        };
        let same = row.kind == kind.as_u8()
            && row.schema_version == schema_version
            && row.definition_blob == definition_blob;
        if same {
            return Ok(ExtractorId::from(id_raw));
        }
        return Err(ExtractorOpError::AlreadyExists {
            qname: q,
            existing_id: ExtractorId::from(id_raw),
        });
    }

    // Fresh registration. Allocate next id.
    let next_id_raw: u32 = {
        let t = wtxn.open_table(EXTRACTORS_TABLE)?;
        let mut max: u32 = 0;
        for entry in t.iter()? {
            let (k, _) = entry?;
            let id = k.value();
            if id > max {
                max = id;
            }
        }
        max.checked_add(1).expect("ExtractorId space exhausted")
    };

    let row = ExtractorDefinition::new(
        ExtractorId::from(next_id_raw),
        namespace.to_string(),
        name.to_string(),
        kind,
        true, // enabled by default
        schema_version,
        definition_blob,
        now_unix_nanos,
    );

    {
        let mut t = wtxn.open_table(EXTRACTORS_TABLE)?;
        t.insert(&next_id_raw, &row)?;
    }
    {
        let mut idx = wtxn.open_table(EXTRACTORS_BY_QNAME_TABLE)?;
        idx.insert(&q.as_str(), &next_id_raw)?;
    }
    Ok(ExtractorId::from(next_id_raw))
}

/// Flip the `enabled` flag on an extractor. Returns the **previous**
/// state, mirroring the `EXTRACTOR_DISABLE` / `_ENABLE` wire
/// semantics (`previously_enabled` / `previously_disabled`).
///
/// Idempotent: setting an already-`enabled` extractor to enabled
/// returns `true` (the previous state) and writes the row again
/// (which redb deduplicates) without changing meaning.
pub fn extractor_set_enabled(
    wtxn: &WriteTransaction,
    id: ExtractorId,
    enabled: bool,
) -> Result<bool, ExtractorOpError> {
    let id_raw = id.raw();
    let mut row = {
        let t = wtxn.open_table(EXTRACTORS_TABLE)?;
        let guard = t.get(&id_raw)?;
        match guard {
            Some(g) => g.value(),
            None => return Err(ExtractorOpError::NotFound { id }),
        }
    };
    let previous = row.is_enabled();
    row.enabled = u8::from(enabled);
    let mut t = wtxn.open_table(EXTRACTORS_TABLE)?;
    t.insert(&id_raw, &row)?;
    Ok(previous)
}

// ---------------------------------------------------------------------------
// Reads.
// ---------------------------------------------------------------------------

pub fn extractor_get(
    rtxn: &ReadTransaction,
    id: ExtractorId,
) -> Result<Option<ExtractorDefinition>, ExtractorOpError> {
    let t = rtxn.open_table(EXTRACTORS_TABLE)?;
    let guard = t.get(&id.raw())?;
    Ok(guard.map(|g| g.value()))
}

pub fn extractor_lookup_by_qname(
    rtxn: &ReadTransaction,
    namespace: &str,
    name: &str,
) -> Result<Option<ExtractorDefinition>, ExtractorOpError> {
    let q = qname(namespace, name);
    let idx = rtxn.open_table(EXTRACTORS_BY_QNAME_TABLE)?;
    let id_raw: Option<u32> = idx.get(q.as_str())?.map(|g| g.value());
    drop(idx);
    let Some(id_raw) = id_raw else {
        return Ok(None);
    };
    let t = rtxn.open_table(EXTRACTORS_TABLE)?;
    let guard = t.get(&id_raw)?;
    Ok(guard.map(|g| g.value()))
}

/// All registered extractors. Order is by `extractor_id` ascending.
pub fn extractor_list(
    rtxn: &ReadTransaction,
) -> Result<Vec<ExtractorDefinition>, ExtractorOpError> {
    let t = rtxn.open_table(EXTRACTORS_TABLE)?;
    let mut out = Vec::new();
    for entry in t.iter()? {
        let (_, v) = entry?;
        out.push(v.value());
    }
    Ok(out)
}

/// Drop every extractor row in `namespace`. Extractors don't track an
/// implicit-vs-declared origin (every row is schema-declared in v1),
/// so this is an unconditional namespace sweep. Counterpart to
/// [`crate::schema::predicate::predicate_drop_schema_declared`] and
/// [`crate::relation::types::relation_type_drop_schema_declared`];
/// used by `SCHEMA_REPLACE`.
pub fn extractor_drop_namespace(
    wtxn: &WriteTransaction,
    namespace: &str,
) -> Result<usize, ExtractorOpError> {
    validate_namespace(namespace)?;

    let victims: Vec<(u32, String)> = {
        let t = wtxn.open_table(EXTRACTORS_TABLE)?;
        let mut out = Vec::new();
        for entry in t.iter()? {
            let (k, v) = entry?;
            let row: ExtractorDefinition = v.value();
            if row.namespace == namespace {
                out.push((k.value(), qname(&row.namespace, &row.name)));
            }
        }
        out
    };
    let count = victims.len();
    {
        let mut t = wtxn.open_table(EXTRACTORS_TABLE)?;
        for (id, _) in &victims {
            t.remove(id)?;
        }
    }
    {
        let mut idx = wtxn.open_table(EXTRACTORS_BY_QNAME_TABLE)?;
        for (_, q) in &victims {
            idx.remove(q.as_str())?;
        }
    }
    Ok(count)
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

pub const NAMESPACE_MAX_LEN: usize = 32;
pub const NAME_MAX_LEN: usize = 64;

fn qname(namespace: &str, name: &str) -> String {
    format!("{namespace}:{name}")
}

fn validate_namespace(s: &str) -> Result<(), ExtractorOpError> {
    if s.is_empty() {
        return Err(ExtractorOpError::InvalidIdentifier {
            reason: "namespace must be non-empty",
        });
    }
    if s.len() > NAMESPACE_MAX_LEN {
        return Err(ExtractorOpError::InvalidIdentifier {
            reason: "namespace exceeds 32-char limit",
        });
    }
    Ok(())
}

fn validate_name(s: &str) -> Result<(), ExtractorOpError> {
    if s.is_empty() {
        return Err(ExtractorOpError::InvalidIdentifier {
            reason: "name must be non-empty",
        });
    }
    if s.len() > NAME_MAX_LEN {
        return Err(ExtractorOpError::InvalidIdentifier {
            reason: "name exceeds 64-char limit",
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use redb::{Database, ReadableDatabase};

    fn open_db(dir: &tempfile::TempDir) -> Database {
        let db = Database::create(dir.path().join("test.redb")).unwrap();
        let wtxn = db.begin_write().unwrap();
        crate::tables::materialize_all_tables(&wtxn).unwrap();
        wtxn.commit().unwrap();
        db
    }

    fn intern_pattern(
        wtxn: &WriteTransaction,
        ns: &str,
        name: &str,
        blob: &[u8],
    ) -> Result<ExtractorId, ExtractorOpError> {
        extractor_intern(wtxn, ns, name, ExtractorKind::Pattern, 1, blob.to_vec(), 0)
    }

    #[test]
    fn intern_fresh_assigns_id_1() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        let wtxn = db.begin_write().unwrap();
        let id = intern_pattern(&wtxn, "acme", "p1", b"blob").unwrap();
        assert_eq!(id.raw(), 1);
        wtxn.commit().unwrap();
    }

    #[test]
    fn intern_idempotent_on_identical_definition() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        let wtxn = db.begin_write().unwrap();
        let id1 = intern_pattern(&wtxn, "acme", "p1", b"x").unwrap();
        let id2 = intern_pattern(&wtxn, "acme", "p1", b"x").unwrap();
        assert_eq!(id1, id2);
        wtxn.commit().unwrap();
    }

    #[test]
    fn intern_rejects_diverging_definition() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        let wtxn = db.begin_write().unwrap();
        let id1 = intern_pattern(&wtxn, "acme", "p1", b"x").unwrap();
        let err = intern_pattern(&wtxn, "acme", "p1", b"different").unwrap_err();
        match err {
            ExtractorOpError::AlreadyExists {
                qname: q,
                existing_id,
            } => {
                assert_eq!(q, "acme:p1");
                assert_eq!(existing_id, id1);
            }
            other => panic!("expected AlreadyExists, got {other:?}"),
        }
        wtxn.commit().unwrap();
    }

    #[test]
    fn intern_allocates_max_plus_one() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        let wtxn = db.begin_write().unwrap();
        let a = intern_pattern(&wtxn, "acme", "p1", b"x").unwrap();
        let b = intern_pattern(&wtxn, "acme", "p2", b"y").unwrap();
        let c = intern_pattern(&wtxn, "acme", "p3", b"z").unwrap();
        assert_eq!(a.raw(), 1);
        assert_eq!(b.raw(), 2);
        assert_eq!(c.raw(), 3);
        wtxn.commit().unwrap();
    }

    #[test]
    fn lookup_by_qname_returns_row() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        {
            let wtxn = db.begin_write().unwrap();
            let _ = intern_pattern(&wtxn, "acme", "p1", b"x").unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.begin_read().unwrap();
        let got = extractor_lookup_by_qname(&rtxn, "acme", "p1")
            .unwrap()
            .unwrap();
        assert_eq!(got.namespace, "acme");
        assert_eq!(got.name, "p1");
        assert_eq!(got.definition_blob, b"x");
    }

    #[test]
    fn lookup_unknown_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        let rtxn = db.begin_read().unwrap();
        assert!(extractor_lookup_by_qname(&rtxn, "acme", "missing")
            .unwrap()
            .is_none());
        assert!(extractor_get(&rtxn, ExtractorId::from(99))
            .unwrap()
            .is_none());
    }

    #[test]
    fn list_returns_all_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        {
            let wtxn = db.begin_write().unwrap();
            intern_pattern(&wtxn, "acme", "p1", b"x").unwrap();
            intern_pattern(&wtxn, "acme", "p2", b"y").unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.begin_read().unwrap();
        let all = extractor_list(&rtxn).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn set_enabled_toggles_and_returns_previous() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        let id = {
            let wtxn = db.begin_write().unwrap();
            let id = intern_pattern(&wtxn, "acme", "p1", b"x").unwrap();
            wtxn.commit().unwrap();
            id
        };
        // Initial state: enabled = true.
        {
            let wtxn = db.begin_write().unwrap();
            let prev = extractor_set_enabled(&wtxn, id, false).unwrap();
            assert!(prev, "first call: extractor was enabled");
            wtxn.commit().unwrap();
        }
        // Now disabled.
        {
            let rtxn = db.begin_read().unwrap();
            let got = extractor_get(&rtxn, id).unwrap().unwrap();
            assert!(!got.is_enabled());
        }
        // Re-enable.
        {
            let wtxn = db.begin_write().unwrap();
            let prev = extractor_set_enabled(&wtxn, id, true).unwrap();
            assert!(!prev, "second call: extractor was disabled");
            wtxn.commit().unwrap();
        }
    }

    #[test]
    fn set_enabled_unknown_id_returns_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        let wtxn = db.begin_write().unwrap();
        let err = extractor_set_enabled(&wtxn, ExtractorId::from(99), false).unwrap_err();
        assert!(matches!(err, ExtractorOpError::NotFound { .. }));
        wtxn.commit().unwrap();
    }

    #[test]
    fn set_enabled_idempotent_on_same_state() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        let id = {
            let wtxn = db.begin_write().unwrap();
            let id = intern_pattern(&wtxn, "acme", "p1", b"x").unwrap();
            wtxn.commit().unwrap();
            id
        };
        // Enable an already-enabled extractor.
        let wtxn = db.begin_write().unwrap();
        let prev = extractor_set_enabled(&wtxn, id, true).unwrap();
        assert!(prev);
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        assert!(extractor_get(&rtxn, id).unwrap().unwrap().is_enabled());
    }

    #[test]
    fn validate_rejects_empty_namespace_or_name() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        let wtxn = db.begin_write().unwrap();
        let err = intern_pattern(&wtxn, "", "p1", b"x").unwrap_err();
        assert!(matches!(err, ExtractorOpError::InvalidIdentifier { .. }));
        let err = intern_pattern(&wtxn, "acme", "", b"x").unwrap_err();
        assert!(matches!(err, ExtractorOpError::InvalidIdentifier { .. }));
        wtxn.commit().unwrap();
    }
}
