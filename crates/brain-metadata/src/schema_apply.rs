//! Fan-out from a `ValidatedSchema` into the existing
//! entity_type / predicate / relation_type intern paths
//! (spec §21/05 §1, phase 19.7).
//!
//! Called by [`crate::schema_store::schema_upload`] after the
//! schema-version row is written. The single code path used both
//! by the system-schema bootstrap and by every user `SCHEMA_UPLOAD`.

use brain_core::knowledge::StatementKind;
use brain_core::{Cardinality, EntityTypeId, ExtractorKind};
use brain_protocol::schema::{
    CardinalityAst, ExtractorKindAst, ObjectTypeDecl, SchemaItem, StatementKindAst,
    ValidatedSchema,
};
use redb::WriteTransaction;

use crate::entity_type_ops::{entity_type_intern, entity_type_lookup_by_name, EntityTypeOpError};
use crate::extractor_ops::{extractor_intern, ExtractorOpError};
use crate::predicate_ops::{predicate_intern, PredicateOpError};
use crate::relation_type_ops::{relation_type_intern, RelationTypeOpError};

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
    #[error("extractor encode: {0}")]
    ExtractorEncode(String),
}

/// Walk `validated.items` in source order and intern each
/// definition. Extractors are skipped (phase 20+).
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
                // `schema_blob` left empty in 19.7 — phase 19+
                // typed accessors will own the encoding.
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
        }
    }
    Ok(())
}

fn map_statement_kind(k: StatementKindAst) -> Option<StatementKind> {
    match k {
        StatementKindAst::Fact => Some(StatementKind::Fact),
        StatementKindAst::Preference => Some(StatementKind::Preference),
        StatementKindAst::Event => Some(StatementKind::Event),
        StatementKindAst::Any => None,
    }
}

/// Mirror of the 17.3 byte encoding: `0` any / `1` Entity /
/// `2` Value / `3` Memory / `4` Statement.
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

fn map_extractor_kind(k: ExtractorKindAst) -> ExtractorKind {
    match k {
        ExtractorKindAst::Pattern => ExtractorKind::Pattern,
        ExtractorKindAst::Classifier => ExtractorKind::Classifier,
        ExtractorKindAst::Llm => ExtractorKind::Llm,
    }
}

/// `"Any"` → `None`; otherwise looks up the entity type by name.
/// In 19.7 the only relation-type targets are `"Any"` so missing
/// lookups fall through as `None` (preserves the pre-19 "no
/// constraint" semantics for unknown / Any).
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
    use crate::extractor_ops::extractor_lookup_by_qname;
    use brain_protocol::schema::{
        parse_schema, validate, ExtractorDef, ExtractorTarget,
    };
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
