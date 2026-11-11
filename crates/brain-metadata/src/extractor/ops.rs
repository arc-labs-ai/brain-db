//! Typed CRUD + interning over the extractor registry.
//!
//! Mirrors [`crate::schema::predicate`] / [`crate::relation::types`]
//! patterns: qname-keyed uniqueness, idempotent intern,
//! `AlreadyExists` on diverging redefinition.

use brain_core::{ExtractorId, ExtractorKind};
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

use crate::tables::extractor::{ExtractorDefinition, EXTRACTORS_BY_SCOPE_TABLE, EXTRACTORS_TABLE};
use crate::tables::scope::{space_chain, DeclScope, SPACE_BASE};

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

    #[error("only the system namespace's extractors are upgraded in place, not {0:?}")]
    NotSystemNamespace(String),
}

// ---------------------------------------------------------------------------
// Scope resolution.
// ---------------------------------------------------------------------------

/// Resolve `qname` through the scope chain, read txn.
fn scope_get_rtxn(
    rtxn: &ReadTransaction,
    qname: &str,
    space: [u8; 16],
) -> Result<Option<u32>, ExtractorOpError> {
    let idx = rtxn.open_table(EXTRACTORS_BY_SCOPE_TABLE)?;
    for sp in space_chain(space) {
        if let Some(id) = idx.get(&(qname, sp))?.map(|g| g.value()) {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

/// Resolve `qname` in exactly one scope, write txn.
///
/// Writes probe exactly; only reads chain. A write that chained would find
/// the tenant-wide declaration and hand it back, so declaring a space's own
/// extractor would silently return the shared row and no space could ever
/// override anything.
fn scope_get_exact_wtxn(
    wtxn: &WriteTransaction,
    qname: &str,
    space: [u8; 16],
) -> Result<Option<u32>, ExtractorOpError> {
    let idx = wtxn.open_table(EXTRACTORS_BY_SCOPE_TABLE)?;
    let got = idx.get(&(qname, space))?.map(|g| g.value());
    Ok(got)
}

/// Remove every scope's entry for `qname` — the row itself is gone, so an
/// Remove the index entry that points at `id`.
///
/// The row is gone, so the entry naming it must go — and ONLY that entry.
/// One qname can have SEVERAL rows, one per scope that declared it, so
/// clearing every `(qname, *)` entry would orphan a sibling scope's row: the
/// row would survive with nothing resolving to it, and that scope's
/// declaration would silently disappear.
fn scope_remove_entry(
    wtxn: &WriteTransaction,
    qname: &str,
    id: u32,
) -> Result<(), ExtractorOpError> {
    let owned: Vec<[u8; 16]> = {
        let idx = wtxn.open_table(EXTRACTORS_BY_SCOPE_TABLE)?;
        let mut out = Vec::new();
        for entry in idx.range((qname, [0u8; 16])..=(qname, [0xFFu8; 16]))? {
            let (k, v) = entry?;
            if v.value() == id {
                out.push(k.value().1);
            }
        }
        out
    };
    let mut idx = wtxn.open_table(EXTRACTORS_BY_SCOPE_TABLE)?;
    for sp in owned {
        idx.remove(&(qname, sp))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Writes.
// ---------------------------------------------------------------------------

/// Intern (or look up) an extractor in one space.
///
/// `SPACE_BASE` declares it tenant-wide; any other space declares an
/// override that only that space resolves. Idempotency is judged against
/// that one space's row, not an inherited one — otherwise a space could
/// never adopt its own rule for a name the tenant already defines.
///
/// # Errors
/// `InvalidIdentifier` for a malformed namespace/name, `AlreadyExists` when
/// this space already declares the name with a diverging definition, and
/// redb storage failures.
#[allow(clippy::too_many_arguments)]
pub fn extractor_intern(
    wtxn: &WriteTransaction,
    scope: DeclScope<'_>,
    name: &str,
    kind: ExtractorKind,
    schema_version: u32,
    definition_blob: Vec<u8>,
    now_unix_nanos: u64,
) -> Result<ExtractorId, ExtractorOpError> {
    let (namespace, space) = (scope.namespace(), scope.space_id());
    validate_namespace(namespace)?;
    validate_name(name)?;

    let q = qname(namespace, name);

    // Idempotency probe, this space only.
    let existing_id: Option<u32> = scope_get_exact_wtxn(wtxn, q.as_str(), space)?;
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
        schema_version,
        definition_blob,
        now_unix_nanos,
    );

    {
        let mut t = wtxn.open_table(EXTRACTORS_TABLE)?;
        t.insert(&next_id_raw, &row)?;
    }
    {
        let mut idx = wtxn.open_table(EXTRACTORS_BY_SCOPE_TABLE)?;
        idx.insert(&(q.as_str(), space), &next_id_raw)?;
    }
    Ok(ExtractorId::from(next_id_raw))
}

/// Replace a SYSTEM-namespace extractor's stored definition in place when
/// it differs from `definition_blob` (same id). A no-op when the row is
/// absent (the normal intern registers it) or already identical. Refuses any
/// namespace but the reserved system one: user extractors keep the strict
/// refuse-on-divergence rule of [`extractor_intern`].
///
/// # Errors
/// [`ExtractorOpError::Storage`]-class failures from redb, or
/// `NotSystemNamespace` for a user namespace.
pub fn extractor_replace_system_definition(
    wtxn: &WriteTransaction,
    namespace: &str,
    name: &str,
    kind: ExtractorKind,
    schema_version: u32,
    definition_blob: Vec<u8>,
) -> Result<bool, ExtractorOpError> {
    if namespace != crate::system_schema::SYSTEM_SCHEMA_NAMESPACE {
        return Err(ExtractorOpError::NotSystemNamespace(namespace.to_string()));
    }
    let q = format!("{namespace}:{name}");
    // Built-ins are declared once for the whole deployment, so the row to
    // upgrade is the tenant-wide one.
    let id_raw = scope_get_exact_wtxn(wtxn, q.as_str(), SPACE_BASE)?;
    let Some(id_raw) = id_raw else {
        return Ok(false);
    };
    let mut t = wtxn.open_table(EXTRACTORS_TABLE)?;
    let current = t.get(&id_raw)?.map(|g| g.value());
    let Some(mut row) = current else {
        return Ok(false);
    };
    if row.kind == kind.as_u8()
        && row.schema_version == schema_version
        && row.definition_blob == definition_blob
    {
        return Ok(false);
    }
    row.kind = kind.as_u8();
    row.schema_version = schema_version;
    row.definition_blob = definition_blob;
    t.insert(&id_raw, &row)?;
    Ok(true)
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

/// Resolve an extractor as `space` sees it: its own declaration when it has
/// one, otherwise the tenant-wide one.
///
/// # Errors
/// redb storage failures.
pub fn extractor_lookup_by_qname(
    rtxn: &ReadTransaction,
    scope: DeclScope<'_>,
    name: &str,
) -> Result<Option<ExtractorDefinition>, ExtractorOpError> {
    let (namespace, space) = (scope.namespace(), scope.space_id());
    let q = qname(namespace, name);
    let Some(id_raw) = scope_get_rtxn(rtxn, q.as_str(), space)? else {
        return Ok(None);
    };
    let t = rtxn.open_table(EXTRACTORS_TABLE)?;
    let guard = t.get(&id_raw)?;
    Ok(guard.map(|g| g.value()))
}

/// Every registered extractor on this shard, across all namespaces and
/// spaces. Order is by `extractor_id` ascending.
///
/// **Not for a tenant-facing surface.** One shard serves many tenants, so
/// this is deliberately unscoped and exists for the paths that must see all
/// of them: building the per-shard extractor registry at startup, rebuilding
/// it after a schema change, and materializing definitions. A wire or HTTP
/// op that returned this would hand every tenant every other tenant's
/// declarations — use [`extractor_list_resolved`], which takes the caller's
/// namespaces and effective space.
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

/// Every extractor a caller in `space` resolves, among those declared in one
/// of `namespaces`.
///
/// Two boundaries, and a listing has to respect both:
///
/// - **Namespace.** An extractor belongs to the tenant that declared it.
///   `namespaces` is the caller's own namespace plus the system namespace,
///   whose built-ins serve every tenant; the caller supplies them because
///   this layer has no opinion about which namespace is the system one.
/// - **Space.** Declarations resolve narrowest-first, so a space sees its own
///   overlays plus the namespace base, and a sibling space's overlay is not
///   something it resolves. Listing by namespace alone would surface it.
///
/// Returns definitions sorted by id so a listing is stable across calls.
///
/// # Errors
/// [`ExtractorOpError`] on a storage failure.
pub fn extractor_list_resolved(
    rtxn: &ReadTransaction,
    space: [u8; 16],
    namespaces: &[&str],
) -> Result<Vec<ExtractorDefinition>, ExtractorOpError> {
    // Candidate qnames first, from the scope index: the index is one row per
    // (declaration, scope) and is small, while resolving each candidate needs
    // the space chain rather than a single lookup.
    let mut candidates: Vec<String> = Vec::new();
    {
        let idx = rtxn.open_table(EXTRACTORS_BY_SCOPE_TABLE)?;
        for entry in idx.iter()? {
            let (k, _) = entry?;
            let (qname, _) = k.value();
            // A qname is `<namespace>:<name>`, so the part before the first
            // colon is the declaring tenant.
            let Some((declaring, _)) = qname.split_once(':') else {
                continue;
            };
            if namespaces.contains(&declaring) && !candidates.iter().any(|q| q == qname) {
                candidates.push(qname.to_string());
            }
        }
    }

    let mut ids: Vec<u32> = Vec::with_capacity(candidates.len());
    for qname in &candidates {
        // Narrowest-first: this is what keeps a sibling space's overlay out,
        // and what lets a space's own override shadow the namespace base.
        if let Some(id) = scope_get_rtxn(rtxn, qname, space)? {
            ids.push(id);
        }
    }
    ids.sort_unstable();
    ids.dedup();

    let t = rtxn.open_table(EXTRACTORS_TABLE)?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(row) = t.get(&id)? {
            out.push(row.value());
        }
    }
    Ok(out)
}

/// Drop every extractor `scope` declared. Extractors don't track an
/// implicit-vs-declared origin (every row is schema-declared in v1), so this
/// is an unconditional sweep of the scope. Counterpart to
/// [`crate::schema::predicate::predicate_drop_schema_declared`] and
/// [`crate::relation::types::relation_type_drop_schema_declared`];
/// used by `SCHEMA_REPLACE`.
pub fn extractor_drop_declared(
    wtxn: &WriteTransaction,
    scope: DeclScope<'_>,
) -> Result<usize, ExtractorOpError> {
    let (namespace, space) = (scope.namespace(), scope.space_id());
    validate_namespace(namespace)?;

    // Victims are the declarations THIS SCOPE made, for the reason given on
    // [`crate::schema::predicate::predicate_drop_schema_declared`].
    let victims: Vec<(u32, String)> = {
        let idx = wtxn.open_table(EXTRACTORS_BY_SCOPE_TABLE)?;
        let t = wtxn.open_table(EXTRACTORS_TABLE)?;
        let mut out = Vec::new();
        for entry in idx.iter()? {
            let (k, v) = entry?;
            let (q, sp) = k.value();
            if sp != space {
                continue;
            }
            let id = v.value();
            let Some(row) = t.get(&id)?.map(|g| g.value()) else {
                continue;
            };
            if row.namespace == namespace {
                out.push((id, q.to_string()));
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
    for (id, q) in &victims {
        scope_remove_entry(wtxn, q.as_str(), *id)?;
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
    // Code points, not bytes — matches the predicate/relation-type name
    // validators so a multibyte name isn't clipped below the stated char
    // limit. 64 code points is ≤ 256 bytes, within the wire identifier bound.
    if s.chars().count() > NAME_MAX_LEN {
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
        extractor_intern(
            wtxn,
            DeclScope::base(ns),
            name,
            ExtractorKind::Pattern,
            1,
            blob.to_vec(),
            0,
        )
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
        let got = extractor_lookup_by_qname(&rtxn, DeclScope::base("acme"), "p1")
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
        assert!(
            extractor_lookup_by_qname(&rtxn, DeclScope::base("acme"), "missing")
                .unwrap()
                .is_none()
        );
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

    // -----------------------------------------------------------------
    // Scoped listing — the tenancy boundary
    // -----------------------------------------------------------------

    const SYSTEM: &str = "brain";
    const TEAM: [u8; 16] = [7u8; 16];
    const OTHER_TEAM: [u8; 16] = [9u8; 16];

    fn names_in(db: &Database, space: [u8; 16], namespaces: &[&str]) -> Vec<String> {
        let rtxn = db.begin_read().unwrap();
        let mut out: Vec<String> = extractor_list_resolved(&rtxn, space, namespaces)
            .unwrap()
            .into_iter()
            .map(|d| qname(&d.namespace, &d.name))
            .collect();
        out.sort();
        out
    }

    #[test]
    fn a_listing_shows_only_the_callers_namespace_and_the_system_builtins() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        let wtxn = db.begin_write().unwrap();
        intern_pattern(&wtxn, "acme", "invoices", b"a").unwrap();
        intern_pattern(&wtxn, "globex", "tickets", b"b").unwrap();
        intern_pattern(&wtxn, SYSTEM, "builtin", b"s").unwrap();
        wtxn.commit().unwrap();

        assert_eq!(
            names_in(&db, SPACE_BASE, &["acme", SYSTEM]),
            vec!["acme:invoices".to_string(), "brain:builtin".to_string()],
            "another tenant's extractor must not appear"
        );
        assert_eq!(
            names_in(&db, SPACE_BASE, &["globex", SYSTEM]),
            vec!["brain:builtin".to_string(), "globex:tickets".to_string()],
        );
    }

    #[test]
    fn a_space_does_not_resolve_a_sibling_spaces_overlay() {
        // The leak a namespace-only filter would leave: two spaces of the
        // same tenant, each with its own extractor.
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        let wtxn = db.begin_write().unwrap();
        extractor_intern(
            &wtxn,
            DeclScope::space("acme", TEAM),
            "team_only",
            ExtractorKind::Pattern,
            1,
            b"t".to_vec(),
            0,
        )
        .unwrap();
        extractor_intern(
            &wtxn,
            DeclScope::space("acme", OTHER_TEAM),
            "sibling_only",
            ExtractorKind::Pattern,
            1,
            b"o".to_vec(),
            0,
        )
        .unwrap();
        wtxn.commit().unwrap();

        assert_eq!(
            names_in(&db, TEAM, &["acme"]),
            vec!["acme:team_only".to_string()],
            "a space must not see a sibling space's overlay"
        );
        assert_eq!(
            names_in(&db, OTHER_TEAM, &["acme"]),
            vec!["acme:sibling_only".to_string()],
        );
        assert!(
            names_in(&db, SPACE_BASE, &["acme"]).is_empty(),
            "the namespace base declared nothing of its own"
        );
    }

    #[test]
    fn a_space_resolves_the_namespace_base_as_well_as_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        let wtxn = db.begin_write().unwrap();
        intern_pattern(&wtxn, "acme", "tenant_wide", b"b").unwrap();
        extractor_intern(
            &wtxn,
            DeclScope::space("acme", TEAM),
            "team_only",
            ExtractorKind::Pattern,
            1,
            b"t".to_vec(),
            0,
        )
        .unwrap();
        wtxn.commit().unwrap();

        assert_eq!(
            names_in(&db, TEAM, &["acme"]),
            vec!["acme:team_only".to_string(), "acme:tenant_wide".to_string()],
        );
        assert_eq!(
            names_in(&db, OTHER_TEAM, &["acme"]),
            vec!["acme:tenant_wide".to_string()],
            "a sibling space inherits the base and nothing else"
        );
    }

    #[test]
    fn a_spaces_own_declaration_shadows_the_base_for_that_space_alone() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        let wtxn = db.begin_write().unwrap();
        let base = intern_pattern(&wtxn, "acme", "shared", b"base").unwrap();
        let overlay = extractor_intern(
            &wtxn,
            DeclScope::space("acme", TEAM),
            "shared",
            ExtractorKind::Pattern,
            1,
            b"overlay".to_vec(),
            0,
        )
        .unwrap();
        wtxn.commit().unwrap();
        assert_ne!(base.raw(), overlay.raw(), "the overlay is its own row");

        // One name, and each scope resolves exactly one of them.
        let rtxn = db.begin_read().unwrap();
        let in_team = extractor_list_resolved(&rtxn, TEAM, &["acme"]).unwrap();
        assert_eq!(in_team.len(), 1);
        assert_eq!(in_team[0].extractor_id, overlay.raw());

        let at_base = extractor_list_resolved(&rtxn, SPACE_BASE, &["acme"]).unwrap();
        assert_eq!(at_base.len(), 1);
        assert_eq!(at_base[0].extractor_id, base.raw());
    }

    #[test]
    fn an_empty_namespace_list_resolves_nothing() {
        // Fail-closed: a caller whose namespaces could not be determined
        // sees nothing, rather than everything.
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        let wtxn = db.begin_write().unwrap();
        intern_pattern(&wtxn, "acme", "invoices", b"a").unwrap();
        wtxn.commit().unwrap();

        assert!(names_in(&db, SPACE_BASE, &[]).is_empty());
    }
}
