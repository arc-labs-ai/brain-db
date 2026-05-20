//! Apply layer — pure functions that mutate redb tables.
//!
//! For every [`Phase`] variant there is exactly one apply function. The
//! function takes a borrowed [`redb::WriteTransaction`], mutates the
//! tables it owns, and returns a [`PhaseAck`]. That's it.
//!
//! ## Invariants every apply function obeys
//!
//! - **Never opens a wtxn.** The caller (writer / WAL recovery) does.
//! - **Never commits.** Same reason.
//! - **Never allocates ids.** Ids travel inside the phase.
//! - **Never reads the clock.** Timestamps travel inside the phase.
//! - **Never publishes events or signals workers.** The writer does.
//! - **No async / no external IO.** Pure CPU work against the wtxn.
//!
//! The pure shape means apply functions are called from two places:
//! the live writer's submit loop, and WAL recovery on shard startup.
//! Same function, same wtxn type, exactly the same effects.

use redb::WriteTransaction;

use crate::write::{Phase, PhaseAck, Write};

pub mod audit;
pub mod edge;
pub mod entity;
pub mod memory;
pub mod reclaim;
pub mod relation;
pub mod schema;
pub mod statement;
pub mod update;

// ---------------------------------------------------------------------------
// ApplyError
// ---------------------------------------------------------------------------

/// Reasons an apply function can fail. Maps onto the wire `ErrorCode`
/// at the writer's boundary.
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    /// Underlying redb storage error.
    #[error("storage error: {0}")]
    Storage(String),

    /// The phase referenced a row that doesn't exist (e.g. tombstone
    /// of a memory that's already gone).
    #[error("{what} not found: {detail}")]
    NotFound { what: &'static str, detail: String },

    /// The phase's invariants don't hold (e.g. supersession of a
    /// statement whose chain root differs from the recorded one).
    #[error("invariant violated: {0}")]
    Invariant(String),

    /// Predicate / relation-type / entity-type not declared in the
    /// active schema. Maps to `PredicateNotInSchema` on the wire.
    #[error("schema admission failed: {0}")]
    SchemaAdmission(String),

    /// Underlying brain-metadata helper returned an error.
    #[error("metadata helper: {0}")]
    Metadata(String),

    /// Phase encoded a target / payload combination the apply function
    /// doesn't accept (e.g. `Supersede(Statement, Relation)`).
    #[error("phase mis-shape: {0}")]
    PhaseMisShape(&'static str),

    /// Slice not yet fully implemented (covers stub variants while P2
    /// is in progress). Replaced as variants land.
    #[error("apply for {0} not yet implemented")]
    NotYetImplemented(&'static str),
}

impl ApplyError {
    /// Short tag for tracing + metric labels.
    #[must_use]
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Storage(_) => "storage",
            Self::NotFound { .. } => "not_found",
            Self::Invariant(_) => "invariant",
            Self::SchemaAdmission(_) => "schema_admission",
            Self::Metadata(_) => "metadata",
            Self::PhaseMisShape(_) => "phase_mis_shape",
            Self::NotYetImplemented(_) => "not_yet_implemented",
        }
    }
}

impl From<redb::Error> for ApplyError {
    fn from(e: redb::Error) -> Self {
        Self::Storage(e.to_string())
    }
}

impl From<redb::TableError> for ApplyError {
    fn from(e: redb::TableError) -> Self {
        Self::Storage(e.to_string())
    }
}

impl From<redb::StorageError> for ApplyError {
    fn from(e: redb::StorageError) -> Self {
        Self::Storage(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// dispatch
// ---------------------------------------------------------------------------

/// Apply one [`Phase`] against the writer's [`WriteTransaction`].
///
/// `write` is the parent [`Write`] — handed in so apply functions
/// have access to `agent_id`, `started_at_unix_nanos`, and the
/// `write_id` for stamping audit metadata.
///
/// The match is exhaustive — adding a `Phase` variant fails to compile
/// here until a matching apply function is wired.
pub fn dispatch(
    wtxn: &WriteTransaction,
    phase: &Phase,
    write: &Write,
) -> Result<PhaseAck, ApplyError> {
    match phase {
        Phase::UpsertMemory { .. } => memory::apply_upsert_memory(wtxn, phase, write),
        Phase::UpsertEntity { .. } => entity::apply_upsert_entity(wtxn, phase, write),
        Phase::UpsertStatement { .. } => statement::apply_upsert_statement(wtxn, phase, write),
        Phase::UpsertRelation { .. } => relation::apply_upsert_relation(wtxn, phase, write),
        Phase::UpsertSchema { .. } => schema::apply_upsert_schema(wtxn, phase, write),
        Phase::Link { .. } => edge::apply_link(wtxn, phase, write),
        Phase::Unlink { .. } => edge::apply_unlink(wtxn, phase, write),
        Phase::Tombstone { target, .. } => match target {
            crate::write::TombstoneTarget::Memory { .. } => {
                memory::apply_tombstone_memory(wtxn, phase, write)
            }
            crate::write::TombstoneTarget::Entity(_) => {
                entity::apply_tombstone_entity(wtxn, phase, write)
            }
            crate::write::TombstoneTarget::Statement(_) => {
                statement::apply_tombstone_statement(wtxn, phase, write)
            }
            crate::write::TombstoneTarget::Relation(_) => {
                relation::apply_tombstone_relation(wtxn, phase, write)
            }
        },
        Phase::Supersede { target, .. } => match target {
            crate::write::SupersedeTarget::Statement(_) => {
                statement::apply_supersede_statement(wtxn, phase, write)
            }
            crate::write::SupersedeTarget::Relation(_) => {
                relation::apply_supersede_relation(wtxn, phase, write)
            }
        },
        Phase::UpdateSalience { .. } => memory::apply_update_salience(wtxn, phase, write),
        Phase::UpdateKind { .. } => memory::apply_update_kind(wtxn, phase, write),
        Phase::UpdateContext { .. } => memory::apply_update_context(wtxn, phase, write),
        Phase::UpdateEmbedding { .. } => memory::apply_update_embedding(wtxn, phase, write),
        Phase::UpdateAttribute { .. } => update::apply_update_attribute(wtxn, phase, write),
        Phase::Resolve { .. } => entity::apply_resolve(wtxn, phase, write),
        Phase::MergeEntities { .. } => entity::apply_merge_entities(wtxn, phase, write),
        Phase::SetExtractorEnabled { .. } => {
            schema::apply_set_extractor_enabled(wtxn, phase, write)
        }
        Phase::StampAudit { .. } => audit::apply_stamp_audit(wtxn, phase, write),
        Phase::ReclaimSlots { .. } => reclaim::apply_reclaim_slots(wtxn, phase, write),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_error_tag_uses_static_str() {
        let cases = [
            (ApplyError::Storage("x".into()), "storage"),
            (
                ApplyError::NotFound {
                    what: "memory",
                    detail: "y".into(),
                },
                "not_found",
            ),
            (ApplyError::Invariant("z".into()), "invariant"),
            (ApplyError::SchemaAdmission("p".into()), "schema_admission"),
            (ApplyError::Metadata("q".into()), "metadata"),
            (ApplyError::PhaseMisShape("r"), "phase_mis_shape"),
            (ApplyError::NotYetImplemented("s"), "not_yet_implemented"),
        ];
        for (e, expected) in cases {
            assert_eq!(e.tag(), expected);
        }
    }
}
