//! Schema-DSL surface (spec §21).
//!
//! - 19.2 — AST value types.
//! - 19.3 — Pest parser.
//! - 19.4 — Static validator (next).

pub mod ast;
pub mod parse_error;
pub mod parser;
pub mod validator;

pub use ast::{
    AttrType, AttributeDecl, CacheConfig, CardinalityAst, ConditionExpr, ConditionOp,
    ConditionValue, CostExpr, CostUnit, DurationAst, DurationUnit, EntityTypeDef, ExtractorDef,
    ExtractorField, ExtractorKindAst, ExtractorTarget, LiteralValue, ObjectTypeDecl, PredicateDef,
    RelationTypeDef, ResolverConfig, Schema, SchemaItem, StatementKindAst, TriggerExpr,
};
pub use parse_error::ParseError;
pub use parser::parse_schema;
pub use validator::{
    validate, validate_system_schema, SourceSpan, ValidatedSchema, ValidationError,
    ValidationErrorCode, ValidationErrors,
};
