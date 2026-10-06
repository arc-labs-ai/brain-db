//! Per-namespace view of a user schema's DECLARED vocabulary.
//!
//! The extractor needs two things from a namespace's active user schema:
//!
//! 1. an exact local-name lookup, so an extracted `brain:blocked_by` for a
//!    memory owned by namespace `mirror` resolves to the declared
//!    `mirror:blocked_by` instead of coining an open-vocab `brain:` twin;
//! 2. a deterministic prompt block listing that vocabulary, so the LLM
//!    prefers `mirror:clones` over inventing `built_clone_of`.
//!
//! Both are read from the ACTIVE schema version's persisted AST (the
//! `SchemaVersionRow::source` JSON), which — unlike the predicate row —
//! still carries the declared value sub-type (`Value<number>` vs
//! `Value<text>`).
//!
//! Tenant isolation is structural: every entry point takes exactly one
//! namespace and only ever reads that namespace's schema row. The system
//! `brain` namespace is never reported here — its vocabulary is the shared
//! open-vocab pool the extractor already targets.

use brain_protocol::schema::{
    AttrType, CardinalityAst, EntityTypeDef, ObjectTypeDecl, PredicateDef, RelationTypeDef, Schema,
    SchemaItem, StatementKindAst,
};
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

use super::store::SchemaStoreError;
use crate::system_schema::SYSTEM_SCHEMA_NAMESPACE;
use crate::tables::schema_version::{
    SchemaVersionRow, SCHEMA_ACTIVE_VERSIONS_TABLE, SCHEMA_VERSIONS_TABLE,
};

/// The predicates and relation types a namespace's ACTIVE user schema
/// declares, each list sorted by local name (so lookups can binary-search
/// and the rendered prompt block is byte-stable across cycles).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeclaredVocabulary {
    pub namespace: String,
    pub version: u32,
    pub predicates: Vec<PredicateDef>,
    pub relation_types: Vec<RelationTypeDef>,
    /// The namespace's own entity types, shown with their attributes so the
    /// model knows what each is (the DSL has no entity-type description).
    pub entity_types: Vec<EntityTypeDef>,
}

impl DeclaredVocabulary {
    /// Build the view from a decoded schema AST. Entries are sorted by
    /// local name; a duplicate name (the validator rejects these, but be
    /// defensive) keeps the first declaration.
    #[must_use]
    pub fn from_schema(schema: &Schema, version: u32) -> Self {
        let mut predicates: Vec<PredicateDef> = Vec::new();
        let mut relation_types: Vec<RelationTypeDef> = Vec::new();
        let mut entity_types: Vec<EntityTypeDef> = Vec::new();
        for item in &schema.items {
            match item {
                SchemaItem::Predicate(p) => predicates.push(p.clone()),
                SchemaItem::RelationType(r) => relation_types.push(r.clone()),
                SchemaItem::EntityType(e) => entity_types.push(e.clone()),
                _ => {}
            }
        }
        entity_types.sort_by(|a, b| a.name.cmp(&b.name));
        entity_types.dedup_by(|a, b| a.name == b.name);
        predicates.sort_by(|a, b| a.name.cmp(&b.name));
        predicates.dedup_by(|a, b| a.name == b.name);
        relation_types.sort_by(|a, b| a.name.cmp(&b.name));
        relation_types.dedup_by(|a, b| a.name == b.name);
        Self {
            namespace: schema.namespace.clone(),
            version,
            predicates,
            relation_types,
            entity_types,
        }
    }

    /// True when the schema declares neither predicates nor relation types
    /// (e.g. an entity-types-only schema) — nothing for the extractor to
    /// resolve onto or advertise.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.predicates.is_empty() && self.relation_types.is_empty() && self.entity_types.is_empty()
    }

    /// The declared predicate with local name `name`, if any.
    #[must_use]
    pub fn predicate(&self, name: &str) -> Option<&PredicateDef> {
        self.predicates
            .binary_search_by(|p| p.name.as_str().cmp(name))
            .ok()
            .map(|i| &self.predicates[i])
    }

    /// The declared relation type with local name `name`, if any.
    #[must_use]
    pub fn relation_type(&self, name: &str) -> Option<&RelationTypeDef> {
        self.relation_types
            .binary_search_by(|r| r.name.as_str().cmp(name))
            .ok()
            .map(|i| &self.relation_types[i])
    }

    /// Render the vocabulary as an LLM-prompt block. Deterministic: entries
    /// are emitted in local-name order, predicates before relation types.
    /// Empty string when there is nothing declared.
    ///
    /// ```text
    /// Schema-declared vocabulary for namespace `acme` — ...
    /// Predicates (`qname (kind, object): description`):
    /// - acme:blocked_by (Fact, Value<text>): What blocks a build.
    /// Relation types — entity-to-entity links; ... (`qname (from -> to, cardinality): description`):
    /// - acme:clones (CloneEnv -> TargetApp, many-to-one)
    /// ```
    #[must_use]
    pub fn render_prompt_block(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let ns = self.namespace.as_str();
        let mut out = String::new();
        out.push_str("Schema-declared vocabulary for namespace `");
        out.push_str(ns);
        out.push_str(
            "` — when a fact means one of these, emit its EXACT qname below (it takes \
             precedence over any `brain:` name, including the existing ones listed above), \
             and follow its declared kind and object type. When the memory says a \
             declared fact STOPPED holding (fixed, resolved, removed, cancelled, no \
             longer…), emit that SAME declared predicate with the old object and \
             \"retract\": true — never coin a verb such as `fixed` or `resolved`:\n",
        );
        if !self.entity_types.is_empty() {
            out.push_str(
                "Entity types this namespace declares — type a mention with one of these \
                 labels whenever it fits, in preference to a generic built-in type; the \
                 attributes say what each one is:\n",
            );
            for e in &self.entity_types {
                out.push_str("- brain:");
                out.push_str(&e.name);
                if !e.attributes.is_empty() {
                    out.push_str(" (attributes: ");
                    let names: Vec<&str> = e.attributes.iter().map(|a| a.name.as_str()).collect();
                    out.push_str(&names.join(", "));
                    out.push(')');
                }
                out.push('\n');
            }
        }
        if !self.predicates.is_empty() {
            out.push_str("Predicates (`qname (kind, object): description`):\n");
            for p in &self.predicates {
                out.push_str("- ");
                out.push_str(ns);
                out.push(':');
                out.push_str(&p.name);
                out.push_str(" (");
                out.push_str(kind_label(p.kind));
                out.push_str(", ");
                out.push_str(&object_label(&p.object));
                if p.resolved_stateful() {
                    // A single-valued predicate is also how a CHANGE is said:
                    // without this the model coins verbs ("switch", "fixed")
                    // and the stale value never gets superseded.
                    out.push_str(
                        ", single-valued: a newer value replaces the old — use it for \
                         changes and updates too, e.g. \"switched to X\", \"is now X\"",
                    );
                }
                out.push(')');
                push_description(&mut out, p.description.as_deref());
                out.push('\n');
            }
        }
        if !self.relation_types.is_empty() {
            out.push_str(
                "Relation types — entity-to-entity links; emit with kind \"Relation\" and \
                 object_is_entity true (`qname (from -> to, cardinality): description`):\n",
            );
            for r in &self.relation_types {
                out.push_str("- ");
                out.push_str(ns);
                out.push(':');
                out.push_str(&r.name);
                out.push_str(" (");
                out.push_str(&r.from_type);
                out.push_str(" -> ");
                out.push_str(&r.to_type);
                out.push_str(", ");
                out.push_str(cardinality_label(r.cardinality));
                if r.symmetric {
                    out.push_str(", symmetric");
                }
                out.push(')');
                push_description(&mut out, r.description.as_deref());
                out.push('\n');
            }
        }
        out
    }
}

fn push_description(out: &mut String, description: Option<&str>) {
    if let Some(d) = description.map(str::trim).filter(|d| !d.is_empty()) {
        out.push_str(": ");
        // Keep one bullet per line even if a description spans lines.
        out.push_str(&d.replace(['\n', '\r'], " "));
    }
}

fn kind_label(k: StatementKindAst) -> &'static str {
    match k {
        StatementKindAst::Fact => "Fact",
        StatementKindAst::Preference => "Preference",
        StatementKindAst::Event => "Event",
        StatementKindAst::Attribute => "Attribute",
        StatementKindAst::Relation => "Relation",
        StatementKindAst::Directive => "Directive",
        StatementKindAst::Any => "any-kind",
    }
}

fn cardinality_label(c: CardinalityAst) -> &'static str {
    match c {
        CardinalityAst::OneToOne => "one-to-one",
        CardinalityAst::OneToMany => "one-to-many",
        CardinalityAst::ManyToOne => "many-to-one",
        CardinalityAst::ManyToMany => "many-to-many",
    }
}

fn attr_label(a: &AttrType) -> String {
    match a {
        AttrType::Text => "text".to_string(),
        AttrType::Number => "number".to_string(),
        AttrType::Bool => "bool".to_string(),
        AttrType::Date => "date".to_string(),
        AttrType::Timestamp => "timestamp".to_string(),
        AttrType::Enum { variants } => format!("enum[{}]", variants.join("|")),
        AttrType::Ref { target } => format!("ref {target}"),
    }
}

fn object_label(o: &ObjectTypeDecl) -> String {
    match o {
        ObjectTypeDecl::Value { value_type } => format!("Value<{}>", attr_label(value_type)),
        ObjectTypeDecl::Entity { entity_type } => format!("Entity<{entity_type}>"),
        ObjectTypeDecl::Memory => "Memory".to_string(),
        ObjectTypeDecl::Statement => "Statement".to_string(),
        ObjectTypeDecl::Any => "Any".to_string(),
    }
}

fn decode_row(namespace: &str, row: &SchemaVersionRow) -> Option<DeclaredVocabulary> {
    match serde_json::from_slice::<Schema>(&row.source) {
        // Defensive: the row is keyed by namespace, but never hand back a
        // vocabulary whose AST claims a different one.
        Ok(schema) if schema.namespace == namespace => {
            Some(DeclaredVocabulary::from_schema(&schema, row.version))
        }
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(
                target: "brain_metadata::schema",
                namespace,
                version = row.version,
                error = %e,
                "active schema AST failed to decode; treating namespace as undeclared",
            );
            None
        }
    }
}

/// The declared vocabulary of `namespace`'s ACTIVE user schema, or `None`
/// when the namespace is the system `brain` namespace, has no active
/// schema, or its schema declares no predicates / relation types.
pub fn declared_vocabulary(
    rtxn: &ReadTransaction,
    namespace: &str,
) -> Result<Option<DeclaredVocabulary>, SchemaStoreError> {
    if namespace == SYSTEM_SCHEMA_NAMESPACE {
        return Ok(None);
    }
    let active = rtxn.open_table(SCHEMA_ACTIVE_VERSIONS_TABLE)?;
    let Some(version) = active.get(&namespace)?.map(|g| g.value()) else {
        return Ok(None);
    };
    let versions = rtxn.open_table(SCHEMA_VERSIONS_TABLE)?;
    let Some(row) = versions.get(&(namespace, version))?.map(|g| g.value()) else {
        return Ok(None);
    };
    Ok(decode_row(namespace, &row).filter(|v| !v.is_empty()))
}

/// Write-transaction counterpart to [`declared_vocabulary`], for the
/// extractor's apply pass (which resolves predicates inside its wtxn).
pub fn declared_vocabulary_wtxn(
    wtxn: &WriteTransaction,
    namespace: &str,
) -> Result<Option<DeclaredVocabulary>, SchemaStoreError> {
    if namespace == SYSTEM_SCHEMA_NAMESPACE {
        return Ok(None);
    }
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
    let Some(row) = row else {
        return Ok(None);
    };
    Ok(decode_row(namespace, &row).filter(|v| !v.is_empty()))
}

// ---------------------------------------------------------------------------
// Per-namespace entity-type label sets.
//
// Entity types live in ONE flat, namespaceless registry (see
// `crate::entity::types`), so the registry alone cannot say which tenant a
// type belongs to. The declaring namespace is recovered from the ACTIVE
// schema documents instead: a type some OTHER user namespace declares — and
// that neither the system schema nor the memory's own namespace declares —
// is that tenant's private vocabulary and must not reach this namespace's
// extraction prompt or classifier labels.
// ---------------------------------------------------------------------------

/// Entity-type names declared by every namespace with an active schema
/// (including the system `brain` namespace), keyed by namespace. A schema
/// row that fails to decode contributes nothing (its types are then treated
/// as unowned — visible — rather than failing extraction).
fn declared_entity_types_by_namespace(
    rtxn: &ReadTransaction,
) -> Result<std::collections::HashMap<String, std::collections::HashSet<String>>, SchemaStoreError>
{
    let mut out = std::collections::HashMap::new();
    let active = rtxn.open_table(SCHEMA_ACTIVE_VERSIONS_TABLE)?;
    let versions = rtxn.open_table(SCHEMA_VERSIONS_TABLE)?;
    for entry in active.iter()? {
        let (k, v) = entry?;
        let ns = k.value().to_string();
        let version = v.value();
        let Some(row) = versions.get(&(ns.as_str(), version))?.map(|g| g.value()) else {
            continue;
        };
        let Ok(schema) = serde_json::from_slice::<Schema>(&row.source) else {
            continue;
        };
        let names: std::collections::HashSet<String> = schema
            .items
            .iter()
            .filter_map(|i| match i {
                SchemaItem::EntityType(e) => Some(e.name.clone()),
                _ => None,
            })
            .collect();
        out.insert(ns, names);
    }
    Ok(out)
}

/// Entity-type classifier labels (`brain:<Name>`, stable id-order — the
/// same surface as [`crate::entity_type_label_qnames`]) visible to memories
/// of each requested namespace: Brain's built-in (system schema) types, the
/// namespace's own declared types, and any type no user schema claims
/// (e.g. implicitly interned ones) — but never a type declared only by
/// ANOTHER user namespace. The system `brain` namespace sees built-ins and
/// unclaimed types only.
///
/// Computed for a whole batch from one snapshot: the schema documents are
/// decoded once, not once per namespace.
pub fn entity_type_labels_for_namespaces(
    rtxn: &ReadTransaction,
    namespaces: &[&str],
) -> Result<std::collections::HashMap<String, Vec<String>>, SchemaStoreError> {
    let all = crate::entity::types::entity_type_label_qnames(rtxn)
        .map_err(|e| SchemaStoreError::Encode(format!("entity types: {e}")))?;
    let declared = declared_entity_types_by_namespace(rtxn)?;
    let empty = std::collections::HashSet::new();
    let system = declared.get(SYSTEM_SCHEMA_NAMESPACE).unwrap_or(&empty);
    let mut out = std::collections::HashMap::new();
    for &ns in namespaces {
        if out.contains_key(ns) {
            continue;
        }
        let own = declared.get(ns).unwrap_or(&empty);
        let foreign: std::collections::HashSet<&str> = declared
            .iter()
            .filter(|(other, _)| other.as_str() != ns && other.as_str() != SYSTEM_SCHEMA_NAMESPACE)
            .flat_map(|(_, names)| names.iter().map(String::as_str))
            .filter(|n| !own.contains(*n) && !system.contains(*n))
            .collect();
        let labels: Vec<String> = all
            .iter()
            .filter(|label| {
                let bare = label.strip_prefix("brain:").unwrap_or(label);
                !foreign.contains(bare)
            })
            .cloned()
            .collect();
        out.insert(ns.to_string(), labels);
    }
    Ok(out)
}

/// Render an entity-type label set as the `{DECLARED_ENTITY_TYPES}` prompt
/// block — one `- <label>` bullet per type, lexicographically sorted so the
/// block (and any prompt-cache key over it) is deterministic. Same format as
/// [`crate::render_declared_entity_types_block`].
#[must_use]
pub fn render_entity_type_labels_block(labels: &[String]) -> String {
    let mut sorted: Vec<&str> = labels.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    let mut out = String::new();
    for label in sorted {
        out.push_str("- ");
        out.push_str(label);
        out.push('\n');
    }
    out
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::schema::store::schema_upload;
    use crate::tables::fresh_db;
    use redb::ReadableDatabase;

    const NOW: u64 = 1_700_000_000_000_000_000;

    fn upload(db: &redb::Database, src: &str) {
        let parsed = brain_protocol::schema::parse_schema(src).expect("parse");
        let validated = brain_protocol::schema::validate(&parsed).expect("validate");
        let wtxn = db.begin_write().unwrap();
        schema_upload(&wtxn, &validated, NOW).unwrap();
        wtxn.commit().unwrap();
    }

    const ACME: &str = r#"
namespace acme

define entity_type Widget { attributes {} }

define predicate success_rate {
    kind: Fact
    object: Value<number>
    description: "Fraction of runs that succeeded."
}

define predicate blocked_by {
    kind: Fact
    object: Value<text>
}

define relation_type clones {
    from: Widget
    to: Widget
    cardinality: many-to-one
}
"#;

    #[test]
    fn declared_vocabulary_reads_active_schema_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        upload(&db, ACME);
        let rtxn = db.begin_read().unwrap();
        let v = declared_vocabulary(&rtxn, "acme")
            .unwrap()
            .expect("declared");
        assert_eq!(v.namespace, "acme");
        let names: Vec<&str> = v.predicates.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["blocked_by", "success_rate"]);
        assert!(v.predicate("success_rate").is_some());
        assert!(v.predicate("works_at").is_none());
        assert!(v.relation_type("clones").is_some());
        assert!(v.relation_type("blocked_by").is_none());
    }

    #[test]
    fn declared_vocabulary_is_namespace_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        upload(&db, ACME);
        let rtxn = db.begin_read().unwrap();
        // Another namespace, the system namespace, and an unknown one all
        // see nothing of acme's vocabulary.
        assert!(declared_vocabulary(&rtxn, "globex").unwrap().is_none());
        assert!(declared_vocabulary(&rtxn, SYSTEM_SCHEMA_NAMESPACE)
            .unwrap()
            .is_none());
        drop(rtxn);
        let wtxn = db.begin_write().unwrap();
        assert!(declared_vocabulary_wtxn(&wtxn, "globex").unwrap().is_none());
        assert!(declared_vocabulary_wtxn(&wtxn, "acme").unwrap().is_some());
    }

    #[test]
    fn render_prompt_block_is_deterministic_and_typed() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        upload(&db, ACME);
        let rtxn = db.begin_read().unwrap();
        let v = declared_vocabulary(&rtxn, "acme").unwrap().unwrap();
        let block = v.render_prompt_block();
        assert_eq!(block, v.render_prompt_block(), "stable across calls");
        assert!(
            block.contains(
                "- acme:success_rate (Fact, Value<number>): Fraction of runs that succeeded.\n"
            ),
            "{block}"
        );
        assert!(
            block.contains("- acme:blocked_by (Fact, Value<text>)\n"),
            "{block}"
        );
        assert!(
            block.contains("- acme:clones (Widget -> Widget, many-to-one)\n"),
            "{block}"
        );
        // Predicates are sorted by name; relation types follow predicates.
        let blocked = block.find("acme:blocked_by").unwrap();
        let rate = block.find("acme:success_rate").unwrap();
        let clones = block.find("acme:clones").unwrap();
        assert!(blocked < rate && rate < clones, "{block}");
    }

    const GLOBEX: &str = r#"
namespace globex
define entity_type Invoice { attributes {} }
"#;

    #[test]
    fn entity_type_labels_are_scoped_per_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        // Built-in types come from the seeded system schema.
        crate::seed_system_schema(&db).unwrap();
        upload(&db, ACME);
        upload(&db, GLOBEX);
        let rtxn = db.begin_read().unwrap();
        let by_ns =
            entity_type_labels_for_namespaces(&rtxn, &["acme", "globex", "brain", "unknown"])
                .unwrap();
        let has = |ns: &str, label: &str| by_ns[ns].iter().any(|l| l == label);
        for ns in ["acme", "globex", "brain", "unknown"] {
            assert!(has(ns, "brain:Person"), "{ns} must see built-in Person");
        }
        // Each tenant sees its own types and never the other's.
        assert!(has("acme", "brain:Widget"));
        assert!(
            !has("acme", "brain:Invoice"),
            "acme must not see globex's Invoice"
        );
        assert!(has("globex", "brain:Invoice"));
        assert!(
            !has("globex", "brain:Widget"),
            "globex must not see acme's Widget"
        );
        // The system namespace and an unschema'd one see neither.
        for ns in ["brain", "unknown"] {
            assert!(
                !has(ns, "brain:Widget") && !has(ns, "brain:Invoice"),
                "{ns}"
            );
        }
        let block = render_entity_type_labels_block(&by_ns["acme"]);
        assert!(block.contains("- brain:Widget\n"), "{block}");
        assert!(!block.contains("Invoice"), "{block}");
    }

    #[test]
    fn render_prompt_block_explains_types_and_single_valued_predicates() {
        let schema = brain_protocol::schema::parse_schema(
            "namespace acme\n\
             define entity_type CloneEnv { attributes { status: text optional framework: text optional } }\n\
             define predicate env_status { kind: Fact object: Value<text> stateful: true }\n\
             define predicate note { kind: Fact object: Value<text> }\n",
        )
        .unwrap();
        let block = DeclaredVocabulary::from_schema(&schema, 1).render_prompt_block();
        assert!(
            block.contains("- brain:CloneEnv (attributes: status, framework)\n"),
            "{block}"
        );
        assert!(
            block.contains("- acme:env_status (Fact, Value<text>, single-valued: a newer value replaces the old"),
            "{block}"
        );
        assert!(
            block.contains("- acme:note (Fact, Value<text>)\n"),
            "{block}"
        );
        assert!(block.contains("\"retract\": true"), "{block}");
        // Entity types are listed before predicates.
        assert!(block.find("brain:CloneEnv").unwrap() < block.find("acme:env_status").unwrap());
    }

    #[test]
    fn render_prompt_block_empty_for_empty_vocabulary() {
        assert!(DeclaredVocabulary::default()
            .render_prompt_block()
            .is_empty());
    }
}
