# 19.2 — Schema AST in brain-protocol

Pure value-typed AST per `spec/21_schema_dsl/02_ast.md`. Lives in
`brain-protocol` (not `brain-core`) because the schema document is
fundamentally a wire concern: the parser (`19.3`), validator
(`19.4`), wire request types (`19.6`), and the SDK schema builder
(`19.8`) all sit in or above `brain-protocol`. Storage projects the
AST into existing `EntityTypeDefinition` / `PredicateDefinition` /
`RelationTypeDefinition` rkyv rows in `brain-metadata`.

## Crate / dep changes

- Folder: `crates/brain-protocol/src/schema/`
- New deps on `brain-protocol`:
  - `serde` (workspace, `features = ["derive"]`) — JSON round-trip + SDK ergonomics.
  - `serde_json` (workspace) — used by `ExtractorField::Examples(serde_json::Value)` per §21/02 §5.

Per memory feedback `feedback_src_folder_layout` — concerns go in
their own folder under `src/`. `schema/` houses `ast.rs` and a
`mod.rs` re-exporting the AST surface. Subsequent sub-tasks add
`parser.rs`, `validator.rs`, etc. alongside.

## Files written

| Path | Purpose |
|---|---|
| `crates/brain-protocol/src/schema/mod.rs` | Re-exports of the AST surface. |
| `crates/brain-protocol/src/schema/ast.rs` | AST types per §21/02 §1–§7. |
| `crates/brain-protocol/Cargo.toml` | Add `serde`, `serde_json`. |
| `crates/brain-protocol/src/lib.rs` | Add `pub mod schema;`. |
| `Cargo.toml` (root) | (verify only — workspace already exposes `serde` / `serde_json`). |

## AST surface (exactly the spec)

Top-level:
- `Schema { namespace, items, source }`
- `SchemaItem { EntityType, Predicate, RelationType, Extractor }`

Entity types:
- `EntityTypeDef { name, attributes }`
- `AttributeDecl { name, attr_type, required, unique, indexed, default }`
- `AttrType { Text, Number, Bool, Date, Timestamp, Enum{variants}, Ref{target} }`
- `LiteralValue { Text, Number, Bool, Date, Timestamp, Null }`

Predicates:
- `PredicateDef { name, kind, object, description }`
- `StatementKindAst { Fact, Preference, Event, Any }`
- `ObjectTypeDecl { Value{value_type}, Entity{entity_type}, Memory, Statement, Any }`

Relations:
- `RelationTypeDef { name, from_type, to_type, cardinality, symmetric, properties, description }`
- `CardinalityAst { OneToOne, OneToMany, ManyToOne, ManyToMany }`

Extractors:
- `ExtractorDef { name, kind, target, fields }`
- `ExtractorKindAst { Pattern, Classifier, Llm }`
- `ExtractorTarget { Entity{entity_type}, Statement{kind}, Relation{relation_type}, EntityOrStatement }`
- `ExtractorField` — flat enum carrying all kind-specific config.
- `CacheConfig { Enabled, Disabled }`
- `DurationAst { amount, unit }` + `DurationUnit { Seconds, Minutes, Hours, Days }`
- `CostExpr { amount, unit }` + `CostUnit { PerMemory, PerRequest, PerDay }`
- `TriggerExpr { OnEncode, OnEncodeWhere(ConditionExpr), OnDemand, OnSchemaChange, Periodic{cron} }`
- `ConditionExpr { Atom, Matches, And, Or }` — boxed for recursion.
- `ConditionOp { Eq, Neq, Lt, Lte, Gt, Gte, In }`
- `ConditionValue { Text, Number, Bool, List }`
- `ResolverConfig` — placeholder struct; §22 fills in fields. For
  19.2 ships an empty struct with `pub`-fields room to grow.

## Derives + serde

All types: `Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize`.

Notably skip:
- `Eq` — `LiteralValue::Number(f64)` / `CostExpr::amount(f64)` /
  `ConditionValue::Number(f64)` / `ExtractorField::Confidence(f32)`
  prevent `Eq`. Per-variant manual `Eq` not worth the noise; keep
  `PartialEq` only.
- `Hash` — same reason.
- `rkyv` — §21/02 §6 mandates value-typed AST. The persisted
  `SchemaVersionRow` in `brain-metadata` carries its own rkyv form
  (19.5).

Serde tag conventions:
- All enums use externally-tagged JSON (serde default). Consistent
  with §29/00 examples of the SDK builder API.

## Tests (~10 unit tests in `schema/ast.rs`)

Per §21/02 §8:
1. `Schema::default_namespace_is_empty` — zero-construction sanity.
2. `attribute_decl_defaults_optional` — `required=false, unique=false, indexed=false, default=None`.
3. `attr_type_enum_variants_preserved` — `Enum { variants }` round-trip.
4. `attr_type_ref_target_preserved` — `Ref { target: "Person" }`.
5. `predicate_def_round_trip_json` — JSON ↔ struct.
6. `relation_def_with_properties` — relation-type AST with attribute properties.
7. `extractor_pattern_round_trip` — pattern extractor with `Confidence(0.7)`.
8. `extractor_llm_round_trip` — LLM extractor with `Model`, `Prompt`,
   `Schema(serde_json::Value)`, `CacheTtl`, `CostBudget`.
9. `condition_expr_nested` — `And(Atom, Or(Matches, Atom))`.
10. `schema_full_document_round_trip` — Schema with 1 entity + 2
    predicates + 1 relation + 1 extractor ↔ JSON.

## Out of scope

- Display impls — not needed; debug + serde cover it.
- `From<&str>` / builder helpers — those land in 19.8 SDK.
- Validator integration — 19.4.
- Wire-side rkyv structs — 19.6.

## Single commit

`feat(protocol): 19.2 — schema AST value types`

## Verification

```
cargo zigbuild --target x86_64-unknown-linux-gnu -p brain-protocol --tests
cargo test --target x86_64-unknown-linux-gnu -p brain-protocol schema::
cargo clippy --target x86_64-unknown-linux-gnu -p brain-protocol --all-targets -- -D warnings
```
