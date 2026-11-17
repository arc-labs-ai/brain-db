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

pub mod edge;
pub mod encode_helpers;
pub mod entity;
pub mod memory;
pub mod reclaim;
pub mod registry;
pub mod relation;
pub mod schema;
pub mod statement;

// ---------------------------------------------------------------------------
// ApplyError
// ---------------------------------------------------------------------------

/// Reasons an apply function can fail. Maps onto the wire `ErrorCode`
/// at the writer's boundary.
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    /// Underlying redb failure, carrying both the retry classification and the
    /// rendered text. The kind is what a caller matches on to decide whether
    /// to retry; the detail is what an operator reads in a log.
    #[error("storage error ({fault}): {detail}")]
    Storage { fault: StorageFault, detail: String },

    /// The phase referenced a row that doesn't exist (e.g. tombstone
    /// of a memory that's already gone).
    #[error("{what} not found: {detail}")]
    NotFound { what: &'static str, detail: String },

    /// The phase's invariants don't hold (e.g. supersession of a
    /// statement whose chain root differs from the recorded one).
    #[error("invariant violated: {0}")]
    Invariant(String),

    /// Predicate / relation-type / entity-type not declared in the
    /// active schema. A strict-mode schema refuses the name the request
    /// carries, so the caller fixes it by declaring the type or writing a
    /// declared one: it crosses the writer boundary as an invalid request,
    /// in the same `Validation` wire category as the precise
    /// `PredicateNotInSchema` code the handlers raise pre-flight.
    #[error("schema admission failed: {0}")]
    SchemaAdmission(String),

    /// The caller asked for something that conflicts with state already
    /// committed — a duplicate canonical name being the live case.
    ///
    /// Distinct from [`ApplyError::Metadata`] on purpose. Both originate in
    /// the same helpers, but this one is the caller's doing and retrying it
    /// unchanged can never succeed, whereas `Metadata` is the engine's own
    /// fault and may well be transient. Collapsing the two tells a client to
    /// retry forever against a request that is permanently refused.
    #[error("conflict: {0}")]
    Conflict(String),

    /// The phase carries input the apply layer refuses: an undeclared type, an
    /// endpoint of the wrong entity type, or a supersede whose shape does not
    /// match its target. Distinct from [`ApplyError::Metadata`] for the same
    /// reason [`ApplyError::Conflict`] is — this one is the caller's doing and
    /// no retry of the same request can succeed, whereas `Metadata` is our own
    /// fault and may well be transient.
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// Underlying brain-metadata helper returned an error.
    #[error("metadata helper: {0}")]
    Metadata(String),

    /// Phase encoded a target / payload combination the apply function
    /// doesn't accept (e.g. `Supersede(Statement, Relation)`).
    #[error("phase mis-shape: {0}")]
    PhaseMisShape(&'static str),
}

impl ApplyError {
    /// Wrap a redb failure with the operation that produced it.
    ///
    /// `context` names the table and the operation ("open MEMORIES",
    /// "TIMELINE insert"); the redb value is rendered with `Debug` so its
    /// constructor and payload both reach the log.
    pub fn storage<E: RedbFault + std::fmt::Debug>(context: &str, e: &E) -> Self {
        Self::Storage {
            fault: e.storage_fault(),
            detail: format!("{context}: {e:?}"),
        }
    }

    /// Wrap a storage failure whose redb value was already flattened to text.
    ///
    /// Classified `Transient` because that is the fallback that costs a retry
    /// instead of abandoning work on a guess. The artifact-bundle helpers and
    /// the metadata registry are the two such layers.
    pub fn storage_opaque(context: &str, e: impl std::fmt::Display) -> Self {
        Self::Storage {
            fault: StorageFault::Transient,
            detail: format!("{context}: {e}"),
        }
    }

    /// Short tag for tracing + metric labels.
    #[must_use]
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Storage { .. } => "storage",
            Self::NotFound { .. } => "not_found",
            Self::Invariant(_) => "invariant",
            Self::SchemaAdmission(_) => "schema_admission",
            Self::Conflict(_) => "conflict",
            Self::InvalidRequest(_) => "invalid_request",
            Self::Metadata(_) => "metadata",
            Self::PhaseMisShape(_) => "phase_mis_shape",
        }
    }
}

/// How a redb failure behaves when the same work is retried.
///
/// Deliberately coarser than redb's own taxonomy, which splits its failures
/// across four overlapping enums. The only question any consumer asks is
/// whether re-running the identical write could ever produce a different
/// outcome; the three deterministic kinds are kept apart because an operator
/// responds to each differently — restore the file, shrink the payload, fix
/// the code. `Transient` is the fallback for anything unclassified, so a redb
/// variant nobody has mapped costs a retry rather than permanently abandoning
/// the work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageFault {
    /// redb reports the on-disk state as damaged. No retry succeeds; the file
    /// needs repair or restore.
    Corrupted,
    /// The value exceeds redb's 3 GiB per-value ceiling. The same payload is
    /// refused every time.
    ValueTooLarge,
    /// The code asked redb for something it refuses on every attempt against
    /// this database: a table opened at the wrong key/value types, a table
    /// opened twice in one write transaction, a changed type definition, a
    /// table that does or does not exist, an invalid savepoint, or a file
    /// format needing a manual upgrade. An engine or migration defect.
    Misuse,
    /// I/O, a closed database, a poisoned lock, or a transaction still held by
    /// a live reader. A later attempt may succeed.
    Transient,
}

impl StorageFault {
    /// Whether retrying the failed work can ever produce a different outcome.
    ///
    /// Callers that permanently abandon work — rather than re-queueing it —
    /// gate on this. Only `Transient` is retryable, which makes every
    /// unclassified failure retryable by construction.
    #[must_use]
    pub fn is_deterministic(self) -> bool {
        !matches!(self, Self::Transient)
    }
}

impl std::fmt::Display for StorageFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Corrupted => "corrupted",
            Self::ValueTooLarge => "value_too_large",
            Self::Misuse => "misuse",
            Self::Transient => "transient",
        };
        f.write_str(s)
    }
}

/// A redb error that can name how it behaves on retry.
///
/// redb splits its failures across `StorageError`, `TableError`,
/// `TransactionError`, `CommitError` and the catch-all `redb::Error`, which
/// overlap but are not one type. This trait is the single place the engine
/// maps all of them onto [`StorageFault`], so the classification exists once
/// rather than at every call site.
pub trait RedbFault {
    /// Classify this failure for retry.
    fn storage_fault(&self) -> StorageFault;
}

impl RedbFault for redb::StorageError {
    fn storage_fault(&self) -> StorageFault {
        match self {
            Self::Corrupted(_) => StorageFault::Corrupted,
            Self::ValueTooLarge(_) => StorageFault::ValueTooLarge,
            Self::Io(_) | Self::PreviousIo | Self::DatabaseClosed | Self::LockPoisoned(_) => {
                StorageFault::Transient
            }
            // redb marks this enum `#[non_exhaustive]`, so a wildcard is
            // required. An unrecognised failure is treated as retryable: a
            // misclassification then costs retries rather than abandoning
            // durable work.
            _ => StorageFault::Transient,
        }
    }
}

impl RedbFault for redb::TableError {
    fn storage_fault(&self) -> StorageFault {
        match self {
            Self::Storage(e) => e.storage_fault(),
            Self::TableTypeMismatch { .. }
            | Self::TableIsMultimap(_)
            | Self::TableIsNotMultimap(_)
            | Self::TypeDefinitionChanged { .. }
            | Self::TableDoesNotExist(_)
            | Self::TableExists(_)
            | Self::TableAlreadyOpen(..) => StorageFault::Misuse,
            // Required by `#[non_exhaustive]`; unknown failures stay retryable.
            _ => StorageFault::Transient,
        }
    }
}

impl RedbFault for redb::TransactionError {
    fn storage_fault(&self) -> StorageFault {
        match self {
            Self::Storage(e) => e.storage_fault(),
            // A write transaction blocked by a reader that has not been
            // dropped yet; the same call succeeds once it is.
            Self::ReadTransactionStillInUse(_) => StorageFault::Transient,
            // Required by `#[non_exhaustive]`; unknown failures stay retryable.
            _ => StorageFault::Transient,
        }
    }
}

impl RedbFault for redb::CommitError {
    fn storage_fault(&self) -> StorageFault {
        match self {
            Self::Storage(e) => e.storage_fault(),
            // Required by `#[non_exhaustive]`; unknown failures stay retryable.
            _ => StorageFault::Transient,
        }
    }
}

impl RedbFault for redb::Error {
    fn storage_fault(&self) -> StorageFault {
        match self {
            Self::Corrupted(_) => StorageFault::Corrupted,
            Self::ValueTooLarge(_) => StorageFault::ValueTooLarge,
            Self::TableTypeMismatch { .. }
            | Self::TableIsMultimap(_)
            | Self::TableIsNotMultimap(_)
            | Self::TypeDefinitionChanged { .. }
            | Self::TableDoesNotExist(_)
            | Self::TableExists(_)
            | Self::TableAlreadyOpen(..)
            | Self::InvalidSavepoint
            | Self::UpgradeRequired(_) => StorageFault::Misuse,
            // `redb::Error` is `#[non_exhaustive]`, and its remaining known
            // variants are lock, durability and savepoint-lifecycle states a
            // later attempt can clear. Anything redb adds lands here too,
            // which keeps an unmapped failure retryable rather than terminal.
            _ => StorageFault::Transient,
        }
    }
}

impl From<redb::Error> for ApplyError {
    fn from(e: redb::Error) -> Self {
        Self::Storage {
            fault: e.storage_fault(),
            detail: e.to_string(),
        }
    }
}

impl From<redb::TableError> for ApplyError {
    fn from(e: redb::TableError) -> Self {
        Self::Storage {
            fault: e.storage_fault(),
            detail: e.to_string(),
        }
    }
}

impl From<redb::StorageError> for ApplyError {
    fn from(e: redb::StorageError) -> Self {
        Self::Storage {
            fault: e.storage_fault(),
            detail: e.to_string(),
        }
    }
}

impl From<redb::TransactionError> for ApplyError {
    fn from(e: redb::TransactionError) -> Self {
        Self::Storage {
            fault: e.storage_fault(),
            detail: e.to_string(),
        }
    }
}

impl From<redb::CommitError> for ApplyError {
    fn from(e: redb::CommitError) -> Self {
        Self::Storage {
            fault: e.storage_fault(),
            detail: e.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// dispatch
// ---------------------------------------------------------------------------

/// Apply one [`Phase`] against the writer's [`WriteTransaction`].
///
/// `write` is the parent [`Write`] — handed in so apply functions
/// have access to `space_id`, `started_at_unix_nanos`, and the
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
        Phase::RestoreMemory { .. } => memory::apply_restore_memory(wtxn, phase, write),
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
        Phase::UpdateSession { .. } => memory::apply_update_session(wtxn, phase, write),
        Phase::UpdateEmbedding { .. } => memory::apply_update_embedding(wtxn, phase, write),
        Phase::UpdateEntity { .. } => entity::apply_update_entity(wtxn, phase, write),
        Phase::RenameEntity { .. } => entity::apply_rename_entity(wtxn, phase, write),
        Phase::UnmergeEntities { .. } => entity::apply_unmerge_entities(wtxn, phase, write),
        Phase::MergeEntities { .. } => entity::apply_merge_entities(wtxn, phase, write),
        Phase::ApproveMerge { .. } => entity::apply_approve_merge(wtxn, phase, write),
        Phase::RejectMerge { .. } => entity::apply_reject_merge(wtxn, phase, write),
        Phase::ReclaimSlots { .. } => reclaim::apply_reclaim_slots(wtxn, phase, write),
        Phase::SpaceCreate { .. } => registry::apply_space_create(wtxn, phase, write),
        Phase::SpaceDelete { .. } => registry::apply_space_delete(wtxn, phase, write),
        Phase::SessionCreate { .. } => registry::apply_session_create(wtxn, phase, write),
        Phase::SessionDelete { .. } => registry::apply_session_delete(wtxn, phase, write),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_error_tag_uses_static_str() {
        let cases = [
            (
                ApplyError::Storage {
                    fault: StorageFault::Transient,
                    detail: "x".into(),
                },
                "storage",
            ),
            (
                ApplyError::NotFound {
                    what: "memory",
                    detail: "y".into(),
                },
                "not_found",
            ),
            (ApplyError::Invariant("z".into()), "invariant"),
            (ApplyError::SchemaAdmission("p".into()), "schema_admission"),
            (
                ApplyError::InvalidRequest("relation_type".into()),
                "invalid_request",
            ),
            (ApplyError::Metadata("q".into()), "metadata"),
            (ApplyError::PhaseMisShape("r"), "phase_mis_shape"),
        ];
        for (e, expected) in cases {
            assert_eq!(e.tag(), expected);
        }
    }

    /// The redb kind reaches the error value, so a consumer matches rather
    /// than inspecting the rendered message.
    #[test]
    fn construction_keeps_the_redb_kind() {
        let cases: [(ApplyError, StorageFault); 4] = [
            (
                ApplyError::storage(
                    "open MEMORIES",
                    &redb::StorageError::Corrupted("page".into()),
                ),
                StorageFault::Corrupted,
            ),
            (
                ApplyError::storage(
                    "MEMORIES insert",
                    &redb::StorageError::ValueTooLarge(1 << 32),
                ),
                StorageFault::ValueTooLarge,
            ),
            (
                ApplyError::storage(
                    "open TIMELINE",
                    &redb::TableError::TableDoesNotExist("TIMELINE".into()),
                ),
                StorageFault::Misuse,
            ),
            (
                ApplyError::storage("MEMORIES get", &redb::StorageError::DatabaseClosed),
                StorageFault::Transient,
            ),
        ];
        for (err, expected) in cases {
            match err {
                ApplyError::Storage { fault, .. } => assert_eq!(fault, expected),
                other => panic!("expected Storage, got {other:?}"),
            }
        }
    }

    /// The context string and redb's own rendering both survive into detail.
    #[test]
    fn construction_keeps_the_rendered_detail() {
        let err = ApplyError::storage(
            "open MEMORIES",
            &redb::StorageError::Corrupted("bad page 7".into()),
        );
        let ApplyError::Storage { detail, .. } = &err else {
            panic!("expected Storage, got {err:?}");
        };
        assert!(detail.starts_with("open MEMORIES: "), "{detail}");
        assert!(detail.contains("bad page 7"), "{detail}");
        let rendered = err.to_string();
        assert!(rendered.contains("corrupted"), "{rendered}");
        assert!(rendered.contains("bad page 7"), "{rendered}");
    }

    /// CommitError and TransactionError classify by their inner storage error.
    #[test]
    fn wrapper_errors_classify_by_their_inner_storage_error() {
        let commit = ApplyError::storage(
            "commit",
            &redb::CommitError::from(redb::StorageError::Corrupted("x".into())),
        );
        assert!(matches!(
            commit,
            ApplyError::Storage {
                fault: StorageFault::Corrupted,
                ..
            }
        ));
        let txn = ApplyError::storage(
            "write_txn",
            &redb::TransactionError::from(redb::StorageError::Io(std::io::Error::other("disk"))),
        );
        assert!(matches!(
            txn,
            ApplyError::Storage {
                fault: StorageFault::Transient,
                ..
            }
        ));
    }

    /// A flattened storage failure reads as transient so the work is retried.
    #[test]
    fn an_opaque_storage_failure_reads_as_transient() {
        let err = ApplyError::storage_opaque("registry touch", "space row write failed");
        let ApplyError::Storage { fault, detail } = &err else {
            panic!("expected Storage, got {err:?}");
        };
        assert_eq!(*fault, StorageFault::Transient);
        assert!(!fault.is_deterministic());
        assert_eq!(detail, "registry touch: space row write failed");
    }

    /// Only a transient failure is worth retrying; deterministic kinds fail forever.
    #[test]
    fn only_transient_faults_are_retryable() {
        assert!(StorageFault::Corrupted.is_deterministic());
        assert!(StorageFault::ValueTooLarge.is_deterministic());
        assert!(StorageFault::Misuse.is_deterministic());
        assert!(!StorageFault::Transient.is_deterministic());
    }
}
