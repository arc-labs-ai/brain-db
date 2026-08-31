//! Admin memory-restore handler — the acked production trigger for the
//! FORGET soft-cascade revert.
//!
//! This is *not* a wire op. The `AdminRestore` opcode stays permanently
//! rejected on the wire (admin lives on the HTTP plane); the admin HTTP
//! listener drives `POST /v1/memories/{id}/restore` through each shard's
//! message loop, which calls [`handle_admin_restore`] on the shard
//! executor.
//!
//! The revert has two halves. This handler un-tombstones the *memory row*
//! itself through the unified writer (a WAL-durable [`Phase::RestoreMemory`]),
//! and the writer's post-commit fan-out enqueues a
//! `ForgetCascadeJob { kind: Revert }` so the already-built
//! `cascade_revert_forget` replays the soft FORGET's undo journal and
//! re-attaches every dependent statement / relation.
//!
//! A restore is refused when it cannot be honored:
//! - **hard-forgotten** — hard FORGET is the irreversible privacy escape
//!   hatch (invariant #6); its plaintext + artifacts are already purged.
//! - **past grace** — once `tombstoned_at + grace` passes, slot
//!   reclamation reaps the memory and its undo journal, so the forget is
//!   no longer reversible.

use brain_core::MemoryId;

use crate::context::OpsContext;
use crate::error::OpError;
use crate::handlers::link::downcast_writer_pub;
use crate::write::{Phase, PhaseAck, Write, WriteId};

/// What [`handle_admin_restore`] resolved the request to. The HTTP layer
/// maps each variant to a status code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdminRestoreOutcome {
    /// No such memory under this `(namespace)` — absent, foreign tenant,
    /// or the namespace was never interned. Maps to `404`.
    NotFound,
    /// The memory was soft-forgotten within grace and has been
    /// un-tombstoned; the revert cascade was enqueued. Maps to `200`.
    Restored,
    /// The memory was already active (never tombstoned). Idempotent
    /// no-op success. Maps to `200`.
    AlreadyActive,
    /// The memory was hard-forgotten — irreversible. Maps to `409`.
    HardForgotten,
    /// The soft-forget grace window has passed — irreversible. Maps to
    /// `409`.
    PastGrace,
}

/// Restore a soft-forgotten memory. Validates the id + owning namespace,
/// rejects a hard-forgotten or past-grace memory, and otherwise submits a
/// WAL-durable [`Phase::RestoreMemory`]. `grace_nanos` is the deploy-time
/// tombstone-grace window (must match the slot-reclamation window);
/// `now_unix_nanos` is the restore wall-clock.
pub async fn handle_admin_restore(
    ctx: &OpsContext,
    memory_id: MemoryId,
    namespace: &str,
    grace_nanos: u64,
    now_unix_nanos: u64,
) -> Result<AdminRestoreOutcome, OpError> {
    use brain_metadata::tables::memory::{flags, MEMORIES_TABLE};

    // Peek the memory row + resolve the owning namespace in one read txn.
    let (space_id, namespace_id, is_hard, is_active, tombstoned_at) = {
        let rtxn = ctx.executor.metadata.read_txn().map_err(|e| {
            OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
        })?;

        // Resolve the caller-named namespace. A namespace that was never
        // interned owns no memories — read as NotFound (no tenant leak).
        let resolved_ns = brain_metadata::namespace::namespace_lookup_by_name(&rtxn, namespace)
            .map_err(|e| {
                OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
            })?;
        let Some(resolved_ns) = resolved_ns else {
            return Ok(AdminRestoreOutcome::NotFound);
        };

        let t = rtxn.open_table(MEMORIES_TABLE).map_err(|e| {
            OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
        })?;
        let Some(guard) = t.get(memory_id.to_be_bytes()).map_err(|e| {
            OpError::ExecError(brain_planner::ExecError::MetadataReadFailed(e.to_string()))
        })?
        else {
            return Ok(AdminRestoreOutcome::NotFound);
        };
        let row = guard.value();

        // Tenant wall — a memory owned by another namespace is
        // indistinguishable from a missing one to this caller.
        if row.namespace_id != resolved_ns.raw() {
            return Ok(AdminRestoreOutcome::NotFound);
        }

        (
            row.space_id(),
            resolved_ns,
            row.flags & flags::HARD_FORGOTTEN != 0,
            row.flags & flags::ACTIVE != 0,
            row.tombstoned_at_unix_nanos,
        )
    };

    // Irreversible: hard forget purged the data.
    if is_hard {
        return Ok(AdminRestoreOutcome::HardForgotten);
    }

    // Idempotent no-op: the memory was never tombstoned.
    if is_active {
        return Ok(AdminRestoreOutcome::AlreadyActive);
    }

    // Past grace: slot reclamation has (or soon will have) reaped the
    // memory and its undo journal, so the forget is no longer reversible.
    if is_past_grace(tombstoned_at, grace_nanos, now_unix_nanos) {
        return Ok(AdminRestoreOutcome::PastGrace);
    }

    // Submit the WAL-durable un-tombstone. The writer's post-commit
    // fan-out enqueues the revert cascade for the dependent graph rows.
    let real_writer = downcast_writer_pub(ctx)?;
    let phase = Phase::RestoreMemory {
        id: memory_id,
        at_unix_nanos: now_unix_nanos,
    };
    let write = Write::single(WriteId::new(), space_id, phase).with_namespace(namespace_id);
    let ack = real_writer
        .submit(write)
        .await
        .map_err(|e| OpError::ExecError(brain_planner::ExecError::WriterFailed(e)))?;

    match ack.single_phase() {
        PhaseAck::MemoryRestored {
            already_active: true,
            ..
        } => Ok(AdminRestoreOutcome::AlreadyActive),
        PhaseAck::MemoryRestored { .. } => Ok(AdminRestoreOutcome::Restored),
        other => Err(OpError::Internal(format!(
            "restore returned unexpected phase ack: {other:?}"
        ))),
    }
}

/// Whether a soft-forgotten memory is past its restore window. A soft
/// tombstone always stamps `tombstoned_at`; a cleared-ACTIVE row without
/// one is malformed and treated as un-restorable rather than guessing a
/// grace window. `tombstoned_at + grace_nanos < now` is past grace.
fn is_past_grace(tombstoned_at: Option<u64>, grace_nanos: u64, now_unix_nanos: u64) -> bool {
    match tombstoned_at {
        None => true,
        Some(forgot_at) => forgot_at.saturating_add(grace_nanos) < now_unix_nanos,
    }
}

#[cfg(test)]
mod tests {
    use super::is_past_grace;

    const HOUR_NS: u64 = 3_600_000_000_000;

    #[test]
    fn within_grace_is_restorable() {
        // Forgotten 1h ago, 7-day grace, now = forgot + 1h → not past grace.
        let forgot = 1_000 * HOUR_NS;
        let grace = 7 * 24 * HOUR_NS;
        assert!(!is_past_grace(Some(forgot), grace, forgot + HOUR_NS));
    }

    #[test]
    fn beyond_grace_is_past() {
        let forgot = 1_000 * HOUR_NS;
        let grace = HOUR_NS; // 1-hour grace
                             // now is 2h after the forget → past a 1h grace window.
        assert!(is_past_grace(Some(forgot), grace, forgot + 2 * HOUR_NS));
    }

    #[test]
    fn exactly_at_expiry_is_still_restorable() {
        // `<` boundary: now == forgot + grace is NOT past grace.
        let forgot = 1_000 * HOUR_NS;
        let grace = HOUR_NS;
        assert!(!is_past_grace(Some(forgot), grace, forgot + grace));
    }

    #[test]
    fn missing_tombstoned_at_is_past_grace() {
        // A cleared-ACTIVE row with no tombstoned_at is malformed → refuse.
        assert!(is_past_grace(None, 7 * 24 * HOUR_NS, 5_000 * HOUR_NS));
    }
}
