//! Statement metadata operations.
//!
//! Free functions over `redb::{ReadTransaction, WriteTransaction}`,
//! mirroring the [`crate::entity::ops`] / [`crate::schema::predicate`]
//! precedent. Every mutation runs inside the caller-supplied write
//! txn; commit is the caller's responsibility, which keeps multi-table
//! atomicity in the substrate's single-writer-per-shard discipline.
//!
//! The original `statement_ops.rs` is split here into four siblings:
//!
//! - [`crud`]: create / get + the shared invariant + index helpers.
//! - [`supersede`]: supersession chain mechanics.
//! - [`tombstone`]: soft-delete (`tombstone`) and v1 retract.
//! - [`list`]: listing, history-walk, contradiction surface, filter
//!   struct.

pub mod crud;
pub mod embed_queue;
pub mod evidence;
pub mod list;
pub mod supersede;
pub mod tombstone;

// Flat re-exports so `brain_metadata::statement::*` covers the original
// statement_ops API surface without consumers having to walk the
// sub-modules.
pub use crud::{
    allocate_evidence_overflow, evidence_overflow_load, statement_create, statement_get,
};
pub use evidence::{
    pack_evidence_entries, pack_evidence_ids, read_evidence_entries_w, read_evidence_ids,
    read_evidence_ids_w, reclaim_evidence_overflow,
};
pub use embed_queue::{
    statement_embed_queue_len, statement_embed_queue_peek, statement_embed_queue_remove,
    statement_embed_queue_remove_many,
};
pub use list::{
    statement_history, statement_list, statements_citing_memory, statements_contradicting,
    StatementListFilter, DEFAULT_LIST_LIMIT,
};
pub use supersede::{
    statement_create_with_decision, statement_supersede, JudgeError, JudgeFuture, JudgeVerdict,
    StatementJudge, StatementSimilarityCandidate, StatementSimilaritySource, SupersedeDecision,
    TieredSupersedeDecider, TieredThresholds,
};
pub use tombstone::{statement_retract, statement_tombstone};

use brain_core::{EntityId, StatementId, StatementKind};

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

#[derive(thiserror::Error, Debug)]
pub enum StatementOpError {
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),

    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),

    #[error("statement {0:?} not found")]
    NotFound(StatementId),

    #[error("statement {0:?} already exists")]
    AlreadyExists(StatementId),

    #[error("predicate {0} not registered")]
    UnknownPredicate(u32),

    #[error("subject {0:?} not registered")]
    UnknownSubject(EntityId),

    #[error("invalid argument: {0}")]
    InvalidArgument(&'static str),

    #[error("statement {0:?} already superseded by {1:?}")]
    AlreadySuperseded(StatementId, StatementId),

    #[error("statement {0:?} is tombstoned")]
    AlreadyTombstoned(StatementId),

    #[error("events cannot be superseded")]
    EventCannotSupersede,

    #[error("kind mismatch on supersede: old={old:?} new={new:?}")]
    KindMismatch {
        old: StatementKind,
        new: StatementKind,
    },

    #[error("subject mismatch on supersede")]
    SubjectMismatch,

    #[error("predicate mismatch on supersede")]
    PredicateMismatch,

    #[error("metadata row decode failed — file may be corrupt")]
    DecodeFailed,

    #[error("predicate op: {0}")]
    PredicateOp(#[from] crate::schema::predicate::PredicateOpError),

    #[error("entity op: {0}")]
    EntityOp(#[from] crate::entity::ops::EntityOpError),
}
