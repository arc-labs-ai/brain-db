//! Typed CRUD + interning over the predicate registry.
//!
//! Free functions over `redb::{ReadTransaction, WriteTransaction}`
//! mirroring the [`crate::entity::ops`] precedent: callers compose them
//! inside their own redb txns so a `statement_create` can
//! validate-and-write atomically.

use std::collections::HashSet;

use brain_core::PredicateId;
use brain_core::{Predicate, StatementKind};
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

use crate::tables::predicate::{
    PredicateDefinition, SchemaOrigin, PREDICATES_BY_QNAME_TABLE, PREDICATES_TABLE,
    PREDICATE_EMBEDDINGS_TABLE,
};

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

#[derive(thiserror::Error, Debug)]
pub enum PredicateOpError {
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),

    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),

    #[error("invalid predicate identifier: {reason}")]
    InvalidIdentifier { reason: &'static str },

    #[error("predicate {qname:?} already exists with id {existing_id:?} but constraints differ")]
    AlreadyExists {
        qname: String,
        existing_id: PredicateId,
    },
}

// ---------------------------------------------------------------------------
// Identifier validation.
// ---------------------------------------------------------------------------

/// Max length for `namespace`.
pub const NAMESPACE_MAX_LEN: usize = 32;
/// Max length for `name`.
pub const NAME_MAX_LEN: usize = 64;

/// Validate a namespace segment of a predicate qname. Conservative
/// grammar: `[a-z][a-z0-9_]*`, ASCII only, no leading digit, no `:`
/// (qname separator). Empty is rejected.
fn validate_namespace(s: &str) -> Result<(), PredicateOpError> {
    validate_identifier(s, NAMESPACE_MAX_LEN, "namespace")
}

/// Validate a name segment of a predicate qname.
///
/// Predicate names are an OPEN vocabulary coined from arbitrary-domain,
/// arbitrary-language source text (`作用于`, `wirkt_gegen`, `5ht2a_agonist`,
/// `inhibits`). The grammar is therefore **structural-safety only**, not an
/// ASCII/English grammar: any-script letters and digits plus `_`/`-` are
/// accepted; only characters that would break the qname or the store are
/// rejected (the `:` qname separator, whitespace, control chars). This keeps
/// the write path from rejecting — and thereby dropping — a genuine non-English
/// or symbol-bearing relation. Length is bounded by code points, not bytes, so
/// a multibyte name isn't unfairly clipped.
fn validate_name(s: &str) -> Result<(), PredicateOpError> {
    if s.is_empty() {
        return Err(PredicateOpError::InvalidIdentifier {
            reason: "name must not be empty",
        });
    }
    if s.chars().count() > NAME_MAX_LEN {
        return Err(PredicateOpError::InvalidIdentifier {
            reason: "name exceeds 64 characters",
        });
    }
    let mut saw_alnum = false;
    let mut first = true;
    for c in s.chars() {
        // Structural hazards: `:` splits the qname; whitespace/control break
        // storage, logging, and embedding-phrase derivation.
        if c == ':' || c.is_whitespace() || c.is_control() {
            return Err(PredicateOpError::InvalidIdentifier {
                reason: "name must not contain ':', whitespace, or control characters",
            });
        }
        let is_alnum = c.is_alphanumeric();
        if is_alnum {
            saw_alnum = true;
        }
        // A leading connector (`_`/`-`) reads as a malformed fragment, not a
        // relation; require the name to start with an actual letter/digit.
        if first && !is_alnum {
            return Err(PredicateOpError::InvalidIdentifier {
                reason: "name must start with a letter or digit",
            });
        }
        // Body: letters/digits of any script, plus the two connectors a
        // coined relation legitimately uses.
        if !is_alnum && c != '_' && c != '-' {
            return Err(PredicateOpError::InvalidIdentifier {
                reason: "name may contain only letters, digits, '_', or '-'",
            });
        }
        first = false;
    }
    if !saw_alnum {
        return Err(PredicateOpError::InvalidIdentifier {
            reason: "name must contain at least one letter or digit",
        });
    }
    Ok(())
}

fn validate_identifier(s: &str, max: usize, label: &'static str) -> Result<(), PredicateOpError> {
    if s.is_empty() {
        return Err(PredicateOpError::InvalidIdentifier {
            reason: match label {
                "namespace" => "namespace must not be empty",
                _ => "name must not be empty",
            },
        });
    }
    if s.len() > max {
        return Err(PredicateOpError::InvalidIdentifier {
            reason: match label {
                "namespace" => "namespace exceeds 32 chars",
                _ => "name exceeds 64 chars",
            },
        });
    }
    let mut chars = s.chars();
    let first = chars.next().expect("non-empty checked above");
    if !first.is_ascii_lowercase() {
        return Err(PredicateOpError::InvalidIdentifier {
            reason: match label {
                "namespace" => "namespace must start with [a-z]",
                _ => "name must start with [a-z]",
            },
        });
    }
    for c in std::iter::once(first).chain(chars) {
        let ok = c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_';
        if !ok {
            return Err(PredicateOpError::InvalidIdentifier {
                reason: match label {
                    "namespace" => "namespace must match [a-z][a-z0-9_]*",
                    _ => "name must match [a-z][a-z0-9_]*",
                },
            });
        }
    }
    Ok(())
}

/// Canonical qname: `"namespace:name"`.
#[must_use]
fn qname(namespace: &str, name: &str) -> String {
    format!("{namespace}:{name}")
}

// ---------------------------------------------------------------------------
// Read paths.
// ---------------------------------------------------------------------------

/// Fetch a predicate by id. Returns `None` if the row doesn't exist.
pub fn predicate_get(
    rtxn: &ReadTransaction,
    id: PredicateId,
) -> Result<Option<Predicate>, PredicateOpError> {
    let t = rtxn.open_table(PREDICATES_TABLE)?;
    let row: Option<PredicateDefinition> = t.get(&id.raw())?.map(|g| g.value());
    Ok(row.as_ref().map(PredicateDefinition::to_predicate))
}

/// Look up a predicate by its namespaced qname. Identifier validation
/// is enforced — invalid namespace/name produces
/// [`PredicateOpError::InvalidIdentifier`] instead of `Ok(None)`.
pub fn predicate_lookup_by_qname(
    rtxn: &ReadTransaction,
    namespace: &str,
    name: &str,
) -> Result<Option<Predicate>, PredicateOpError> {
    validate_namespace(namespace)?;
    validate_name(name)?;

    let q = qname(namespace, name);
    let idx = rtxn.open_table(PREDICATES_BY_QNAME_TABLE)?;
    let id_raw: Option<u32> = idx.get(q.as_str())?.map(|g| g.value());
    let Some(id_raw) = id_raw else {
        return Ok(None);
    };

    let t = rtxn.open_table(PREDICATES_TABLE)?;
    let row: Option<PredicateDefinition> = t.get(&id_raw)?.map(|g| g.value());
    Ok(row.as_ref().map(PredicateDefinition::to_predicate))
}

/// Set of `PredicateId`s the active schema version of `namespace`
/// declares. Used by schema-strict STATEMENT_CREATE / QUERY to
/// validate incoming predicate qnames without doing per-row lookups.
///
/// Includes only rows whose `SchemaOrigin == SchemaDeclared { version }`
/// — implicit-from-write rows are never reported here even if their
/// namespace later gains a schema. Schema-strict callers want exactly
/// the schema's vocabulary, not the vocabulary plus historical
/// schemaless drift.
pub fn predicates_active_for_schema(
    rtxn: &ReadTransaction,
    namespace: &str,
    version: u32,
) -> Result<HashSet<PredicateId>, PredicateOpError> {
    validate_namespace(namespace)?;
    let t = rtxn.open_table(PREDICATES_TABLE)?;
    let mut out = HashSet::new();
    for entry in t.iter()? {
        let (_, v) = entry?;
        let row = v.value();
        if row.namespace != namespace {
            continue;
        }
        if let SchemaOrigin::SchemaDeclared { version: v_decl } = row.origin() {
            if v_decl == version {
                out.insert(PredicateId::from(row.predicate_id));
            }
        }
    }
    Ok(out)
}

/// List predicates. With `namespace_filter = None`, returns every
/// registered predicate; with `Some(ns)`, only that namespace's
/// predicates. O(N) over the primary table; predicate count is small.
pub fn predicate_list(
    rtxn: &ReadTransaction,
    namespace_filter: Option<&str>,
) -> Result<Vec<Predicate>, PredicateOpError> {
    if let Some(ns) = namespace_filter {
        validate_namespace(ns)?;
    }
    let t = rtxn.open_table(PREDICATES_TABLE)?;
    let mut out = Vec::new();
    for entry in t.iter()? {
        let (_, v) = entry?;
        let row = v.value();
        if let Some(ns) = namespace_filter {
            if row.namespace != ns {
                continue;
            }
        }
        out.push(row.to_predicate());
    }
    Ok(out)
}

/// Render the active predicates as a prompt block for the LLM extractor's
/// `{DECLARED_PREDICATES}` placeholder. Each predicate becomes one bullet
/// line of the form `qname (Kind, single|set): description`. This makes the
/// extractor's closed vocabulary track the **active schema** at runtime —
/// the seeded system core together with any predicates a user declared via
/// `SCHEMA_UPLOAD` — so a user's declared predicate appears here
/// automatically with no system-schema edits.
///
/// Predicates whose name starts with `behavior_` are excluded; they are
/// procedural-memory sinks materialized by `MATERIALIZE_PROCEDURAL`, not
/// extraction targets, and listing them only invites mis-mapping.
pub fn render_declared_predicates_block(
    rtxn: &ReadTransaction,
) -> Result<String, PredicateOpError> {
    let mut preds = predicate_list(rtxn, None)?;
    // Stable order: namespace then name, so the prompt block (and its
    // prompt-cache key) is deterministic across cycles.
    preds.sort_by(|a, b| (a.namespace.as_str(), a.name.as_str()).cmp(&(&b.namespace, &b.name)));
    let mut out = String::new();
    for p in &preds {
        if p.name.starts_with("behavior_") {
            continue;
        }
        let kind = match p.kind_constraint {
            Some(StatementKind::Fact) => "Fact",
            Some(StatementKind::Preference) => "Preference",
            Some(StatementKind::Event) => "Event",
            Some(StatementKind::Attribute) => "Attribute",
            Some(StatementKind::Relation) => "Relation",
            Some(StatementKind::Directive) => "Directive",
            Some(StatementKind::Custom(_)) | None => "any-kind",
        };
        let card = if p.is_stateful { "single" } else { "set" };
        out.push_str("- ");
        out.push_str(&p.namespace);
        out.push(':');
        out.push_str(&p.name);
        out.push_str(" (");
        out.push_str(kind);
        out.push_str(", ");
        out.push_str(card);
        out.push(')');
        if !p.description.is_empty() {
            out.push_str(": ");
            out.push_str(&p.description);
        }
        out.push('\n');
    }
    Ok(out)
}

/// Store the semantic embedding for a predicate. Called when a predicate
/// is first interned at extraction time. `vec` is the BGE-small output
/// (384 dims); stored as little-endian `f32` bytes. Idempotent overwrite.
pub fn predicate_embedding_put(
    wtxn: &WriteTransaction,
    predicate_id: PredicateId,
    vec: &[f32],
) -> Result<(), PredicateOpError> {
    let mut bytes = Vec::with_capacity(vec.len() * 4);
    for f in vec {
        bytes.extend_from_slice(&f.to_le_bytes());
    }
    let mut t = wtxn.open_table(PREDICATE_EMBEDDINGS_TABLE)?;
    t.insert(predicate_id.raw(), bytes.as_slice())?;
    Ok(())
}

/// Load a predicate's embedding, decoding the little-endian `f32` bytes.
/// Returns `None` when no vector was stored (e.g. predicates interned
/// before the embedding pass, or user-authored facts with no extractor
/// embedding).
pub fn predicate_embedding_get(
    rtxn: &ReadTransaction,
    predicate_id: PredicateId,
) -> Result<Option<Vec<f32>>, PredicateOpError> {
    let t = rtxn.open_table(PREDICATE_EMBEDDINGS_TABLE)?;
    let Some(g) = t.get(predicate_id.raw())? else {
        return Ok(None);
    };
    let bytes = g.value();
    let mut out = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.chunks_exact(4) {
        out.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Ok(Some(out))
}

/// Record one sighting of an undeclared predicate qname in the review
/// queue, incrementing its count. Called by the extractor's closed-vocab
/// gate when a proposed predicate isn't in the active schema, so the
/// candidate is captured durably for later promotion instead of silently
/// lost. Composed inside the caller's write txn.
pub fn predicate_review_record(
    wtxn: &WriteTransaction,
    qname: &str,
) -> Result<(), PredicateOpError> {
    let mut t = wtxn.open_table(crate::tables::predicate::PREDICATE_REVIEW_QUEUE_TABLE)?;
    let prev = t.get(qname)?.map(|g| g.value()).unwrap_or(0);
    t.insert(qname, &prev.saturating_add(1))?;
    Ok(())
}

/// List the review queue as `(qname, count)` pairs, descending by count.
/// For operator review — which coined predicates recur enough to promote.
pub fn predicate_review_list(
    rtxn: &ReadTransaction,
) -> Result<Vec<(String, u64)>, PredicateOpError> {
    let t = match rtxn.open_table(crate::tables::predicate::PREDICATE_REVIEW_QUEUE_TABLE) {
        Ok(t) => t,
        // Never-written table on a fresh DB → empty queue.
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out: Vec<(String, u64)> = Vec::new();
    for entry in t.iter()? {
        let (k, v) = entry?;
        out.push((k.value().to_string(), v.value()));
    }
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Ok(out)
}

// ---------------------------------------------------------------------------
// Write path.
// ---------------------------------------------------------------------------

/// Intern (or look up) a predicate by its qname.
///
/// Semantics:
/// - If no row exists for `(namespace, name)`: allocate a fresh
///   `PredicateId` (scan-max-and-increment, starting at 1), write the
///   primary row + qname index, return the new id.
/// - If a row exists and **all** constraint fields match the requested
///   values: return the existing id (idempotent re-registration).
/// - If a row exists with **different** constraints: error
///   [`PredicateOpError::AlreadyExists`] — caller must not silently
///   override schema decisions.
///
/// Validation is enforced before any storage access.
#[allow(clippy::too_many_arguments)]
pub fn predicate_intern(
    wtxn: &WriteTransaction,
    namespace: &str,
    name: &str,
    kind_constraint: Option<StatementKind>,
    object_type_constraint_byte: u8,
    schema_version: u32,
    description: &str,
    is_stateful: bool,
    now_unix_nanos: u64,
) -> Result<PredicateId, PredicateOpError> {
    validate_namespace(namespace)?;
    validate_name(name)?;

    let q = qname(namespace, name);

    // Idempotency / adoption probe.
    //
    // Three cases for a pre-existing row:
    // 1. Constraints match exactly → idempotent re-registration,
    //    return existing id.
    // 2. Row's origin is `ImplicitFromWrite` → adopt it: upgrade
    //    to `SchemaDeclared` at this schema_version with the new
    //    constraints. This is the schemaless-to-strict transition
    //    that lets a deployment add a schema after open-vocabulary
    //    use without losing data or rewriting predicate ids.
    // 3. Constraints diverge AND the existing row is schema-
    //    declared → genuine conflict, error.
    let probed: Option<(u32, PredicateDefinition)> = {
        let idx = wtxn.open_table(PREDICATES_BY_QNAME_TABLE)?;
        let id_raw: Option<u32> = idx.get(q.as_str())?.map(|g| g.value());
        match id_raw {
            None => None,
            Some(id_raw) => {
                let t = wtxn.open_table(PREDICATES_TABLE)?;
                let row = t
                    .get(&id_raw)?
                    .map(|g| g.value())
                    .expect("qname index points to a missing row — file is corrupt");
                Some((id_raw, row))
            }
        }
    };
    if let Some((id_raw, row)) = probed {
        // Constraint-only equality — version is treated separately so
        // an unchanged predicate carried into a new schema version is a
        // no-op rather than a conflict.
        let constraints_match = row.kind_constraint
            == crate::tables::predicate::encode_kind_constraint(kind_constraint)
            && row.object_type_constraint_byte == object_type_constraint_byte
            && row.description == description
            && row.is_stateful == is_stateful;

        if constraints_match && row.schema_version == schema_version {
            return Ok(PredicateId::from(id_raw));
        }

        // Constraints match but schema_version moved forward: bump the
        // stored row's version (and the SchemaDeclared origin payload)
        // in-place so v2 schemas that keep v1's predicates verbatim can
        // be uploaded cleanly. Same row id is reused so already-written
        // statements that reference it stay valid.
        if constraints_match && row.origin().is_schema_declared() {
            let pred = Predicate {
                id: PredicateId::from(id_raw),
                namespace: namespace.to_string(),
                name: name.to_string(),
                kind_constraint,
                object_type_constraint_byte,
                schema_version,
                description: description.to_string(),
                is_stateful,
            };
            let new_row = PredicateDefinition::from_predicate_with_origin(
                &pred,
                now_unix_nanos,
                SchemaOrigin::SchemaDeclared {
                    version: schema_version,
                },
            );
            let mut t = wtxn.open_table(PREDICATES_TABLE)?;
            t.insert(&id_raw, &new_row)?;
            return Ok(PredicateId::from(id_raw));
        }

        // Schemaless adoption path: rewrite the row with the new
        // constraints, preserving the existing id so any in-flight
        // statements that reference it stay valid.
        if !row.origin().is_schema_declared() {
            let pred = Predicate {
                id: PredicateId::from(id_raw),
                namespace: namespace.to_string(),
                name: name.to_string(),
                kind_constraint,
                object_type_constraint_byte,
                schema_version,
                description: description.to_string(),
                is_stateful,
            };
            let new_row = PredicateDefinition::from_predicate_with_origin(
                &pred,
                now_unix_nanos,
                SchemaOrigin::SchemaDeclared {
                    version: schema_version,
                },
            );
            let mut t = wtxn.open_table(PREDICATES_TABLE)?;
            t.insert(&id_raw, &new_row)?;
            return Ok(PredicateId::from(id_raw));
        }
        return Err(PredicateOpError::AlreadyExists {
            qname: q,
            existing_id: PredicateId::from(id_raw),
        });
    }

    // Fresh registration. Allocate id = max(existing) + 1.
    let next_id_raw: u32 = {
        let t = wtxn.open_table(PREDICATES_TABLE)?;
        let mut max: u32 = 0;
        for entry in t.iter()? {
            let (k, _) = entry?;
            let id = k.value();
            if id > max {
                max = id;
            }
        }
        max.checked_add(1).expect("PredicateId space exhausted")
    };

    let pred = Predicate {
        id: PredicateId::from(next_id_raw),
        namespace: namespace.to_string(),
        name: name.to_string(),
        kind_constraint,
        object_type_constraint_byte,
        schema_version,
        description: description.to_string(),
        is_stateful,
    };
    let row = PredicateDefinition::from_predicate(&pred, now_unix_nanos);

    {
        let mut t = wtxn.open_table(PREDICATES_TABLE)?;
        t.insert(&row.predicate_id, &row)?;
    }
    {
        let mut idx = wtxn.open_table(PREDICATES_BY_QNAME_TABLE)?;
        idx.insert(q.as_str(), &row.predicate_id)?;
    }

    Ok(PredicateId::from(next_id_raw))
}

/// Open-vocabulary intern: look up `(namespace, name)` and return its
/// `PredicateId` if it exists, else allocate a fresh row with
/// [`SchemaOrigin::ImplicitFromWrite`].
///
/// This is the path schemaless STATEMENT_CREATE takes. It always
/// succeeds (modulo identifier validation + redb errors) — it never
/// returns `AlreadyExists` for a constraint mismatch the way
/// [`predicate_intern`] does, because schemaless writers don't carry
/// kind / object-type constraints in their request shape.
///
/// `first_seen_lsn` is captured for operator traceability; pass `0`
/// from call sites that don't have an LSN handy.
pub fn predicate_intern_or_get(
    wtxn: &WriteTransaction,
    namespace: &str,
    name: &str,
    first_seen_lsn: u64,
    now_unix_nanos: u64,
) -> Result<PredicateId, PredicateOpError> {
    validate_namespace(namespace)?;
    validate_name(name)?;

    let q = qname(namespace, name);

    // Probe the index first — common-case happy path for repeat
    // mentions of the same predicate.
    let existing_id: Option<u32> = {
        let idx = wtxn.open_table(PREDICATES_BY_QNAME_TABLE)?;
        let got = idx.get(q.as_str())?.map(|g| g.value());
        got
    };
    if let Some(id_raw) = existing_id {
        return Ok(PredicateId::from(id_raw));
    }

    // Fresh allocation. Same `max + 1` scheme as `predicate_intern`
    // so id ordering stays globally monotone across both write paths.
    let next_id_raw: u32 = {
        let t = wtxn.open_table(PREDICATES_TABLE)?;
        let mut max: u32 = 0;
        for entry in t.iter()? {
            let (k, _) = entry?;
            let id = k.value();
            if id > max {
                max = id;
            }
        }
        max.checked_add(1).expect("PredicateId space exhausted")
    };

    let pred = Predicate {
        id: PredicateId::from(next_id_raw),
        namespace: namespace.to_string(),
        name: name.to_string(),
        // Open-vocabulary writes don't carry constraints — `None`
        // means "any kind / object type"; the schema validator would
        // tighten this on a subsequent SCHEMA_UPLOAD.
        kind_constraint: None,
        object_type_constraint_byte: 0,
        // `schema_version = 0` reserves the slot for "not declared
        // by any schema yet". The origin tag carries the real
        // provenance via `ImplicitFromWrite`.
        schema_version: 0,
        description: String::new(),
        // Open-vocabulary intern defaults to cumulative; a subsequent
        // SCHEMA_UPLOAD can adopt and flip the flag.
        is_stateful: false,
    };
    let row = PredicateDefinition::from_predicate_with_origin(
        &pred,
        now_unix_nanos,
        SchemaOrigin::ImplicitFromWrite { first_seen_lsn },
    );

    {
        let mut t = wtxn.open_table(PREDICATES_TABLE)?;
        t.insert(&row.predicate_id, &row)?;
    }
    {
        let mut idx = wtxn.open_table(PREDICATES_BY_QNAME_TABLE)?;
        idx.insert(q.as_str(), &row.predicate_id)?;
    }
    Ok(PredicateId::from(next_id_raw))
}

/// Drop every schema-declared predicate row in `namespace`. Implicit-
/// from-write rows are preserved — they belong to the open-vocabulary
/// world, not the declared schema. Used by `SCHEMA_REPLACE`: callers
/// invoke this before re-running `apply_schema_definitions` with the
/// new schema so the destructive replace doesn't trip the constraint
/// conflict check on same-name diverging declarations.
///
/// Returns the number of rows removed.
pub fn predicate_drop_schema_declared(
    wtxn: &WriteTransaction,
    namespace: &str,
) -> Result<usize, PredicateOpError> {
    validate_namespace(namespace)?;

    // Collect victims first so we don't mutate while iterating.
    let victims: Vec<(u32, String)> = {
        let t = wtxn.open_table(PREDICATES_TABLE)?;
        let mut out = Vec::new();
        for entry in t.iter()? {
            let (k, v) = entry?;
            let row: PredicateDefinition = v.value();
            if row.namespace == namespace && row.origin().is_schema_declared() {
                out.push((k.value(), qname(&row.namespace, &row.name)));
            }
        }
        out
    };
    let count = victims.len();
    {
        let mut t = wtxn.open_table(PREDICATES_TABLE)?;
        for (id, _) in &victims {
            t.remove(id)?;
        }
    }
    {
        let mut idx = wtxn.open_table(PREDICATES_BY_QNAME_TABLE)?;
        for (_, q) in &victims {
            idx.remove(q.as_str())?;
        }
    }
    Ok(count)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::fresh_db;
    use redb::ReadableDatabase;

    fn open_db() -> (tempfile::TempDir, redb::Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        (dir, db)
    }

    #[test]
    fn embedding_round_trips() {
        let (_dir, db) = open_db();
        let pid = PredicateId::from(42);
        let vec: Vec<f32> = (0..384).map(|i| (i as f32) * 0.001 - 0.19).collect();
        {
            let wtxn = db.begin_write().unwrap();
            predicate_embedding_put(&wtxn, pid, &vec).unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.begin_read().unwrap();
        let got = predicate_embedding_get(&rtxn, pid).unwrap().unwrap();
        assert_eq!(got.len(), 384);
        for (a, b) in got.iter().zip(vec.iter()) {
            assert!((a - b).abs() < 1e-9, "{a} != {b}");
        }
        // Absent predicate → None.
        assert!(predicate_embedding_get(&rtxn, PredicateId::from(99))
            .unwrap()
            .is_none());
    }

    #[test]
    fn intern_fresh_allocates_id_one() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let id = predicate_intern(
            &wtxn,
            "brain",
            "is_a",
            Some(StatementKind::Fact),
            1,
            1,
            "entity type assertion",
            false,
            1_700_000_000_000_000_000,
        )
        .unwrap();
        wtxn.commit().unwrap();
        assert_eq!(id, PredicateId::from(1));

        // Round-trip via get + lookup_by_qname.
        let rtxn = db.begin_read().unwrap();
        let by_id = predicate_get(&rtxn, id).unwrap().unwrap();
        assert_eq!(by_id.canonical(), "brain:is_a");
        assert_eq!(by_id.kind_constraint, Some(StatementKind::Fact));

        let by_qname = predicate_lookup_by_qname(&rtxn, "brain", "is_a")
            .unwrap()
            .unwrap();
        assert_eq!(by_qname.id, id);
    }

    #[test]
    fn intern_idempotent_returns_same_id() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let id1 = predicate_intern(
            &wtxn,
            "brain",
            "is_a",
            Some(StatementKind::Fact),
            1,
            1,
            "x",
            false,
            0,
        )
        .unwrap();
        let id2 = predicate_intern(
            &wtxn,
            "brain",
            "is_a",
            Some(StatementKind::Fact),
            1,
            1,
            "x",
            false,
            999,
        )
        .unwrap();
        wtxn.commit().unwrap();
        assert_eq!(id1, id2);

        let rtxn = db.begin_read().unwrap();
        let all = predicate_list(&rtxn, None).unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn intern_conflict_on_constraint_mismatch() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let _ = predicate_intern(
            &wtxn,
            "brain",
            "is_a",
            Some(StatementKind::Fact),
            1,
            1,
            "x",
            false,
            0,
        )
        .unwrap();
        // Same qname, different kind_constraint → conflict.
        let err = predicate_intern(
            &wtxn,
            "brain",
            "is_a",
            Some(StatementKind::Preference),
            1,
            1,
            "x",
            false,
            0,
        )
        .unwrap_err();
        match err {
            PredicateOpError::AlreadyExists { qname, .. } => {
                assert_eq!(qname, "brain:is_a");
            }
            other => panic!("expected AlreadyExists, got {other:?}"),
        }
    }

    #[test]
    fn intern_two_distinct_qnames_get_distinct_ids() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let a = predicate_intern(
            &wtxn,
            "brain",
            "is_a",
            Some(StatementKind::Fact),
            0,
            1,
            "",
            false,
            0,
        )
        .unwrap();
        let b = predicate_intern(&wtxn, "brain", "mentions", None, 0, 1, "", false, 0).unwrap();
        wtxn.commit().unwrap();
        assert_ne!(a, b);
        assert_eq!(a, PredicateId::from(1));
        assert_eq!(b, PredicateId::from(2));
    }

    #[test]
    fn lookup_missing_returns_none() {
        let (_dir, db) = open_db();
        let rtxn = db.begin_read().unwrap();
        // Table must exist; touch it in a write txn first.
        drop(rtxn);
        let wtxn = db.begin_write().unwrap();
        let _ = wtxn.open_table(PREDICATES_TABLE).unwrap();
        let _ = wtxn.open_table(PREDICATES_BY_QNAME_TABLE).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let got = predicate_lookup_by_qname(&rtxn, "brain", "nothing").unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn list_by_namespace_filters() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let _ = predicate_intern(&wtxn, "brain", "is_a", None, 0, 1, "", false, 0).unwrap();
        let _ = predicate_intern(&wtxn, "brain", "mentions", None, 0, 1, "", false, 0).unwrap();
        let _ = predicate_intern(&wtxn, "acme", "manages", None, 1, 1, "", false, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let brain_only = predicate_list(&rtxn, Some("brain")).unwrap();
        assert_eq!(brain_only.len(), 2);
        let acme_only = predicate_list(&rtxn, Some("acme")).unwrap();
        assert_eq!(acme_only.len(), 1);
        let all = predicate_list(&rtxn, None).unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn name_accepts_open_vocabulary_forms() {
        // Predicate names are an open vocabulary. Hyphens, digits, non-ASCII
        // letters, and CJK are all legitimate relation surfaces and must NOT
        // be rejected — rejecting would silently drop a real fact.
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        for name in [
            "is-a",
            "5ht2a_agonist",
            "co2_binds",
            "wirkt_gegen",
            "作用于",
        ] {
            predicate_intern(&wtxn, "brain", name, None, 0, 1, "", false, 0)
                .unwrap_or_else(|e| panic!("open-vocab name {name:?} must intern, got {e:?}"));
        }
        wtxn.commit().unwrap();
    }

    #[test]
    fn name_rejects_structural_hazards() {
        // Only structurally-unsafe names are rejected: the qname separator,
        // whitespace, and a leading connector.
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        for bad in ["is:a", "works at", "_leading", "-leading", "\tctrl"] {
            let err = predicate_intern(&wtxn, "brain", bad, None, 0, 1, "", false, 0)
                .expect_err("structurally-unsafe name must be rejected");
            matches!(err, PredicateOpError::InvalidIdentifier { .. })
                .then_some(())
                .unwrap_or_else(|| panic!("expected InvalidIdentifier for {bad:?}"));
        }
    }

    // ----- Open-vocabulary intern path. -----

    #[test]
    fn predicates_intern_or_get_allocates_then_returns_same_id() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let a = predicate_intern_or_get(&wtxn, "acme", "loves", 0, 0).unwrap();
        let b = predicate_intern_or_get(&wtxn, "acme", "loves", 0, 0).unwrap();
        wtxn.commit().unwrap();
        assert_eq!(a, b);

        let rtxn = db.begin_read().unwrap();
        let row = predicate_get(&rtxn, a).unwrap().unwrap();
        assert_eq!(row.namespace, "acme");
        assert_eq!(row.name, "loves");
        // No constraints — open-vocabulary writer doesn't know them.
        assert_eq!(row.kind_constraint, None);
        assert_eq!(row.object_type_constraint_byte, 0);
    }

    #[test]
    fn predicates_intern_or_get_marks_origin_implicit() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let id = predicate_intern_or_get(&wtxn, "acme", "x", 7, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(PREDICATES_TABLE).unwrap();
        let row = t.get(&id.raw()).unwrap().unwrap().value();
        assert_eq!(row.origin_tag, 1);
        assert_eq!(row.origin_payload, 7);
    }

    #[test]
    fn predicates_active_for_schema_excludes_implicit_rows() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        // Schema-declared row at v=2.
        let declared =
            predicate_intern(&wtxn, "acme", "in_schema", None, 0, 2, "", false, 0).unwrap();
        // Implicit row.
        let implicit = predicate_intern_or_get(&wtxn, "acme", "implicit", 0, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let active = predicates_active_for_schema(&rtxn, "acme", 2).unwrap();
        assert!(active.contains(&declared));
        assert!(!active.contains(&implicit));

        let active_v3 = predicates_active_for_schema(&rtxn, "acme", 3).unwrap();
        assert!(active_v3.is_empty(), "no rows for unknown schema version");
    }

    #[test]
    fn predicates_active_for_schema_distinguishes_implicit_from_declared_after_upgrade() {
        // After schema-upload adopts an implicit row, that row should
        // appear in `predicates_active_for_schema` for the new
        // version. A separate still-implicit predicate must not.
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let adopted = predicate_intern_or_get(&wtxn, "acme", "prefers", 0, 0).unwrap();
        let _untouched = predicate_intern_or_get(&wtxn, "acme", "other", 0, 0).unwrap();
        // SCHEMA_UPLOAD path adopts only `prefers`.
        let adopted2 =
            predicate_intern(&wtxn, "acme", "prefers", None, 0, 4, "", false, 0).unwrap();
        assert_eq!(adopted, adopted2);
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let active = predicates_active_for_schema(&rtxn, "acme", 4).unwrap();
        assert!(
            active.contains(&adopted),
            "adopted predicate must be considered active at the new schema version",
        );
        assert_eq!(
            active.len(),
            1,
            "implicit-only predicates must not appear in the active set",
        );
    }

    #[test]
    fn schema_upload_adopts_implicit_predicate_preserving_id() {
        // Pre-existing implicit row at id=1, no constraints.
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let implicit_id = predicate_intern_or_get(&wtxn, "acme", "prefers", 0, 0).unwrap();
        // Now run the schema-declared intern path with a tighter
        // constraint — the registry should adopt the existing row
        // rather than allocate a fresh id or error.
        let declared_id = predicate_intern(
            &wtxn,
            "acme",
            "prefers",
            Some(StatementKind::Preference),
            2,
            1,
            "preferred meeting style",
            true,
            0,
        )
        .unwrap();
        wtxn.commit().unwrap();
        assert_eq!(implicit_id, declared_id);

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(PREDICATES_TABLE).unwrap();
        let row = t.get(&implicit_id.raw()).unwrap().unwrap().value();
        // Origin must flip to SchemaDeclared(1) and constraints land.
        assert_eq!(row.origin(), SchemaOrigin::SchemaDeclared { version: 1 });
        assert_eq!(row.kind_constraint, 2); // Preference
        assert_eq!(row.object_type_constraint_byte, 2);
        assert_eq!(row.description, "preferred meeting style");
    }

    #[test]
    fn predicate_intern_at_higher_version_with_same_constraints_bumps_version() {
        // A v2 schema that keeps a v1 predicate verbatim must upload
        // cleanly: same id, version field bumped, origin payload
        // updated to v2.
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let v1_id = predicate_intern(
            &wtxn,
            "acme",
            "prefers",
            Some(StatementKind::Preference),
            2,
            1,
            "preferred meeting style",
            true,
            0,
        )
        .unwrap();
        let v2_id = predicate_intern(
            &wtxn,
            "acme",
            "prefers",
            Some(StatementKind::Preference),
            2,
            2,
            "preferred meeting style",
            true,
            42,
        )
        .unwrap();
        wtxn.commit().unwrap();
        assert_eq!(v1_id, v2_id, "id must be preserved across version bump");

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(PREDICATES_TABLE).unwrap();
        let row = t.get(&v1_id.raw()).unwrap().unwrap().value();
        assert_eq!(row.schema_version, 2);
        assert_eq!(row.origin(), SchemaOrigin::SchemaDeclared { version: 2 });
        assert_eq!(row.kind_constraint, 2); // Preference
        assert_eq!(row.object_type_constraint_byte, 2);

        // Only one row exists.
        let all = predicate_list(&rtxn, Some("acme")).unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn predicate_intern_at_higher_version_with_different_constraints_errors() {
        // A v2 schema that *changes* a predicate's constraints must
        // still hit AlreadyExists — version-bump only applies when the
        // constraint shape is unchanged.
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let _ = predicate_intern(
            &wtxn,
            "acme",
            "prefers",
            Some(StatementKind::Preference),
            2,
            1,
            "",
            true,
            0,
        )
        .unwrap();
        let err = predicate_intern(
            &wtxn,
            "acme",
            "prefers",
            Some(StatementKind::Fact), // changed
            2,
            2,
            "",
            false,
            0,
        )
        .unwrap_err();
        match err {
            PredicateOpError::AlreadyExists { qname, .. } => {
                assert_eq!(qname, "acme:prefers");
            }
            other => panic!("expected AlreadyExists, got {other:?}"),
        }
    }

    #[test]
    fn predicates_intern_or_get_validates_identifier() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let err = predicate_intern_or_get(&wtxn, "Bad", "x", 0, 0).unwrap_err();
        matches!(err, PredicateOpError::InvalidIdentifier { .. })
            .then_some(())
            .expect("expected InvalidIdentifier");
    }
}
