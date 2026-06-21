//! # brain-core
//!
//! Foundational types for the Brain cognitive substrate, shared across the
//! workspace. Everything in here is a pure value type — no I/O, no async,
//! no runtime dependency.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
#![forbid(unsafe_code)]

pub mod edges;
pub mod error;
pub mod ids;
pub mod migration;
pub mod nodes;
pub mod resolution;
pub mod worker_state;

pub use edges::{
    edge::{Edge, EdgeKind, EdgeOrigin},
    edge_kind_ref::{EdgeKindRef, EdgeKindRefError},
    node_ref::{NodeRef, NodeRefError},
};
pub use error::{Error, Result};
pub use ids::{
    AgentId, AuditId, ContextId, EntityId, EntityTypeId, EvidenceOverflowId, ExtractorId, MemoryId,
    MergeId, PredicateId, RelationId, RelationTypeId, RequestId, ShardId, SlotIndex, SlotVersion,
    StatementId, TxnId, MAX_SLOT_INDEX,
};
pub use migration::{
    MigrationByReason, MigrationId, MigrationItem, MigrationPlan, MigrationReason, MigrationSummary,
};
pub use nodes::{
    entity::{Entity, EntityAttributes, EntityType},
    kinds::{
        Cardinality, ExtractorKind, KindBehavior, KindCardinality, StatementKind, TemporalModel,
    },
    memory::{Memory, MemoryKind, Salience},
    relation::{canonical_pair, Relation, RelationType},
    statement::{
        EvidenceEntry, EvidenceRef, Predicate, Statement, StatementObject, StatementValue,
        SubjectRef, TombstoneReason, INLINE_EVIDENCE_CAP,
    },
};
pub use resolution::{
    confidence::{aggregate_confidence, ConfidenceConfig},
    referential::is_non_referential_surface,
    resolver::{
        resolve_entity, ResolutionOutcome, ResolverConfig, ResolverEmbedder, ResolverError,
        ResolverIndex, ResolverLlm, ResolverLlmDecision, ResolverStorage, ResolverTier,
        TypeConstraint, VECTOR_DIM,
    },
    trigrams::{extract_trigrams, jaccard},
};
pub use worker_state::{
    BackfillId, BackfillProgress, BackfillRange, BackfillRequest, WorkerPriority,
};
