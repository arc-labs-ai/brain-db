//! Knowledge-layer types (Layer 2/3 in the three-layer model).
//!
//! See `spec/17_knowledge_model/00_purpose.md` for the conceptual frame:
//!
//! - **Layer 1** (substrate) — memories (the rest of `brain-core`).
//! - **Layer 2** — entities and relations.
//! - **Layer 3** — statements (Fact / Preference / Event).
//!
//! These types are pure value types — no I/O, no async. Storage,
//! resolution, and extraction live in their respective phases (15.x
//! storage, 16 entities, 17 statements, 18 relations, 19 schema DSL,
//! 20–21 extractors).
//!
//! Phase 15.1 — types and identifiers only. Behavior follows in later
//! phases.

pub mod confidence;
pub mod entity;
pub mod ids;
pub mod kinds;
pub mod relation;
pub mod resolver;
pub mod statement;
pub mod trigrams;

pub use confidence::{aggregate_confidence, ConfidenceConfig};
pub use entity::{Entity, EntityAttributes, EntityType};
pub use ids::{
    AuditId, EntityId, EntityTypeId, EvidenceOverflowId, ExtractorId, MergeId, PredicateId,
    RelationId, RelationTypeId, StatementId,
};
pub use kinds::{Cardinality, ExtractorKind, StatementKind};
pub use relation::{canonical_pair, Relation, RelationType};
pub use resolver::{
    resolve_entity, ResolutionOutcome, ResolverConfig, ResolverEmbedder, ResolverError,
    ResolverIndex, ResolverStorage, ResolverTier, TypeConstraint, VECTOR_DIM,
};
pub use statement::{
    EvidenceEntry, EvidenceRef, Predicate, Statement, StatementObject, StatementValue,
    SubjectRef, TombstoneReason, INLINE_EVIDENCE_CAP,
};
pub use trigrams::{extract_trigrams, jaccard};
