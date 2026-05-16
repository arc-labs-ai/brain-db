//! Schema-DSL surface (spec §21).
//!
//! Phase 19.2 lands the AST. Subsequent sub-tasks add the parser
//! (19.3), validator (19.4), and wire request / response types
//! (19.6) alongside in this folder.
//!
//! The AST is value-typed (`serde` + plain `Debug` / `Clone`) — no
//! `rkyv` derives. The persisted shape lives in `brain-metadata`
//! as `SchemaVersionRow` (§21/05).

pub mod ast;

pub use ast::{
    AttrType, AttributeDecl, CacheConfig, CardinalityAst, ConditionExpr, ConditionOp,
    ConditionValue, CostExpr, CostUnit, DurationAst, DurationUnit, EntityTypeDef, ExtractorDef,
    ExtractorField, ExtractorKindAst, ExtractorTarget, LiteralValue, ObjectTypeDecl, PredicateDef,
    RelationTypeDef, ResolverConfig, Schema, SchemaItem, StatementKindAst, TriggerExpr,
};
