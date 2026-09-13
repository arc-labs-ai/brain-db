//! Merge-review-queue admin handlers — list pending proposals, approve,
//! reject.
//!
//! These don't yet have wire opcodes; the CLI and future admin protocol
//! layers call into the async functions directly. The approve / reject
//! path submits through the unified writer so the underlying
//! `merge_entity` call lands in WAL + redb in one atomic transaction;
//! the list path is a plain read.

use brain_core::MergeId;
use brain_metadata::entity::merge::{MergeActor, DEFAULT_MERGE_GRACE_NANOS};
use brain_metadata::entity::review::{list_proposals_by_status, proposal_get};
use brain_metadata::tables::merge_review_queue::{proposal_status, MergeReviewProposal};

use crate::context::OpsContext;
use crate::error::OpError;
use crate::handlers::link::downcast_writer_pub;
use crate::write::{Phase, PhaseAck, Write, WriteId};

/// Default grace window for an admin-approved merge (matches
/// `DEFAULT_MERGE_GRACE_NANOS` from the metadata layer, expressed in
/// seconds for the Phase wire shape).
pub const DEFAULT_ADMIN_APPROVE_GRACE_SECS: u64 = DEFAULT_MERGE_GRACE_NANOS / 1_000_000_000;

/// Soft cap on the number of proposals returned by `list_pending_proposals`.
/// Operators paginating through more than this should bump the limit
/// (the underlying redb range scan is cheap; the cap exists to prevent
/// a runaway response on a misbehaving deployment).
pub const DEFAULT_LIST_LIMIT: usize = 256;

/// Snapshot of one queued proposal — what the admin tooling renders.
#[derive(Debug, Clone)]
pub struct MergeProposalView {
    pub proposal_id: [u8; 16],
    pub source_entity: [u8; 16],
    pub candidate_entity: [u8; 16],
    pub confidence: f32,
    pub tier_that_proposed: u8,
    pub status: u8,
    pub proposed_at_unix_nanos: u64,
    pub last_recheck_confidence: f32,
    pub last_recheck_unix_nanos: u64,
    pub resolved_at_unix_nanos: u64,
}

impl From<MergeReviewProposal> for MergeProposalView {
    fn from(p: MergeReviewProposal) -> Self {
        Self {
            proposal_id: p.proposal_id,
            source_entity: p.source_entity,
            candidate_entity: p.candidate_entity,
            confidence: p.confidence,
            tier_that_proposed: p.tier_that_proposed,
            status: p.status,
            proposed_at_unix_nanos: p.proposed_at_unix_nanos,
            last_recheck_confidence: p.last_recheck_confidence,
            last_recheck_unix_nanos: p.last_recheck_unix_nanos,
            resolved_at_unix_nanos: p.resolved_at_unix_nanos,
        }
    }
}

/// Ack returned from an admin-approve.
#[derive(Debug, Clone)]
pub struct ApproveMergeAck {
    pub proposal_id: [u8; 16],
    pub audit_id: [u8; 16],
}

/// List Pending proposals on the merge-review queue, up to `limit`.
/// Pass `None` for the spec default.
pub async fn handle_admin_list_merge_proposals(
    limit: Option<usize>,
    ctx: &OpsContext,
) -> Result<Vec<MergeProposalView>, OpError> {
    let limit = limit.unwrap_or(DEFAULT_LIST_LIMIT);
    let rows = {
        let rtxn = ctx
            .executor
            .metadata
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
        list_proposals_by_status(&rtxn, proposal_status::PENDING, limit)
            .map_err(|e| OpError::Internal(format!("list_proposals: {e}")))?
    };
    Ok(rows.into_iter().map(MergeProposalView::from).collect())
}

/// Approve a Pending merge proposal. Builds a `Phase::ApproveMerge` and
/// submits through the writer, so the underlying `merge_entity` call
/// lands in one wtxn alongside the proposal's terminal stamp.
pub async fn handle_admin_approve_merge(
    proposal_id: MergeId,
    ctx: &OpsContext,
) -> Result<ApproveMergeAck, OpError> {
    // Cheap pre-check so operators see "not found" instead of a generic
    // submit failure when the id is bogus.
    {
        let rtxn = ctx
            .executor
            .metadata
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
        let row = proposal_get(&rtxn, proposal_id)
            .map_err(|e| OpError::Internal(format!("proposal_get: {e}")))?;
        let Some(row) = row else {
            return Err(OpError::NotFound {
                what: "merge_proposal",
                detail: format!("{proposal_id:?}"),
            });
        };
        if row.is_terminal() {
            return Err(OpError::Conflict(format!(
                "proposal already in terminal state {}",
                row.status
            )));
        }
    }

    let now = crate::txn::now_unix_nanos_pub();
    let actor = MergeActor::Space(ctx.executor.caller_space.into());
    let phase = Phase::ApproveMerge {
        proposal_id,
        actor,
        grace_seconds: DEFAULT_ADMIN_APPROVE_GRACE_SECS,
        at_unix_nanos: now,
    };
    let write = Write::single(WriteId::new(), ctx.executor.caller_space, phase);
    let real_writer = downcast_writer_pub(ctx)?;
    let ack = real_writer
        .submit(write)
        .await
        .map_err(|e| OpError::Internal(format!("submit: {e}")))?;
    match ack.single_phase() {
        PhaseAck::MergeProposalApproved {
            proposal_id: pid,
            audit_id,
        } => Ok(ApproveMergeAck {
            proposal_id: pid.to_bytes(),
            audit_id: audit_id.to_bytes(),
        }),
        other => Err(OpError::Internal(format!(
            "unexpected phase ack for ApproveMerge: {other:?}"
        ))),
    }
}

/// Reject a Pending merge proposal. Stamps Rejected; doesn't touch the
/// source / candidate entities.
pub async fn handle_admin_reject_merge(
    proposal_id: MergeId,
    ctx: &OpsContext,
) -> Result<(), OpError> {
    {
        let rtxn = ctx
            .executor
            .metadata
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
        let row = proposal_get(&rtxn, proposal_id)
            .map_err(|e| OpError::Internal(format!("proposal_get: {e}")))?;
        let Some(row) = row else {
            return Err(OpError::NotFound {
                what: "merge_proposal",
                detail: format!("{proposal_id:?}"),
            });
        };
        if row.is_terminal() {
            return Err(OpError::Conflict(format!(
                "proposal already in terminal state {}",
                row.status
            )));
        }
    }

    let now = crate::txn::now_unix_nanos_pub();
    let phase = Phase::RejectMerge {
        proposal_id,
        at_unix_nanos: now,
    };
    let write = Write::single(WriteId::new(), ctx.executor.caller_space, phase);
    let real_writer = downcast_writer_pub(ctx)?;
    let ack = real_writer
        .submit(write)
        .await
        .map_err(|e| OpError::Internal(format!("submit: {e}")))?;
    match ack.single_phase() {
        PhaseAck::MergeProposalRejected { .. } => Ok(()),
        other => Err(OpError::Internal(format!(
            "unexpected phase ack for RejectMerge: {other:?}"
        ))),
    }
}
