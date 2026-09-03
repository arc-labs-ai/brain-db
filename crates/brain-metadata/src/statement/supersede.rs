//! Statement supersession.
//!
//! The sync, in-write-txn primitive [`statement_supersede`] — the atomic
//! two-step "flip old to is_current=0, insert new with chain_root + version
//! filled in" — plus the statement-similarity + LLM-judge surfaces
//! ([`StatementSimilaritySource`], [`StatementJudge`]) the extraction path's
//! negation/disambiguation flows consult. The live supersession/contradiction
//! decision itself is made by [`crate::statement::statement_create`] (see
//! `crud.rs`); this module only supplies the primitive + the surfaces.

use std::future::Future;
use std::pin::Pin;

use brain_core::{Statement, SubjectRef};
use brain_core::{StatementId, StatementKind};
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

use crate::tables::scope::RowScope;
use crate::tables::statement::{StatementMetadata, STATEMENTS_TABLE};

use super::crud::{
    flip_by_subject_to_noncurrent, insert_new_statement, recompute_confidence_from_evidence,
    validate_statement_shape,
};
use super::StatementOpError;

// ---------------------------------------------------------------------------
// statement_supersede (sync, in-write-txn primitive — unchanged behaviour).
// ---------------------------------------------------------------------------

/// Supersede `old_id` with `new_statement`. Atomic two-step inside
/// `wtxn`: insert new (and chain row), update old in place + flip
/// `is_current` bit, set `valid_to` if not already pinned, stamp
/// `record_invalidated_at` to mark when the substrate stopped believing
/// the prior row.
pub fn statement_supersede(
    wtxn: &WriteTransaction,
    scope: RowScope,
    session: brain_core::SessionId,
    old_id: StatementId,
    new_statement: &Statement,
    now_unix_nanos: u64,
) -> Result<StatementId, StatementOpError> {
    validate_statement_shape(new_statement)?;

    // Load old.
    let mut old = {
        let t = wtxn.open_table(STATEMENTS_TABLE)?;
        let row: Option<StatementMetadata> = t.get(&old_id.to_bytes())?.map(|g| g.value());
        row.ok_or(StatementOpError::NotFound(old_id))?
    };
    // Tenant wall (authoritative). The old row's owning scope is adopted
    // for the new row + every rewritten index key, so a caller from a
    // different tenant could otherwise re-home its replacement into the
    // victim's scope. The caller must own `old`; a row owned by another
    // tenant reads as NotFound (no existence leak), exactly like a
    // missing one. Once past this guard the caller scope and the row's
    // own scope are identical, so adopting either is safe.
    if scope != old.scope() {
        return Err(StatementOpError::NotFound(old_id));
    }

    // Pre-conditions.
    if old.is_tombstoned() {
        return Err(StatementOpError::AlreadyTombstoned(old_id));
    }
    if let Some(succ) = old.superseded_by_bytes {
        return Err(StatementOpError::AlreadySuperseded(
            old_id,
            StatementId::from(succ),
        ));
    }
    let old_kind = old.kind().ok_or(StatementOpError::InvalidArgument(
        "old row has unknown kind",
    ))?;
    if old_kind == StatementKind::Event {
        return Err(StatementOpError::EventCannotSupersede);
    }
    if old_kind != new_statement.kind {
        return Err(StatementOpError::KindMismatch {
            old: old_kind,
            new: new_statement.kind,
        });
    }
    if old.subject_entity_bytes
        != match new_statement.subject {
            SubjectRef::Entity(e) => e.to_bytes(),
            SubjectRef::Pending(audit) => audit.to_bytes(),
            SubjectRef::Memory(id) => id.to_be_bytes(),
        }
    {
        return Err(StatementOpError::SubjectMismatch);
    }
    if old.predicate_id != new_statement.predicate.raw() {
        return Err(StatementOpError::PredicateMismatch);
    }

    // ID uniqueness on new.
    {
        let t = wtxn.open_table(STATEMENTS_TABLE)?;
        if t.get(&new_statement.id.to_bytes())?.is_some() {
            return Err(StatementOpError::AlreadyExists(new_statement.id));
        }
    }

    // Compute new chain_root + version.
    let chain_root_bytes = if old.supersedes_bytes.is_none() {
        old.statement_id_bytes
    } else {
        old.chain_root_bytes
    };
    let new_version = old.version.saturating_add(1);

    // Build the new row with derived fields filled in.
    let mut new_to_insert = new_statement.clone();
    new_to_insert.version = new_version;
    new_to_insert.supersedes = Some(old_id);
    new_to_insert.superseded_by = None;
    new_to_insert.chain_root = StatementId::from(chain_root_bytes);

    // Aggregate confidence over per-entry evidence metadata when
    // present (shared with statement_create — wire-vs-in-process split).
    let extracted_at = new_to_insert.extracted_at_unix_nanos;
    recompute_confidence_from_evidence(wtxn, &mut new_to_insert, extracted_at)?;

    // Update old in place — flip is_current, set valid_to (Fact /
    // Preference only) if not already pinned (caller-supplied
    // valid_to wins).
    let old_subject_bytes = old.subject_entity_bytes;
    let old_kind_byte = old.kind;
    let old_pred = old.predicate_id;
    let old_was_current = old.is_current != 0;

    old.superseded_by_bytes = Some(new_to_insert.id.to_bytes());
    if old.kind != StatementKind::Event.as_u8() && old.valid_to_unix_nanos.is_none() {
        old.valid_to_unix_nanos = Some(new_to_insert.extracted_at_unix_nanos);
    }
    // Record-time invalidation: the substrate stops believing the prior
    // row at supersession wall-clock. Callers that pass `0` ("did not
    // stamp") get the new row's extraction time instead, so the field
    // never carries a zero — zero would read as "invalidated at the
    // unix epoch", a false positive for as-of filters.
    let invalidated_at = if now_unix_nanos == 0 {
        new_to_insert.extracted_at_unix_nanos
    } else {
        now_unix_nanos
    };
    old.record_invalidated_at_unix_nanos = Some(invalidated_at);
    old.is_current = 0;

    {
        let mut t = wtxn.open_table(STATEMENTS_TABLE)?;
        t.insert(&old.statement_id_bytes, &old)?;
    }

    // Flip the by-subject index for old: current -> non-current.
    if old_was_current {
        flip_by_subject_to_noncurrent(
            wtxn,
            scope,
            old_subject_bytes,
            old_kind_byte,
            old_pred,
            &old.statement_id_bytes,
        )?;
        // The old row is no longer current — drop its predicate-bucket
        // entry before the new row (possibly) claims the same bucket
        // below. Ownership-guarded so it never evicts a sibling.
        crate::statement::remove_from_predicate_index(
            wtxn,
            scope,
            old_pred,
            old_kind_byte,
            old.confidence,
            &old.statement_id_bytes,
        )?;
    }

    // Insert new statement + all indexes. The new row carries the new
    // utterance's session (auto-supersede within `statement_create` passes
    // its create session through; the explicit supersede paths pass the
    // caller's).
    insert_new_statement(wtxn, scope, session, &new_to_insert)?;

    Ok(new_to_insert.id)
}

// ---------------------------------------------------------------------------
// Statement similarity + judge surfaces (used by the extraction path's
// negation/disambiguation flows).
// ---------------------------------------------------------------------------

/// One candidate returned by [`StatementSimilaritySource`]: the
/// existing statement that's near the new one in embedding space.
#[derive(Debug, Clone)]
pub struct StatementSimilarityCandidate {
    pub statement_id: StatementId,
    pub statement: Statement,
    /// Cosine similarity in `[-1, 1]`; `1.0` is identical. Tier
    /// thresholds compare against this value.
    pub score: f32,
}

/// Abstracts the per-shard statement HNSW so the decider stays free
/// of `brain-index` (which would invert the dep graph). Implementations
/// live in the worker / extractor crates that own the index handle.
pub trait StatementSimilaritySource {
    /// Return the top-k candidates near `query_vector`, sorted by
    /// `score` descending. The caller-supplied `rtxn` is the read
    /// txn the decider used for Tier 0; passing it through lets the
    /// source materialise full [`Statement`] rows from the same
    /// snapshot so Tier 1/2 sees consistent state.
    fn nearest(
        &self,
        rtxn: &ReadTransaction,
        query_vector: &[f32],
        k: usize,
    ) -> Result<Vec<StatementSimilarityCandidate>, StatementOpError>;
}

/// Tier 2 LLM judge. Returns one of three verdicts for a pair of
/// statements that are similar but not Tier 1 (cosine in the
/// `[judge_lower, auto_supersede)` band).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JudgeVerdict {
    Supersedes,
    Contradicts,
    Coexists,
}

/// Error surface for the judge call. Held as a boxed message string
/// to avoid coupling the metadata crate to the LLM transport's error
/// types.
#[derive(Debug, thiserror::Error)]
pub enum JudgeError {
    #[error("judge transport: {0}")]
    Transport(String),
    #[error("judge response could not be parsed: {0}")]
    Parse(String),
    #[error("judge budget exceeded: {0}")]
    Budget(String),
}

/// Future returned by the judge. Boxed so the trait stays object-safe
/// without pulling `async_trait` or `futures` into the metadata crate.
pub type JudgeFuture<'a> =
    Pin<Box<dyn Future<Output = Result<JudgeVerdict, JudgeError>> + Send + 'a>>;

/// LLM judge surface. The implementation lives in `brain-extractors`
/// where the LLM client + prompt cache + budget enforcement already
/// reside.
pub trait StatementJudge: Send + Sync {
    fn judge_supersedes<'a>(
        &'a self,
        new_stmt: &'a Statement,
        existing_stmt: &'a Statement,
        rtxn: &'a ReadTransaction,
    ) -> JudgeFuture<'a>;
}
