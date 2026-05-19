//! # brain-core
//!
//! Foundational types for the Brain cognitive substrate, shared across the
//! workspace. Everything in here is a pure value type — no I/O, no async,
//! no runtime dependency.
//!
//! See `spec/02_data_model/` for the authoritative definitions.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

pub mod edge;
pub mod edge_kind_ref;
pub mod error;
pub mod ids;
pub mod knowledge;
pub mod memory;
pub mod migration;
pub mod node;
pub mod worker_state;

pub use edge::{Edge, EdgeKind, EdgeOrigin};
pub use edge_kind_ref::{EdgeKindRef, EdgeKindRefError};
pub use error::{Error, Result};
pub use ids::{
    AgentId, ContextId, MemoryId, RequestId, ShardId, SlotIndex, SlotVersion, TxnId, MAX_SLOT_INDEX,
};
pub use knowledge::{
    AuditId, Cardinality, Entity, EntityAttributes, EntityId, EntityType, EntityTypeId,
    EvidenceOverflowId, ExtractorId, ExtractorKind, MergeId, PredicateId, RelationId,
    RelationTypeId, ResolutionOutcome, ResolverConfig, ResolverTier, StatementId, StatementKind,
    TypeConstraint,
};
pub use memory::{Memory, MemoryKind, Salience};
pub use migration::{
    MigrationByReason, MigrationId, MigrationItem, MigrationPlan, MigrationReason, MigrationSummary,
};
pub use node::{NodeRef, NodeRefError};
pub use worker_state::{
    BackfillId, BackfillProgress, BackfillRange, BackfillRequest, WorkerPriority,
};
