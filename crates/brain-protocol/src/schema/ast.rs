//! Schema-DSL AST — value-typed.
//!
//! Consumed by the parser, validator, persistence, and client-side
//! schema building. One source of truth for the in-memory shape of a
//! schema document.
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
    /// A user-declared statement kind — the expansion lever. Predicates are
    /// open-vocab (never declared/gated); the schema instead declares the
    /// *shape* of facts (cardinality / temporal / polarity) so the
    /// classifier and the read engine treat them correctly.
    Kind(KindDef),
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
    Enum {
        variants: Vec<String>,
    },
    Ref {
        target: String,
    },
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
    /// Explicit per-predicate supersession flag. When set, a new
    /// statement with the same `(subject, predicate)` tombstones the
    /// prior active one; when unset, observations accumulate. `None`
    /// defers to the kind-derived default (Preference → true; Fact,
    /// Event, Any → false) — declared predicates inherit kind's
    /// natural semantics unless the schema author overrides them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stateful: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl PredicateDef {
    /// Resolve the `stateful` flag against its kind-derived default.
    /// Preference predicates default to stateful (each new preference
    /// supersedes the prior one); Fact and Event default to cumulative.
    /// `Any` is treated as Fact-like — no auto-supersession unless the
    /// author opts in explicitly.
    #[must_use]
    pub fn resolved_stateful(&self) -> bool {
        self.stateful.unwrap_or(match self.kind {
            // Single-valued kinds supersede; set/append kinds accumulate.
            StatementKindAst::Attribute | StatementKindAst::Directive => true,
            StatementKindAst::Fact
            | StatementKindAst::Preference
            | StatementKindAst::Event
            | StatementKindAst::Relation
            | StatementKindAst::Any => false,
        })
    }
}

/// The built-in statement kinds, as named in the DSL. Mirrors
/// `brain_core::StatementKind` (minus `Custom`, which is named via a
/// `kind {}` declaration rather than this enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StatementKindAst {
    Fact,
    Preference,
    Event,
    Attribute,
    Relation,
    Directive,
    Any,
}

// ---------------------------------------------------------------------------
// 3b. User-declared kinds (the expansion lever).
// ---------------------------------------------------------------------------

/// A user-declared statement kind. Carries the behavioral semantics the
/// storage + read layers need (cardinality / temporal / polarity) plus a
/// natural-language `hint` the extractor's kind classifier uses to decide
/// when a fact belongs to this kind.
///
/// ```text
/// kind investment {
///     cardinality: set
///     temporal:    event
///     object:      [entity, quantity]
///     polarity:    false
///     hint: "an entity funded/invested in another, with an amount"
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KindDef {
    /// Local name; the qname is `{schema.namespace}:{name}`.
    pub name: String,
    pub cardinality: KindCardinalityAst,
    pub temporal: TemporalModelAst,
    /// Object kinds this kind accepts (empty = any).
    #[serde(default)]
    pub object: Vec<ObjectKindAst>,
    #[serde(default)]
    pub polarity: bool,
    /// Classifier hint — what facts belong to this kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// How many current values a `(subject, predicate)` pair may hold under a
/// kind. Mirrors `brain_core::KindCardinality`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum KindCardinalityAst {
    Single,
    Set,
}

/// How a kind relates to time. Mirrors `brain_core::TemporalModel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TemporalModelAst {
    State,
    Event,
    None,
}

/// The object shape a kind accepts. Maps onto `StatementObject`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ObjectKindAst {
    Entity,
    Value,
    Time,
    Quantity,
    List,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ObjectTypeDecl {
    Value {
        value_type: AttrType,
    },
    Entity {
        entity_type: String,
    },
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
    /// the source line for diagnostics.
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
    /// Resolver configuration (placeholder; to be filled in).
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

// The `Per` prefix is intrinsic — these are rates ("per memory", "per request",
// "per day") matching the DSL grammar verbatim; renaming to `Memory`/`Request`/
// `Day` would change the meaning (a `Day` is a span, not a rate).
#[allow(clippy::enum_variant_names)]
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

/// Placeholder for resolver config — to be populated later.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ResolverConfig {}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
                    stateful: None,
                    description: None,
                }),
                SchemaItem::Predicate(PredicateDef {
                    name: "prefers".into(),
                    kind: StatementKindAst::Preference,
                    object: ObjectTypeDecl::Value {
                        value_type: AttrType::Text,
                    },
                    stateful: None,
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
}
