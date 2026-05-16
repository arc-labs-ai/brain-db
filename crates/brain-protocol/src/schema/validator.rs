//! Static schema validator (spec §21/03).
//!
//! Pure function — no I/O, no async. Accumulates all errors before
//! returning. The `ValidatedSchema` newtype proves the schema
//! cleared the validator; the only constructor is [`validate`], so
//! downstream storage code that takes `&ValidatedSchema` cannot be
//! handed a raw `Schema`.
//!
//! Migration-time compatibility checks are out of scope for v1
//! (§21/07 Q3).

use std::collections::HashMap;

use crate::schema::ast::{
    AttrType, AttributeDecl, CardinalityAst, ExtractorDef, ExtractorField, ExtractorKindAst,
    ExtractorTarget, LiteralValue, ObjectTypeDecl, PredicateDef, RelationTypeDef, Schema,
    SchemaItem, StatementKindAst,
};

// ---------------------------------------------------------------------------
// Surface types.
// ---------------------------------------------------------------------------

/// Schema that has cleared the validator. Storage / wire code that
/// accepts only validated schemas takes `&ValidatedSchema`. The only
/// constructor is [`validate`].
#[derive(Debug, Clone)]
pub struct ValidatedSchema(Schema);

impl ValidatedSchema {
    pub fn as_schema(&self) -> &Schema {
        &self.0
    }

    pub fn into_schema(self) -> Schema {
        self.0
    }
}

pub type ValidationErrors = Vec<ValidationError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    pub code: ValidationErrorCode,
    pub message: String,
    pub source_span: Option<SourceSpan>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceSpan {
    pub line: u32,
    pub column: u32,
    pub length: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValidationErrorCode {
    NamespaceMissing,
    NamespaceInvalidIdentifier,
    DuplicateDefinition,
    UnresolvedTypeRef,
    PredicateKindObjectMismatch,
    RelationCardinalitySymmetricInvalid,
    ExtractorMissingRequired,
    ExtractorDuplicateField,
    ExtractorInvalidConfig,
    AttributeUniqueOnRefType,
    DefaultIncompatibleWithType,
    NameInvalidIdentifier,
    NameTooLong,
}

// ---------------------------------------------------------------------------
// Constants.
// ---------------------------------------------------------------------------

const RESERVED_NAMESPACE: &str = "brain";
const NAMESPACE_MAX_LEN: usize = 32;
const ATTRIBUTE_NAME_MAX_LEN: usize = 64;
const ANY_TYPE_LITERAL: &str = "Any";

// ---------------------------------------------------------------------------
// Entry point.
// ---------------------------------------------------------------------------

/// Validate a schema. Returns all errors at once; an `Err` always
/// carries at least one element. Successful validation produces a
/// `ValidatedSchema` carrying the input.
pub fn validate(schema: &Schema) -> Result<ValidatedSchema, ValidationErrors> {
    validate_inner(schema, ValidatorMode::User)
}

/// Validate the **system schema** — same rules as [`validate`]
/// except `namespace = "brain"` is allowed (it's the reserved
/// namespace for the substrate's own built-in types).
///
/// Restricted to the substrate's seed path (`brain-metadata`'s
/// `system_schema` module). User uploads of `namespace brain`
/// must continue to be rejected by [`validate`].
pub fn validate_system_schema(
    schema: &Schema,
) -> Result<ValidatedSchema, ValidationErrors> {
    validate_inner(schema, ValidatorMode::System)
}

#[derive(Clone, Copy)]
enum ValidatorMode {
    User,
    System,
}

fn validate_inner(
    schema: &Schema,
    mode: ValidatorMode,
) -> Result<ValidatedSchema, ValidationErrors> {
    let mut errors: ValidationErrors = Vec::new();

    check_namespace(schema, &mut errors, mode);
    check_duplicates(schema, &mut errors);

    let entity_names = collect_entity_names(schema);
    let relation_names = collect_relation_names(schema);

    for item in &schema.items {
        match item {
            SchemaItem::EntityType(e) => check_entity_attributes(e, &mut errors),
            SchemaItem::Predicate(p) => {
                check_predicate(p, &entity_names, &mut errors);
            }
            SchemaItem::RelationType(r) => {
                check_relation(r, &entity_names, &mut errors);
            }
            SchemaItem::Extractor(x) => {
                check_extractor(x, &entity_names, &relation_names, &mut errors);
            }
        }
    }

    if errors.is_empty() {
        Ok(ValidatedSchema(schema.clone()))
    } else {
        Err(errors)
    }
}

// ---------------------------------------------------------------------------
// §2.1 Namespace.
// ---------------------------------------------------------------------------

fn check_namespace(schema: &Schema, errors: &mut ValidationErrors, mode: ValidatorMode) {
    if schema.namespace.is_empty() {
        errors.push(ValidationError {
            code: ValidationErrorCode::NamespaceMissing,
            message: "schema must declare a `namespace`".into(),
            source_span: None,
        });
        return;
    }
    if schema.namespace == RESERVED_NAMESPACE && matches!(mode, ValidatorMode::User) {
        errors.push(ValidationError {
            code: ValidationErrorCode::NamespaceInvalidIdentifier,
            message: format!(
                "namespace {:?} is reserved for the system schema",
                schema.namespace
            ),
            source_span: None,
        });
        return;
    }
    if !is_lower_snake_ident(&schema.namespace) || schema.namespace.len() > NAMESPACE_MAX_LEN {
        errors.push(ValidationError {
            code: ValidationErrorCode::NamespaceInvalidIdentifier,
            message: format!(
                "namespace {:?} must match `[a-z][a-z0-9_]*` and be ≤{NAMESPACE_MAX_LEN} chars",
                schema.namespace
            ),
            source_span: None,
        });
    }
}

fn is_lower_snake_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

// ---------------------------------------------------------------------------
// §2.2 Duplicate definitions.
// ---------------------------------------------------------------------------

fn check_duplicates(schema: &Schema, errors: &mut ValidationErrors) {
    let mut entities: HashMap<&str, usize> = HashMap::new();
    let mut predicates: HashMap<&str, usize> = HashMap::new();
    let mut relations: HashMap<&str, usize> = HashMap::new();
    let mut extractors: HashMap<&str, usize> = HashMap::new();

    for item in &schema.items {
        match item {
            SchemaItem::EntityType(e) => {
                if let Some(_prev) = entities.insert(e.name.as_str(), 0) {
                    errors.push(ValidationError {
                        code: ValidationErrorCode::DuplicateDefinition,
                        message: format!("duplicate entity_type {:?}", e.name),
                        source_span: None,
                    });
                }
            }
            SchemaItem::Predicate(p) => {
                if predicates.insert(p.name.as_str(), 0).is_some() {
                    errors.push(ValidationError {
                        code: ValidationErrorCode::DuplicateDefinition,
                        message: format!("duplicate predicate {:?}", p.name),
                        source_span: None,
                    });
                }
            }
            SchemaItem::RelationType(r) => {
                if relations.insert(r.name.as_str(), 0).is_some() {
                    errors.push(ValidationError {
                        code: ValidationErrorCode::DuplicateDefinition,
                        message: format!("duplicate relation_type {:?}", r.name),
                        source_span: None,
                    });
                }
            }
            SchemaItem::Extractor(x) => {
                if extractors.insert(x.name.as_str(), 0).is_some() {
                    errors.push(ValidationError {
                        code: ValidationErrorCode::DuplicateDefinition,
                        message: format!("duplicate extractor {:?}", x.name),
                        source_span: None,
                    });
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// §2.3 helpers.
// ---------------------------------------------------------------------------

fn collect_entity_names(schema: &Schema) -> Vec<&str> {
    schema
        .items
        .iter()
        .filter_map(|i| match i {
            SchemaItem::EntityType(e) => Some(e.name.as_str()),
            _ => None,
        })
        .collect()
}

fn collect_relation_names(schema: &Schema) -> Vec<&str> {
    schema
        .items
        .iter()
        .filter_map(|i| match i {
            SchemaItem::RelationType(r) => Some(r.name.as_str()),
            _ => None,
        })
        .collect()
}

fn resolves_to_entity(name: &str, entity_names: &[&str]) -> bool {
    name == ANY_TYPE_LITERAL || entity_names.iter().any(|e| *e == name)
}

// ---------------------------------------------------------------------------
// §2.6 Entity attributes.
// ---------------------------------------------------------------------------

fn check_entity_attributes(entity: &crate::schema::ast::EntityTypeDef, errors: &mut ValidationErrors) {
    for attr in &entity.attributes {
        check_attribute_decl(attr, &entity.name, errors);
    }
}

fn check_attribute_decl(
    attr: &AttributeDecl,
    owner_label: &str,
    errors: &mut ValidationErrors,
) {
    // Name.
    if !is_lower_snake_ident(&attr.name) {
        errors.push(ValidationError {
            code: ValidationErrorCode::NameInvalidIdentifier,
            message: format!(
                "{owner_label}.{}: attribute name must match `[a-z][a-z0-9_]*`",
                attr.name
            ),
            source_span: None,
        });
    } else if attr.name.len() > ATTRIBUTE_NAME_MAX_LEN {
        errors.push(ValidationError {
            code: ValidationErrorCode::NameTooLong,
            message: format!(
                "{owner_label}.{}: attribute name exceeds {ATTRIBUTE_NAME_MAX_LEN} chars",
                attr.name
            ),
            source_span: None,
        });
    }

    // `unique` not allowed on Ref<...>.
    if attr.unique && matches!(attr.attr_type, AttrType::Ref { .. }) {
        errors.push(ValidationError {
            code: ValidationErrorCode::AttributeUniqueOnRefType,
            message: format!(
                "{owner_label}.{}: `unique` is not allowed on `ref<>` attributes — use a relation type instead",
                attr.name
            ),
            source_span: None,
        });
    }

    // `default` literal must match `attr_type`.
    if let Some(default) = &attr.default {
        if !default_matches_attr_type(default, &attr.attr_type) {
            errors.push(ValidationError {
                code: ValidationErrorCode::DefaultIncompatibleWithType,
                message: format!(
                    "{owner_label}.{}: default value {:?} is incompatible with attribute type",
                    attr.name, default
                ),
                source_span: None,
            });
        }
    }
}

fn default_matches_attr_type(default: &LiteralValue, attr: &AttrType) -> bool {
    match (default, attr) {
        (LiteralValue::Null, _) => true,
        (LiteralValue::Text(_), AttrType::Text) => true,
        (LiteralValue::Text(_), AttrType::Date) => true, // ISO date as text.
        (LiteralValue::Text(s), AttrType::Enum { variants }) => variants.iter().any(|v| v == s),
        (LiteralValue::Text(_), AttrType::Ref { .. }) => true,
        (LiteralValue::Number(_), AttrType::Number) => true,
        (LiteralValue::Number(_), AttrType::Timestamp) => true,
        (LiteralValue::Bool(_), AttrType::Bool) => true,
        (LiteralValue::Date(_), AttrType::Date) => true,
        (LiteralValue::Date(_), AttrType::Text) => true,
        (LiteralValue::Timestamp(_), AttrType::Timestamp) => true,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// §2.3 + §2.4 Predicate.
// ---------------------------------------------------------------------------

fn check_predicate(
    pred: &PredicateDef,
    entity_names: &[&str],
    errors: &mut ValidationErrors,
) {
    // Type ref resolution for Entity<...>.
    if let ObjectTypeDecl::Entity { entity_type } = &pred.object {
        if !resolves_to_entity(entity_type, entity_names) {
            errors.push(ValidationError {
                code: ValidationErrorCode::UnresolvedTypeRef,
                message: format!(
                    "predicate {:?}: object Entity<{:?}> is not a declared entity_type",
                    pred.name, entity_type
                ),
                source_span: None,
            });
        }
    }

    // Kind/object compatibility.
    if !predicate_kind_object_compatible(pred.kind, &pred.object) {
        errors.push(ValidationError {
            code: ValidationErrorCode::PredicateKindObjectMismatch,
            message: format!(
                "predicate {:?}: kind {:?} is incompatible with object {:?}",
                pred.name, pred.kind, pred.object
            ),
            source_span: None,
        });
    }
}

fn predicate_kind_object_compatible(kind: StatementKindAst, object: &ObjectTypeDecl) -> bool {
    match kind {
        StatementKindAst::Fact | StatementKindAst::Any => true,
        StatementKindAst::Preference => matches!(
            object,
            ObjectTypeDecl::Value { .. } | ObjectTypeDecl::Any
        ),
        StatementKindAst::Event => matches!(
            object,
            ObjectTypeDecl::Value { .. } | ObjectTypeDecl::Entity { .. } | ObjectTypeDecl::Any
        ),
    }
}

// ---------------------------------------------------------------------------
// §2.3 + §2.5 + §2.6 Relation.
// ---------------------------------------------------------------------------

fn check_relation(
    rel: &RelationTypeDef,
    entity_names: &[&str],
    errors: &mut ValidationErrors,
) {
    if !resolves_to_entity(&rel.from_type, entity_names) {
        errors.push(ValidationError {
            code: ValidationErrorCode::UnresolvedTypeRef,
            message: format!(
                "relation_type {:?}: from_type {:?} is not a declared entity_type",
                rel.name, rel.from_type
            ),
            source_span: None,
        });
    }
    if !resolves_to_entity(&rel.to_type, entity_names) {
        errors.push(ValidationError {
            code: ValidationErrorCode::UnresolvedTypeRef,
            message: format!(
                "relation_type {:?}: to_type {:?} is not a declared entity_type",
                rel.name, rel.to_type
            ),
            source_span: None,
        });
    }
    if rel.symmetric
        && !matches!(
            rel.cardinality,
            CardinalityAst::OneToOne | CardinalityAst::ManyToMany
        )
    {
        errors.push(ValidationError {
            code: ValidationErrorCode::RelationCardinalitySymmetricInvalid,
            message: format!(
                "relation_type {:?}: `symmetric: true` is only valid with one-to-one or many-to-many",
                rel.name
            ),
            source_span: None,
        });
    }
    for prop in &rel.properties {
        check_attribute_decl(prop, &rel.name, errors);
    }
}

// ---------------------------------------------------------------------------
// §2.7 Extractor.
// ---------------------------------------------------------------------------

fn check_extractor(
    ext: &ExtractorDef,
    entity_names: &[&str],
    relation_names: &[&str],
    errors: &mut ValidationErrors,
) {
    // Target ref resolution.
    match &ext.target {
        ExtractorTarget::Entity { entity_type } => {
            if !resolves_to_entity(entity_type, entity_names) {
                errors.push(ValidationError {
                    code: ValidationErrorCode::UnresolvedTypeRef,
                    message: format!(
                        "extractor {:?}: target entity {:?} is not a declared entity_type",
                        ext.name, entity_type
                    ),
                    source_span: None,
                });
            }
        }
        ExtractorTarget::Relation { relation_type } => {
            if !relation_names.iter().any(|n| *n == relation_type) {
                errors.push(ValidationError {
                    code: ValidationErrorCode::UnresolvedTypeRef,
                    message: format!(
                        "extractor {:?}: target relation {:?} is not a declared relation_type",
                        ext.name, relation_type
                    ),
                    source_span: None,
                });
            }
        }
        ExtractorTarget::Statement { .. } | ExtractorTarget::EntityOrStatement => {}
    }

    // Duplicate field detection.
    let mut counts: HashMap<u8, usize> = HashMap::new();
    for f in &ext.fields {
        *counts.entry(field_discriminant(f)).or_insert(0) += 1;
    }
    for (&disc, &n) in &counts {
        if n > 1 {
            errors.push(ValidationError {
                code: ValidationErrorCode::ExtractorDuplicateField,
                message: format!(
                    "extractor {:?}: field {} appears {} times (max once)",
                    ext.name,
                    discriminant_name(disc),
                    n
                ),
                source_span: None,
            });
        }
    }

    // Required fields per kind.
    let has_patterns = ext
        .fields
        .iter()
        .any(|f| matches!(f, ExtractorField::Patterns(p) if !p.is_empty()));
    let has_model = ext
        .fields
        .iter()
        .any(|f| matches!(f, ExtractorField::Model(_)));
    let has_prompt = ext
        .fields
        .iter()
        .any(|f| matches!(f, ExtractorField::Prompt(_)));

    match ext.kind {
        ExtractorKindAst::Pattern => {
            if !has_patterns {
                errors.push(ValidationError {
                    code: ValidationErrorCode::ExtractorMissingRequired,
                    message: format!(
                        "extractor {:?}: `pattern` extractor requires a non-empty `patterns:` field",
                        ext.name
                    ),
                    source_span: None,
                });
            }
        }
        ExtractorKindAst::Classifier => {
            if !has_model {
                errors.push(ValidationError {
                    code: ValidationErrorCode::ExtractorMissingRequired,
                    message: format!(
                        "extractor {:?}: `classifier` extractor requires a `model:` field",
                        ext.name
                    ),
                    source_span: None,
                });
            }
        }
        ExtractorKindAst::Llm => {
            if !has_model {
                errors.push(ValidationError {
                    code: ValidationErrorCode::ExtractorMissingRequired,
                    message: format!(
                        "extractor {:?}: `llm` extractor requires a `model:` field",
                        ext.name
                    ),
                    source_span: None,
                });
            }
            if !has_prompt {
                errors.push(ValidationError {
                    code: ValidationErrorCode::ExtractorMissingRequired,
                    message: format!(
                        "extractor {:?}: `llm` extractor requires a `prompt:` field",
                        ext.name
                    ),
                    source_span: None,
                });
            }
        }
    }

    // Confidence ranges.
    for f in &ext.fields {
        match f {
            ExtractorField::Confidence(c) => {
                if !(0.0..=1.0).contains(c) {
                    errors.push(ValidationError {
                        code: ValidationErrorCode::ExtractorInvalidConfig,
                        message: format!(
                            "extractor {:?}: confidence {} not in [0, 1]",
                            ext.name, c
                        ),
                        source_span: None,
                    });
                }
            }
            ExtractorField::ConfidenceThreshold(c) => {
                if !(0.0..=1.0).contains(c) {
                    errors.push(ValidationError {
                        code: ValidationErrorCode::ExtractorInvalidConfig,
                        message: format!(
                            "extractor {:?}: confidence_threshold {} not in [0, 1]",
                            ext.name, c
                        ),
                        source_span: None,
                    });
                }
            }
            _ => {}
        }
    }
}

fn field_discriminant(f: &ExtractorField) -> u8 {
    match f {
        ExtractorField::Patterns(_) => 0,
        ExtractorField::Model(_) => 1,
        ExtractorField::FeatureExtraction(_) => 2,
        ExtractorField::Prompt(_) => 3,
        ExtractorField::Examples(_) => 4,
        ExtractorField::Schema(_) => 5,
        ExtractorField::Cache(_) => 6,
        ExtractorField::CacheTtl(_) => 7,
        ExtractorField::Confidence(_) => 8,
        ExtractorField::ConfidenceThreshold(_) => 9,
        ExtractorField::Trigger(_) => 10,
        ExtractorField::CostBudget(_) => 11,
        ExtractorField::DependsOn(_) => 12,
        ExtractorField::Resolver(_) => 13,
    }
}

fn discriminant_name(d: u8) -> &'static str {
    match d {
        0 => "patterns",
        1 => "model",
        2 => "feature_extraction",
        3 => "prompt",
        4 => "examples",
        5 => "schema",
        6 => "cache",
        7 => "cache_ttl",
        8 => "confidence",
        9 => "confidence_threshold",
        10 => "trigger",
        11 => "cost_budget",
        12 => "depends_on",
        13 => "resolver",
        _ => "unknown",
    }
}

// ---------------------------------------------------------------------------
// Unit tests (lower-level — full happy-path lives in tests/).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::ast::*;

    fn base_schema() -> Schema {
        Schema {
            namespace: "acme".into(),
            source: None,
            items: vec![SchemaItem::EntityType(EntityTypeDef {
                name: "Person".into(),
                attributes: vec![],
            })],
        }
    }

    #[test]
    fn empty_namespace_fails() {
        let mut s = base_schema();
        s.namespace.clear();
        let errs = validate(&s).unwrap_err();
        assert!(errs
            .iter()
            .any(|e| e.code == ValidationErrorCode::NamespaceMissing));
    }

    #[test]
    fn reserved_brain_namespace_rejected() {
        let mut s = base_schema();
        s.namespace = "brain".into();
        let errs = validate(&s).unwrap_err();
        assert!(errs
            .iter()
            .any(|e| e.code == ValidationErrorCode::NamespaceInvalidIdentifier));
    }

    #[test]
    fn uppercase_namespace_rejected() {
        let mut s = base_schema();
        s.namespace = "ACME".into();
        let errs = validate(&s).unwrap_err();
        assert!(errs
            .iter()
            .any(|e| e.code == ValidationErrorCode::NamespaceInvalidIdentifier));
    }

    #[test]
    fn predicate_kind_object_compatible_matrix() {
        for (kind, obj, ok) in &[
            (
                StatementKindAst::Fact,
                ObjectTypeDecl::Memory,
                true,
            ),
            (
                StatementKindAst::Preference,
                ObjectTypeDecl::Memory,
                false,
            ),
            (
                StatementKindAst::Preference,
                ObjectTypeDecl::Value {
                    value_type: AttrType::Text,
                },
                true,
            ),
            (
                StatementKindAst::Event,
                ObjectTypeDecl::Statement,
                false,
            ),
            (
                StatementKindAst::Event,
                ObjectTypeDecl::Entity {
                    entity_type: "Person".into(),
                },
                true,
            ),
        ] {
            assert_eq!(
                predicate_kind_object_compatible(*kind, obj),
                *ok,
                "matrix entry kind={kind:?} obj={obj:?}"
            );
        }
    }

    #[test]
    fn default_matches_attr_type_table() {
        assert!(default_matches_attr_type(
            &LiteralValue::Text("x".into()),
            &AttrType::Text
        ));
        assert!(!default_matches_attr_type(
            &LiteralValue::Number(1.0),
            &AttrType::Text
        ));
        assert!(default_matches_attr_type(
            &LiteralValue::Bool(true),
            &AttrType::Bool
        ));
        assert!(default_matches_attr_type(
            &LiteralValue::Text("red".into()),
            &AttrType::Enum {
                variants: vec!["red".into(), "blue".into()]
            }
        ));
        assert!(!default_matches_attr_type(
            &LiteralValue::Text("yellow".into()),
            &AttrType::Enum {
                variants: vec!["red".into(), "blue".into()]
            }
        ));
    }
}
