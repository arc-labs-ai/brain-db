//! Schema-DSL surface.
//!
//! - 19.2 — AST value types.
//! - 19.3 — Pest parser.
//! - 19.4 — Static validator (next).

pub mod ast;
pub mod locate;
pub mod ops;
pub mod parse_error;
pub mod parser;
pub mod render;
pub mod validator;

pub use ast::{
    AttrType, AttributeDecl, CacheConfig, CardinalityAst, ConditionExpr, ConditionOp,
    ConditionValue, CostExpr, CostUnit, DurationAst, DurationUnit, EntityTypeDef, ExtractorDef,
    ExtractorField, ExtractorKindAst, ExtractorTarget, KindCardinalityAst, KindDef, LiteralValue,
    ObjectKindAst, ObjectTypeDecl, PredicateDef, RelationTypeDef, ResolverConfig, Schema,
    SchemaItem, StatementKindAst, SubjectTypeDecl, TemporalModelAst, TriggerExpr,
};
pub use locate::{locate_spans, validate_located, validate_located_with};
pub use parse_error::ParseError;
pub use parser::parse_schema;
pub use render::{render_schema, render_schema_verified};
pub use validator::{
    validate, validate_namespace, validate_system_schema, validate_with, DeclaredContext,
    SourceSpan, ValidatedSchema, ValidationError, ValidationErrorCode, ValidationErrors,
};
