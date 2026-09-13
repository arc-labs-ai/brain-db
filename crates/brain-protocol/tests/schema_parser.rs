//! Schema-DSL parser integration tests.
//!
//! Exercises the parser end-to-end against schema-document fixtures.
//! Unit tests in `crates/brain-protocol/src/schema/parser.rs` cover
//! small grammar pieces; this file covers full schema documents.

use brain_protocol::schema::{
    parse_schema, AttrType, CardinalityAst, ExtractorField, ExtractorKindAst, ExtractorTarget,
    LiteralValue, ObjectTypeDecl, ParseError, SchemaItem, StatementKindAst, TriggerExpr,
};

const FULL_SCHEMA: &str = r#"
# Schema for a CRM-like cognitive substrate
# Version 3, 2026-05-13

namespace acme

# ─── Entity types ─────────────────────────────────────────────

define entity_type Person {
    attributes {
        email:       text optional unique
        role:        text optional
        team:        text optional
        timezone:    text optional
    }
}

define entity_type Project {
    attributes {
        slug:        text required unique
        repo_url:    text optional
        active:      bool default true
    }
}

# ─── Predicates ───────────────────────────────────────────────

define predicate prefers {
    kind: Preference
    object: Value<text>
}

define predicate role {
    kind: Fact
    object: Value<text>
}

define predicate scheduled {
    kind: Event
    object: Value<text>
}

# ─── Relations ────────────────────────────────────────────────

define relation_type reports_to {
    from: Person
    to: Person
    cardinality: many-to-one
}

define relation_type owns {
    from: Person
    to: Project
    cardinality: many-to-many
    properties {
        since: date optional
    }
}

# ─── Extractors ───────────────────────────────────────────────

define extractor person_mentions {
    kind: pattern
    target: entity Person
    patterns [
        /\b([A-Z][a-z]+(?:\s+[A-Z][a-z]+){1,2})\b/
    ]
    confidence: 0.7
}

define extractor preferences {
    kind: llm
    target: statement Preference
    trigger: on encode where memory.kind = episodic
    prompt: """
        Extract any preferences stated about a person.
    """
    examples: [{"input": "Priya likes async meetings"}]
    schema: {"type": "array"}
    model: "claude-haiku-4-5"
    confidence_threshold: 0.7
    cache: enabled
}

define extractor reporting_lines {
    kind: classifier
    target: relation reports_to
    trigger: on encode where memory.text matches /.*report.*to.*/
    model: "brain-reporting-line-classifier-v3"
    confidence_threshold: 0.8
}
"#;

#[test]
fn full_schema_parses() {
    let schema = parse_schema(FULL_SCHEMA).expect("full schema parses");
    assert_eq!(schema.namespace, "acme");
    // 2 entity types + 3 predicates + 2 relation types + 3 extractors.
    assert_eq!(schema.items.len(), 10);
    assert!(schema.source.is_some());
}

#[test]
fn entity_type_attributes() {
    let s = parse_schema(FULL_SCHEMA).unwrap();
    let SchemaItem::EntityType(person) = &s.items[0] else {
        panic!("expected first item to be Person entity type")
    };
    assert_eq!(person.name, "Person");
    assert_eq!(person.attributes.len(), 4);
    assert_eq!(person.attributes[0].name, "email");
    assert!(person.attributes[0].unique);
    assert!(!person.attributes[0].required);

    let SchemaItem::EntityType(project) = &s.items[1] else {
        panic!("expected second item to be Project entity type")
    };
    assert_eq!(project.attributes[0].name, "slug");
    assert!(project.attributes[0].required);
    assert!(project.attributes[0].unique);
    assert_eq!(
        project.attributes[2].default,
        Some(LiteralValue::Bool(true))
    );
}

#[test]
fn predicates_cover_all_kinds() {
    let s = parse_schema(FULL_SCHEMA).unwrap();
    let preds: Vec<_> = s
        .items
        .iter()
        .filter_map(|i| match i {
            SchemaItem::Predicate(p) => Some(p),
            _ => None,
        })
        .collect();
    assert_eq!(preds.len(), 3);
    let kinds: Vec<_> = preds.iter().map(|p| p.kind).collect();
    assert!(kinds.contains(&StatementKindAst::Preference));
    assert!(kinds.contains(&StatementKindAst::Fact));
    assert!(kinds.contains(&StatementKindAst::Event));
    for p in &preds {
        assert_eq!(
            p.object,
            ObjectTypeDecl::Value {
                value_type: AttrType::Text
            }
        );
    }
}

#[test]
fn relations_with_properties_and_cardinality() {
    let s = parse_schema(FULL_SCHEMA).unwrap();
    let rels: Vec<_> = s
        .items
        .iter()
        .filter_map(|i| match i {
            SchemaItem::RelationType(r) => Some(r),
            _ => None,
        })
        .collect();
    assert_eq!(rels.len(), 2);
    let reports_to = rels.iter().find(|r| r.name == "reports_to").unwrap();
    assert_eq!(reports_to.cardinality, CardinalityAst::ManyToOne);
    assert!(reports_to.properties.is_empty());
    let owns = rels.iter().find(|r| r.name == "owns").unwrap();
    assert_eq!(owns.cardinality, CardinalityAst::ManyToMany);
    assert_eq!(owns.properties.len(), 1);
    assert_eq!(owns.properties[0].name, "since");
    assert_eq!(owns.properties[0].attr_type, AttrType::Date);
}

#[test]
fn pattern_extractor_carries_regex_and_confidence() {
    let s = parse_schema(FULL_SCHEMA).unwrap();
    let ext = s
        .items
        .iter()
        .find_map(|i| match i {
            SchemaItem::Extractor(e) if e.name == "person_mentions" => Some(e),
            _ => None,
        })
        .unwrap();
    assert_eq!(ext.kind, ExtractorKindAst::Pattern);
    assert!(matches!(
        ext.target,
        ExtractorTarget::Entity { ref entity_type } if entity_type == "Person"
    ));
    let patterns = ext
        .fields
        .iter()
        .find_map(|f| match f {
            ExtractorField::Patterns(p) => Some(p),
            _ => None,
        })
        .unwrap();
    assert_eq!(patterns.len(), 1);
    assert!(patterns[0].starts_with(r"\b"));
    let conf = ext
        .fields
        .iter()
        .find_map(|f| match f {
            ExtractorField::Confidence(c) => Some(*c),
            _ => None,
        })
        .unwrap();
    assert!((conf - 0.7).abs() < 1e-6);
}

#[test]
fn llm_extractor_carries_heredoc_and_json() {
    let s = parse_schema(FULL_SCHEMA).unwrap();
    let ext = s
        .items
        .iter()
        .find_map(|i| match i {
            SchemaItem::Extractor(e) if e.name == "preferences" => Some(e),
            _ => None,
        })
        .unwrap();
    assert_eq!(ext.kind, ExtractorKindAst::Llm);
    assert!(matches!(
        ext.target,
        ExtractorTarget::Statement {
            kind: StatementKindAst::Preference
        }
    ));
    let prompt = ext
        .fields
        .iter()
        .find_map(|f| match f {
            ExtractorField::Prompt(p) => Some(p),
            _ => None,
        })
        .unwrap();
    assert!(prompt.contains("Extract any preferences stated about a person."));

    let examples = ext
        .fields
        .iter()
        .find_map(|f| match f {
            ExtractorField::Examples(v) => Some(v),
            _ => None,
        })
        .unwrap();
    assert!(examples.is_array());

    let schema = ext
        .fields
        .iter()
        .find_map(|f| match f {
            ExtractorField::Schema(v) => Some(v),
            _ => None,
        })
        .unwrap();
    assert_eq!(schema["type"].as_str(), Some("array"));

    let trigger = ext
        .fields
        .iter()
        .find_map(|f| match f {
            ExtractorField::Trigger(t) => Some(t),
            _ => None,
        })
        .unwrap();
    assert!(matches!(trigger, TriggerExpr::OnEncodeWhere(_)));
}

#[test]
fn classifier_extractor_with_matches_trigger() {
    let s = parse_schema(FULL_SCHEMA).unwrap();
    let ext = s
        .items
        .iter()
        .find_map(|i| match i {
            SchemaItem::Extractor(e) if e.name == "reporting_lines" => Some(e),
            _ => None,
        })
        .unwrap();
    assert_eq!(ext.kind, ExtractorKindAst::Classifier);
    assert!(matches!(
        ext.target,
        ExtractorTarget::Relation { ref relation_type } if relation_type == "reports_to"
    ));
    let trigger = ext
        .fields
        .iter()
        .find_map(|f| match f {
            ExtractorField::Trigger(t) => Some(t),
            _ => None,
        })
        .unwrap();
    let TriggerExpr::OnEncodeWhere(_expr) = trigger else {
        panic!("expected OnEncodeWhere trigger");
    };
}

#[test]
fn crlf_line_endings_parse() {
    let src = "namespace acme\r\ndefine entity_type Person {\r\n}\r\n";
    let s = parse_schema(src).expect("CRLF parses");
    assert_eq!(s.namespace, "acme");
    assert_eq!(s.items.len(), 1);
}

#[test]
fn trailing_commas_in_lists_ok() {
    let src = r#"
        namespace t
        define entity_type X {
            attributes {
                kind: enum [red, green, blue,]
            }
        }
    "#;
    let s = parse_schema(src).expect("trailing commas accepted");
    let SchemaItem::EntityType(e) = &s.items[0] else {
        panic!("entity_type expected")
    };
    let AttrType::Enum { variants } = &e.attributes[0].attr_type else {
        panic!("enum expected");
    };
    assert_eq!(variants, &vec!["red", "green", "blue"]);
}

#[test]
fn syntax_error_reports_position() {
    let src = "namespace acme\ndefine entity_type {";
    let err = parse_schema(src).unwrap_err();
    match err {
        ParseError::Syntax { line, col, .. } => {
            assert!(line >= 2, "expected line>=2 got {line}");
            assert!(col >= 1);
        }
        other => panic!("expected Syntax error, got {other:?}"),
    }
}
