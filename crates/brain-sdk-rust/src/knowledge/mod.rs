//! Knowledge-layer SDK helpers. Spec §29.
//!
//! Phase 16.8 lands the hand-written `Entity` slice — the built-in
//! [`Person`] type plus an `EntityHandle<T>` wrapper covering all
//! 9 entity wire opcodes. Phase 19's schema DSL + derive macro
//! (`#[derive(BrainEntity)]`) generalises this to user-declared types.
//!
//! See `spec/29_knowledge_sdk/00_purpose.md` "Phase scope" for the
//! roadmap across phases 16-24.

pub mod builder;
pub mod entity;
pub mod errors;
pub mod query;
pub mod relation;
pub mod schema;
pub mod statement;

pub use builder::{
    EntityClient, EntityCreateBuilder, EntityListBuilder, EntityMergeBuilder, EntityResolveBuilder,
    EntityUpdateBuilder, MergeOutcome, ResolutionOutcome,
};
pub use entity::{
    BrainEntityType, EntityHandle, EntityHandleFromViewError, Person, PersonAttributes,
};
pub use errors::{
    ClientErrorEntityExt, ClientErrorRelationExt, ClientErrorStatementExt, EntityErrorKind,
    RelationErrorKind, StatementErrorKind,
};
pub use query::{
    ExplainResult, FusionConfig, ItemKind, ItemRef, QueryBuilder, QueryBuilderError, QueryHit,
    QueryResult, Retriever, RetrieverContribution, RetrieverOutcome, RetrieverOutcomeStatus,
    RetrieverSelection, TimeRange, TraceResult, MAX_EXPLICIT_RETRIEVERS, MAX_QUERY_TEXT_BYTES,
};
pub use relation::{
    RelationBuilder, RelationHandle, RelationListFromBuilder, RelationListToBuilder,
    RelationTraverseBuilder, RelationsClient, TraversalPath, TraversalStep, TraverseDirection,
};
pub use schema::{
    print_schema, SchemaBuilder, SchemaClient, SchemaListEntry, SchemaListView,
    SchemaUploadOutcome, SchemaValidateOutcome, SchemaValidationIssue, SchemaView,
};
pub use statement::{
    EventBuilder, FactBuilder, PreferenceBuilder, StatementHandle, StatementListBuilder,
    StatementsClient,
};
