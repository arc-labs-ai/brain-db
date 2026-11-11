//! Typed CRUD + interning over the predicate registry.
//!
//! Free functions over `redb::{ReadTransaction, WriteTransaction}`
//! mirroring the [`crate::entity::ops`] precedent: callers compose them
//! inside their own redb txns so a `statement_create` can
//! validate-and-write atomically.

use std::collections::{HashMap, HashSet};

use brain_core::PredicateId;
use brain_core::{Predicate, StatementKind};
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

use crate::tables::predicate::{
    PredicateDefinition, SchemaOrigin, PREDICATES_BY_SCOPE_TABLE, PREDICATES_TABLE,
    PREDICATE_EMBEDDINGS_TABLE,
};
use crate::tables::scope::{space_chain, DeclScope, SPACE_BASE};

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

    #[error("predicate {0:?} not found")]
    NotFound(PredicateId),

    /// A neighbouring subsystem a predicate operation had to consult
    /// failed — the namespace registry or the statement index during
    /// orphan reclamation. Carries that error's message.
    #[error("dependency failed: {0}")]
    Dependency(String),
}

// ---------------------------------------------------------------------------
// Object-type constraint.
// ---------------------------------------------------------------------------

/// The object-type constraint a predicate declaration carries.
///
/// `object_type_byte` selects the `StatementObject` variant (`0` any /
/// `1` Entity / `2` Value / `3` Memory / `4` Statement).
/// `entity_type_id` narrows the `Entity` case to a single declared
/// entity type — `object: Entity<Person>` stores Person's
/// `EntityTypeId`, a bare `Entity` stores `0` (any).
///
/// ```
/// # use brain_metadata::schema::predicate::ObjectConstraint;
/// // `object: Value<text>` — variant only, nothing to narrow.
/// assert_eq!(ObjectConstraint::from(2), ObjectConstraint { object_type_byte: 2, entity_type_id: 0 });
/// // `object: Entity<Person>` where Person is EntityTypeId(1).
/// assert_eq!(ObjectConstraint::entity(1).entity_type_id, 1);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ObjectConstraint {
    pub object_type_byte: u8,
    pub entity_type_id: u32,
}

impl ObjectConstraint {
    /// No constraint at all — any object variant, any entity type.
    pub const ANY: Self = Self {
        object_type_byte: 0,
        entity_type_id: 0,
    };

    /// An `Entity` object narrowed to `entity_type_id` (`0` = any
    /// entity type).
    #[must_use]
    pub fn entity(entity_type_id: u32) -> Self {
        Self {
            object_type_byte: 1,
            entity_type_id,
        }
    }
}

/// A bare variant byte with no entity-type narrowing. Lets call sites
/// that only pin the object variant keep passing the byte directly.
impl From<u8> for ObjectConstraint {
    fn from(object_type_byte: u8) -> Self {
        Self {
            object_type_byte,
            entity_type_id: 0,
        }
    }
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

/// Set (or clear, with `0`) the explicit retention TTL for a predicate, in
/// seconds. Called from schema-apply after the predicate is interned — retention
/// is a storage-only policy the projected `Predicate` value type doesn't carry,
/// so it's stamped directly on the row here. Idempotent; a no-op when the value
/// is already current. Errors if the predicate row doesn't exist.
pub fn predicate_set_retention(
    wtxn: &WriteTransaction,
    id: PredicateId,
    retention_seconds: u64,
) -> Result<(), PredicateOpError> {
    let mut t = wtxn.open_table(PREDICATES_TABLE)?;
    let Some(mut row) = t.get(&id.raw())?.map(|g| g.value()) else {
        return Err(PredicateOpError::NotFound(id));
    };
    if row.retention_seconds == retention_seconds {
        return Ok(());
    }
    row.retention_seconds = retention_seconds;
    t.insert(&id.raw(), &row)?;
    Ok(())
}

/// The explicit retention TTL for a predicate, in seconds (`0` = none). Reads
/// the persisted row directly, since [`predicate_get`]'s projected `Predicate`
/// drops this storage-only field. Missing predicate ⇒ `0`.
pub fn predicate_retention_seconds(
    rtxn: &ReadTransaction,
    id: PredicateId,
) -> Result<u64, PredicateOpError> {
    let t = rtxn.open_table(PREDICATES_TABLE)?;
    Ok(t.get(&id.raw())?
        .map(|g| g.value().retention_seconds)
        .unwrap_or(0))
}

/// Resolve `qname` through the scope chain, read txn.
fn scope_get_rtxn(
    rtxn: &ReadTransaction,
    qname: &str,
    space: [u8; 16],
) -> Result<Option<u32>, PredicateOpError> {
    let idx = rtxn.open_table(PREDICATES_BY_SCOPE_TABLE)?;
    for sp in space_chain(space) {
        if let Some(id) = idx.get(&(qname, sp))?.map(|g| g.value()) {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

/// Resolve `qname` through the scope chain, write txn.
fn scope_get_wtxn(
    wtxn: &WriteTransaction,
    qname: &str,
    space: [u8; 16],
) -> Result<Option<u32>, PredicateOpError> {
    let idx = wtxn.open_table(PREDICATES_BY_SCOPE_TABLE)?;
    for sp in space_chain(space) {
        if let Some(id) = idx.get(&(qname, sp))?.map(|g| g.value()) {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

/// Remove one scope's index entry for `qname`.
///
/// Used by a targeted drop, which must take away exactly the declaration it
/// names: dropping the namespace-wide one leaves a space that declared its
/// own standing, because that space asked for it independently.
fn scope_remove(
    wtxn: &WriteTransaction,
    qname: &str,
    space: [u8; 16],
) -> Result<(), PredicateOpError> {
    let mut idx = wtxn.open_table(PREDICATES_BY_SCOPE_TABLE)?;
    idx.remove(&(qname, space))?;
    Ok(())
}

/// Remove every scope's index entry for `qname`.
///
/// Used when the predicate ROW itself goes — consolidation merging duplicates
/// away. Every space that pointed at that id has to let go, or its entry
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
) -> Result<(), PredicateOpError> {
    let owned: Vec<[u8; 16]> = {
        let idx = wtxn.open_table(PREDICATES_BY_SCOPE_TABLE)?;
        let mut out = Vec::new();
        for entry in idx.range((qname, [0u8; 16])..=(qname, [0xFFu8; 16]))? {
            let (k, v) = entry?;
            if v.value() == id {
                out.push(k.value().1);
            }
        }
        out
    };
    let mut idx = wtxn.open_table(PREDICATES_BY_SCOPE_TABLE)?;
    for sp in owned {
        idx.remove(&(qname, sp))?;
    }
    Ok(())
}

/// Resolve `qname` in exactly one scope, write txn.
///
/// Writes probe exactly; only reads walk the chain. A write that chained
/// would find the namespace base and hand it back, so declaring an overlay
/// would silently return the base row and no space could ever have its own.
fn scope_get_exact_wtxn(
    wtxn: &WriteTransaction,
    qname: &str,
    space: [u8; 16],
) -> Result<Option<u32>, PredicateOpError> {
    let idx = wtxn.open_table(PREDICATES_BY_SCOPE_TABLE)?;
    let got = idx.get(&(qname, space))?.map(|g| g.value());
    Ok(got)
}

/// The id of `qname` in any space, if some space declares it.
///
/// `qname` leads the index key, so one predicate's copies across spaces are
/// a contiguous range and this is a short scan rather than a table walk.
fn scope_any_rtxn(rtxn: &ReadTransaction, qname: &str) -> Result<Option<u32>, PredicateOpError> {
    let idx = rtxn.open_table(PREDICATES_BY_SCOPE_TABLE)?;
    let first = idx
        .range((qname, [0u8; 16])..=(qname, [0xFFu8; 16]))?
        .next()
        .transpose()?
        .map(|(_, v)| v.value());
    Ok(first)
}

/// Resolve a predicate as `space` sees it: the space's own declaration when
/// it has one, otherwise the namespace-wide one.
///
/// # Errors
/// `InvalidIdentifier` for a malformed namespace/name, and redb storage
/// failures.
pub fn predicate_lookup_by_qname(
    rtxn: &ReadTransaction,
    scope: DeclScope<'_>,
    name: &str,
) -> Result<Option<Predicate>, PredicateOpError> {
    let (namespace, space) = (scope.namespace(), scope.space_id());
    validate_namespace(namespace)?;
    validate_name(name)?;

    let q = qname(namespace, name);
    let Some(id_raw) = scope_get_rtxn(rtxn, q.as_str(), space)? else {
        return Ok(None);
    };

    let t = rtxn.open_table(PREDICATES_TABLE)?;
    let row: Option<PredicateDefinition> = t.get(&id_raw)?.map(|g| g.value());
    Ok(row.as_ref().map(PredicateDefinition::to_predicate))
}

/// The predicate ids `space` resolves that version `version` declares.
///
/// Membership is judged over what the scope RESOLVES, not over every row in
/// the namespace. A base row and a sibling space's overlay can both record
/// the same declaring version, so filtering by version alone would admit
/// another space's vocabulary into this one's strict check.
///
/// # Errors
/// `InvalidIdentifier` for a malformed namespace, and redb storage failures.
pub fn predicates_active_for_schema(
    rtxn: &ReadTransaction,
    scope: DeclScope<'_>,
    version: u32,
) -> Result<HashSet<PredicateId>, PredicateOpError> {
    let (namespace, space) = (scope.namespace(), scope.space_id());
    validate_namespace(namespace)?;
    let resolved = resolved_predicate_ids(rtxn, space)?;
    let t = rtxn.open_table(PREDICATES_TABLE)?;
    let mut out = HashSet::new();
    for id in resolved.into_values() {
        let Some(row) = t.get(&id)?.map(|g| g.value()) else {
            continue;
        };
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

/// Every qname `space` resolves, mapped to the id it resolves to.
///
/// The index is keyed `(qname, space)` and [`SPACE_BASE`] is the all-zero —
/// hence lowest — space, so for one qname the base entry is visited before
/// any overlay. The base therefore fills a slot and the overlay replaces it,
/// which is the narrowest-wins order [`space_chain`] states.
fn resolved_predicate_ids(
    rtxn: &ReadTransaction,
    space: [u8; 16],
) -> Result<HashMap<String, u32>, PredicateOpError> {
    let idx = rtxn.open_table(PREDICATES_BY_SCOPE_TABLE)?;
    let mut out: HashMap<String, u32> = HashMap::new();
    for entry in idx.iter()? {
        let (k, v) = entry?;
        let (qname, sp) = k.value();
        if sp == space {
            out.insert(qname.to_string(), v.value());
        } else if sp == SPACE_BASE {
            out.entry(qname.to_string()).or_insert(v.value());
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
    for chunk in bytes.as_chunks::<4>().0.iter() {
        out.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Ok(Some(out))
}

/// Whether this qname is declared anywhere in the schema — as a
/// SCHEMA-DECLARED predicate, or as a relation type.
///
/// Note what does NOT count: a predicate row that merely exists. Every
/// coined name has one (that is what "coined" means, and it carries
/// `SchemaOrigin::ImplicitFromWrite`), so presence alone would filter the
/// entire queue away. Only an explicit declaration disqualifies a
/// candidate.
fn qname_is_declared(rtxn: &ReadTransaction, qname: &str) -> Result<bool, PredicateOpError> {
    use crate::tables::relation_type::RELATION_TYPES_BY_SCOPE_TABLE;

    // Declared as a relation type in ANY space counts, for the same reason
    // it does for predicates below.
    if let Ok(idx) = rtxn.open_table(RELATION_TYPES_BY_SCOPE_TABLE) {
        if idx
            .range((qname, [0u8; 16])..=(qname, [0xFFu8; 16]))?
            .next()
            .is_some()
        {
            return Ok(true);
        }
    }
    // Declared in ANY space counts. The review queue asks whether a coined
    // name has since been given a real declaration, and a declaration in one
    // space answers that.
    let Some(id) = scope_any_rtxn(rtxn, qname)? else {
        return Ok(false);
    };
    let Ok(t) = rtxn.open_table(PREDICATES_TABLE) else {
        return Ok(false);
    };
    let origin = t.get(&id)?.map(|g| g.value().origin());
    Ok(origin.is_some_and(|o| o.is_schema_declared()))
}

/// Whether `(namespace, name)` is declared as a relation type.
///
/// Used to keep the predicate review queue honest: the same local name can
/// be declared as a relation type and still arrive here as a statement
/// triple, and that is a declared name, not a coined one.
fn name_is_declared_relation_type(
    wtxn: &WriteTransaction,
    namespace: &str,
    name: &str,
) -> Result<bool, PredicateOpError> {
    use crate::tables::relation_type::RELATION_TYPES_BY_SCOPE_TABLE;
    let q = qname(namespace, name);
    let idx = match wtxn.open_table(RELATION_TYPES_BY_SCOPE_TABLE) {
        Ok(t) => t,
        // Never-written table on a fresh DB → nothing is declared.
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    // Any scope: the question is whether this local name is spoken for as a
    // relation type, not which space spoke for it.
    let found = idx
        .range((q.as_str(), [0u8; 16])..=(q.as_str(), [0xFFu8; 16]))?
        .next()
        .is_some();
    Ok(found)
}

/// Record one sighting of an undeclared predicate qname in the review
/// queue, incrementing its count.
///
/// Called from [`predicate_intern_or_get`] — the open-vocabulary write
/// path — each time a qname is minted that no schema declared. Brain's
/// vocabulary is OPEN: the statement is still committed and queryable,
/// exactly as before. This queue is the operator's list of names the
/// corpus coined often enough to be worth promoting into a real
/// `SCHEMA_UPLOAD`, read back via [`predicate_review_list`].
///
/// (An earlier version of this comment claimed a "closed-vocab gate"
/// called it and dropped the statement. No such gate exists, and nothing
/// called this function at all, so coined predicates were invisible.)
///
/// Composed inside the caller's write txn.
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
/// For operator review — which coined predicates are worth promoting.
///
/// Filters on READ, not just on write. A name is a promotion CANDIDATE only
/// while it is still undeclared, and that can stop being true after the
/// sighting was recorded: the operator promotes it in a `SCHEMA_UPLOAD`, or
/// it turns out to be declared as a relation type. A queue that only
/// filtered at write time would keep recommending work already done, and
/// the operator would have to remember which entries they had dealt with.
///
/// So a row is dropped when either:
///   - its predicate is now `SchemaDeclared` (promoted, or adopted by a
///     later upload), or
///   - the same qname is declared as a relation type.
///
/// The row is left in place rather than deleted: this is a read-only call,
/// and a name can become undeclared again if a schema drops it — at which
/// point its history is worth keeping.
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
        let qname = k.value().to_string();
        if qname_is_declared(rtxn, &qname)? {
            continue;
        }
        out.push((qname, v.value()));
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
/// Intern a predicate with no declared subject domain — any subject is
/// admitted. This is the shape every caller but the schema-apply path
/// wants, and what every schema written before `subject:` existed means.
#[allow(clippy::too_many_arguments)]
pub fn predicate_intern(
    wtxn: &WriteTransaction,
    scope: DeclScope<'_>,
    name: &str,
    kind_constraint: Option<StatementKind>,
    object_constraint: impl Into<ObjectConstraint>,
    schema_version: u32,
    description: &str,
    is_stateful: bool,
    now_unix_nanos: u64,
) -> Result<PredicateId, PredicateOpError> {
    predicate_intern_with_subject(
        wtxn,
        scope,
        name,
        kind_constraint,
        object_constraint,
        /* subject_entity_type_id */ 0,
        schema_version,
        description,
        is_stateful,
        now_unix_nanos,
    )
}

/// As [`predicate_intern`], plus the declared subject domain.
///
/// `subject_entity_type_id` is the `EntityTypeId` a statement's SUBJECT
/// must have, from a declared `subject: Entity<T>`; `0` means any. Only
/// the schema-apply path passes a non-zero value — a predicate minted any
/// other way has no schema to narrow it.
/// Declare a predicate in `scope`, with its declared subject domain.
///
/// [`DeclScope::base`] is the namespace-wide declaration; any other space is
/// an overlay that a lookup from that space prefers over the base.
#[allow(clippy::too_many_arguments)]
pub fn predicate_intern_with_subject(
    wtxn: &WriteTransaction,
    scope: DeclScope<'_>,
    name: &str,
    kind_constraint: Option<StatementKind>,
    object_constraint: impl Into<ObjectConstraint>,
    subject_entity_type_id: u32,
    schema_version: u32,
    description: &str,
    is_stateful: bool,
    now_unix_nanos: u64,
) -> Result<PredicateId, PredicateOpError> {
    let (namespace, space) = (scope.namespace(), scope.space_id());
    validate_namespace(namespace)?;
    validate_name(name)?;

    let ObjectConstraint {
        object_type_byte: object_type_constraint_byte,
        entity_type_id: object_entity_type_id,
    } = object_constraint.into();

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
        let id_raw: Option<u32> = scope_get_exact_wtxn(wtxn, q.as_str(), space)?;
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
            && row.object_entity_type_id == object_entity_type_id
            // Without this, re-uploading a schema that ADDS `subject:
            // Entity<T>` to an existing predicate would match as
            // "unchanged" and the new constraint would never reach the
            // row — the upload would report success and enforce nothing.
            && row.subject_entity_type_id == subject_entity_type_id
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
                object_entity_type_id,
                subject_entity_type_id,
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
                object_entity_type_id,
                subject_entity_type_id,
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
        object_entity_type_id,
        subject_entity_type_id,
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
        let mut idx = wtxn.open_table(PREDICATES_BY_SCOPE_TABLE)?;
        idx.insert(&(q.as_str(), space), &row.predicate_id)?;
    }

    Ok(PredicateId::from(next_id_raw))
}

/// Resolve a predicate in `scope`, coining it if nothing resolves.
///
/// The open-vocabulary path: a write mentions a qname that no schema
/// declares, and the predicate is minted from the mention.
///
/// COINING ADOPTS, IT DOES NOT OVERRIDE. Unlike the declaring paths, this
/// one resolves through the scope chain: if the namespace already interned
/// the qname, a write from a space reuses that id. Probing only this space
/// would mint a second row and a second `PredicateId` for one name that
/// nobody declared — statements from the space would then carry a different
/// predicate than the tenant's, and any read keyed on predicate id would
/// see them as unrelated facts. An override is something a schema states;
/// a mention is not a statement about scope.
///
/// A total miss coins at `scope`, so a space that is the first to mention a
/// name owns the row it created.
pub fn predicate_intern_or_get(
    wtxn: &WriteTransaction,
    scope: DeclScope<'_>,
    name: &str,
    first_seen_lsn: u64,
    now_unix_nanos: u64,
) -> Result<PredicateId, PredicateOpError> {
    let (namespace, space) = (scope.namespace(), scope.space_id());
    validate_namespace(namespace)?;
    validate_name(name)?;

    let q = qname(namespace, name);

    // Probe the chain first — common-case happy path for repeat mentions of
    // the same predicate, and the adoption rule above.
    let existing_id: Option<u32> = scope_get_wtxn(wtxn, q.as_str(), space)?;
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
        object_entity_type_id: 0,
        // No declared subject domain: an open-vocabulary write has no
        // schema to narrow it, and inventing one here would reject the
        // very statement that caused the predicate to exist.
        subject_entity_type_id: 0,
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
        let mut idx = wtxn.open_table(PREDICATES_BY_SCOPE_TABLE)?;
        idx.insert(&(q.as_str(), space), &row.predicate_id)?;
    }

    // Reaching the mint branch means the qname was in no PREDICATE index,
    // so no schema declared it as a predicate — the schema path is
    // `predicate_intern`, not this function.
    //
    // That is not the same as "the schema never mentions this name". A
    // name declared as a RELATION TYPE reaches here whenever the extractor
    // emits it as a statement triple, and listing it as a promotion
    // candidate would tell an operator to declare something they already
    // declared. "Coined" means declared in NEITHER registry.
    if !name_is_declared_relation_type(wtxn, namespace, name)? {
        // Composed inside the caller's write txn, so it commits with the
        // predicate or not at all.
        predicate_review_record(wtxn, &q)?;
    }

    Ok(PredicateId::from(next_id_raw))
}

/// The id `namespace:name` resolves to from `space`.
///
/// Resolves narrowest-first: the space's own declaration, then the
/// namespace-wide one. A write in a space that declared its own meaning for a
/// predicate gets that meaning; one that declared nothing gets the tenant's.
///
/// # Errors
/// [`PredicateOpError::InvalidIdentifier`] for an invalid namespace or name.
pub fn predicate_id_by_qname(
    wtxn: &WriteTransaction,
    scope: DeclScope<'_>,
    name: &str,
) -> Result<Option<PredicateId>, PredicateOpError> {
    let (namespace, space) = (scope.namespace(), scope.space_id());
    validate_namespace(namespace)?;
    validate_name(name)?;
    let q = qname(namespace, name);
    let found = scope_get_wtxn(wtxn, q.as_str(), space)?.map(PredicateId::from);
    Ok(found)
}

/// Point a fresh open-vocab qname at an already-interned predicate id,
/// creating NO new `PREDICATES_TABLE` row and NO new embedding.
///
/// This is the write half of embedding-based predicate consolidation: a
/// near-synonym surface form (`keen_about`) is aliased onto the canonical
/// predicate (`keen_on`) so future exact lookups of the variant hit the
/// fast path and the typed graph converges on one predicate id instead of
/// fragmenting across morphological variants.
///
/// Refuses to alias onto a schema-declared target: declared vocabulary is
/// authoritative and must never silently absorb open-vocab drift — merging
/// a free predicate into, say, the seeded `brain:occurred_at` would corrupt
/// the meaning of every statement that predicate keys.
pub fn predicate_alias_qname(
    wtxn: &WriteTransaction,
    namespace: &str,
    name: &str,
    target: PredicateId,
) -> Result<(), PredicateOpError> {
    validate_namespace(namespace)?;
    validate_name(name)?;

    // The target must exist and be open-vocab. A vanished or declared
    // target means the caller's candidate scan raced or was buggy; refuse
    // rather than mint a dangling / meaning-corrupting alias.
    {
        let t = wtxn.open_table(PREDICATES_TABLE)?;
        // Materialize the row before matching so the access guard drops here,
        // not held across the `t` binding's lifetime.
        let row = t.get(&target.raw())?.map(|g| g.value());
        match row {
            None => {
                return Err(PredicateOpError::InvalidIdentifier {
                    reason: "alias target predicate does not exist",
                })
            }
            Some(row) if row.origin().is_schema_declared() => {
                return Err(PredicateOpError::InvalidIdentifier {
                    reason: "cannot alias onto a schema-declared predicate",
                })
            }
            Some(_) => {}
        }
    }

    let q = qname(namespace, name);
    {
        let mut idx = wtxn.open_table(PREDICATES_BY_SCOPE_TABLE)?;
        idx.insert(&(q.as_str(), SPACE_BASE), &target.raw())?;
    }
    Ok(())
}

/// One embedding-consolidation candidate: `(id, name, embedding,
/// is_schema_declared)`. See [`predicate_consolidation_candidates`].
pub type PredicateConsolidationCandidate = (PredicateId, String, Vec<f32>, bool);

/// Gather the consolidation candidate set for `namespace`: every predicate
/// that carries a stored embedding, as `(id, name, embedding,
/// is_schema_declared)`. The extractor compares a would-be-fresh
/// predicate's embedding against these to reuse a near-synonym's id.
///
/// Rows without an embedding are omitted — they can't be compared. The
/// `is_schema_declared` flag is returned rather than filtered here so the
/// caller's selection logic can enforce the "never merge into declared
/// vocabulary" rule (and unit-test it in isolation).
pub fn predicate_consolidation_candidates(
    wtxn: &WriteTransaction,
    namespace: &str,
) -> Result<Vec<PredicateConsolidationCandidate>, PredicateOpError> {
    validate_namespace(namespace)?;
    let preds = wtxn.open_table(PREDICATES_TABLE)?;
    let embs = wtxn.open_table(PREDICATE_EMBEDDINGS_TABLE)?;
    let mut out = Vec::new();
    for entry in preds.iter()? {
        let (k, v) = entry?;
        let row = v.value();
        if row.namespace != namespace {
            continue;
        }
        let Some(g) = embs.get(k.value())? else {
            continue;
        };
        out.push(decode_candidate(&row, g.value()));
    }
    Ok(out)
}

/// Read-transaction counterpart to [`predicate_consolidation_candidates`].
///
/// The extractor's per-memory prompt build runs under a read txn (it only
/// reads the active vocabulary to render the candidate-predicate block); a
/// write txn would be both wrong for that context and needlessly contended.
/// Same rows, same `(id, name, embedding, is_declared)` shape.
pub fn predicate_consolidation_candidates_rtxn(
    rtxn: &ReadTransaction,
    namespace: &str,
) -> Result<Vec<PredicateConsolidationCandidate>, PredicateOpError> {
    validate_namespace(namespace)?;
    let preds = rtxn.open_table(PREDICATES_TABLE)?;
    let embs = rtxn.open_table(PREDICATE_EMBEDDINGS_TABLE)?;
    let mut out = Vec::new();
    for entry in preds.iter()? {
        let (k, v) = entry?;
        let row = v.value();
        if row.namespace != namespace {
            continue;
        }
        let Some(g) = embs.get(k.value())? else {
            continue;
        };
        out.push(decode_candidate(&row, g.value()));
    }
    Ok(out)
}

/// Decode one `(row, embedding-bytes)` pair into a consolidation candidate.
/// The embedding is stored as little-endian `f32`s.
fn decode_candidate(row: &PredicateDefinition, bytes: &[u8]) -> PredicateConsolidationCandidate {
    let mut vec = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.as_chunks::<4>().0.iter() {
        vec.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    (
        PredicateId::from(row.predicate_id),
        row.name.clone(),
        vec,
        row.origin().is_schema_declared(),
    )
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
    scope: DeclScope<'_>,
) -> Result<usize, PredicateOpError> {
    let (namespace, space) = (scope.namespace(), scope.space_id());
    validate_namespace(namespace)?;

    // Victims are the declarations THIS SCOPE made, found through the scope
    // index — not by scanning the registry for a namespace match.
    //
    // A scan cannot tell a space's overlay from the tenant's base: both rows
    // carry the same `namespace`, and only the index says which scope
    // declared them. Scanning therefore made a base REPLACE take every
    // sibling space's declarations with it, and a space REPLACE take the
    // tenant's — the opposite of what an overlay is for.
    //
    // Collect first so we don't mutate while iterating.
    let victims: Vec<(u32, String)> = {
        let idx = wtxn.open_table(PREDICATES_BY_SCOPE_TABLE)?;
        let t = wtxn.open_table(PREDICATES_TABLE)?;
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
            // The qname carries a namespace, so one scope's slice of the
            // index still spans every tenant; the row is what settles it.
            if row.namespace == namespace && row.origin().is_schema_declared() {
                out.push((id, q.to_string()));
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
    for (id, q) in &victims {
        scope_remove_entry(wtxn, q.as_str(), *id)?;
    }
    Ok(count)
}

/// Drop a single schema-declared predicate row identified by
/// `(namespace, name)`. The scoped counterpart to
/// [`predicate_drop_schema_declared`]: `SCHEMA_DROP` narrows one
/// declaration instead of wiping the whole namespace.
///
/// Returns `Some(id)` when a schema-declared predicate with that qname
/// existed and was removed, `None` when no schema-declared predicate
/// with that qname exists. An implicit-from-write row sharing the qname
/// is deliberately left untouched — the declared vocabulary is the only
/// thing `SCHEMA_DROP` narrows, and an open-vocab row is not part of it.
/// The embedding row is left in place (a harmless orphan), matching
/// [`predicate_drop_schema_declared`].
pub fn predicate_drop_one(
    wtxn: &WriteTransaction,
    scope: DeclScope<'_>,
    name: &str,
) -> Result<Option<PredicateId>, PredicateOpError> {
    let (namespace, space) = (scope.namespace(), scope.space_id());
    validate_namespace(namespace)?;
    validate_name(name)?;

    let q = qname(namespace, name);
    let victim: Option<u32> = {
        // Narrowing names ONE declaration, so the probe is exact rather than
        // chained: a chained read would find the declaration this scope
        // INHERITS and remove a row the caller does not own — a space
        // dropping an inherited name would narrow the whole tenant.
        let id = scope_get_exact_wtxn(wtxn, q.as_str(), space)?;
        match id {
            Some(id) => {
                let t = wtxn.open_table(PREDICATES_TABLE)?;
                let row: Option<PredicateDefinition> = t.get(&id)?.map(|g| g.value());
                match row {
                    Some(r) if r.origin().is_schema_declared() => Some(id),
                    _ => None,
                }
            }
            None => None,
        }
    };
    if let Some(id) = victim {
        {
            let mut t = wtxn.open_table(PREDICATES_TABLE)?;
            t.remove(&id)?;
        }
        scope_remove(wtxn, q.as_str(), space)?;
    }
    Ok(victim.map(PredicateId::from))
}

// ---------------------------------------------------------------------------
// Orphaned-predicate reclamation.
//
// Brain's predicate vocabulary is open: a name the corpus uses but no schema
// declares is interned on demand (`SchemaOrigin::ImplicitFromWrite`). Nothing
// ever removed those rows, while the statements that justified them do get
// removed — `statement_reclaim` hard-deletes retracted and retention-expired
// rows, FORGET cascades delete more. The predicate that was coined for them
// outlives its last user, keeping a `PREDICATES_TABLE` row, a qname-index
// entry, possibly an embedding, and a review-queue entry naming vocabulary
// the corpus no longer contains.
//
// A declared predicate is never a candidate, however unused: the schema is
// the statement of intent, and dropping it belongs to SCHEMA_DROP.
// ---------------------------------------------------------------------------

/// What one reclamation pass did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PredicateReclaimSummary {
    /// Coined predicates examined (past grace, not schema-declared).
    pub examined: usize,
    /// Coined predicates dropped because no live statement used them.
    pub reclaimed: usize,
    /// Coined predicates left alone because a live statement still uses them.
    pub still_referenced: usize,
}

/// Drop coined predicates that no live statement references.
///
/// `grace_nanos` protects a name that was just coined: interning happens in
/// the same write as the statement, but a crash between them — or a row
/// tombstoned immediately — would otherwise let the next pass reap a
/// predicate the corpus is actively minting. `limit` bounds how many
/// predicates one pass may reclaim so a worker cycle stays short.
///
/// `dry_run` reports what would be reclaimed without deleting anything —
/// the same escape hatch [`crate::extractor::sweep::reclaim_retracted_statements`]
/// offers, and worth having before trusting a GC with vocabulary.
pub fn predicate_reclaim_orphans(
    wtxn: &WriteTransaction,
    now_unix_nanos: u64,
    grace_nanos: u64,
    limit: usize,
    dry_run: bool,
) -> Result<PredicateReclaimSummary, PredicateOpError> {
    let mut summary = PredicateReclaimSummary::default();
    if limit == 0 {
        return Ok(summary);
    }

    // Pass 1 — collect candidates, then close the table before the
    // statement-index reads and the deletes below.
    let mut candidates: Vec<(u32, String, String)> = Vec::new();
    {
        let t = wtxn.open_table(PREDICATES_TABLE)?;
        for entry in t.iter()? {
            let (k, v) = entry?;
            let row = v.value();
            if row.origin().is_schema_declared() {
                continue;
            }
            if now_unix_nanos.saturating_sub(row.created_at_unix_nanos) < grace_nanos {
                continue;
            }
            candidates.push((k.value(), row.namespace.clone(), row.name.clone()));
        }
    }

    // Pass 2 — keep only those with no live statement.
    let mut victims: Vec<(u32, String)> = Vec::new();
    for (id, namespace, name) in candidates {
        if victims.len() >= limit {
            break;
        }
        summary.examined += 1;
        // Deliberately namespace-agnostic: a statement's `namespace_id` need
        // not match the namespace its predicate was coined under, so scoping
        // this to the predicate's own namespace would miss live references
        // and delete a row something still points at.
        let live =
            crate::statement::crud::statement_any_live_by_predicate(wtxn, PredicateId::from(id))
                .map_err(|e| PredicateOpError::Dependency(format!("live statement probe: {e}")))?;
        if live {
            summary.still_referenced += 1;
            continue;
        }
        victims.push((id, qname(&namespace, &name)));
    }

    // Pass 3 — delete the row and everything keyed off it.
    summary.reclaimed = victims.len();
    if dry_run {
        return Ok(summary);
    }
    for (id, q) in &victims {
        {
            let mut t = wtxn.open_table(PREDICATES_TABLE)?;
            t.remove(id)?;
        }
        scope_remove_entry(wtxn, q.as_str(), *id)?;
        {
            let mut emb = wtxn.open_table(PREDICATE_EMBEDDINGS_TABLE)?;
            emb.remove(id)?;
        }
        {
            // The review queue is a shortlist of names worth promoting into a
            // schema; a name with nothing left to promote does not belong on it.
            let mut rq = wtxn.open_table(crate::tables::predicate::PREDICATE_REVIEW_QUEUE_TABLE)?;
            rq.remove(q.as_str())?;
        }
    }
    Ok(summary)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::fresh_db;
    use redb::ReadableDatabase;

    const T0: u64 = 1_700_000_000_000_000_000;

    fn open_db() -> (tempfile::TempDir, redb::Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        (dir, db)
    }

    // ── The space overlay ─────────────────────────────────────────────────
    //
    // A predicate is addressed by its qname, so the namespace is already in
    // the key. `space` adds the dimension the index lacked: two applications
    // inside one tenant can mean different things by the same predicate.
    // Narrowest-first resolution, falling back to the namespace base.

    const SPACE_A: [u8; 16] = [0xA1; 16];
    const SPACE_B: [u8; 16] = [0xB2; 16];

    fn intern_at(db: &redb::Database, space: [u8; 16], ns: &str, name: &str) -> PredicateId {
        let wtxn = db.begin_write().unwrap();
        let id = predicate_intern_with_subject(
            &wtxn,
            DeclScope::space(ns, space),
            name,
            Some(StatementKind::Fact),
            ObjectConstraint::ANY,
            0,
            1,
            "",
            false,
            T0,
        )
        .unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn resolve(db: &redb::Database, space: [u8; 16], ns: &str, name: &str) -> Option<u32> {
        let wtxn = db.begin_write().unwrap();
        let got = scope_get_wtxn(&wtxn, qname(ns, name).as_str(), space).unwrap();
        wtxn.commit().unwrap();
        got
    }

    fn coin_at(db: &redb::Database, space: [u8; 16], ns: &str, name: &str) -> PredicateId {
        let wtxn = db.begin_write().unwrap();
        let id = predicate_intern_or_get(&wtxn, DeclScope::space(ns, space), name, 0, T0).unwrap();
        wtxn.commit().unwrap();
        id
    }

    /// Declaring in a space is an override; MENTIONING a name in a space is
    /// not. An open-vocabulary write adopts whatever the scope resolves, so
    /// one qname keeps one id across the tenant unless a schema says
    /// otherwise.
    #[test]
    fn coining_in_a_space_adopts_the_namespaces_predicate() {
        let (_dir, db) = open_db();
        let base = coin_at(&db, SPACE_BASE, "acme", "mentions");
        let from_space = coin_at(&db, SPACE_A, "acme", "mentions");

        assert_eq!(
            base, from_space,
            "a mention must not mint a second predicate for one qname"
        );
        assert_eq!(resolve(&db, SPACE_A, "acme", "mentions"), Some(base.raw()));
        assert_eq!(
            resolve(&db, SPACE_B, "acme", "mentions"),
            Some(base.raw()),
            "and no space-local row was left behind for a sibling to miss"
        );
    }

    /// The space that first mentions a name owns the row it created; nothing
    /// is promoted to the base behind the tenant's back.
    #[test]
    fn coining_a_name_nobody_has_mentioned_lands_in_the_asking_space() {
        let (_dir, db) = open_db();
        let coined = coin_at(&db, SPACE_A, "acme", "noticed");

        assert_eq!(resolve(&db, SPACE_A, "acme", "noticed"), Some(coined.raw()));
        assert_eq!(resolve(&db, SPACE_BASE, "acme", "noticed"), None);
        assert_eq!(resolve(&db, SPACE_B, "acme", "noticed"), None);
    }

    /// A declaration still overrides: the space gets its own row even though
    /// the base already holds the name.
    #[test]
    fn declaring_over_a_coined_predicate_still_mints_an_overlay() {
        let (_dir, db) = open_db();
        let base = coin_at(&db, SPACE_BASE, "acme", "owns");
        let declared = intern_at(&db, SPACE_A, "acme", "owns");

        assert_ne!(base, declared, "a declaration in a space is an override");
        assert_eq!(resolve(&db, SPACE_A, "acme", "owns"), Some(declared.raw()));
        assert_eq!(resolve(&db, SPACE_BASE, "acme", "owns"), Some(base.raw()));
    }

    /// Resolution CHAINS, so `resolve` cannot prove a row is gone — it would
    /// report the declaration the scope inherits. These assertions need the
    /// exact question: does THIS scope declare the name itself?
    fn declares_exactly(db: &redb::Database, space: [u8; 16], ns: &str, name: &str) -> Option<u32> {
        let wtxn = db.begin_write().unwrap();
        let got = scope_get_exact_wtxn(&wtxn, qname(ns, name).as_str(), space).unwrap();
        wtxn.commit().unwrap();
        got
    }

    /// A REPLACE at the namespace base must leave a space's own declaration
    /// standing.
    ///
    /// The registry scan this replaced could not tell the two apart — both
    /// rows carry the same `namespace`, and only the scope index records
    /// which scope declared them — so a base replace silently took every
    /// sibling space's vocabulary with it.
    #[test]
    fn a_base_replace_leaves_a_spaces_declaration_standing() {
        let (_dir, db) = open_db();
        intern_at(&db, SPACE_BASE, "acme", "owns");
        let overlay = intern_at(&db, SPACE_A, "acme", "owns");

        let wtxn = db.begin_write().unwrap();
        let dropped = predicate_drop_schema_declared(&wtxn, DeclScope::base("acme")).unwrap();
        wtxn.commit().unwrap();

        assert_eq!(dropped, 1, "only the base's own declaration is a victim");
        assert_eq!(declares_exactly(&db, SPACE_BASE, "acme", "owns"), None);
        assert_eq!(
            declares_exactly(&db, SPACE_A, "acme", "owns"),
            Some(overlay.raw()),
            "the overlay must survive a replace it was not the target of"
        );
    }

    /// And the converse: a space narrowing its own vocabulary must not
    /// narrow the tenant's, which every sibling space resolves.
    #[test]
    fn a_space_replace_leaves_the_namespace_base_standing() {
        let (_dir, db) = open_db();
        let base = intern_at(&db, SPACE_BASE, "acme", "owns");
        intern_at(&db, SPACE_A, "acme", "owns");

        let wtxn = db.begin_write().unwrap();
        let dropped =
            predicate_drop_schema_declared(&wtxn, DeclScope::space("acme", SPACE_A)).unwrap();
        wtxn.commit().unwrap();

        assert_eq!(dropped, 1);
        assert_eq!(declares_exactly(&db, SPACE_A, "acme", "owns"), None);
        assert_eq!(
            declares_exactly(&db, SPACE_BASE, "acme", "owns"),
            Some(base.raw())
        );
    }

    /// A targeted drop from a space may not remove a name the space only
    /// INHERITS — that would narrow the whole tenant from inside one space.
    #[test]
    fn dropping_one_from_a_space_spares_an_inherited_declaration() {
        let (_dir, db) = open_db();
        let base = intern_at(&db, SPACE_BASE, "acme", "owns");

        let wtxn = db.begin_write().unwrap();
        let got = predicate_drop_one(&wtxn, DeclScope::space("acme", SPACE_A), "owns").unwrap();
        wtxn.commit().unwrap();

        assert_eq!(got, None, "the space declares no `owns` of its own");
        assert_eq!(
            declares_exactly(&db, SPACE_BASE, "acme", "owns"),
            Some(base.raw()),
            "and the tenant's declaration is untouched"
        );
    }

    /// Two tenants' declarations share one scope value, so the scope slice of
    /// the index spans both; the row is what settles ownership.
    #[test]
    fn a_replace_in_one_namespace_spares_another_at_the_same_scope() {
        let (_dir, db) = open_db();
        intern_at(&db, SPACE_A, "acme", "owns");
        let other = intern_at(&db, SPACE_A, "globex", "owns");

        let wtxn = db.begin_write().unwrap();
        let dropped =
            predicate_drop_schema_declared(&wtxn, DeclScope::space("acme", SPACE_A)).unwrap();
        wtxn.commit().unwrap();

        assert_eq!(dropped, 1);
        assert_eq!(declares_exactly(&db, SPACE_A, "acme", "owns"), None);
        assert_eq!(
            declares_exactly(&db, SPACE_A, "globex", "owns"),
            Some(other.raw())
        );
    }

    #[test]
    fn a_space_declaration_wins_over_the_namespace_base() {
        let (_dir, db) = open_db();
        let base = intern_at(&db, SPACE_BASE, "acme", "owns");
        let overlay = intern_at(&db, SPACE_A, "acme", "owns");

        assert_ne!(base, overlay, "an overlay is its own predicate row");
        assert_eq!(resolve(&db, SPACE_A, "acme", "owns"), Some(overlay.raw()));
        assert_eq!(resolve(&db, SPACE_BASE, "acme", "owns"), Some(base.raw()));
    }

    /// A space that declared nothing of its own sees the tenant's base, which
    /// is what keeps the overlay additive rather than isolating.
    #[test]
    fn a_space_without_its_own_declaration_falls_back_to_the_base() {
        let (_dir, db) = open_db();
        let base = intern_at(&db, SPACE_BASE, "acme", "owns");
        assert_eq!(resolve(&db, SPACE_B, "acme", "owns"), Some(base.raw()));
    }

    /// Sibling spaces do not see each other. This is the whole point: a sales
    /// copilot's vocabulary must not reach a support bot in the same tenant.
    #[test]
    fn one_space_cannot_see_anothers_declaration() {
        let (_dir, db) = open_db();
        let only_in_a = intern_at(&db, SPACE_A, "acme", "stage");
        assert_eq!(
            resolve(&db, SPACE_A, "acme", "stage"),
            Some(only_in_a.raw())
        );
        assert_eq!(
            resolve(&db, SPACE_B, "acme", "stage"),
            None,
            "a sibling space must not inherit another's vocabulary"
        );
        assert_eq!(resolve(&db, SPACE_BASE, "acme", "stage"), None);
    }

    /// The namespace stays part of the address: two tenants' identically
    /// named predicates are different rows even in the same space.
    #[test]
    fn the_namespace_still_separates_two_tenants_in_one_space() {
        let (_dir, db) = open_db();
        let acme = intern_at(&db, SPACE_A, "acme", "owns");
        let globex = intern_at(&db, SPACE_A, "globex", "owns");
        assert_ne!(acme, globex);
        assert_eq!(resolve(&db, SPACE_A, "acme", "owns"), Some(acme.raw()));
        assert_eq!(resolve(&db, SPACE_A, "globex", "owns"), Some(globex.raw()));
    }

    /// A targeted drop takes away the declaration it names and no other. A
    /// space asked for its overlay independently, so removing the tenant's
    /// base must not silently remove it.
    #[test]
    fn dropping_the_base_leaves_a_spaces_own_declaration_standing() {
        let (_dir, db) = open_db();
        intern_at(&db, SPACE_BASE, "acme", "owns");
        let overlay = intern_at(&db, SPACE_A, "acme", "owns");

        let wtxn = db.begin_write().unwrap();
        scope_remove(&wtxn, qname("acme", "owns").as_str(), SPACE_BASE).unwrap();
        wtxn.commit().unwrap();

        assert_eq!(resolve(&db, SPACE_BASE, "acme", "owns"), None);
        assert_eq!(
            resolve(&db, SPACE_A, "acme", "owns"),
            Some(overlay.raw()),
            "the space declared this itself; the base's removal is not its own"
        );
    }

    /// When the predicate ROW goes — consolidation merging duplicates away —
    /// every space's entry has to go with it, or an entry dangles at a row
    /// When a ROW goes, the entry naming it goes — and no other.
    ///
    /// This replaced an assertion that every space's entry went with it.
    /// That premise held only while a qname meant one row; a predicate now has
    /// one row PER SCOPE that declared it, so clearing them all orphaned the
    /// sibling scopes' rows — each survived with nothing resolving to it, and
    /// the declaration silently vanished.
    #[test]
    fn removing_a_row_clears_only_its_own_entry() {
        let (_dir, db) = open_db();
        let base = intern_at(&db, SPACE_BASE, "acme", "owns");
        let in_a = intern_at(&db, SPACE_A, "acme", "owns");
        let in_b = intern_at(&db, SPACE_B, "acme", "owns");
        assert_ne!(base, in_a, "each scope declared its own row");
        assert_ne!(in_a, in_b);

        let wtxn = db.begin_write().unwrap();
        scope_remove_entry(&wtxn, qname("acme", "owns").as_str(), in_a.raw()).unwrap();
        wtxn.commit().unwrap();

        assert_eq!(
            declares_exactly(&db, SPACE_A, "acme", "owns"),
            None,
            "the removed row's entry is gone"
        );
        assert_eq!(
            declares_exactly(&db, SPACE_BASE, "acme", "owns"),
            Some(base.raw()),
            "the base's entry is untouched"
        );
        assert_eq!(
            declares_exactly(&db, SPACE_B, "acme", "owns"),
            Some(in_b.raw()),
            "and the sibling space's entry is untouched"
        );
    }

    /// `scope_any_rtxn` answers "declared in any space", which is what the
    /// review queue asks before deciding a coined name is still unclaimed.
    #[test]
    fn declared_in_any_space_finds_an_overlay_only_declaration() {
        let (_dir, db) = open_db();
        let only_in_a = intern_at(&db, SPACE_A, "acme", "stage");
        let rtxn = db.begin_read().unwrap();
        assert_eq!(
            scope_any_rtxn(&rtxn, qname("acme", "stage").as_str()).unwrap(),
            Some(only_in_a.raw())
        );
        assert_eq!(
            scope_any_rtxn(&rtxn, qname("acme", "absent").as_str()).unwrap(),
            None
        );
    }

    /// The base entry points mean `SPACE_BASE`, so every existing caller
    /// resolves exactly as it did before the space dimension existed.
    #[test]
    fn the_base_entry_point_declares_at_the_base() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let id = predicate_intern_or_get(&wtxn, DeclScope::base("acme"), "coined", 7, T0).unwrap();
        wtxn.commit().unwrap();
        assert_eq!(resolve(&db, SPACE_BASE, "acme", "coined"), Some(id.raw()));
        // And a space with no overlay of its own still sees it.
        assert_eq!(resolve(&db, SPACE_A, "acme", "coined"), Some(id.raw()));
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
            DeclScope::base("brain"),
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

        let by_qname = predicate_lookup_by_qname(&rtxn, DeclScope::base("brain"), "is_a")
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
            DeclScope::base("brain"),
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
            DeclScope::base("brain"),
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
            DeclScope::base("brain"),
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
            DeclScope::base("brain"),
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
            DeclScope::base("brain"),
            "is_a",
            Some(StatementKind::Fact),
            0,
            1,
            "",
            false,
            0,
        )
        .unwrap();
        let b = predicate_intern(
            &wtxn,
            DeclScope::base("brain"),
            "mentions",
            None,
            0,
            1,
            "",
            false,
            0,
        )
        .unwrap();
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
        let _ = wtxn.open_table(PREDICATES_BY_SCOPE_TABLE).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let got = predicate_lookup_by_qname(&rtxn, DeclScope::base("brain"), "nothing").unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn list_by_namespace_filters() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let _ = predicate_intern(
            &wtxn,
            DeclScope::base("brain"),
            "is_a",
            None,
            0,
            1,
            "",
            false,
            0,
        )
        .unwrap();
        let _ = predicate_intern(
            &wtxn,
            DeclScope::base("brain"),
            "mentions",
            None,
            0,
            1,
            "",
            false,
            0,
        )
        .unwrap();
        let _ = predicate_intern(
            &wtxn,
            DeclScope::base("acme"),
            "manages",
            None,
            1,
            1,
            "",
            false,
            0,
        )
        .unwrap();
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
            predicate_intern(
                &wtxn,
                DeclScope::base("brain"),
                name,
                None,
                0,
                1,
                "",
                false,
                0,
            )
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
            let err = predicate_intern(
                &wtxn,
                DeclScope::base("brain"),
                bad,
                None,
                0,
                1,
                "",
                false,
                0,
            )
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
        let a = predicate_intern_or_get(&wtxn, DeclScope::base("acme"), "loves", 0, 0).unwrap();
        let b = predicate_intern_or_get(&wtxn, DeclScope::base("acme"), "loves", 0, 0).unwrap();
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
        let id = predicate_intern_or_get(&wtxn, DeclScope::base("acme"), "x", 7, 0).unwrap();
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
        let declared = predicate_intern(
            &wtxn,
            DeclScope::base("acme"),
            "in_schema",
            None,
            0,
            2,
            "",
            false,
            0,
        )
        .unwrap();
        // Implicit row.
        let implicit =
            predicate_intern_or_get(&wtxn, DeclScope::base("acme"), "implicit", 0, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let active = predicates_active_for_schema(&rtxn, DeclScope::base("acme"), 2).unwrap();
        assert!(active.contains(&declared));
        assert!(!active.contains(&implicit));

        let active_v3 = predicates_active_for_schema(&rtxn, DeclScope::base("acme"), 3).unwrap();
        assert!(active_v3.is_empty(), "no rows for unknown schema version");
    }

    #[test]
    fn predicates_active_for_schema_distinguishes_implicit_from_declared_after_upgrade() {
        // After schema-upload adopts an implicit row, that row should
        // appear in `predicates_active_for_schema` for the new
        // version. A separate still-implicit predicate must not.
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let adopted =
            predicate_intern_or_get(&wtxn, DeclScope::base("acme"), "prefers", 0, 0).unwrap();
        let _untouched =
            predicate_intern_or_get(&wtxn, DeclScope::base("acme"), "other", 0, 0).unwrap();
        // SCHEMA_UPLOAD path adopts only `prefers`.
        let adopted2 = predicate_intern(
            &wtxn,
            DeclScope::base("acme"),
            "prefers",
            None,
            0,
            4,
            "",
            false,
            0,
        )
        .unwrap();
        assert_eq!(adopted, adopted2);
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let active = predicates_active_for_schema(&rtxn, DeclScope::base("acme"), 4).unwrap();
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
        let implicit_id =
            predicate_intern_or_get(&wtxn, DeclScope::base("acme"), "prefers", 0, 0).unwrap();
        // Now run the schema-declared intern path with a tighter
        // constraint — the registry should adopt the existing row
        // rather than allocate a fresh id or error.
        let declared_id = predicate_intern(
            &wtxn,
            DeclScope::base("acme"),
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
            DeclScope::base("acme"),
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
            DeclScope::base("acme"),
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
            DeclScope::base("acme"),
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
            DeclScope::base("acme"),
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
    fn id_by_qname_probes_without_minting() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        // Unknown qname → None, and no row minted.
        assert!(
            predicate_id_by_qname(&wtxn, DeclScope::base("acme"), "keen_on")
                .unwrap()
                .is_none()
        );
        let id = predicate_intern_or_get(&wtxn, DeclScope::base("acme"), "keen_on", 0, 0).unwrap();
        assert_eq!(
            predicate_id_by_qname(&wtxn, DeclScope::base("acme"), "keen_on").unwrap(),
            Some(id)
        );
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        // The probe must not have created rows: only the one real intern.
        assert_eq!(predicate_list(&rtxn, Some("acme")).unwrap().len(), 1);
    }

    #[test]
    fn alias_qname_points_variant_at_canonical_without_new_row() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let canonical =
            predicate_intern_or_get(&wtxn, DeclScope::base("acme"), "keen_on", 0, 0).unwrap();
        predicate_alias_qname(&wtxn, "acme", "keen_about", canonical).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        // The variant qname resolves to the canonical id...
        let by_qname = predicate_lookup_by_qname(&rtxn, DeclScope::base("acme"), "keen_about")
            .unwrap()
            .unwrap();
        assert_eq!(by_qname.id, canonical);
        // ...but its NAME is still the canonical's — no second PREDICATES_TABLE
        // row was minted, so the qname index simply aliases.
        assert_eq!(by_qname.name, "keen_on");
        assert_eq!(predicate_list(&rtxn, Some("acme")).unwrap().len(), 1);
    }

    #[test]
    fn alias_qname_refuses_schema_declared_target() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let declared = predicate_intern(
            &wtxn,
            DeclScope::base("brain"),
            "occurred_at",
            None,
            2,
            1,
            "",
            false,
            0,
        )
        .unwrap();
        let err = predicate_alias_qname(&wtxn, "brain", "happened_at", declared).unwrap_err();
        matches!(err, PredicateOpError::InvalidIdentifier { .. })
            .then_some(())
            .expect("declared target must be refused");
    }

    #[test]
    fn consolidation_candidates_yields_only_embedded_rows_with_flags() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let implicit =
            predicate_intern_or_get(&wtxn, DeclScope::base("acme"), "keen_on", 0, 0).unwrap();
        predicate_embedding_put(&wtxn, implicit, &[1.0, 0.0, 0.0]).unwrap();
        let declared = predicate_intern(
            &wtxn,
            DeclScope::base("acme"),
            "manages",
            None,
            1,
            1,
            "",
            false,
            0,
        )
        .unwrap();
        predicate_embedding_put(&wtxn, declared, &[0.0, 1.0, 0.0]).unwrap();
        // No embedding → excluded from candidates.
        let _bare =
            predicate_intern_or_get(&wtxn, DeclScope::base("acme"), "unembedded", 0, 0).unwrap();
        wtxn.commit().unwrap();

        let wtxn = db.begin_write().unwrap();
        let cands = predicate_consolidation_candidates(&wtxn, "acme").unwrap();
        assert_eq!(cands.len(), 2, "only the two embedded rows are candidates");
        let implicit_c = cands.iter().find(|c| c.0 == implicit).unwrap();
        assert_eq!(implicit_c.1, "keen_on");
        assert_eq!(implicit_c.2, vec![1.0, 0.0, 0.0]);
        assert!(!implicit_c.3, "implicit-from-write row is not declared");
        let declared_c = cands.iter().find(|c| c.0 == declared).unwrap();
        assert!(
            declared_c.3,
            "schema-declared row carries the declared flag"
        );
    }

    #[test]
    fn predicates_intern_or_get_validates_identifier() {
        let (_dir, db) = open_db();
        let wtxn = db.begin_write().unwrap();
        let err = predicate_intern_or_get(&wtxn, DeclScope::base("Bad"), "x", 0, 0).unwrap_err();
        matches!(err, PredicateOpError::InvalidIdentifier { .. })
            .then_some(())
            .expect("expected InvalidIdentifier");
    }

    // ── review queue ────────────────────────────────────────────────────

    #[test]
    fn open_vocab_intern_records_a_review_sighting() {
        let (_dir, db) = open_db();
        {
            let wtxn = db.begin_write().unwrap();
            predicate_intern_or_get(&wtxn, DeclScope::base("brain"), "difficulty", 7, 1).unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.begin_read().unwrap();
        let queue = predicate_review_list(&rtxn).unwrap();
        assert_eq!(queue, vec![("brain:difficulty".to_string(), 1)]);
    }

    #[test]
    fn repeat_sightings_accumulate_but_only_mint_once() {
        let (_dir, db) = open_db();
        let mut ids = Vec::new();
        for lsn in 0..3 {
            let wtxn = db.begin_write().unwrap();
            ids.push(
                predicate_intern_or_get(&wtxn, DeclScope::base("brain"), "horizon", lsn, 1)
                    .unwrap(),
            );
            wtxn.commit().unwrap();
        }
        // Same predicate id every time — the registry is not duplicated.
        assert_eq!(ids[0], ids[1]);
        assert_eq!(ids[1], ids[2]);

        // The count is 1, NOT 3: the qname index short-circuits before the
        // mint branch on every repeat. The queue answers "which names did
        // this corpus have to invent", which is a property of the name, not
        // of how often it is mentioned. If a future change moves the record
        // call above the index probe, this assertion is what catches it.
        let rtxn = db.begin_read().unwrap();
        assert_eq!(
            predicate_review_list(&rtxn).unwrap(),
            vec![("brain:horizon".to_string(), 1)]
        );
    }

    #[test]
    fn schema_declared_predicates_never_enter_the_review_queue() {
        let (_dir, db) = open_db();
        {
            let wtxn = db.begin_write().unwrap();
            // `predicate_intern` is the SCHEMA_UPLOAD path. A declared
            // predicate is the opposite of a promotion candidate, so it must
            // not show up in the operator's list.
            predicate_intern(
                &wtxn,
                DeclScope::base("mirror"),
                "clone_status",
                None,
                0,
                1,
                "",
                true,
                0,
            )
            .unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.begin_read().unwrap();
        assert!(predicate_review_list(&rtxn).unwrap().is_empty());
    }

    #[test]
    fn promoting_a_coined_predicate_removes_it_from_the_queue() {
        // The operator's workflow: see a candidate, declare it, stop being
        // told about it. A write-time-only filter could never do this — the
        // sighting was recorded back when the name really was undeclared.
        let (_dir, db) = open_db();
        {
            let wtxn = db.begin_write().unwrap();
            predicate_intern_or_get(&wtxn, DeclScope::base("mirror"), "difficulty", 0, 1).unwrap();
            wtxn.commit().unwrap();
        }
        {
            let rtxn = db.begin_read().unwrap();
            assert_eq!(predicate_review_list(&rtxn).unwrap().len(), 1);
        }

        // Promote it via the schema path.
        {
            let wtxn = db.begin_write().unwrap();
            predicate_intern(
                &wtxn,
                DeclScope::base("mirror"),
                "difficulty",
                None,
                0,
                1,
                "",
                false,
                0,
            )
            .unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.begin_read().unwrap();
        assert!(
            predicate_review_list(&rtxn).unwrap().is_empty(),
            "a promoted name is no longer a candidate",
        );
    }

    #[test]
    fn a_name_declared_as_a_relation_type_is_not_a_candidate() {
        // Declared on the other axis. Listing it would tell the operator to
        // declare something they already declared.
        let (_dir, db) = open_db();
        {
            let wtxn = db.begin_write().unwrap();
            // Record directly, as a pre-existing row from before the write
            // -time filter existed.
            predicate_review_record(&wtxn, "mirror:requested").unwrap();
            crate::relation::types::relation_type_intern(
                &wtxn,
                DeclScope::base("mirror"),
                "requested",
                None,
                None,
                brain_core::Cardinality::ManyToMany,
                false,
                1,
                "",
                0,
            )
            .unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.begin_read().unwrap();
        assert!(
            predicate_review_list(&rtxn).unwrap().is_empty(),
            "a declared relation type is not a coined predicate",
        );
    }

    #[test]
    fn review_list_orders_by_count_then_name() {
        let (_dir, db) = open_db();
        {
            let wtxn = db.begin_write().unwrap();
            for _ in 0..3 {
                predicate_review_record(&wtxn, "brain:often").unwrap();
            }
            predicate_review_record(&wtxn, "brain:zeta").unwrap();
            predicate_review_record(&wtxn, "brain:alpha").unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.begin_read().unwrap();
        assert_eq!(
            predicate_review_list(&rtxn).unwrap(),
            vec![
                ("brain:often".to_string(), 3),
                ("brain:alpha".to_string(), 1),
                ("brain:zeta".to_string(), 1),
            ]
        );
    }
}

#[cfg(all(test, not(miri)))]
mod predicate_gc_tests {
    use super::*;
    use crate::tables::fresh_db;
    use redb::ReadableDatabase;

    const DAY: u64 = 24 * 60 * 60 * 1_000_000_000;
    const GRACE: u64 = 7 * DAY;

    fn db() -> (tempfile::TempDir, redb::Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        (dir, db)
    }

    /// Coin a predicate (`ImplicitFromWrite`) created `age_nanos` ago.
    fn coin(db: &redb::Database, name: &str, created_at: u64) -> PredicateId {
        let wtxn = db.begin_write().unwrap();
        let id =
            predicate_intern_or_get(&wtxn, DeclScope::base("brain"), name, 1, created_at).unwrap();
        wtxn.commit().unwrap();
        id
    }

    fn exists(db: &redb::Database, name: &str) -> bool {
        let rtxn = db.begin_read().unwrap();
        let idx = rtxn.open_table(PREDICATES_BY_SCOPE_TABLE).unwrap();
        idx.get(&(qname("brain", name).as_str(), SPACE_BASE))
            .unwrap()
            .is_some()
    }

    fn reclaim(db: &redb::Database, now: u64, limit: usize) -> PredicateReclaimSummary {
        let wtxn = db.begin_write().unwrap();
        let s = predicate_reclaim_orphans(&wtxn, now, GRACE, limit, false).unwrap();
        wtxn.commit().unwrap();
        s
    }

    /// The leak this closes: a coined name whose statements are gone.
    #[test]
    fn an_unreferenced_coined_predicate_is_reclaimed() {
        let (_d, db) = db();
        coin(&db, "agreed_to_lead", 0);
        assert!(exists(&db, "agreed_to_lead"));

        let s = reclaim(&db, 30 * DAY, 100);
        assert_eq!(s.reclaimed, 1);
        assert_eq!(s.still_referenced, 0);
        assert!(!exists(&db, "agreed_to_lead"), "orphaned name is gone");
    }

    /// A declared predicate is intent, not residue — never a candidate,
    /// however unused. Dropping it belongs to SCHEMA_DROP.
    #[test]
    fn a_schema_declared_predicate_is_never_reclaimed() {
        let (_d, db) = db();
        {
            let wtxn = db.begin_write().unwrap();
            predicate_intern(
                &wtxn,
                DeclScope::base("acme"),
                "manages",
                None,
                1,
                1,
                "",
                false,
                0,
            )
            .unwrap();
            wtxn.commit().unwrap();
        }
        let s = reclaim(&db, 365 * DAY, 100);
        assert_eq!(s.reclaimed, 0, "declared vocabulary is untouched");
        let rtxn = db.begin_read().unwrap();
        let idx = rtxn.open_table(PREDICATES_BY_SCOPE_TABLE).unwrap();
        assert!(idx
            .get(&(qname("acme", "manages").as_str(), SPACE_BASE))
            .unwrap()
            .is_some());
    }

    /// Interning happens in the same write as the statement; a freshly
    /// coined name must survive until the grace window closes.
    #[test]
    fn a_freshly_coined_predicate_is_inside_the_grace_window() {
        let (_d, db) = db();
        coin(&db, "just_minted", 10 * DAY);

        let s = reclaim(&db, 10 * DAY + 1, 100);
        assert_eq!(s.examined, 0, "inside grace, not even examined");
        assert!(exists(&db, "just_minted"));

        let s = reclaim(&db, 10 * DAY + GRACE + 1, 100);
        assert_eq!(s.reclaimed, 1, "past grace it is reclaimable");
    }

    /// The review queue is a shortlist of names worth promoting into a
    /// schema; a reclaimed name has nothing left to promote.
    #[test]
    fn reclaiming_clears_the_review_queue_entry() {
        let (_d, db) = db();
        coin(&db, "queued_name", 0);
        {
            let rtxn = db.begin_read().unwrap();
            let q = predicate_review_list(&rtxn).unwrap();
            assert!(
                q.iter().any(|(n, _)| n == "brain:queued_name"),
                "coining enqueues it for review"
            );
        }
        reclaim(&db, 30 * DAY, 100);
        let rtxn = db.begin_read().unwrap();
        let q = predicate_review_list(&rtxn).unwrap();
        assert!(
            !q.iter().any(|(n, _)| n == "brain:queued_name"),
            "and reclaiming dequeues it"
        );
    }

    /// Report-only must report and delete nothing.
    #[test]
    fn a_dry_run_reports_without_deleting() {
        let (_d, db) = db();
        coin(&db, "preview_me", 0);

        let s = {
            let wtxn = db.begin_write().unwrap();
            let s = predicate_reclaim_orphans(&wtxn, 30 * DAY, GRACE, 100, true).unwrap();
            wtxn.commit().unwrap();
            s
        };
        assert_eq!(s.reclaimed, 1, "reports what it would reclaim");
        assert!(exists(&db, "preview_me"), "but the row is still there");

        let s = reclaim(&db, 30 * DAY, 100);
        assert_eq!(s.reclaimed, 1, "a real pass then removes it");
        assert!(!exists(&db, "preview_me"));
    }

    /// One pass must stay bounded so a worker cycle stays short.
    #[test]
    fn a_pass_reclaims_at_most_its_limit() {
        let (_d, db) = db();
        for i in 0..5 {
            coin(&db, &format!("spare_{i}"), 0);
        }
        let s = reclaim(&db, 30 * DAY, 2);
        assert_eq!(s.reclaimed, 2, "limit honoured");
        let s = reclaim(&db, 30 * DAY, 100);
        assert_eq!(s.reclaimed, 3, "the rest go on the next pass");
    }
}
