//! Per-opcode builders for every cognitive operation.
//!
//! Each op has its own file; the `Client` exposes a method that
//! returns the builder-§11.
//!
//! Streaming ops (RECALL / PLAN / REASON / SUBSCRIBE) ship with a
//! Vec-collecting `send()` (or `collect()`); the async-iterator
//! surface lives on the same builders.

pub mod admin;
pub mod common;
pub mod encode;
pub mod entity;
pub mod forget;
pub mod link;
pub mod plan;
pub mod procedural;
pub mod query;
pub mod reason;
pub mod recall;
pub mod relation;
pub mod schema;
pub mod statement;
pub mod stream;
pub mod subscribe;
pub mod txn;
pub mod unlink;

pub use stream::FrameStream;

pub use admin::{AdminClient, BackfillBuilder, BackfillHandle};
pub use encode::{EncodeBuilder, EncodeResponseExt};
pub use forget::ForgetBuilder;
pub use link::LinkBuilder;
pub use plan::{PlanBuilder, PlanOutcome};
pub use procedural::MaterializeProceduralBuilder;
pub use reason::ReasonBuilder;
pub use recall::RecallBuilder;
pub use subscribe::SubscribeBuilder;
pub use unlink::UnlinkBuilder;

pub use entity::{
    EntityClient, EntityCreateBuilder, EntityListBuilder, EntityMergeBuilder, EntityResolveBuilder,
    EntityUpdateBuilder, MergeOutcome, ResolutionOutcome,
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
