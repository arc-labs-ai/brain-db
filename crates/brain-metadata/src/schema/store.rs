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
use crate::tables::scope::{space_chain, DeclScope};

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

/// Whether the document `space` currently resolves to already names every
/// item in `schema` — the test for "this upload would change nothing".
///
/// The comparison must be made against the scope the upload will land at. A
/// space that inherits the tenant's document and re-uploads it unchanged is
/// still a no-op; one that uploads a different document is not.
///
/// # Errors
/// Propagates a schema-store read failure.
pub fn active_source_covers(
    rtxn: &ReadTransaction,
    scope: DeclScope<'_>,
    schema: &Schema,
) -> Result<bool, SchemaStoreError> {
    let Some(active) = active_schema_ast(rtxn, scope)? else {
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

/// Decode the schema AST `space` resolves to: its own active document when
/// it has one, otherwise the namespace-wide one.
///
/// # Errors
/// Propagates a schema-store read failure.
pub fn active_schema_ast(
    rtxn: &ReadTransaction,
    scope: DeclScope<'_>,
) -> Result<Option<Schema>, SchemaStoreError> {
    let namespace = scope.namespace();
    Ok(schema_active_row(rtxn, scope)?.and_then(|row| decode_source(namespace, &row)))
}

/// The active schema AST for one scope, resolved narrowest-first.
///
/// A space with its own document gets that document; a space that uploaded
/// none gets the tenant's. Without the fallback a space would appear to have
/// no schema at all the moment the dimension existed.
///
/// # Errors
/// Propagates a schema-store read failure.
pub fn active_schema_ast_wtxn(
    wtxn: &WriteTransaction,
    scope: DeclScope<'_>,
) -> Result<Option<Schema>, SchemaStoreError> {
    let (namespace, space) = (scope.namespace(), scope.space_id());
    for sp in space_chain(space) {
        let version = {
            let active = wtxn.open_table(SCHEMA_ACTIVE_VERSIONS_TABLE)?;
            let v = active.get(&(namespace, sp))?.map(|g| g.value());
            v
        };
        let Some(version) = version else {
            continue;
        };
        let row = {
            let versions = wtxn.open_table(SCHEMA_VERSIONS_TABLE)?;
            let r = versions.get(&(namespace, sp, version))?.map(|g| g.value());
            r
        };
        if let Some(schema) = row.and_then(|row| decode_source(namespace, &row)) {
            return Ok(Some(schema));
        }
    }
    Ok(None)
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

/// The names `space` may reference without re-declaring them.
///
/// The system schema is always consulted at the base: built-ins belong to
/// the deployment, not to any one space.
///
/// # Errors
/// Propagates a schema-store read failure.
pub fn declared_context_wtxn(
    wtxn: &WriteTransaction,
    scope: DeclScope<'_>,
) -> Result<DeclaredContext, SchemaStoreError> {
    let mut ctx = DeclaredContext::default();
    // The system namespace is always read at the base: built-ins belong to
    // the deployment, not to any one space.
    for sc in [scope, DeclScope::base(SYSTEM_SCHEMA_NAMESPACE)] {
        if let Some(schema) = active_schema_ast_wtxn(wtxn, sc)? {
            collect_into(&schema, &mut ctx);
        }
    }
    Ok(ctx)
}

/// Read-transaction counterpart to [`declared_context_wtxn`].
///
/// # Errors
/// Propagates a schema-store read failure.
pub fn declared_context(
    rtxn: &ReadTransaction,
    scope: DeclScope<'_>,
) -> Result<DeclaredContext, SchemaStoreError> {
    let mut ctx = DeclaredContext::default();
    // The system namespace is always read at the base: built-ins belong to
    // the deployment, not to any one space.
    for sc in [scope, DeclScope::base(SYSTEM_SCHEMA_NAMESPACE)] {
        if let Some(schema) = active_schema_ast(rtxn, sc)? {
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
    space: [u8; 16],
    now_unix_nanos: u64,
) -> Result<u32, SchemaStoreError> {
    schema_upload_with_mode(wtxn, validated, space, now_unix_nanos, SourceMode::Merge)
}

/// [`schema_upload`] with an explicit [`SourceMode`]. `Merge` is the additive
/// `SCHEMA_UPLOAD` contract; `Replace` is for `SCHEMA_REPLACE` and
/// `SCHEMA_DROP`, which deliberately narrow the namespace.
/// `space` is the scope this document is uploaded AT: the all-zero base for
/// the tenant's namespace-wide schema, or a real space for an overlay
/// only that space sees. See [`apply_schema_definitions`].
pub fn schema_upload_with_mode(
    wtxn: &WriteTransaction,
    validated: &ValidatedSchema,
    space: [u8; 16],
    now_unix_nanos: u64,
    mode: SourceMode,
) -> Result<u32, SchemaStoreError> {
    let schema = validated.as_schema();
    let namespace = schema.namespace.clone();
    let new_version = next_version_in(wtxn, &namespace, space)?;

    // What gets PERSISTED is the merged document; what gets APPLIED below is
    // the uploaded one. Applying the merge too would re-intern every
    // already-declared item on every upload for no gain.
    let stored: Schema = match mode {
        SourceMode::Replace => schema.clone(),
        SourceMode::Merge => {
            match active_schema_ast_wtxn(wtxn, DeclScope::space(&namespace, space))? {
                Some(active) => Schema {
                    items: merge_schema_items(&active, schema),
                    ..schema.clone()
                },
                None => schema.clone(),
            }
        }
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
        versions.insert(&(namespace.as_str(), space, new_version), &row)?;
    }
    {
        let mut active = wtxn.open_table(SCHEMA_ACTIVE_VERSIONS_TABLE)?;
        active.insert(&(namespace.as_str(), space), &new_version)?;
    }

    // Fan out new + changed definitions into the existing
    // entity_type / predicate / relation_type intern paths.
    apply_schema_definitions(wtxn, validated, space, new_version, now_unix_nanos)?;

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

/// The next version number for one declaration scope.
///
/// Counted per `(namespace, space)`, so a space's first upload is version 1
/// even when the namespace base is already at 7. Sharing one counter would
/// make a space's version numbers jump whenever the tenant uploaded, and the
/// WAL's `(namespace, space, version)` idempotency key is only meaningful if
/// the number identifies a document within its own scope.
fn next_version_in(
    wtxn: &WriteTransaction,
    namespace: &str,
    space: [u8; 16],
) -> Result<u32, SchemaStoreError> {
    let active = wtxn.open_table(SCHEMA_ACTIVE_VERSIONS_TABLE)?;
    let guard = active.get(&(namespace, space))?;
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

/// One exact `(namespace, space, version)` row.
///
/// Exact, not chained: this answers "does this scope already hold this
/// version", which is the WAL's idempotency question. Chaining would let a
/// space-scoped replay be skipped because the namespace base happened to
/// hold the same version number, and the space's schema would silently never
/// apply.
///
/// # Errors
/// Propagates a schema-store read failure.
pub fn schema_get(
    rtxn: &ReadTransaction,
    scope: DeclScope<'_>,
    version: u32,
) -> Result<Option<SchemaVersionRow>, SchemaStoreError> {
    let (namespace, space) = (scope.namespace(), scope.space_id());
    let versions = rtxn.open_table(SCHEMA_VERSIONS_TABLE)?;
    let guard = versions.get(&(namespace, space, version))?;
    Ok(guard.map(|g| g.value()))
}

/// Write-transaction counterpart to [`schema_active`].
///
/// A write path has to ask inside its own txn: resolving the version from a
/// separate read txn would read a snapshot taken before the write, so an
/// upload and a statement in the same transaction could disagree about
/// which document is active.
///
/// # Errors
/// Propagates a schema-store read failure.
pub fn schema_active_wtxn(
    wtxn: &WriteTransaction,
    scope: DeclScope<'_>,
) -> Result<Option<u32>, SchemaStoreError> {
    let (namespace, space) = (scope.namespace(), scope.space_id());
    let active = wtxn.open_table(SCHEMA_ACTIVE_VERSIONS_TABLE)?;
    for sp in space_chain(space) {
        if let Some(v) = active.get(&(namespace, sp))?.map(|g| g.value()) {
            return Ok(Some(v));
        }
    }
    Ok(None)
}

/// The active version declared by exactly one scope.
///
/// Distinct from [`schema_active`]: a caller that needs to know WHICH
/// scope supplied a version cannot use the chained read, because it hands
/// back a number without saying where it came from.
///
/// # Errors
/// Propagates a schema-store read failure.
pub fn schema_active_exact(
    rtxn: &ReadTransaction,
    scope: DeclScope<'_>,
) -> Result<Option<u32>, SchemaStoreError> {
    let (namespace, space) = (scope.namespace(), scope.space_id());
    let active = rtxn.open_table(SCHEMA_ACTIVE_VERSIONS_TABLE)?;
    let got = active.get(&(namespace, space))?.map(|g| g.value());
    Ok(got)
}

/// Which scope a reader at `space` takes its declarations from: its own,
/// when it declares a document, otherwise the namespace base.
///
/// Every read that reports a scope's schema as a whole — its active version,
/// its row, its version history — must resolve this once and then stay in
/// that scope. Resolving each question independently is how a space ends up
/// told its active version is its own while its version list is the
/// tenant's.
///
/// `None` when neither scope declares anything.
///
/// # Errors
/// Propagates a schema-store read failure.
pub fn declaring_scope(
    rtxn: &ReadTransaction,
    scope: DeclScope<'_>,
) -> Result<Option<[u8; 16]>, SchemaStoreError> {
    let (namespace, space) = (scope.namespace(), scope.space_id());
    for sp in space_chain(space) {
        if schema_active_exact(rtxn, DeclScope::space(namespace, sp))?.is_some() {
            return Ok(Some(sp));
        }
    }
    Ok(None)
}

/// Version history of the scope `space` reads its declarations from,
/// **newest first** — empty when neither scope declares anything.
///
/// # Errors
/// Propagates a schema-store read failure.
pub fn schema_list_resolved(
    rtxn: &ReadTransaction,
    scope: DeclScope<'_>,
) -> Result<Vec<SchemaVersionRow>, SchemaStoreError> {
    let namespace = scope.namespace();
    match declaring_scope(rtxn, scope)? {
        Some(sp) => schema_list(rtxn, DeclScope::space(namespace, sp)),
        None => Ok(Vec::new()),
    }
}

/// The active version for one scope, resolved narrowest-first.
///
/// # Errors
/// Propagates a schema-store read failure.
pub fn schema_active(
    rtxn: &ReadTransaction,
    scope: DeclScope<'_>,
) -> Result<Option<u32>, SchemaStoreError> {
    let (namespace, space) = (scope.namespace(), scope.space_id());
    let active = rtxn.open_table(SCHEMA_ACTIVE_VERSIONS_TABLE)?;
    for sp in space_chain(space) {
        if let Some(v) = active.get(&(namespace, sp))?.map(|g| g.value()) {
            return Ok(Some(v));
        }
    }
    Ok(None)
}

/// The active version row `space` resolves to.
///
/// The version number is resolved through the scope chain, and the row is
/// then read from the scope that supplied it — reading it back at `space`
/// would miss the row whenever the version came from the namespace base.
///
/// # Errors
/// Propagates a schema-store read failure.
pub fn schema_active_row(
    rtxn: &ReadTransaction,
    scope: DeclScope<'_>,
) -> Result<Option<SchemaVersionRow>, SchemaStoreError> {
    let namespace = scope.namespace();
    let Some(sp) = declaring_scope(rtxn, scope)? else {
        return Ok(None);
    };
    let declaring = DeclScope::space(namespace, sp);
    let Some(v) = schema_active_exact(rtxn, declaring)? else {
        return Ok(None);
    };
    schema_get(rtxn, declaring, v)
}

/// All versions for one `(namespace, space)` scope, **newest first**.
///
/// Scoped rather than chained: this is a history listing, and splicing the
/// tenant's versions into a space's history would present documents the space
/// never uploaded as its own.
///
/// # Errors
/// Propagates a schema-store read failure.
pub fn schema_list(
    rtxn: &ReadTransaction,
    scope: DeclScope<'_>,
) -> Result<Vec<SchemaVersionRow>, SchemaStoreError> {
    let (namespace, space) = (scope.namespace(), scope.space_id());
    let versions = rtxn.open_table(SCHEMA_VERSIONS_TABLE)?;
    let lo = (namespace, space, 0u32);
    let hi = (namespace, space, u32::MAX);
    let mut out = Vec::new();
    for entry in versions.range(lo..=hi)? {
        let (_k, v) = entry?;
        out.push(v.value());
    }
    out.reverse();
    Ok(out)
}

/// All namespaces with at least one active schema, in any scope.
///
/// De-duplicated: a namespace whose base and two spaces each have a document
/// is one namespace, and callers use this to enumerate tenants.
pub fn schema_namespaces(rtxn: &ReadTransaction) -> Result<Vec<String>, SchemaStoreError> {
    let active = rtxn.open_table(SCHEMA_ACTIVE_VERSIONS_TABLE)?;
    let mut out: Vec<String> = Vec::new();
    for entry in active.iter()? {
        let (k, _v) = entry?;
        let (ns, _space) = k.value();
        if out.last().map(String::as_str) != Some(ns) {
            out.push(ns.to_string());
        }
    }
    out.dedup();
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::scope::{RowScope, SPACE_BASE};
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

    // ── Per-scope schema versions ─────────────────────────────────────────
    //
    // A document is uploaded AT a scope. Keyed by namespace alone, two
    // spaces' uploads overwrote each other's active pointer while their
    // registry rows stayed separate, so the document and the registry
    // disagreed about what a space had declared.

    const SP_A: [u8; 16] = [0xA1; 16];
    const SP_B: [u8; 16] = [0xB2; 16];

    fn up_at(db: &Database, v: &ValidatedSchema, space: [u8; 16]) -> u32 {
        let wtxn = db.begin_write().unwrap();
        let ver = schema_upload(&wtxn, v, space, 1).unwrap();
        wtxn.commit().unwrap();
        ver
    }

    #[test]
    fn versions_count_per_scope_not_per_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        assert_eq!(up_at(&db, &acme_schema_v1(), SPACE_BASE), 1);
        assert_eq!(up_at(&db, &acme_schema_v1(), SPACE_BASE), 2);
        // A space's first document is version 1, not 3: the number has to
        // identify a document within its own scope for the WAL's
        // (namespace, space, version) key to mean anything.
        assert_eq!(up_at(&db, &acme_schema_v1(), SP_A), 1);
        assert_eq!(up_at(&db, &acme_schema_v1(), SP_A), 2);
        assert_eq!(up_at(&db, &acme_schema_v1(), SP_B), 1);
    }

    #[test]
    fn two_spaces_do_not_overwrite_each_others_active_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        up_at(&db, &acme_schema_v1(), SPACE_BASE);
        up_at(&db, &acme_schema_v1(), SP_A);
        up_at(&db, &acme_schema_v1(), SP_A);

        let rtxn = db.begin_read().unwrap();
        assert_eq!(
            schema_active(&rtxn, DeclScope::base("acme")).unwrap(),
            Some(1)
        );
        assert_eq!(
            schema_active(&rtxn, DeclScope::space("acme", SP_A)).unwrap(),
            Some(2)
        );
    }

    /// A space that uploaded nothing sees the tenant's document, which is
    /// what keeps an overlay additive rather than isolating.
    #[test]
    fn a_space_without_a_document_falls_back_to_the_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        up_at(&db, &acme_schema_v1(), SPACE_BASE);
        let rtxn = db.begin_read().unwrap();
        assert_eq!(
            schema_active(&rtxn, DeclScope::space("acme", SP_B)).unwrap(),
            Some(1)
        );
    }

    /// The WAL's idempotency check asks whether THIS scope already holds the
    /// version. Chained, it would find the namespace base's version 1 and
    /// skip a space's replay, and that space's schema would never apply.
    #[test]
    fn an_exact_get_does_not_confuse_a_spaces_version_with_the_bases() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        up_at(&db, &acme_schema_v1(), SPACE_BASE);

        let rtxn = db.begin_read().unwrap();
        assert!(
            schema_get(&rtxn, DeclScope::base("acme"), 1)
                .unwrap()
                .is_some(),
            "the base holds version 1"
        );
        assert!(
            schema_get(&rtxn, DeclScope::space("acme", SP_A), 1)
                .unwrap()
                .is_none(),
            "the space does not, and must not inherit the base's row — a \
             replay for the space would otherwise be skipped"
        );
    }

    /// A history listing is per scope: splicing the tenant's versions into a
    /// space's history would present documents it never uploaded as its own.
    #[test]
    fn a_history_listing_stays_within_its_scope() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        up_at(&db, &acme_schema_v1(), SPACE_BASE);
        up_at(&db, &acme_schema_v1(), SPACE_BASE);
        up_at(&db, &acme_schema_v1(), SP_A);

        let rtxn = db.begin_read().unwrap();
        assert_eq!(
            schema_list(&rtxn, DeclScope::base("acme")).unwrap().len(),
            2
        );
        assert_eq!(
            schema_list(&rtxn, DeclScope::space("acme", SP_A))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            schema_list(&rtxn, DeclScope::space("acme", SP_B))
                .unwrap()
                .len(),
            0
        );
    }

    /// One tenant with documents in several scopes is still one namespace.
    #[test]
    fn namespaces_are_listed_once_however_many_scopes_they_use() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        up_at(&db, &acme_schema_v1(), SPACE_BASE);
        up_at(&db, &acme_schema_v1(), SP_A);
        up_at(&db, &acme_schema_v1(), SP_B);

        let rtxn = db.begin_read().unwrap();
        assert_eq!(schema_namespaces(&rtxn).unwrap(), vec!["acme".to_string()]);
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
        let v = schema_upload(
            &wtxn,
            &acme_schema_v1(),
            crate::tables::scope::SPACE_BASE,
            1_700_000_000_000_000_000,
        )
        .unwrap();
        assert_eq!(v, 1);
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        assert_eq!(
            schema_active(&rtxn, DeclScope::base("acme")).unwrap(),
            Some(1)
        );
        assert!(schema_get(&rtxn, DeclScope::base("acme"), 1)
            .unwrap()
            .is_some());
    }

    #[test]
    fn second_upload_bumps_version() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        {
            let wtxn = db.begin_write().unwrap();
            schema_upload(
                &wtxn,
                &acme_schema_v1(),
                crate::tables::scope::SPACE_BASE,
                1,
            )
            .unwrap();
            wtxn.commit().unwrap();
        }
        let v2 = {
            let wtxn = db.begin_write().unwrap();
            let v = schema_upload(
                &wtxn,
                &acme_schema_v2(),
                crate::tables::scope::SPACE_BASE,
                2,
            )
            .unwrap();
            wtxn.commit().unwrap();
            v
        };
        assert_eq!(v2, 2);

        let rtxn = db.begin_read().unwrap();
        assert_eq!(
            schema_active(&rtxn, DeclScope::base("acme")).unwrap(),
            Some(2)
        );
        // v1 still readable.
        assert!(schema_get(&rtxn, DeclScope::base("acme"), 1)
            .unwrap()
            .is_some());
        assert!(schema_get(&rtxn, DeclScope::base("acme"), 2)
            .unwrap()
            .is_some());
    }

    #[test]
    fn schema_get_missing_version_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        let rtxn = db.begin_read().unwrap();
        assert!(schema_get(&rtxn, DeclScope::base("acme"), 7)
            .unwrap()
            .is_none());
        assert_eq!(schema_active(&rtxn, DeclScope::base("acme")).unwrap(), None);
    }

    #[test]
    fn schema_list_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        for (i, s) in [acme_schema_v1(), acme_schema_v2()].iter().enumerate() {
            let wtxn = db.begin_write().unwrap();
            schema_upload(&wtxn, s, crate::tables::scope::SPACE_BASE, i as u64).unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.begin_read().unwrap();
        let list = schema_list(&rtxn, DeclScope::base("acme")).unwrap();
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
            schema_upload(&wtxn, &s, crate::tables::scope::SPACE_BASE, 1).unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.begin_read().unwrap();
        assert_eq!(
            schema_active(&rtxn, DeclScope::base("acme")).unwrap(),
            Some(2)
        );
        assert_eq!(
            schema_active(&rtxn, DeclScope::base("crm")).unwrap(),
            Some(1)
        );
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
            schema_upload(
                &wtxn,
                &acme_schema_v1(),
                crate::tables::scope::SPACE_BASE,
                1,
            )
            .unwrap();
            wtxn.commit().unwrap();
        }
        let db = Database::open(dir.path().join("test.redb")).unwrap();
        let rtxn = db.begin_read().unwrap();
        assert_eq!(
            schema_active(&rtxn, DeclScope::base("acme")).unwrap(),
            Some(1)
        );
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

            let p_in =
                predicate_intern_or_get(&wtxn, DeclScope::base("acme"), "prefers", 0, 0).unwrap();
            let p_out =
                predicate_intern_or_get(&wtxn, DeclScope::base("acme"), "ghost", 0, 0).unwrap();
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
            schema_upload(
                &wtxn,
                &acme_schema_v2(),
                crate::tables::scope::SPACE_BASE,
                0,
            )
            .unwrap();
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
            predicates_active_for_schema(&rtxn, DeclScope::base("acme"), 1).unwrap()
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
        schema_upload(
            &wtxn,
            &acme_schema_v2(),
            crate::tables::scope::SPACE_BASE,
            42,
        )
        .unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let row = schema_active_row(&rtxn, DeclScope::base("acme"))
            .unwrap()
            .unwrap();
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
        let ver = schema_upload_with_mode(&wtxn, v, crate::tables::scope::SPACE_BASE, 1, mode)
            .expect("upload");
        wtxn.commit().unwrap();
        ver
    }

    fn stored_item_names(db: &Database, namespace: &str) -> Vec<String> {
        let rtxn = db.begin_read().unwrap();
        let schema = active_schema_ast(&rtxn, DeclScope::base(namespace))
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
            active_source_covers(&rtxn, DeclScope::base("acme"), two_types().as_schema()).unwrap(),
            "an unchanged re-upload is a true no-op"
        );
        assert!(
            !active_source_covers(&rtxn, DeclScope::base("acme"), one_other_type().as_schema())
                .unwrap(),
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
            !active_source_covers(&rtxn, DeclScope::base("acme"), two_types().as_schema()).unwrap(),
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
        let ctx = declared_context(&rtxn, DeclScope::base("acme")).expect("context");
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
        let other = declared_context(&rtxn, DeclScope::base("crm")).expect("context");
        assert!(
            !other.entity_types.iter().any(|n| n == "Person"),
            "acme's declarations must not be visible to crm"
        );
    }
}
