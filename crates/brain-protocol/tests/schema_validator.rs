//! Schema validator integration tests (spec §21/03 §5).

use brain_protocol::schema::{
    parse_schema, validate, AttrType, AttributeDecl, CardinalityAst, EntityTypeDef, ExtractorDef,
    ExtractorField, ExtractorKindAst, ExtractorTarget, LiteralValue, ObjectTypeDecl, PredicateDef,
    RelationTypeDef, Schema, SchemaItem, StatementKindAst, ValidationErrorCode,
};

fn person_entity() -> SchemaItem {
    SchemaItem::EntityType(EntityTypeDef {
        name: "Person".into(),
        attributes: vec![],
    })
}

fn s_with_items(items: Vec<SchemaItem>) -> Schema {
    Schema {
        namespace: "acme".into(),
        source: None,
        items,
    }
}

fn assert_has_code(schema: &Schema, code: ValidationErrorCode) {
    let errs = validate(schema).expect_err("expected validation error");
    assert!(
        errs.iter().any(|e| e.code == code),
        "expected {code:?}, got {errs:#?}"
    );
}

#[test]
fn missing_namespace_fails() {
    let mut s = s_with_items(vec![person_entity()]);
    s.namespace.clear();
    assert_has_code(&s, ValidationErrorCode::NamespaceMissing);
}

#[test]
fn reserved_brain_namespace_fails() {
    let mut s = s_with_items(vec![person_entity()]);
    s.namespace = "brain".into();
    assert_has_code(&s, ValidationErrorCode::NamespaceInvalidIdentifier);
}

#[test]
fn duplicate_entity_type_fails() {
    let s = s_with_items(vec![person_entity(), person_entity()]);
    assert_has_code(&s, ValidationErrorCode::DuplicateDefinition);
}

#[test]
fn duplicate_predicate_fails() {
    let p = || {
        SchemaItem::Predicate(PredicateDef {
            name: "prefers".into(),
            kind: StatementKindAst::Preference,
            object: ObjectTypeDecl::Value {
                value_type: AttrType::Text,
            },
            description: None,
        })
    };
    let s = s_with_items(vec![p(), p()]);
    assert_has_code(&s, ValidationErrorCode::DuplicateDefinition);
}

#[test]
fn unresolved_relation_from_type_fails() {
    let rel = SchemaItem::RelationType(RelationTypeDef {
        name: "reports_to".into(),
        from_type: "Persoon".into(), // typo
        to_type: "Person".into(),
        cardinality: CardinalityAst::ManyToOne,
        symmetric: false,
        properties: vec![],
        description: None,
    });
    let s = s_with_items(vec![person_entity(), rel]);
    assert_has_code(&s, ValidationErrorCode::UnresolvedTypeRef);
}

#[test]
fn preference_entity_object_mismatches() {
    let p = SchemaItem::Predicate(PredicateDef {
        name: "prefers".into(),
        kind: StatementKindAst::Preference,
        object: ObjectTypeDecl::Entity {
            entity_type: "Person".into(),
        },
        description: None,
    });
    let s = s_with_items(vec![person_entity(), p]);
    assert_has_code(&s, ValidationErrorCode::PredicateKindObjectMismatch);
}

#[test]
fn event_statement_object_mismatches() {
    let p = SchemaItem::Predicate(PredicateDef {
        name: "scheduled".into(),
        kind: StatementKindAst::Event,
        object: ObjectTypeDecl::Statement,
        description: None,
    });
    let s = s_with_items(vec![p]);
    assert_has_code(&s, ValidationErrorCode::PredicateKindObjectMismatch);
}

#[test]
fn one_to_many_symmetric_invalid() {
    let rel = SchemaItem::RelationType(RelationTypeDef {
        name: "managed_by".into(),
        from_type: "Person".into(),
        to_type: "Person".into(),
        cardinality: CardinalityAst::OneToMany,
        symmetric: true,
        properties: vec![],
        description: None,
    });
    let s = s_with_items(vec![person_entity(), rel]);
    assert_has_code(&s, ValidationErrorCode::RelationCardinalitySymmetricInvalid);
}

#[test]
fn unique_on_ref_invalid() {
    let entity = SchemaItem::EntityType(EntityTypeDef {
        name: "Doc".into(),
        attributes: vec![AttributeDecl {
            name: "owner".into(),
            attr_type: AttrType::Ref {
                target: "Person".into(),
            },
            required: false,
            unique: true,
            indexed: false,
            default: None,
        }],
    });
    let s = s_with_items(vec![person_entity(), entity]);
    assert_has_code(&s, ValidationErrorCode::AttributeUniqueOnRefType);
}

#[test]
fn default_type_mismatch_fails() {
    let entity = SchemaItem::EntityType(EntityTypeDef {
        name: "Doc".into(),
        attributes: vec![AttributeDecl {
            name: "title".into(),
            attr_type: AttrType::Text,
            required: false,
            unique: false,
            indexed: false,
            default: Some(LiteralValue::Number(42.0)),
        }],
    });
    let s = s_with_items(vec![entity]);
    assert_has_code(&s, ValidationErrorCode::DefaultIncompatibleWithType);
}

#[test]
fn pattern_extractor_without_patterns_fails() {
    let ext = SchemaItem::Extractor(ExtractorDef {
        name: "no_patterns".into(),
        kind: ExtractorKindAst::Pattern,
        target: ExtractorTarget::Entity {
            entity_type: "Person".into(),
        },
        fields: vec![ExtractorField::Confidence(0.5)],
    });
    let s = s_with_items(vec![person_entity(), ext]);
    assert_has_code(&s, ValidationErrorCode::ExtractorMissingRequired);
}

#[test]
fn duplicate_extractor_field_fails() {
    let ext = SchemaItem::Extractor(ExtractorDef {
        name: "two_models".into(),
        kind: ExtractorKindAst::Llm,
        target: ExtractorTarget::Statement {
            kind: StatementKindAst::Fact,
        },
        fields: vec![
            ExtractorField::Model("a".into()),
            ExtractorField::Model("b".into()),
            ExtractorField::Prompt("p".into()),
        ],
    });
    let s = s_with_items(vec![ext]);
    assert_has_code(&s, ValidationErrorCode::ExtractorDuplicateField);
}

#[test]
fn confidence_threshold_out_of_range_fails() {
    let ext = SchemaItem::Extractor(ExtractorDef {
        name: "bad_conf".into(),
        kind: ExtractorKindAst::Llm,
        target: ExtractorTarget::Statement {
            kind: StatementKindAst::Fact,
        },
        fields: vec![
            ExtractorField::Model("m".into()),
            ExtractorField::Prompt("p".into()),
            ExtractorField::ConfidenceThreshold(1.5),
        ],
    });
    let s = s_with_items(vec![ext]);
    assert_has_code(&s, ValidationErrorCode::ExtractorInvalidConfig);
}

#[test]
fn any_as_relation_endpoint_resolves() {
    let rel = SchemaItem::RelationType(RelationTypeDef {
        name: "related_to".into(),
        from_type: "Any".into(),
        to_type: "Any".into(),
        cardinality: CardinalityAst::ManyToMany,
        symmetric: true,
        properties: vec![],
        description: None,
    });
    let s = s_with_items(vec![rel]);
    validate(&s).expect("Any endpoints should validate");
}

#[test]
fn full_example_schema_validates() {
    let src = r#"
        namespace acme

        define entity_type Person {
            attributes {
                email: text optional unique
                role:  text optional
            }
        }
        define entity_type Project {
            attributes {
                slug: text required unique
            }
        }
        define predicate prefers {
            kind: Preference
            object: Value<text>
        }
        define predicate role {
            kind: Fact
            object: Value<text>
        }
        define relation_type reports_to {
            from: Person
            to: Person
            cardinality: many-to-one
        }
        define relation_type owns {
            from: Person
            to: Project
            cardinality: many-to-many
        }
        define extractor person_mentions {
            kind: pattern
            target: entity Person
            patterns [ /\b([A-Z][a-z]+)\b/ ]
            confidence: 0.7
        }
    "#;
    let schema = parse_schema(src).expect("parse");
    let validated = validate(&schema).expect("validate");
    assert_eq!(validated.as_schema().namespace, "acme");
}

#[test]
fn classifier_extractor_requires_model() {
    let ext = SchemaItem::Extractor(ExtractorDef {
        name: "no_model".into(),
        kind: ExtractorKindAst::Classifier,
        target: ExtractorTarget::Statement {
            kind: StatementKindAst::Fact,
        },
        fields: vec![ExtractorField::ConfidenceThreshold(0.5)],
    });
    let s = s_with_items(vec![ext]);
    assert_has_code(&s, ValidationErrorCode::ExtractorMissingRequired);
}
