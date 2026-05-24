//! # brain-sdk-rust
//!
//! Idiomatic async Rust SDK for the Brain cognitive substrate **and**
//! its knowledge layer.
//!
//! ## Substrate surface
//!
//! - [`Client`] — single-connection async entry point. `Client::connect`
//!   opens a TCP socket, drives the handshake (HELLO →
//!   WELCOME → AUTH → AUTH_OK), and returns a usable client.
//! - [`ClientConfig`] with defaults.
//! - [`ClientError`] — `#[non_exhaustive]` error taxonomy.
//! - Op builders: [`EncodeBuilder`], [`RecallBuilder`], [`PlanBuilder`],
//!   [`ReasonBuilder`], [`ForgetBuilder`], [`LinkBuilder`],
//!   [`UnlinkBuilder`], [`SubscribeBuilder`].
//!
//! ## Knowledge surface (phase 16.8+)
//!
//! When a schema is declared on the deployment, the SDK exposes a
//! typed entity API via [`Client::entity`]:
//!
//! ```no_run
//! # use brain_sdk_rust::{Client, Person};
//! # async fn ex(client: Client) -> Result<(), brain_sdk_rust::ClientError> {
//! let alice = client.entity::<Person>()
//!     .create()
//!     .canonical_name("Alice")
//!     .alias("A.")
//!     .with_email("alice@example.com")
//!     .send()
//!     .await?;
//!
//! let resolved = client.entity::<Person>()
//!     .resolve("Alice")
//!     .send()
//!     .await?;
//! # let _ = (alice, resolved);
//! # Ok(()) }
//! ```
//!
//! Covers all 9 entity opcodes (CREATE / GET / UPDATE / RENAME / MERGE
//! / UNMERGE / RESOLVE / LIST / TOMBSTONE) for the built-in
//! [`Person`] type. The `#[derive(BrainEntity)]` macro generalising
//! to user types lands in phase 19 alongside the schema DSL —
//! [`BrainEntityType`] is the trait contract.
//!
//! Statement / relation / query builders land in phases 17 / 18 /
//! 22-23. See `spec/29_knowledge_sdk/00_purpose.md` "Phase scope".
//!
//! Error inspection helpers for knowledge errors:
//! [`ClientErrorEntityExt`] + [`EntityErrorKind`] let callers
//! dispatch on entity-specific failures without string-matching.
//!
//! ## Layout
//!
//! Every concern under `src/` lives in its own folder; only
//! `lib.rs` sits at the crate root. See
//! `.claude/plans/phase-10-task-01.md` §3 for the rationale.
//!
//! ## Spec reference
//!
//! - `spec/06_sdk/` — substrate SDK design.
//! - `spec/29_knowledge_sdk/00_purpose.md` — knowledge SDK design +
//!   phase scope.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

pub mod client;
pub mod config;
pub mod error;
pub mod models;
pub mod observability;
pub mod ops;
pub mod pool;
pub mod proto;
pub mod request_id;
pub mod retry;

pub use brain_core::TombstoneReason;
pub use brain_core::{
    BackfillId, EntityId, MemoryId, RelationId, RequestId, StatementId, StatementKind,
};
pub use brain_protocol::envelope::request::BackfillScope;
pub use brain_protocol::envelope::response::{
    AdminBackfillCancelResponse, AdminBackfillResponse, BackfillProgress,
};
pub use client::Client;
pub use config::{AuthMethod, ClientConfig};
pub use error::ClientError;
pub use models::entity::{
    BrainEntityType, EntityHandle, EntityHandleFromViewError, Person, PersonAttributes,
};
pub use models::errors::{
    ClientErrorEntityExt, ClientErrorRelationExt, ClientErrorStatementExt, EntityErrorKind,
    RelationErrorKind, StatementErrorKind,
};
pub use observability::{MetricsSnapshot, OpMetrics};
pub use ops::{
    AdminClient, BackfillBuilder, BackfillHandle, EncodeBuilder, EncodeResponseExt, EntityClient,
    EntityCreateBuilder, EntityListBuilder,
    EntityMergeBuilder, EntityResolveBuilder, EntityUpdateBuilder, EventBuilder, ExplainResult,
    FactBuilder, ForgetBuilder, FrameStream, FusionConfig, ItemKind, ItemRef, LinkBuilder,
    MaterializeProceduralBuilder, MergeOutcome, PlanBuilder, PlanOutcome, PreferenceBuilder,
    QueryBuilder, QueryBuilderError, QueryHit, QueryResult, ReasonBuilder, RecallBuilder,
    RelationBuilder, RelationHandle, RelationListFromBuilder, RelationListToBuilder,
    RelationTraverseBuilder, RelationsClient, ResolutionOutcome, Retriever, RetrieverContribution,
    RetrieverOutcome, RetrieverOutcomeStatus, RetrieverSelection, SchemaBuilder, SchemaClient,
    SchemaListEntry, SchemaListView, SchemaUploadOutcome, SchemaValidateOutcome,
    SchemaValidationIssue, SchemaView, StatementHandle, StatementListBuilder, StatementsClient,
    SubscribeBuilder, TimeRange, TraceResult, TraversalPath, TraversalStep, TraverseDirection,
    UnlinkBuilder,
};
pub use pool::{Connection, Pool, PoolConfig, PoolGuard};
pub use proto::handshake::{ClientIdentity, NegotiatedSession};
pub use request_id::{DefaultRequestIdSource, RequestIdSource};
pub use retry::RetryConfig;
