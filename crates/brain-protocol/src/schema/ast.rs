//! Schema-DSL AST — value-typed (spec §21/02).
//!
//! Consumed by the parser (§21/01 / phase 19.3), validator
//! (§21/03 / phase 19.4), persistence (§21/05 / phase 19.5), and
//! the SDK `SchemaBuilder` (§29/00 / phase 19.8). One source of
//! truth for the in-memory shape of a schema document.
//!
//! Flat per document — namespaces don't nest. `Schema::namespace`
//! qualifies every predicate / entity / relation declared inside.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// 1. Top level.
// ---------------------------------------------------------------------------

/// A single schema document — one namespace, a flat list of items.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Schema {
    pub namespace: String,
    pub items: Vec<SchemaItem>,
    /// Original DSL source text, when uploaded via the text form of
    /// `SCHEMA_UPLOAD`. Programmatic `SchemaBuilder` uploads omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SchemaItem {
    EntityType(EntityTypeDef),
    Predicate(PredicateDef),
    RelationType(RelationTypeDef),
    Extractor(ExtractorDef),
}

// ---------------------------------------------------------------------------
// 2. Entity types.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct EntityTypeDef {
    pub name: String,
    #[serde(default)]
    pub attributes: Vec<AttributeDecl>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttributeDecl {
    pub name: String,
    pub attr_type: AttrType,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub unique: bool,
    #[serde(default)]
    pub indexed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<LiteralValue>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AttrType {
    Text,
    Number,
    Bool,
    /// YYYY-MM-DD logical date.
    Date,
    /// Unix nanoseconds.
    Timestamp,
    Enum { variants: Vec<String> },
    Ref { target: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LiteralValue {
    Text(String),
    Number(f64),
    Bool(bool),
    Date(String),
    Timestamp(u64),
    Null,
}

// ---------------------------------------------------------------------------
// 3. Predicates.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PredicateDef {
    /// Local name; the qname is `{schema.namespace}:{name}`.
    pub name: String,
    pub kind: StatementKindAst,
    pub object: ObjectTypeDecl,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StatementKindAst {
    Fact,
    Preference,
    Event,
    Any,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ObjectTypeDecl {
    Value { value_type: AttrType },
    Entity { entity_type: String },
    Memory,
    Statement,
    /// Surface form `Any`. Stored as `Value<Text>` at the storage layer.
    Any,
}

// ---------------------------------------------------------------------------
// 4. Relation types.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelationTypeDef {
    pub name: String,
    /// Source entity type name, or the literal `Any` sentinel.
    pub from_type: String,
    /// Target entity type name, or the literal `Any` sentinel.
    pub to_type: String,
    pub cardinality: CardinalityAst,
    #[serde(default)]
    pub symmetric: bool,
    #[serde(default)]
    pub properties: Vec<AttributeDecl>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CardinalityAst {
    OneToOne,
    OneToMany,
    ManyToOne,
    ManyToMany,
}

// ---------------------------------------------------------------------------
// 5. Extractors.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtractorDef {
    pub name: String,
    pub kind: ExtractorKindAst,
    pub target: ExtractorTarget,
    /// Source-order-preserved configuration fields. The validator
    /// rejects duplicates (e.g., two `Model` entries) and reports
    /// the source line for diagnostics (§21/02 §7).
    #[serde(default)]
    pub fields: Vec<ExtractorField>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ExtractorKindAst {
    Pattern,
    Classifier,
    Llm,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ExtractorTarget {
    Entity { entity_type: String },
    Statement { kind: StatementKindAst },
    Relation { relation_type: String },
    EntityOrStatement,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ExtractorField {
    /// Pattern extractor: list of regex patterns.
    Patterns(Vec<String>),
    /// Classifier / LLM extractor: model identifier.
    Model(String),
    /// Classifier extractor: feature-extraction spec.
    FeatureExtraction(String),
    /// LLM extractor: prompt body.
    Prompt(String),
    /// LLM extractor: few-shot examples (free-form JSON).
    Examples(serde_json::Value),
    /// LLM extractor: structured-output JSON schema.
    Schema(serde_json::Value),
    /// LLM extractor: cache on/off.
    Cache(CacheConfig),
    /// LLM extractor: cache TTL.
    CacheTtl(DurationAst),
    /// Pattern extractor: fixed confidence per match.
    Confidence(f32),
    /// Classifier / LLM extractor: minimum confidence to emit.
    ConfidenceThreshold(f32),
    /// When the extractor runs.
    Trigger(TriggerExpr),
    /// LLM extractor: spend budget.
    CostBudget(CostExpr),
    /// Names of upstream extractors this one depends on.
    DependsOn(Vec<String>),
    /// Resolver configuration (placeholder; §22 fills in).
    Resolver(ResolverConfig),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CacheConfig {
    Enabled,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DurationAst {
    pub amount: u64,
    pub unit: DurationUnit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DurationUnit {
    Seconds,
    Minutes,
    Hours,
    Days,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CostExpr {
    pub amount: f64,
    pub unit: CostUnit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CostUnit {
    PerMemory,
    PerRequest,
    PerDay,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TriggerExpr {
    OnEncode,
    OnEncodeWhere(ConditionExpr),
    OnDemand,
    OnSchemaChange,
    Periodic { cron: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ConditionExpr {
    Atom {
        /// Dotted field path, e.g. `["entity", "type"]`.
        field: Vec<String>,
        op: ConditionOp,
        value: ConditionValue,
    },
    Matches {
        field: Vec<String>,
        regex: String,
    },
    And(Box<ConditionExpr>, Box<ConditionExpr>),
    Or(Box<ConditionExpr>, Box<ConditionExpr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConditionOp {
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
    In,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ConditionValue {
    Text(String),
    Number(f64),
    Bool(bool),
    List(Vec<ConditionValue>),
}

/// Placeholder for resolver config — §22 will populate this.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ResolverConfig {}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn schema_defaults_are_empty() {
        let s = Schema::default();
        assert_eq!(s.namespace, "");
        assert!(s.items.is_empty());
        assert!(s.source.is_none());
    }

    #[test]
    fn attribute_decl_defaults_optional() {
        let attr = AttributeDecl {
            name: "email".into(),
            attr_type: AttrType::Text,
            required: false,
            unique: false,
            indexed: false,
            default: None,
        };
        assert!(!attr.required);
        assert!(!attr.unique);
        assert!(!attr.indexed);
        assert!(attr.default.is_none());
    }

    #[test]
    fn attr_type_enum_variants_preserved() {
        let t = AttrType::Enum {
            variants: vec!["red".into(), "green".into(), "blue".into()],
        };
        let json_text = serde_json::to_string(&t).unwrap();
        let back: AttrType = serde_json::from_str(&json_text).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn attr_type_ref_target_preserved() {
        let t = AttrType::Ref {
            target: "Person".into(),
        };
        let back: AttrType = serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn predicate_def_round_trip_json() {
        let p = PredicateDef {
            name: "prefers".into(),
            kind: StatementKindAst::Preference,
            object: ObjectTypeDecl::Value {
                value_type: AttrType::Text,
            },
            description: Some("user preference".into()),
        };
        let back: PredicateDef =
            serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn relation_def_with_properties() {
        let r = RelationTypeDef {
            name: "reports_to".into(),
            from_type: "Person".into(),
            to_type: "Person".into(),
            cardinality: CardinalityAst::ManyToOne,
            symmetric: false,
            properties: vec![AttributeDecl {
                name: "since".into(),
                attr_type: AttrType::Date,
                required: false,
                unique: false,
                indexed: false,
                default: None,
            }],
            description: None,
        };
        let back: RelationTypeDef =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(r, back);
        assert_eq!(back.properties.len(), 1);
    }

    #[test]
    fn extractor_pattern_round_trip() {
        let e = ExtractorDef {
            name: "person_mentions".into(),
            kind: ExtractorKindAst::Pattern,
            target: ExtractorTarget::Entity {
                entity_type: "Person".into(),
            },
            fields: vec![
                ExtractorField::Patterns(vec![r"\b[A-Z][a-z]+\b".into()]),
                ExtractorField::Confidence(0.7),
                ExtractorField::Trigger(TriggerExpr::OnEncode),
            ],
        };
        let back: ExtractorDef =
            serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
        assert_eq!(e, back);
    }

    #[test]
    fn extractor_llm_round_trip() {
        let e = ExtractorDef {
            name: "preferences".into(),
            kind: ExtractorKindAst::Llm,
            target: ExtractorTarget::Statement {
                kind: StatementKindAst::Preference,
            },
            fields: vec![
                ExtractorField::Model("claude-haiku-4-5".into()),
                ExtractorField::Prompt("Extract user preferences.".into()),
                ExtractorField::Schema(json!({"type": "object"})),
                ExtractorField::Cache(CacheConfig::Enabled),
                ExtractorField::CacheTtl(DurationAst {
                    amount: 24,
                    unit: DurationUnit::Hours,
                }),
                ExtractorField::CostBudget(CostExpr {
                    amount: 0.10,
                    unit: CostUnit::PerMemory,
                }),
                ExtractorField::ConfidenceThreshold(0.8),
            ],
        };
        let back: ExtractorDef =
            serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
        assert_eq!(e, back);
    }

    #[test]
    fn condition_expr_nested() {
        let cond = ConditionExpr::And(
            Box::new(ConditionExpr::Atom {
                field: vec!["entity".into(), "type".into()],
                op: ConditionOp::Eq,
                value: ConditionValue::Text("Person".into()),
            }),
            Box::new(ConditionExpr::Or(
                Box::new(ConditionExpr::Matches {
                    field: vec!["text".into()],
                    regex: "(?i)meeting".into(),
                }),
                Box::new(ConditionExpr::Atom {
                    field: vec!["confidence".into()],
                    op: ConditionOp::Gte,
                    value: ConditionValue::Number(0.5),
                }),
            )),
        );
        let back: ConditionExpr =
            serde_json::from_str(&serde_json::to_string(&cond).unwrap()).unwrap();
        assert_eq!(cond, back);
    }

    #[test]
    fn schema_full_document_round_trip() {
        let schema = Schema {
            namespace: "acme".into(),
            source: None,
            items: vec![
                SchemaItem::EntityType(EntityTypeDef {
                    name: "Person".into(),
                    attributes: vec![AttributeDecl {
                        name: "email".into(),
                        attr_type: AttrType::Text,
                        required: true,
                        unique: true,
                        indexed: true,
                        default: None,
                    }],
                }),
                SchemaItem::Predicate(PredicateDef {
                    name: "role".into(),
                    kind: StatementKindAst::Fact,
                    object: ObjectTypeDecl::Value {
                        value_type: AttrType::Text,
                    },
                    description: None,
                }),
                SchemaItem::Predicate(PredicateDef {
                    name: "prefers".into(),
                    kind: StatementKindAst::Preference,
                    object: ObjectTypeDecl::Value {
                        value_type: AttrType::Text,
                    },
                    description: None,
                }),
                SchemaItem::RelationType(RelationTypeDef {
                    name: "reports_to".into(),
                    from_type: "Person".into(),
                    to_type: "Person".into(),
                    cardinality: CardinalityAst::ManyToOne,
                    symmetric: false,
                    properties: vec![],
                    description: None,
                }),
                SchemaItem::Extractor(ExtractorDef {
                    name: "person_mentions".into(),
                    kind: ExtractorKindAst::Pattern,
                    target: ExtractorTarget::Entity {
                        entity_type: "Person".into(),
                    },
                    fields: vec![
                        ExtractorField::Patterns(vec![r"\b[A-Z][a-z]+\b".into()]),
                        ExtractorField::Confidence(0.7),
                    ],
                }),
            ],
        };
        let json_text = serde_json::to_string(&schema).unwrap();
        let back: Schema = serde_json::from_str(&json_text).unwrap();
        assert_eq!(schema, back);
        assert_eq!(back.items.len(), 5);
    }

    #[test]
    fn statement_kind_round_trips_all_variants() {
        for k in [
            StatementKindAst::Fact,
            StatementKindAst::Preference,
            StatementKindAst::Event,
            StatementKindAst::Any,
        ] {
            let back: StatementKindAst =
                serde_json::from_str(&serde_json::to_string(&k).unwrap()).unwrap();
            assert_eq!(k, back);
        }
    }
}
