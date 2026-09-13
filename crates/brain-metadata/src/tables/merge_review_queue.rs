//! `merge_review_queue` — entity-merge proposals in the [0.7, 0.95)
//! confidence band.
//!
//! ## Why this exists
//!
//! The resolver's auto-merge path requires a strong signal (cosine ≥
//! ~0.95 after Tier 3 embedding). Below that band, the resolver still
//! sees the close-but-not-confident candidate and would either drop it
//! on the floor (losing the signal) or aggressively merge (corrupting
//! the graph). The middle ground: park the proposal on a durable
//! queue, mint a fresh entity for the current write so progress is not
//! blocked, and let a periodic worker re-check the proposal as the
//! entity HNSW grows. When the recomputed cosine clears the
//! auto-apply threshold, the worker promotes the proposal to an actual
//! merge; when it drops below the floor, the proposal is rejected;
//! after a long timeout the proposal expires.
//!
//! ## Key layout
//!
//! Primary table: `proposal_id → MergeReviewProposal`. Reads keyed on
//! the proposal id (admin "approve / reject by id", worker "re-check
//! this proposal").
//!
//! Secondary index by status: `(status_byte, proposal_id) → ()`. The
//! worker scans `(Pending, _) ..= (Pending, _)` to find pending rows
//! without reading every proposal; admin "list pending" issues the
//! same range query.

use crate::impl_redb_rkyv_value;
use redb::TableDefinition;

/// `proposal_id → MergeReviewProposal`. Primary store.
pub const MERGE_REVIEW_QUEUE_TABLE: TableDefinition<'static, [u8; 16], MergeReviewProposal> =
    TableDefinition::new("merge_review_queue");

/// `(status_byte, proposal_id) → ()`. Scan-by-status index.
pub const MERGE_REVIEW_BY_STATUS_TABLE: TableDefinition<'static, (u8, [u8; 16]), ()> =
    TableDefinition::new("merge_review_by_status");

// ---------------------------------------------------------------------------
// Status.
// ---------------------------------------------------------------------------

/// Status of a queued merge proposal. The byte values are the on-disk
/// encoding — the secondary index keys on them directly.
pub mod proposal_status {
    /// Awaiting re-evaluation or operator decision.
    pub const PENDING: u8 = 0;
    /// Worker promoted the proposal to an actual merge after the
    /// recomputed cosine cleared the auto-apply threshold.
    pub const AUTO_APPLIED: u8 = 1;
    /// Admin manually approved — the merge has been applied.
    pub const APPROVED: u8 = 2;
    /// Admin manually rejected, or worker observed the recomputed cosine
    /// drop below the partial-match floor.
    pub const REJECTED: u8 = 3;
    /// Sat in the queue past `expire_after_secs` without ever clearing
    /// either threshold. The worker writes this terminal state and
    /// never re-visits the row.
    pub const EXPIRED: u8 = 4;
}

/// Tier that originally produced the proposal — recorded on the
/// proposal row so an operator can interpret the signal strength.
pub mod proposal_tier {
    pub const EXACT: u8 = 1;
    pub const ALIAS: u8 = 2;
    pub const EMBEDDING: u8 = 3;
    pub const LLM: u8 = 4;
}

// ---------------------------------------------------------------------------
// MergeReviewProposal.
// ---------------------------------------------------------------------------

/// One entity-merge proposal in the confidence-band queue.
///
/// `source_entity` is the entity the resolver just created (or wanted
/// to create) for the surface form being processed; `candidate_entity`
/// is the close-but-not-confident existing entity the resolver looked
/// at. Promotion of the proposal merges `source_entity` *into*
/// `candidate_entity` — the candidate is canonical because it pre-dated
/// the source.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct MergeReviewProposal {
    pub proposal_id: [u8; 16],
    pub source_entity: [u8; 16],
    pub candidate_entity: [u8; 16],
    /// Cosine (or equivalent) at proposal time. Always in `[0.0, 1.0]`.
    pub confidence: f32,
    /// See [`proposal_tier`].
    pub tier_that_proposed: u8,
    pub proposed_at_unix_nanos: u64,
    /// See [`proposal_status`].
    pub status: u8,
    /// `0` while pending; the resolution time (auto-apply / approve /
    /// reject / expire) once terminal.
    pub resolved_at_unix_nanos: u64,
    /// Cosine recomputed by the worker the last time it re-evaluated
    /// the proposal. `0.0` until the first worker tick visits the row.
    /// Lets operators see why a proposal is sticking in `Pending`.
    pub last_recheck_confidence: f32,
    /// Server clock of the most recent worker re-evaluation. `0` until
    /// the first tick visits the row.
    pub last_recheck_unix_nanos: u64,
}

impl MergeReviewProposal {
    /// Build a fresh `Pending` proposal with the supplied tier confidence.
    #[must_use]
    pub fn new_pending(
        proposal_id: [u8; 16],
        source_entity: [u8; 16],
        candidate_entity: [u8; 16],
        confidence: f32,
        tier_that_proposed: u8,
        now_unix_nanos: u64,
    ) -> Self {
        Self {
            proposal_id,
            source_entity,
            candidate_entity,
            confidence,
            tier_that_proposed,
            proposed_at_unix_nanos: now_unix_nanos,
            status: proposal_status::PENDING,
            resolved_at_unix_nanos: 0,
            last_recheck_confidence: 0.0,
            last_recheck_unix_nanos: 0,
        }
    }

    #[must_use]
    pub fn is_pending(&self) -> bool {
        self.status == proposal_status::PENDING
    }

    #[must_use]
    pub fn is_terminal(&self) -> bool {
        !self.is_pending()
    }
}

impl_redb_rkyv_value!(MergeReviewProposal, "brain_metadata::MergeReviewProposal");

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::fresh_db;
    use redb::ReadableDatabase;

    fn sample_proposal(status: u8) -> MergeReviewProposal {
        let mut p = MergeReviewProposal::new_pending(
            [7u8; 16],
            [1u8; 16],
            [2u8; 16],
            0.82,
            proposal_tier::EMBEDDING,
            1_700_000_000_000_000_000,
        );
        p.status = status;
        if status != proposal_status::PENDING {
            p.resolved_at_unix_nanos = p.proposed_at_unix_nanos + 60_000_000_000;
        }
        p
    }

    #[test]
    fn round_trip_pending() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let p = sample_proposal(proposal_status::PENDING);

        let wtxn = db.begin_write().unwrap();
        {
            let mut t = wtxn.open_table(MERGE_REVIEW_QUEUE_TABLE).unwrap();
            t.insert(&p.proposal_id, &p).unwrap();
        }
        {
            let mut s = wtxn.open_table(MERGE_REVIEW_BY_STATUS_TABLE).unwrap();
            s.insert(&(p.status, p.proposal_id), &()).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let t = rtxn.open_table(MERGE_REVIEW_QUEUE_TABLE).unwrap();
        let got = t.get(&p.proposal_id).unwrap().unwrap().value();
        assert_eq!(got, p);
        assert!(got.is_pending());
        assert!(!got.is_terminal());

        let s = rtxn.open_table(MERGE_REVIEW_BY_STATUS_TABLE).unwrap();
        let mut seen: Vec<[u8; 16]> = Vec::new();
        for entry in s
            .range((proposal_status::PENDING, [0u8; 16])..=(proposal_status::PENDING, [0xFFu8; 16]))
            .unwrap()
        {
            let (k, _) = entry.unwrap();
            seen.push(k.value().1);
        }
        assert_eq!(seen, vec![p.proposal_id]);
    }

    #[test]
    fn terminal_statuses_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        for st in [
            proposal_status::AUTO_APPLIED,
            proposal_status::APPROVED,
            proposal_status::REJECTED,
            proposal_status::EXPIRED,
        ] {
            let mut p = sample_proposal(st);
            // Use a unique id per status so all four coexist.
            p.proposal_id = [st; 16];
            let wtxn = db.begin_write().unwrap();
            {
                let mut t = wtxn.open_table(MERGE_REVIEW_QUEUE_TABLE).unwrap();
                t.insert(&p.proposal_id, &p).unwrap();
            }
            wtxn.commit().unwrap();
            let rtxn = db.begin_read().unwrap();
            let t = rtxn.open_table(MERGE_REVIEW_QUEUE_TABLE).unwrap();
            let got = t.get(&p.proposal_id).unwrap().unwrap().value();
            assert_eq!(got.status, st);
            assert!(got.is_terminal());
        }
    }
}
