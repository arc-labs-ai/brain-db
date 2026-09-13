//! Merge-review-queue CRUD helpers — free functions over
//! `WriteTransaction` / `ReadTransaction` so callers can compose them
//! into multi-table writes (the resolver enqueues alongside an entity
//! create; the worker promotes alongside a `merge_entity` call).
//!
//! The queue itself is two tables — `MERGE_REVIEW_QUEUE_TABLE` keyed on
//! the proposal id and `MERGE_REVIEW_BY_STATUS_TABLE` for the by-status
//! scan. Both updates happen in the same wtxn so the index never drifts
//! from the primary.

use brain_core::{EntityId, MergeId};
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

use crate::tables::merge_review_queue::{
    proposal_status, MergeReviewProposal, MERGE_REVIEW_BY_STATUS_TABLE, MERGE_REVIEW_QUEUE_TABLE,
};

/// Errors from the review-queue layer. All "row missing" / "wrong
/// state" cases are structured so callers can map them to wire errors
/// without parsing strings.
#[derive(thiserror::Error, Debug)]
pub enum MergeReviewError {
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),

    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),

    #[error("proposal {0:?} not found")]
    NotFound(MergeId),

    #[error("proposal {id:?} is in terminal state {status} — already resolved")]
    AlreadyResolved { id: MergeId, status: u8 },
}

/// Enqueue a fresh `Pending` merge proposal. The caller mints the
/// `proposal_id` (`MergeId::new()` mints a UUIDv7) and passes the
/// recovered cosine, the tier byte, and a server clock. Returns the
/// proposal id for symmetry with the wire surface.
///
/// Idempotency: if a proposal with the same id already exists, the
/// caller's row replaces it. Callers minting a fresh `MergeId` per
/// resolve attempt avoid the conflict; the worker re-evaluating an
/// existing proposal uses [`update_proposal_status`] instead of
/// re-inserting.
pub fn enqueue_merge_proposal(
    wtxn: &WriteTransaction,
    proposal_id: MergeId,
    source: EntityId,
    candidate: EntityId,
    confidence: f32,
    tier_that_proposed: u8,
    now_unix_nanos: u64,
) -> Result<MergeId, MergeReviewError> {
    let row = MergeReviewProposal::new_pending(
        proposal_id.to_bytes(),
        source.to_bytes(),
        candidate.to_bytes(),
        confidence,
        tier_that_proposed,
        now_unix_nanos,
    );
    {
        let mut t = wtxn.open_table(MERGE_REVIEW_QUEUE_TABLE)?;
        t.insert(&row.proposal_id, &row)?;
    }
    {
        let mut s = wtxn.open_table(MERGE_REVIEW_BY_STATUS_TABLE)?;
        s.insert(&(row.status, row.proposal_id), &())?;
    }
    Ok(proposal_id)
}

/// Read a proposal by id. Returns `Ok(None)` when the row doesn't exist.
pub fn proposal_get(
    rtxn: &ReadTransaction,
    proposal_id: MergeId,
) -> Result<Option<MergeReviewProposal>, MergeReviewError> {
    let t = rtxn.open_table(MERGE_REVIEW_QUEUE_TABLE)?;
    let row = t.get(&proposal_id.to_bytes())?.map(|g| g.value());
    Ok(row)
}

/// Inside-wtxn variant of [`proposal_get`] for callers driving the txn
/// (worker promotion path needs read + write atomic).
pub fn proposal_get_inside_wtxn(
    wtxn: &WriteTransaction,
    proposal_id: MergeId,
) -> Result<Option<MergeReviewProposal>, MergeReviewError> {
    let t = wtxn.open_table(MERGE_REVIEW_QUEUE_TABLE)?;
    let row = t.get(&proposal_id.to_bytes())?.map(|g| g.value());
    Ok(row)
}

/// List up to `limit` proposals in the given status. Returns rows in
/// `proposal_id` order (UUIDv7, which is roughly time-ordered).
pub fn list_proposals_by_status(
    rtxn: &ReadTransaction,
    status: u8,
    limit: usize,
) -> Result<Vec<MergeReviewProposal>, MergeReviewError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let s = rtxn.open_table(MERGE_REVIEW_BY_STATUS_TABLE)?;
    let q = rtxn.open_table(MERGE_REVIEW_QUEUE_TABLE)?;
    let lo = (status, [0u8; 16]);
    let hi = (status, [0xFFu8; 16]);
    let mut out: Vec<MergeReviewProposal> = Vec::new();
    for entry in s.range(lo..=hi)? {
        let (k, _) = entry?;
        let (_, id_bytes) = k.value();
        if let Some(row) = q.get(&id_bytes)?.map(|g| g.value()) {
            out.push(row);
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(out)
}

/// Transition a proposal to a terminal status. Used by the worker
/// (auto-apply / reject / expire) and the admin path (manual approve /
/// reject). Also records the recheck cosine + recheck timestamp so
/// operators see why the worker took the action.
///
/// Idempotency: writing the same terminal status twice produces the
/// same row contents, so re-runs of a worker tick that already promoted
/// a proposal are safe.
#[allow(clippy::too_many_arguments)]
pub fn update_proposal_status(
    wtxn: &WriteTransaction,
    proposal_id: MergeId,
    new_status: u8,
    last_recheck_confidence: f32,
    now_unix_nanos: u64,
) -> Result<MergeReviewProposal, MergeReviewError> {
    let id_bytes = proposal_id.to_bytes();
    let current = {
        let t = wtxn.open_table(MERGE_REVIEW_QUEUE_TABLE)?;
        let row = t
            .get(&id_bytes)?
            .map(|g| g.value())
            .ok_or(MergeReviewError::NotFound(proposal_id))?;
        row
    };
    let old_status = current.status;
    let mut next = current;
    next.status = new_status;
    next.last_recheck_confidence = last_recheck_confidence;
    next.last_recheck_unix_nanos = now_unix_nanos;
    if new_status != proposal_status::PENDING {
        next.resolved_at_unix_nanos = now_unix_nanos;
    }
    {
        let mut t = wtxn.open_table(MERGE_REVIEW_QUEUE_TABLE)?;
        t.insert(&id_bytes, &next)?;
    }
    if old_status != new_status {
        let mut s = wtxn.open_table(MERGE_REVIEW_BY_STATUS_TABLE)?;
        s.remove(&(old_status, id_bytes))?;
        s.insert(&(new_status, id_bytes), &())?;
    }
    Ok(next)
}

/// Variant for the worker's "no transition — just re-checked, still
/// pending" path. Updates the recheck cosine + timestamp without
/// touching the status index.
pub fn update_proposal_recheck(
    wtxn: &WriteTransaction,
    proposal_id: MergeId,
    last_recheck_confidence: f32,
    now_unix_nanos: u64,
) -> Result<(), MergeReviewError> {
    let id_bytes = proposal_id.to_bytes();
    let current = {
        let t = wtxn.open_table(MERGE_REVIEW_QUEUE_TABLE)?;
        let row = t
            .get(&id_bytes)?
            .map(|g| g.value())
            .ok_or(MergeReviewError::NotFound(proposal_id))?;
        row
    };
    let mut next = current;
    next.last_recheck_confidence = last_recheck_confidence;
    next.last_recheck_unix_nanos = now_unix_nanos;
    let mut t = wtxn.open_table(MERGE_REVIEW_QUEUE_TABLE)?;
    t.insert(&id_bytes, &next)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::merge_review_queue::proposal_tier;
    use crate::MetadataDb;
    use tempfile::TempDir;

    const NOW: u64 = 1_700_000_000_000_000_000;

    fn db(dir: &TempDir) -> MetadataDb {
        MetadataDb::open(dir.path().join("metadata.redb")).expect("open")
    }

    #[test]
    fn enqueue_and_list() {
        let dir = TempDir::new().unwrap();
        let d = db(&dir);
        let source = EntityId::new();
        let candidate = EntityId::new();
        let pid = MergeId::new();
        {
            let wtxn = d.write_txn().unwrap();
            enqueue_merge_proposal(
                &wtxn,
                pid,
                source,
                candidate,
                0.82,
                proposal_tier::EMBEDDING,
                NOW,
            )
            .unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = d.read_txn().unwrap();
        let got = proposal_get(&rtxn, pid).unwrap().unwrap();
        assert_eq!(got.source_entity, source.to_bytes());
        assert_eq!(got.candidate_entity, candidate.to_bytes());
        assert!((got.confidence - 0.82).abs() < 1e-6);
        assert!(got.is_pending());

        let pending = list_proposals_by_status(&rtxn, proposal_status::PENDING, 10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].proposal_id, pid.to_bytes());
    }

    #[test]
    fn update_promotes_status_and_flips_index() {
        let dir = TempDir::new().unwrap();
        let d = db(&dir);
        let pid = MergeId::new();
        {
            let wtxn = d.write_txn().unwrap();
            enqueue_merge_proposal(
                &wtxn,
                pid,
                EntityId::new(),
                EntityId::new(),
                0.82,
                proposal_tier::EMBEDDING,
                NOW,
            )
            .unwrap();
            wtxn.commit().unwrap();
        }
        {
            let wtxn = d.write_txn().unwrap();
            let updated = update_proposal_status(
                &wtxn,
                pid,
                proposal_status::AUTO_APPLIED,
                0.97,
                NOW + 60_000_000_000,
            )
            .unwrap();
            assert_eq!(updated.status, proposal_status::AUTO_APPLIED);
            assert!((updated.last_recheck_confidence - 0.97).abs() < 1e-6);
            assert_eq!(updated.resolved_at_unix_nanos, NOW + 60_000_000_000);
            wtxn.commit().unwrap();
        }
        let rtxn = d.read_txn().unwrap();
        let pending = list_proposals_by_status(&rtxn, proposal_status::PENDING, 10).unwrap();
        assert!(pending.is_empty());
        let auto = list_proposals_by_status(&rtxn, proposal_status::AUTO_APPLIED, 10).unwrap();
        assert_eq!(auto.len(), 1);
        assert_eq!(auto[0].proposal_id, pid.to_bytes());
    }

    #[test]
    fn update_recheck_keeps_index() {
        let dir = TempDir::new().unwrap();
        let d = db(&dir);
        let pid = MergeId::new();
        {
            let wtxn = d.write_txn().unwrap();
            enqueue_merge_proposal(
                &wtxn,
                pid,
                EntityId::new(),
                EntityId::new(),
                0.82,
                proposal_tier::EMBEDDING,
                NOW,
            )
            .unwrap();
            wtxn.commit().unwrap();
        }
        {
            let wtxn = d.write_txn().unwrap();
            update_proposal_recheck(&wtxn, pid, 0.85, NOW + 1_000_000_000).unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = d.read_txn().unwrap();
        let got = proposal_get(&rtxn, pid).unwrap().unwrap();
        assert_eq!(got.status, proposal_status::PENDING);
        assert!((got.last_recheck_confidence - 0.85).abs() < 1e-6);
        assert_eq!(got.last_recheck_unix_nanos, NOW + 1_000_000_000);
        let pending = list_proposals_by_status(&rtxn, proposal_status::PENDING, 10).unwrap();
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn update_unknown_proposal_errors() {
        let dir = TempDir::new().unwrap();
        let d = db(&dir);
        let wtxn = d.write_txn().unwrap();
        let err =
            update_proposal_status(&wtxn, MergeId::new(), proposal_status::APPROVED, 0.5, NOW)
                .unwrap_err();
        assert!(matches!(err, MergeReviewError::NotFound(_)));
    }
}
