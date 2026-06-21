//! Nouns Brain stores: memories, entities, statements, and relations —
//! plus the kind discriminators that classify them.
//!
//! These are pure value types. Storage, resolution, and extraction live
//! elsewhere in the workspace; this module just defines the data shapes.

pub mod entity;
pub mod kinds;
pub mod memory;
pub mod relation;
pub mod statement;

pub use entity::{Entity, EntityAttributes, EntityType};
pub use kinds::{
    Cardinality, ExtractorKind, KindBehavior, KindCardinality, StatementKind, TemporalModel,
};
pub use memory::{Memory, MemoryKind, Salience};
pub use relation::{canonical_pair, Relation, RelationType};
pub use statement::{
    EvidenceEntry, EvidenceRef, Predicate, Statement, StatementObject, StatementValue, SubjectRef,
    TombstoneReason, INLINE_EVIDENCE_CAP,
};
