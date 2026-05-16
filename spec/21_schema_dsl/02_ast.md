# 21.02 Schema AST

Typed AST shape consumed by the parser ([§01](./01_grammar.md)),
validator ([§03](./03_validator.md)), persistence
([§05](./05_versioning.md)), and the SDK
[`SchemaBuilder`](../29_knowledge_sdk/00_purpose.md). One source of
truth for the schema's in-memory shape.

Cross-references:
- [`./01_grammar.md`](./01_grammar.md) — surface syntax.
- [`./03_validator.md`](./03_validator.md) — validation runs over
  this AST.
- [`./05_versioning.md`](./05_versioning.md) — persisted as rkyv-
  archived `SchemaVersionRow`.

## 1. Top-level

```rust
pub struct Schema {
    pub namespace: String,             // single namespace per document.
    pub items: Vec<SchemaItem>,
    pub source: Option<String>,        // original text, if uploaded as DSL.
}

pub enum SchemaItem {
    EntityType(EntityTypeDef),
    Predicate(PredicateDef),
    RelationType(RelationTypeDef),
    Extractor(ExtractorDef),
}
```

The AST is **flat per document** — namespaces don't nest. A
deployment can host multiple schemas under different namespaces;
each is uploaded as its own document.

`source` is the verbatim DSL text when uploaded via `SCHEMA_UPLOAD`
with the text form. Programmatic `SchemaBuilder` uploads omit it.

## 2. Entity types

```rust
pub struct EntityTypeDef {
    pub name: String,                  // "Person"
    pub attributes: Vec<AttributeDecl>,
}

pub struct AttributeDecl {
    pub name: String,
    pub attr_type: AttrType,
    pub required: bool,
    pub unique: bool,
    pub indexed: bool,
    pub default: Option<LiteralValue>,
}

pub enum AttrType {
    Text,
    Number,
    Bool,
    Date,                              // YYYY-MM-DD logical date.
    Timestamp,                         // unix nanos.
    Enum { variants: Vec<String> },
    Ref { target: String },            // entity-type name.
}

pub enum LiteralValue {
    Text(String),
    Number(f64),
    Bool(bool),
    Date(String),
    Timestamp(u64),
    Null,
}
```

## 3. Predicates

```rust
pub struct PredicateDef {
    pub name: String,                  // local name ("prefers")
    pub kind: StatementKindAst,        // Fact / Preference / Event
    pub object: ObjectTypeDecl,
    pub description: Option<String>,
}

pub enum StatementKindAst {
    Fact,
    Preference,
    Event,
    Any,                               // matches §01 grammar
}

pub enum ObjectTypeDecl {
    Value { value_type: AttrType },    // Value<text>
    Entity { entity_type: String },    // Entity<Person>
    Memory,
    Statement,
    Any,                               // Value<text> at storage level
}
```

`name` is the local predicate name; the full qname is
`{schema.namespace}:{name}`. The validator + storage layer never
work with unqualified names — they qualify at parse-completion
time.

## 4. Relation types

```rust
pub struct RelationTypeDef {
    pub name: String,                  // "reports_to"
    pub from_type: String,             // entity-type name
    pub to_type: String,
    pub cardinality: CardinalityAst,
    pub symmetric: bool,
    pub properties: Vec<AttributeDecl>,
    pub description: Option<String>,
}

pub enum CardinalityAst {
    OneToOne,
    OneToMany,
    ManyToOne,
    ManyToMany,
}
```

`from_type` / `to_type` may be the special `Any` (case-sensitive)
to indicate no entity-type constraint — maps to the `0` sentinel
in `RelationTypeDefinition`. The grammar accepts either an
identifier or the literal `Any`.

## 5. Extractors

```rust
pub struct ExtractorDef {
    pub name: String,
    pub kind: ExtractorKindAst,
    pub target: ExtractorTarget,
    pub fields: Vec<ExtractorField>,   // kind-specific configuration
}

pub enum ExtractorKindAst {
    Pattern,
    Classifier,
    Llm,
}

pub enum ExtractorTarget {
    Entity { entity_type: String },
    Statement { kind: StatementKindAst },
    Relation { relation_type: String },
    EntityOrStatement,
}

pub enum ExtractorField {
    Patterns(Vec<String>),                          // pattern kind
    Model(String),                                  // classifier / llm
    FeatureExtraction(String),                      // classifier
    Prompt(String),                                 // llm
    Examples(serde_json::Value),                    // llm
    Schema(serde_json::Value),                      // llm output schema
    Cache(CacheConfig),                             // llm
    CacheTtl(DurationAst),                          // llm
    Confidence(f32),                                // pattern fixed
    ConfidenceThreshold(f32),                       // classifier / llm
    Trigger(TriggerExpr),
    CostBudget(CostExpr),                           // llm
    DependsOn(Vec<String>),                         // extractor names
    Resolver(ResolverConfig),
}

pub enum CacheConfig { Enabled, Disabled }

pub struct DurationAst { pub amount: u64, pub unit: DurationUnit }
pub enum DurationUnit { Seconds, Minutes, Hours, Days }

pub struct CostExpr {
    pub amount: f64,
    pub unit: CostUnit,
}
pub enum CostUnit { PerMemory, PerRequest, PerDay }

pub enum TriggerExpr {
    OnEncode,
    OnEncodeWhere(ConditionExpr),
    OnDemand,
    OnSchemaChange,
    Periodic { cron: String },
}

pub enum ConditionExpr {
    Atom { field: Vec<String>, op: ConditionOp, value: ConditionValue },
    Matches { field: Vec<String>, regex: String },
    And(Box<ConditionExpr>, Box<ConditionExpr>),
    Or(Box<ConditionExpr>, Box<ConditionExpr>),
}

pub enum ConditionOp { Eq, Neq, Lt, Lte, Gt, Gte, In }

pub enum ConditionValue {
    Text(String),
    Number(f64),
    Bool(bool),
    List(Vec<ConditionValue>),
}

pub struct ResolverConfig { ... }      // §22 — extractor specifics.
```

The `ExtractorField` flat-enum shape makes the AST easy to
serialise and validate. The order in which fields appear in the
source DSL is preserved in the `Vec<ExtractorField>`; the validator
normalises (e.g., rejects duplicate `model:` declarations).

## 6. Stability + rkyv

The AST is **value-typed** (no rkyv on brain-core / brain-protocol
side); a separate rkyv shape lives in `brain-metadata` as the
persisted `SchemaVersionRow` (see [§05](./05_versioning.md) §2).

Storage and wire layers don't share the AST verbatim — they each
project to their own representation:

- **Wire** (§28/05): `SchemaUploadRequest.source: String` (DSL
  text) + `Vec<EntityTypeDef>` / `Vec<PredicateDef>` etc. for
  programmatic uploads.
- **Storage**: parsed AST is re-projected into the
  existing `EntityTypeDefinition` / `PredicateDefinition` /
  `RelationTypeDefinition` rows + a `SchemaVersionRow` archive of
  the full document.

This indirection keeps the AST flexible (we can add fields without
breaking on-disk format) and lets the persisted row carry
constraint context the live rows don't (e.g., the source DSL
text).

## 7. Field-order semantics

For `ExtractorField` and `AttributeDecl`, source order matters in
exactly one place: validation diagnostics quote the line a problem
appears on. Storage-side reads + writes don't depend on order.

The SDK `SchemaBuilder` is order-agnostic; the round-trip
`build()` → DSL text → parse may reorder fields (canonical form is
defined in [§05](./05_versioning.md) §6).

## 8. Tests (phase 19.2)

Phase 19.2 lands the types + ~10 unit tests covering:

- Constructor defaults (`AttributeDecl` with all-default modifiers).
- Round-trip serde (Schema → JSON → Schema).
- Discriminant byte stability for `StatementKindAst`,
  `CardinalityAst`, `ExtractorKindAst`.
- `ObjectTypeDecl::Entity { entity_type: "Person" }` resolves to
  the same wire shape as `Entity<Person>`.

## 9. Open questions

See [`./07_open_questions.md`](./07_open_questions.md). Notably:

- Q1 — should `Schema` carry a `parent_version` field for diff
  computation? Deferred per the no-migration scope of v1.
- Q2 — multi-document schemas (one big document vs many small
  documents per namespace).
