//! # brain-sdk-rust
//!
//! Idiomatic async Rust SDK for the Brain cognitive substrate **and**
//! its knowledge layer.
//!
//! ## Substrate surface
//!
//! - [`Client`] — single-connection async entry point. `Client::connect`
//!   opens a TCP socket, drives the spec §03/06 handshake (HELLO →
//!   WELCOME → AUTH → AUTH_OK), and returns a usable client.
//! - [`ClientConfig`] with spec §13/02 §14 defaults.
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
//! - `spec/13_sdk_design/` — substrate SDK design.
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
pub mod knowledge;
pub mod observability;
pub mod ops;
pub mod pool;
pub mod proto;
pub mod request_id;
pub mod retry;

pub use brain_core::{EntityId, MemoryId, RequestId, StatementId, StatementKind};
pub use brain_core::knowledge::TombstoneReason;
pub use client::Client;
pub use config::{AuthMethod, ClientConfig};
pub use error::ClientError;
pub use knowledge::{
    BrainEntityType, ClientErrorEntityExt, ClientErrorStatementExt, EntityClient,
    EntityCreateBuilder, EntityErrorKind, EntityHandle, EntityHandleFromViewError,
    EntityListBuilder, EntityMergeBuilder, EntityResolveBuilder, EntityUpdateBuilder,
    EventBuilder, FactBuilder, MergeOutcome, Person, PersonAttributes, PreferenceBuilder,
    ResolutionOutcome, StatementErrorKind, StatementHandle, StatementListBuilder,
    StatementsClient,
};
pub use observability::{MetricsSnapshot, OpMetrics};
pub use ops::{
    EncodeBuilder, ForgetBuilder, LinkBuilder, PlanBuilder, ReasonBuilder, RecallBuilder,
    SubscribeBuilder, UnlinkBuilder,
};
pub use pool::{Connection, Pool, PoolConfig, PoolGuard};
pub use proto::handshake::{ClientIdentity, NegotiatedSession};
pub use request_id::{DefaultRequestIdSource, RequestIdSource};
pub use retry::RetryConfig;
