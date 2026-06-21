//! Fan-out from a `ValidatedSchema` into the existing
//! entity_type / predicate / relation_type intern paths.
//!
//! Called by [`crate::schema::store::schema_upload`] after the
//! schema-version row is written. The single code path used both
//! by the system-schema bootstrap and by every user `SCHEMA_UPLOAD`.

use std::collections::HashSet;

use brain_core::StatementKind;
use brain_core::{
    Cardinality, EntityTypeId, ExtractorKind, KindCardinality, PredicateId, TemporalModel,
};
use brain_protocol::schema::{
    CardinalityAst, ExtractorKindAst, ObjectTypeDecl, SchemaItem, StatementKindAst, ValidatedSchema,
};
use redb::{ReadableTable, WriteTransaction};

use super::kind::{kind_intern, KindOpError};
use super::predicate::{predicate_intern, PredicateOpError};
use crate::entity::types::{entity_type_intern, entity_type_lookup_by_name, EntityTypeOpError};
use crate::extractor::ops::{extractor_intern, ExtractorOpError};
use crate::relation::types::{relation_type_intern, RelationTypeOpError};
use crate::tables::predicate::{PredicateDefinition, PREDICATES_TABLE};
use crate::tables::statement::{statement_flags, StatementMetadata, STATEMENTS_TABLE};

#[derive(thiserror::Error, Debug)]
pub enum SchemaApplyError {
    #[error("entity_type: {0}")]
    EntityType(#[from] EntityTypeOpError),
    #[error("predicate: {0}")]
    Predicate(#[from] PredicateOpError),
    #[error("relation_type: {0}")]
    RelationType(#[from] RelationTypeOpError),
    #[error("extractor: {0}")]
    Extractor(#[from] ExtractorOpError),
    #[error("kind: {0}")]
    Kind(#[from] KindOpError),
    #[error("extractor encode: {0}")]
    ExtractorEncode(String),
    #[error("redb storage: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("redb table: {0}")]
    Table(#[from] redb::TableError),
}

/// Walk `validated.items` in source order and intern each
/// definition. Extractors are skipped.
pub fn apply_schema_definitions(
    wtxn: &WriteTransaction,
    validated: &ValidatedSchema,
    schema_version: u32,
    now_unix_nanos: u64,
) -> Result<(), SchemaApplyError> {
    let schema = validated.as_schema();
    let namespace = schema.namespace.as_str();

    for item in &schema.items {
        match item {
            SchemaItem::EntityType(e) => {
                // `schema_blob` left empty — typed accessors will own
                // the encoding.
                entity_type_intern(wtxn, &e.name, Vec::new(), now_unix_nanos)?;
            }
            SchemaItem::Predicate(p) => {
                predicate_intern(
                    wtxn,
                    namespace,
                    &p.name,
                    map_statement_kind(p.kind),
                    object_type_constraint_byte(&p.object),
                    schema_version,
                    p.description.as_deref().unwrap_or(""),
                    p.resolved_stateful(),
                    now_unix_nanos,
                )?;
            }
            SchemaItem::RelationType(r) => {
                let from = resolve_entity_type(wtxn, &r.from_type)?;
                let to = resolve_entity_type(wtxn, &r.to_type)?;
                relation_type_intern(
                    wtxn,
                    namespace,
                    &r.name,
                    from,
                    to,
                    map_cardinality(r.cardinality),
                    r.symmetric,
                    schema_version,
                    r.description.as_deref().unwrap_or(""),
                    now_unix_nanos,
                )?;
            }
            SchemaItem::Extractor(e) => {
                let kind = map_extractor_kind(e.kind);
                let blob = serde_json::to_vec(e)
                    .map_err(|err| SchemaApplyError::ExtractorEncode(err.to_string()))?;
                extractor_intern(
                    wtxn,
                    namespace,
                    &e.name,
                    kind,
                    schema_version,
                    blob,
                    now_unix_nanos,
                )?;
            }
            SchemaItem::Kind(k) => {
                kind_intern(
                    wtxn,
                    namespace,
                    &k.name,
                    map_kind_cardinality(k.cardinality),
                    map_temporal_model(k.temporal),
                    k.polarity,
                    k.hint.as_deref().unwrap_or(""),
                    schema_version,
                    now_unix_nanos,
                )?;
            }
        }
    }
    Ok(())
}

fn map_kind_cardinality(c: brain_protocol::schema::KindCardinalityAst) -> KindCardinality {
    use brain_protocol::schema::KindCardinalityAst as A;
    match c {
        A::Single => KindCardinality::Single,
        A::Set => KindCardinality::Set,
    }
}

fn map_temporal_model(t: brain_protocol::schema::TemporalModelAst) -> TemporalModel {
    use brain_protocol::schema::TemporalModelAst as A;
    match t {
        A::State => TemporalModel::State,
        A::Event => TemporalModel::Event,
        A::None => TemporalModel::Atemporal,
    }
}

/// Mark every statement in `namespace` whose predicate isn't in the
/// just-uploaded schema with [`statement_flags::OUTSIDE_ACTIVE_SCHEMA`]
/// (and clear the flag from statements that *are* now in-vocabulary).
///
/// Cost: O(N) over the predicates table (small) plus O(N) over the
/// statements table for the namespace's predicate ids. SCHEMA_UPLOAD
/// is a rare operator action — a full scan inside the upload txn is
/// acceptable per the design note.
///
/// Returns the count of rows whose flag bit changed (for observability).
pub fn flag_statements_outside_schema(
    wtxn: &WriteTransaction,
    namespace: &str,
    active_predicate_ids: &HashSet<PredicateId>,
) -> Result<usize, SchemaApplyError> {
    // First pass: build the namespace ↔ predicate-id map so we don't
    // touch every row in every other namespace.
    let predicate_namespace_map: Vec<(PredicateId, String)> = {
        let t = wtxn.open_table(PREDICATES_TABLE)?;
        let mut out = Vec::new();
        for entry in t.iter()? {
            let (k, v) = entry?;
            let pid = PredicateId::from(k.value());
            let row: PredicateDefinition = v.value();
            out.push((pid, row.namespace));
        }
        out
    };
    let in_namespace: HashSet<PredicateId> = predicate_namespace_map
        .iter()
        .filter(|(_, ns)| ns == namespace)
        .map(|(p, _)| *p)
        .collect();

    let mut changed = 0usize;
    let updates: Vec<([u8; 16], StatementMetadata)> = {
        let t = wtxn.open_table(STATEMENTS_TABLE)?;
        let mut out = Vec::new();
        for entry in t.iter()? {
            let (k, v) = entry?;
            let row: StatementMetadata = v.value();
            let pid = PredicateId::from(row.predicate_id);
            // Only inspect rows whose predicate lives in this
            // namespace — cross-namespace rows are off-topic for
            // this upload.
            if !in_namespace.contains(&pid) {
                continue;
            }
            let should_flag = !active_predicate_ids.contains(&pid);
            let has_flag = row.has_flag(statement_flags::OUTSIDE_ACTIVE_SCHEMA);
            if should_flag != has_flag {
                let mut new_row = row;
                if should_flag {
                    new_row.set_flag(statement_flags::OUTSIDE_ACTIVE_SCHEMA);
                } else {
                    new_row.clear_flag(statement_flags::OUTSIDE_ACTIVE_SCHEMA);
                }
                out.push((k.value(), new_row));
            }
        }
        out
    };
    {
        let mut t = wtxn.open_table(STATEMENTS_TABLE)?;
        for (k, row) in updates {
            t.insert(&k, &row)?;
            changed += 1;
        }
    }
    Ok(changed)
}

fn map_statement_kind(k: StatementKindAst) -> Option<StatementKind> {
    match k {
        StatementKindAst::Fact => Some(StatementKind::Fact),
        StatementKindAst::Preference => Some(StatementKind::Preference),
        StatementKindAst::Event => Some(StatementKind::Event),
        StatementKindAst::Attribute => Some(StatementKind::Attribute),
        StatementKindAst::Relation => Some(StatementKind::Relation),
        StatementKindAst::Directive => Some(StatementKind::Directive),
        StatementKindAst::Any => None,
    }
}

/// Byte encoding for the object-type constraint: `0` any / `1` Entity
/// / `2` Value / `3` Memory / `4` Statement.
fn object_type_constraint_byte(o: &ObjectTypeDecl) -> u8 {
    match o {
        ObjectTypeDecl::Any => 0,
        ObjectTypeDecl::Entity { .. } => 1,
        ObjectTypeDecl::Value { .. } => 2,
        ObjectTypeDecl::Memory => 3,
        ObjectTypeDecl::Statement => 4,
    }
}

fn map_cardinality(c: CardinalityAst) -> Cardinality {
    match c {
        CardinalityAst::OneToOne => Cardinality::OneToOne,
        CardinalityAst::OneToMany => Cardinality::OneToMany,
        CardinalityAst::ManyToOne => Cardinality::ManyToOne,
        CardinalityAst::ManyToMany => Cardinality::ManyToMany,
    }
}

pub(crate) fn map_extractor_kind(k: ExtractorKindAst) -> ExtractorKind {
    match k {
        ExtractorKindAst::Pattern => ExtractorKind::Pattern,
        ExtractorKindAst::Classifier => ExtractorKind::Classifier,
        ExtractorKindAst::Llm => ExtractorKind::Llm,
    }
}

/// `"Any"` → `None`; otherwise looks up the entity type by name.
/// Missing lookups fall through as `None`, preserving the "no
/// constraint" semantics for unknown / Any targets.
fn resolve_entity_type(
    wtxn: &WriteTransaction,
    name: &str,
) -> Result<Option<EntityTypeId>, EntityTypeOpError> {
    if name == "Any" {
        return Ok(None);
    }
    Ok(entity_type_lookup_by_name(wtxn, name)?.map(|d| d.id()))
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::extractor::ops::extractor_lookup_by_qname;
    use brain_protocol::schema::{parse_schema, validate, ExtractorDef, ExtractorTarget};
    use redb::{Database, ReadableDatabase};

    fn open_db(dir: &tempfile::TempDir) -> Database {
        Database::create(dir.path().join("test.redb")).unwrap()
    }

    #[test]
    fn extractor_item_is_persisted_with_json_blob() {
        let src = r#"
            namespace acme
            define entity_type Person { attributes {} }
            define extractor person_mentions {
                kind: pattern
                target: entity Person
                patterns [ /\b([A-Z][a-z]+)\b/ ]
                confidence: 0.7
            }
        "#;
        let schema = parse_schema(src).expect("parse");
        let validated = validate(&schema).expect("validate");

        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        {
            let wtxn = db.begin_write().unwrap();
            apply_schema_definitions(&wtxn, &validated, 1, 1_700_000_000_000_000_000).unwrap();
            wtxn.commit().unwrap();
        }

        let rtxn = db.begin_read().unwrap();
        let row = extractor_lookup_by_qname(&rtxn, "acme", "person_mentions")
            .unwrap()
            .expect("row exists");
        assert_eq!(row.namespace, "acme");
        assert_eq!(row.name, "person_mentions");
        assert_eq!(row.kind, brain_core::ExtractorKind::Pattern.as_u8());
        assert!(row.is_enabled());

        // `definition_blob` decodes back to the same ExtractorDef AST.
        let decoded: ExtractorDef = serde_json::from_slice(&row.definition_blob).unwrap();
        assert_eq!(decoded.name, "person_mentions");
        assert!(matches!(
            decoded.target,
            ExtractorTarget::Entity { entity_type } if entity_type == "Person"
        ));
    }

    #[test]
    fn apply_is_idempotent_for_extractors() {
        let src = r#"
            namespace acme
            define entity_type Person { attributes {} }
            define extractor person_mentions {
                kind: pattern
                target: entity Person
                patterns [ /\b([A-Z][a-z]+)\b/ ]
                confidence: 0.7
            }
        "#;
        let schema = parse_schema(src).expect("parse");
        let validated = validate(&schema).expect("validate");

        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);

        let wtxn = db.begin_write().unwrap();
        apply_schema_definitions(&wtxn, &validated, 1, 0).unwrap();
        // Second apply must succeed (idempotent).
        apply_schema_definitions(&wtxn, &validated, 1, 0).unwrap();
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let row = extractor_lookup_by_qname(&rtxn, "acme", "person_mentions")
            .unwrap()
            .unwrap();
        assert_eq!(row.id().raw(), 1, "id stable across idempotent applies");
    }
}
