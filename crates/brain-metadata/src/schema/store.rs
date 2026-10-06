//! Per-namespace schema persistence.
//!
//! Single transactional path for the four schema-management opcodes:
//!
//! - `SCHEMA_UPLOAD` → [`schema_upload`]: bumps the active version
//!   counter for the namespace and persists the parsed AST.
//! - `SCHEMA_GET` → [`schema_get`]: by `(namespace, version)`.
//! - `SCHEMA_LIST` → [`schema_list`]: newest-first.
//! - `SCHEMA_VALIDATE` → not in storage; the wire handler composes
//!   `parse_schema` + `validate` + [`schema_active`] (for the
//!   would-be-next version hint).
//!
//! Migration-time compatibility checks are out of scope.

use brain_protocol::schema::{DeclaredContext, Schema, SchemaItem, ValidatedSchema};
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

use super::apply::{apply_schema_definitions, SchemaApplyError};
use super::predicate::PredicateOpError;
use crate::system_schema::SYSTEM_SCHEMA_NAMESPACE;
use crate::tables::schema_version::{
    SchemaVersionRow, SCHEMA_ACTIVE_VERSIONS_TABLE, SCHEMA_VERSIONS_TABLE, VALIDATOR_VERSION,
};

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

#[derive(thiserror::Error, Debug)]
pub enum SchemaStoreError {
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),

    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),

    #[error("redb transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),

    #[error("schema_version overflow for namespace {namespace:?}")]
    VersionOverflow { namespace: String },

    #[error("json encode failed: {0}")]
    Encode(String),

    #[error("schema apply: {0}")]
    Apply(#[from] SchemaApplyError),

    #[error("predicate op while flagging pre-existing rows: {0}")]
    Predicate(#[from] PredicateOpError),
}

// ---------------------------------------------------------------------------
// Writes.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Additive-merge support.
//
// SCHEMA_UPLOAD is additive: it merges into the namespace's active schema and
// never replaces it. The DEFINITIONS always behaved that way (they fan out
// into the entity_type / predicate / relation_type intern tables), but the
// stored SOURCE did not — it was overwritten by whatever document was last
// uploaded. The two then disagreed, with user-visible consequences: a
// document declaring one type narrowed the namespace's recorded vocabulary to
// that single type, and because the merge pre-flight classifies against the
// interned definitions it then reported a re-upload of the full schema as
// idempotent and never rewrote the source, so the narrowing could not be
// undone. The source is also what says which tenant OWNS a type
// (`declared.rs`), so narrowing it silently un-claimed that tenant's private
// vocabulary.
//
// The stored source is therefore now the union of the active source and the
// uploaded document. SCHEMA_REPLACE and SCHEMA_DROP still overwrite it: their
// whole purpose is to narrow, and DROP hands us the already-narrowed schema.
// ---------------------------------------------------------------------------

/// How a write relates to the namespace's stored schema source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceMode {
    /// Plain `SCHEMA_UPLOAD`: store the union of the active source and the
    /// uploaded document.
    Merge,
    /// `SCHEMA_REPLACE` / `SCHEMA_DROP`: the document *is* the new source.
    Replace,
}

/// Identity of a declaration within a schema document: two items collide
/// only when they are the same kind with the same name.
fn item_key(item: &SchemaItem) -> (u8, &str) {
    match item {
        SchemaItem::EntityType(e) => (0, e.name.as_str()),
        SchemaItem::Predicate(p) => (1, p.name.as_str()),
        SchemaItem::RelationType(r) => (2, r.name.as_str()),
        SchemaItem::Extractor(x) => (3, x.name.as_str()),
        SchemaItem::Kind(k) => (4, k.name.as_str()),
    }
}

/// Union of `active` and `incoming`, preserving `active`'s order and letting
/// `incoming` win on a same-kind/same-name collision. A conflicting
/// redefinition is already rejected by the upload pre-flight, so "wins" here
/// only ever resolves an identical re-declaration.
fn merge_schema_items(active: &Schema, incoming: &Schema) -> Vec<SchemaItem> {
    let incoming_keys: std::collections::HashSet<(u8, String)> = incoming
        .items
        .iter()
        .map(|i| {
            let (k, n) = item_key(i);
            (k, n.to_string())
        })
        .collect();
    let mut out: Vec<SchemaItem> = active
        .items
        .iter()
        .filter(|i| {
            let (k, n) = item_key(i);
            !incoming_keys.contains(&(k, n.to_string()))
        })
        .cloned()
        .collect();
    out.extend(incoming.items.iter().cloned());
    out
}

/// Whether `namespace`'s active source already declares every item in
/// `schema`.
///
/// The upload pre-flight classifies a document against the INTERNED
/// definitions, so a document can be entirely "idempotent" while the stored
/// source is missing those declarations — which is how a narrowed source
/// became unrecoverable: re-uploading the full schema was classified as a
/// no-op and never rewrote the source. Gating the no-op on this check as well
/// means a re-upload heals the source instead of being skipped, while an
/// unchanged re-upload still costs nothing.
pub fn active_source_covers(
    rtxn: &ReadTransaction,
    namespace: &str,
    schema: &Schema,
) -> Result<bool, SchemaStoreError> {
    let Some(active) = active_schema_ast(rtxn, namespace)? else {
        return Ok(schema.items.is_empty());
    };
    let have: std::collections::HashSet<(u8, String)> = active
        .items
        .iter()
        .map(|i| {
            let (k, n) = item_key(i);
            (k, n.to_string())
        })
        .collect();
    Ok(schema.items.iter().all(|i| {
        let (k, n) = item_key(i);
        have.contains(&(k, n.to_string()))
    }))
}

/// Decode the active schema AST for `namespace`, or `None` when it has no
/// active version or the row fails to decode.
pub fn active_schema_ast(
    rtxn: &ReadTransaction,
    namespace: &str,
) -> Result<Option<Schema>, SchemaStoreError> {
    Ok(schema_active_row(rtxn, namespace)?.and_then(|row| decode_source(namespace, &row)))
}

/// Write-transaction counterpart to [`active_schema_ast`], for the apply and
/// recovery paths that read inside their own wtxn.
pub fn active_schema_ast_wtxn(
    wtxn: &WriteTransaction,
    namespace: &str,
) -> Result<Option<Schema>, SchemaStoreError> {
    let version = {
        let active = wtxn.open_table(SCHEMA_ACTIVE_VERSIONS_TABLE)?;
        let v = active.get(&namespace)?.map(|g| g.value());
        v
    };
    let Some(version) = version else {
        return Ok(None);
    };
    let row = {
        let versions = wtxn.open_table(SCHEMA_VERSIONS_TABLE)?;
        let r = versions.get(&(namespace, version))?.map(|g| g.value());
        r
    };
    Ok(row.and_then(|row| decode_source(namespace, &row)))
}

fn decode_source(namespace: &str, row: &SchemaVersionRow) -> Option<Schema> {
    match serde_json::from_slice::<Schema>(&row.source) {
        // The row is keyed by namespace; never hand back an AST claiming
        // another one.
        Ok(schema) if schema.namespace == namespace => Some(schema),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(
                target: "brain_metadata::schema",
                namespace,
                version = row.version,
                error = %e,
                "active schema source failed to decode; treating namespace as undeclared",
            );
            None
        }
    }
}

/// Type names an additive upload to `namespace` may reference without
/// re-declaring: the namespace's own active schema plus the system `brain`
/// schema.
pub fn declared_context_wtxn(
    wtxn: &WriteTransaction,
    namespace: &str,
) -> Result<DeclaredContext, SchemaStoreError> {
    let mut ctx = DeclaredContext::default();
    for ns in [namespace, SYSTEM_SCHEMA_NAMESPACE] {
        if let Some(schema) = active_schema_ast_wtxn(wtxn, ns)? {
            collect_into(&schema, &mut ctx);
        }
    }
    Ok(ctx)
}

/// Read-transaction counterpart to [`declared_context_wtxn`].
pub fn declared_context(
    rtxn: &ReadTransaction,
    namespace: &str,
) -> Result<DeclaredContext, SchemaStoreError> {
    let mut ctx = DeclaredContext::default();
    for ns in [namespace, SYSTEM_SCHEMA_NAMESPACE] {
        if let Some(schema) = active_schema_ast(rtxn, ns)? {
            collect_into(&schema, &mut ctx);
        }
    }
    Ok(ctx)
}

fn collect_into(schema: &Schema, ctx: &mut DeclaredContext) {
    for item in &schema.items {
        match item {
            SchemaItem::EntityType(e) => ctx.entity_types.push(e.name.clone()),
            SchemaItem::RelationType(r) => ctx.relation_types.push(r.name.clone()),
            _ => {}
        }
    }
}

/// Persist a validated schema as a new version of its namespace.
///
/// - Reads the namespace's current active version, increments by 1.
/// - Writes the version row to `SCHEMA_VERSIONS_TABLE`.
/// - Updates `SCHEMA_ACTIVE_VERSIONS_TABLE` to point at the new
///   version.
///
/// Atomicity: both writes live inside the caller's `wtxn`; on
/// commit they apply together. On rollback (caller dropping the
/// txn) neither row appears.
///
/// Returns the new version number.
pub fn schema_upload(
    wtxn: &WriteTransaction,
    validated: &ValidatedSchema,
    now_unix_nanos: u64,
) -> Result<u32, SchemaStoreError> {
    schema_upload_with_mode(wtxn, validated, now_unix_nanos, SourceMode::Merge)
}

/// [`schema_upload`] with an explicit [`SourceMode`]. `Merge` is the additive
/// `SCHEMA_UPLOAD` contract; `Replace` is for `SCHEMA_REPLACE` and
/// `SCHEMA_DROP`, which deliberately narrow the namespace.
pub fn schema_upload_with_mode(
    wtxn: &WriteTransaction,
    validated: &ValidatedSchema,
    now_unix_nanos: u64,
    mode: SourceMode,
) -> Result<u32, SchemaStoreError> {
    let schema = validated.as_schema();
    let namespace = schema.namespace.clone();
    let new_version = next_version_in(wtxn, &namespace)?;

    // What gets PERSISTED is the merged document; what gets APPLIED below is
    // the uploaded one. Applying the merge too would re-intern every
    // already-declared item on every upload for no gain.
    let stored: Schema = match mode {
        SourceMode::Replace => schema.clone(),
        SourceMode::Merge => match active_schema_ast_wtxn(wtxn, &namespace)? {
            Some(active) => Schema {
                items: merge_schema_items(&active, schema),
                ..schema.clone()
            },
            None => schema.clone(),
        },
    };

    let source =
        serde_json::to_vec(&stored).map_err(|e| SchemaStoreError::Encode(e.to_string()))?;
    let row = SchemaVersionRow {
        namespace: namespace.clone(),
        version: new_version,
        uploaded_at_unix_nanos: now_unix_nanos,
        source,
        source_text: schema.source.clone(),
        validator_version: VALIDATOR_VERSION,
    };

    {
        let mut versions = wtxn.open_table(SCHEMA_VERSIONS_TABLE)?;
        versions.insert(&(namespace.as_str(), new_version), &row)?;
    }
    {
        let mut active = wtxn.open_table(SCHEMA_ACTIVE_VERSIONS_TABLE)?;
        active.insert(&namespace.as_str(), &new_version)?;
    }

    // Fan out new + changed definitions into the existing
    // entity_type / predicate / relation_type intern paths.
    apply_schema_definitions(wtxn, validated, new_version, now_unix_nanos)?;

    // The OUTSIDE_ACTIVE_SCHEMA flag-sweep runs **outside** this
    // wtxn — the writer's post-commit fan-out enqueues a
    // `SchemaFlagSweepJob` to the SchemaMigrationWorker, which opens
    // its own wtxn against the just-committed schema state. Doing the
    // sweep inline would couple SCHEMA_UPLOAD ack latency to a full
    // STATEMENTS_TABLE scan; moving it post-commit keeps the upload
    // commit bounded while the worker catches up within the next tick.

    // `namespace` was cloned above to construct the row; suppress the
    // unused-binding lint now that the inline sweep is gone.
    let _ = namespace;

    Ok(new_version)
}

fn next_version_in(wtxn: &WriteTransaction, namespace: &str) -> Result<u32, SchemaStoreError> {
    let active = wtxn.open_table(SCHEMA_ACTIVE_VERSIONS_TABLE)?;
    let guard = active.get(&namespace)?;
    let current: Option<u32> = guard.map(|g| g.value());
    drop(active);
    match current {
        Some(v) => v
            .checked_add(1)
            .ok_or_else(|| SchemaStoreError::VersionOverflow {
                namespace: namespace.to_string(),
            }),
        None => Ok(1),
    }
}

// ---------------------------------------------------------------------------
// Reads.
// ---------------------------------------------------------------------------

/// Fetch a specific version of a namespace's schema. Returns
/// `Ok(None)` if the row doesn't exist.
pub fn schema_get(
    rtxn: &ReadTransaction,
    namespace: &str,
    version: u32,
) -> Result<Option<SchemaVersionRow>, SchemaStoreError> {
    let versions = rtxn.open_table(SCHEMA_VERSIONS_TABLE)?;
    let guard = versions.get(&(namespace, version))?;
    Ok(guard.map(|g| g.value()))
}

/// Fetch the active version number for a namespace.
pub fn schema_active(
    rtxn: &ReadTransaction,
    namespace: &str,
) -> Result<Option<u32>, SchemaStoreError> {
    let active = rtxn.open_table(SCHEMA_ACTIVE_VERSIONS_TABLE)?;
    let guard = active.get(&namespace)?;
    Ok(guard.map(|g| g.value()))
}

/// Fetch the active version's row in a single call.
pub fn schema_active_row(
    rtxn: &ReadTransaction,
    namespace: &str,
) -> Result<Option<SchemaVersionRow>, SchemaStoreError> {
    let Some(v) = schema_active(rtxn, namespace)? else {
        return Ok(None);
    };
    schema_get(rtxn, namespace, v)
}

/// All versions for a namespace, **newest first**.
pub fn schema_list(
    rtxn: &ReadTransaction,
    namespace: &str,
) -> Result<Vec<SchemaVersionRow>, SchemaStoreError> {
    let versions = rtxn.open_table(SCHEMA_VERSIONS_TABLE)?;
    let lo = (namespace, 0u32);
    let hi = (namespace, u32::MAX);
    let mut out = Vec::new();
    for entry in versions.range(lo..=hi)? {
        let (_k, v) = entry?;
        out.push(v.value());
    }
    out.reverse();
    Ok(out)
}

/// All namespaces with at least one active schema.
pub fn schema_namespaces(rtxn: &ReadTransaction) -> Result<Vec<String>, SchemaStoreError> {
    let active = rtxn.open_table(SCHEMA_ACTIVE_VERSIONS_TABLE)?;
    let mut out = Vec::new();
    for entry in active.iter()? {
        let (k, _v) = entry?;
        out.push(k.value().to_string());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::scope::RowScope;
    use brain_protocol::schema::{parse_schema, validate};
    use redb::{Database, ReadableDatabase};

    fn test_scope() -> RowScope {
        RowScope::from_bytes(brain_core::NamespaceId::SYSTEM.raw(), [0xAB; 16])
    }

    fn open_db(dir: &tempfile::TempDir) -> Database {
        let db = Database::create(dir.path().join("test.redb")).unwrap();
        let wtxn = db.begin_write().unwrap();
        crate::tables::materialize_all_tables(&wtxn).unwrap();
        wtxn.commit().unwrap();
        db
    }

    fn validated(src: &str) -> ValidatedSchema {
        let schema = parse_schema(src).expect("parse");
        validate(&schema).expect("validate")
    }

    fn acme_schema_v1() -> ValidatedSchema {
        validated(
            "
            namespace acme
            define entity_type Person { attributes {} }
            ",
        )
    }

    fn acme_schema_v2() -> ValidatedSchema {
        validated(
            "
            namespace acme
            define entity_type Person { attributes {} }
            define predicate prefers { kind: Preference object: Value<text> }
            ",
        )
    }

    fn crm_schema() -> ValidatedSchema {
        validated(
            "
            namespace crm
            define entity_type Lead { attributes {} }
            ",
        )
    }

    #[test]
    fn first_upload_is_version_one() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        let wtxn = db.begin_write().unwrap();
        let v = schema_upload(&wtxn, &acme_schema_v1(), 1_700_000_000_000_000_000).unwrap();
        assert_eq!(v, 1);
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        assert_eq!(schema_active(&rtxn, "acme").unwrap(), Some(1));
        assert!(schema_get(&rtxn, "acme", 1).unwrap().is_some());
    }

    #[test]
    fn second_upload_bumps_version() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        {
            let wtxn = db.begin_write().unwrap();
            schema_upload(&wtxn, &acme_schema_v1(), 1).unwrap();
            wtxn.commit().unwrap();
        }
        let v2 = {
            let wtxn = db.begin_write().unwrap();
            let v = schema_upload(&wtxn, &acme_schema_v2(), 2).unwrap();
            wtxn.commit().unwrap();
            v
        };
        assert_eq!(v2, 2);

        let rtxn = db.begin_read().unwrap();
        assert_eq!(schema_active(&rtxn, "acme").unwrap(), Some(2));
        // v1 still readable.
        assert!(schema_get(&rtxn, "acme", 1).unwrap().is_some());
        assert!(schema_get(&rtxn, "acme", 2).unwrap().is_some());
    }

    #[test]
    fn schema_get_missing_version_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        let rtxn = db.begin_read().unwrap();
        assert!(schema_get(&rtxn, "acme", 7).unwrap().is_none());
        assert_eq!(schema_active(&rtxn, "acme").unwrap(), None);
    }

    #[test]
    fn schema_list_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        for (i, s) in [acme_schema_v1(), acme_schema_v2()].iter().enumerate() {
            let wtxn = db.begin_write().unwrap();
            schema_upload(&wtxn, s, i as u64).unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.begin_read().unwrap();
        let list = schema_list(&rtxn, "acme").unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].version, 2);
        assert_eq!(list[1].version, 1);
    }

    #[test]
    fn namespaces_are_independent() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        for s in [acme_schema_v1(), acme_schema_v2(), crm_schema()] {
            let wtxn = db.begin_write().unwrap();
            schema_upload(&wtxn, &s, 1).unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.begin_read().unwrap();
        assert_eq!(schema_active(&rtxn, "acme").unwrap(), Some(2));
        assert_eq!(schema_active(&rtxn, "crm").unwrap(), Some(1));
        let nss = schema_namespaces(&rtxn).unwrap();
        assert!(nss.contains(&"acme".to_string()));
        assert!(nss.contains(&"crm".to_string()));
    }

    #[test]
    fn active_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = open_db(&dir);
            let wtxn = db.begin_write().unwrap();
            schema_upload(&wtxn, &acme_schema_v1(), 1).unwrap();
            wtxn.commit().unwrap();
        }
        let db = Database::open(dir.path().join("test.redb")).unwrap();
        let rtxn = db.begin_read().unwrap();
        assert_eq!(schema_active(&rtxn, "acme").unwrap(), Some(1));
    }

    #[test]
    fn schema_upload_does_not_run_inline_flag_sweep() {
        // The OUTSIDE_ACTIVE_SCHEMA flag-sweep is post-commit work
        // owned by the SchemaMigrationWorker — `schema_upload` must
        // never mutate flags inside its own wtxn. This test pins that
        // contract: a pre-existing statement against an
        // open-vocabulary predicate stays clean immediately after
        // `schema_upload` commits a schema that doesn't declare its
        // predicate. The worker (driven by the writer's post-commit
        // enqueue) is what eventually sets the flag.
        use crate::schema::apply::flag_statements_outside_schema;
        use crate::schema::predicate::{predicate_intern_or_get, predicates_active_for_schema};
        use crate::statement::crud::statement_create;
        use crate::tables::statement::{statement_flags, STATEMENTS_TABLE};
        use brain_core::{EntityId, ExtractorId, MemoryId, SessionId, StatementId, StatementKind};
        use brain_core::{
            EvidenceEntry, EvidenceRef, Statement, StatementObject, StatementValue, SubjectRef,
        };

        let dir = tempfile::tempdir().unwrap();
        // Use the seeded wrapper so EntityTypeId(1) (Person) exists.
        let md = crate::MetadataDb::open(dir.path().join("test.redb")).unwrap();
        let db = md.db();

        // Pre-existing schemaless world: intern two predicates and
        // write a statement for each.
        let subject = EntityId::new();
        let (sid_inside, sid_outside) = {
            let wtxn = db.begin_write().unwrap();
            // Ensure the subject exists.
            use crate::entity::ops::entity_put;
            use brain_core::Entity;
            let now = 0u64;
            entity_put(
                &wtxn,
                test_scope(),
                brain_core::SessionId::DEFAULT,
                &Entity::new_active(
                    subject,
                    brain_core::EntityTypeId(1),
                    "anchor".into(),
                    "anchor".into(),
                    now,
                ),
            )
            .unwrap();

            let p_in = predicate_intern_or_get(&wtxn, "acme", "prefers", 0, 0).unwrap();
            let p_out = predicate_intern_or_get(&wtxn, "acme", "ghost", 0, 0).unwrap();
            let mk_stmt = |pid| {
                let id = StatementId::new();
                let evidence_entry = EvidenceEntry::from_parts(
                    MemoryId::pack(1, SessionId::DEFAULT.into(), 0),
                    1.0,
                    0,
                    ExtractorId::default(),
                );
                Statement::new_root(
                    id,
                    StatementKind::Fact,
                    SubjectRef::Entity(subject),
                    pid,
                    StatementObject::Value(StatementValue::Text("x".into())),
                    0.9,
                    EvidenceRef::inline_from_slice(&[evidence_entry]),
                    ExtractorId::default(),
                    0,
                    1,
                )
            };
            let s_in = mk_stmt(p_in);
            let s_out = mk_stmt(p_out);
            let sid_in = statement_create(
                &wtxn,
                test_scope(),
                brain_core::SessionId::DEFAULT,
                &s_in,
                0,
            )
            .unwrap();
            let sid_out = statement_create(
                &wtxn,
                test_scope(),
                brain_core::SessionId::DEFAULT,
                &s_out,
                0,
            )
            .unwrap();
            wtxn.commit().unwrap();
            (sid_in, sid_out)
        };

        // Upload a schema that declares only `prefers`. Immediately
        // after commit, NEITHER row should be flagged — the worker
        // hasn't run yet.
        {
            let wtxn = db.begin_write().unwrap();
            schema_upload(&wtxn, &acme_schema_v2(), 0).unwrap();
            wtxn.commit().unwrap();
        }
        {
            let rtxn = db.begin_read().unwrap();
            let t = rtxn.open_table(STATEMENTS_TABLE).unwrap();
            let inside_pre = t.get(&sid_inside.to_bytes()).unwrap().unwrap().value();
            let outside_pre = t.get(&sid_outside.to_bytes()).unwrap().unwrap().value();
            assert!(
                !inside_pre.has_flag(statement_flags::OUTSIDE_ACTIVE_SCHEMA),
                "in-vocab row must not be flagged inline: flags={:#b}",
                inside_pre.flags,
            );
            assert!(
                !outside_pre.has_flag(statement_flags::OUTSIDE_ACTIVE_SCHEMA),
                "out-of-vocab row must not be flagged inline either — the sweep is post-commit: flags={:#b}",
                outside_pre.flags,
            );
        }

        // Now drive the sweep directly (mirroring what the worker
        // does on its next tick). The contract: in-vocab stays clean,
        // out-of-vocab gains the flag.
        let active = {
            let rtxn = db.begin_read().unwrap();
            predicates_active_for_schema(&rtxn, "acme", 1).unwrap()
        };
        {
            let wtxn = db.begin_write().unwrap();
            flag_statements_outside_schema(&wtxn, "acme", &active).unwrap();
            wtxn.commit().unwrap();
        }

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(STATEMENTS_TABLE).unwrap();
        let inside = t.get(&sid_inside.to_bytes()).unwrap().unwrap().value();
        let outside = t.get(&sid_outside.to_bytes()).unwrap().unwrap().value();
        assert!(
            !inside.has_flag(statement_flags::OUTSIDE_ACTIVE_SCHEMA),
            "in-vocabulary statement must not be flagged post-sweep: flags={:#b}",
            inside.flags,
        );
        assert!(
            outside.has_flag(statement_flags::OUTSIDE_ACTIVE_SCHEMA),
            "out-of-vocabulary statement must be flagged post-sweep: flags={:#b}",
            outside.flags,
        );
    }

    #[test]
    fn schema_active_row_returns_full_row() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        let wtxn = db.begin_write().unwrap();
        schema_upload(&wtxn, &acme_schema_v2(), 42).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let row = schema_active_row(&rtxn, "acme").unwrap().unwrap();
        assert_eq!(row.version, 1);
        assert_eq!(row.namespace, "acme");
        assert_eq!(row.uploaded_at_unix_nanos, 42);
        assert_eq!(row.validator_version, VALIDATOR_VERSION);
        assert!(row.source_text.is_some());
        // Source is JSON; decode round-trips.
        let decoded: brain_protocol::schema::Schema = serde_json::from_slice(&row.source).unwrap();
        assert_eq!(decoded.namespace, "acme");
    }
}

#[cfg(all(test, not(miri)))]
mod additive_merge_tests {
    use super::*;
    use brain_protocol::schema::{parse_schema, validate, validate_with};
    use redb::{Database, ReadableDatabase};

    fn open_db(dir: &tempfile::TempDir) -> Database {
        let db = Database::create(dir.path().join("merge.redb")).unwrap();
        let wtxn = db.begin_write().unwrap();
        crate::tables::materialize_all_tables(&wtxn).unwrap();
        wtxn.commit().unwrap();
        db
    }

    fn validated(src: &str) -> ValidatedSchema {
        let schema = parse_schema(src).expect("parse");
        validate(&schema).expect("validate")
    }

    fn upload(db: &Database, v: &ValidatedSchema, mode: SourceMode) -> u32 {
        let wtxn = db.begin_write().unwrap();
        let ver = schema_upload_with_mode(&wtxn, v, 1, mode).expect("upload");
        wtxn.commit().unwrap();
        ver
    }

    fn stored_item_names(db: &Database, namespace: &str) -> Vec<String> {
        let rtxn = db.begin_read().unwrap();
        let schema = active_schema_ast(&rtxn, namespace)
            .expect("read")
            .expect("active schema");
        let mut names: Vec<String> = schema
            .items
            .iter()
            .map(|i| item_key(i).1.to_string())
            .collect();
        names.sort();
        names
    }

    fn two_types() -> ValidatedSchema {
        validated(
            "
            namespace acme
            define entity_type Person { attributes {} }
            define entity_type Org { attributes {} }
            ",
        )
    }

    fn one_other_type() -> ValidatedSchema {
        validated(
            "
            namespace acme
            define entity_type Widget { attributes {} }
            ",
        )
    }

    /// The defect: uploading a narrow document replaced the stored source, so
    /// the namespace silently stopped declaring the types it had declared.
    #[test]
    fn a_narrow_upload_keeps_the_types_an_earlier_upload_declared() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = open_db(&dir);

        upload(&db, &two_types(), SourceMode::Merge);
        assert_eq!(stored_item_names(&db, "acme"), vec!["Org", "Person"]);

        upload(&db, &one_other_type(), SourceMode::Merge);
        assert_eq!(
            stored_item_names(&db, "acme"),
            vec!["Org", "Person", "Widget"],
            "UPLOAD is additive: a later document must not drop earlier declarations"
        );
    }

    /// REPLACE and DROP exist to narrow, so they must NOT merge.
    #[test]
    fn replace_mode_overwrites_the_stored_source() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = open_db(&dir);

        upload(&db, &two_types(), SourceMode::Merge);
        upload(&db, &one_other_type(), SourceMode::Replace);
        assert_eq!(
            stored_item_names(&db, "acme"),
            vec!["Widget"],
            "REPLACE / DROP carry the whole new schema and overwrite the source"
        );
    }

    /// Re-applying the same DSL must stay free — the no-op short-circuit the
    /// merge gate depends on.
    #[test]
    fn an_unchanged_re_upload_is_already_covered_by_the_source() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = open_db(&dir);
        upload(&db, &two_types(), SourceMode::Merge);

        let rtxn = db.begin_read().unwrap();
        assert!(
            active_source_covers(&rtxn, "acme", two_types().as_schema()).unwrap(),
            "an unchanged re-upload is a true no-op"
        );
        assert!(
            !active_source_covers(&rtxn, "acme", one_other_type().as_schema()).unwrap(),
            "a document carrying a new declaration is not covered"
        );
    }

    /// A source narrowed before the fix must heal rather than stay stuck: the
    /// old no-op test looked only at interned definitions, so re-uploading the
    /// full schema was skipped and the narrowing was permanent.
    #[test]
    fn a_narrowed_source_is_not_reported_as_covering_the_full_schema() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = open_db(&dir);

        upload(&db, &two_types(), SourceMode::Merge);
        upload(&db, &one_other_type(), SourceMode::Replace); // narrow it

        let rtxn = db.begin_read().unwrap();
        assert!(
            !active_source_covers(&rtxn, "acme", two_types().as_schema()).unwrap(),
            "the full schema is NOT covered by the narrowed source, so re-uploading it writes"
        );
    }

    /// The validator half: a document may name a type the namespace already
    /// declares. Validated standalone this is an UnresolvedTypeRef.
    #[test]
    fn an_incremental_document_resolves_against_the_active_schema() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = open_db(&dir);
        upload(&db, &two_types(), SourceMode::Merge);

        let incremental = parse_schema(
            "
            namespace acme
            define predicate works_at { kind: Fact subject: Entity<Person> object: Entity<Org> }
            ",
        )
        .expect("parse");

        assert!(
            validate(&incremental).is_err(),
            "standalone validation cannot see Person/Org — the behaviour being fixed"
        );

        let rtxn = db.begin_read().unwrap();
        let ctx = declared_context(&rtxn, "acme").expect("context");
        assert!(
            validate_with(&incremental, &ctx).is_ok(),
            "validated against the schema it merges into, the reference resolves"
        );
    }

    /// The context must not leak another tenant's vocabulary.
    #[test]
    fn the_merge_context_is_scoped_to_its_own_namespace() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = open_db(&dir);
        upload(&db, &two_types(), SourceMode::Merge);

        let rtxn = db.begin_read().unwrap();
        let other = declared_context(&rtxn, "crm").expect("context");
        assert!(
            !other.entity_types.iter().any(|n| n == "Person"),
            "acme's declarations must not be visible to crm"
        );
    }
}
