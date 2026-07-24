//! Shared memory-cascade helper for SPACE_DELETE / SESSION_DELETE.
//!
//! Enumerates the caller's `(namespace, space)` memory timeline (optionally
//! narrowed to one session) and hard/soft-tombstones each memory through the
//! single `submit(Write)` path. Each `Phase::Tombstone(Memory)` fans out to
//! the existing FORGET cascade post-commit, which tears down the backing
//! entity/statement/relation graph rows — so a scope delete reuses the FORGET
//! machinery rather than re-implementing a graph teardown.

use brain_core::MemoryId;
use brain_metadata::tables::memory::{
    space_timeline_prefix_space, SPACE_TIMELINE_KEY_LEN, MEMORIES_BY_SPACE_TIMELINE_TABLE,
};
use crate::context::OpsContext;
use crate::error::OpError;
use crate::handlers::link::downcast_writer_pub;
use crate::write::phase::TombstoneMode;
use crate::write::{Phase, TombstoneTarget, Write, WriteId};

/// Map a writer error to an `OpError`.
pub(super) fn writer_err(e: brain_planner::WriterError) -> OpError {
    OpError::ExecError(brain_planner::ExecError::WriterFailed(e))
}

/// Collect the active memory ids under the caller's `(namespace, space)`,
/// optionally filtered to `session_filter`, and tombstone each. Returns the
/// number of memories tombstoned.
///
/// TODO(tenancy): a very large space delete should chunk this scan behind a
/// resumable cursor (one WAL record with a cursor) rather than tombstoning
/// every memory in one synchronous pass. v1 is synchronous.
pub(super) async fn tombstone_space_memories(
    ctx: &OpsContext,
    hard: bool,
    session_filter: Option<u64>,
) -> Result<u64, OpError> {
    let ns = ctx.executor.caller_namespace.raw();
    let space = ctx.executor.caller_space;
    let space_bytes: [u8; 16] = space.into();

    // Scope-isolated range over the (namespace, space) timeline prefix: a
    // present timeline row is an active memory (tombstone removes the row).
    let prefix = space_timeline_prefix_space(ns, space_bytes);
    let mut start = [0u8; SPACE_TIMELINE_KEY_LEN];
    start[..20].copy_from_slice(&prefix);
    let mut end = [0xFFu8; SPACE_TIMELINE_KEY_LEN];
    end[..20].copy_from_slice(&prefix);

    let mut ids: Vec<MemoryId> = Vec::new();
    {
        let rtxn = ctx
            .executor
            .metadata
            .read_txn()
            .map_err(|e| OpError::Internal(format!("cascade read: {e}")))?;
        let t = rtxn
            .open_table(MEMORIES_BY_SPACE_TIMELINE_TABLE)
            .map_err(|e| OpError::Internal(format!("open timeline: {e}")))?;
        for entry in t
            .range::<&[u8]>((&start[..])..=(&end[..]))
            .map_err(|e| OpError::Internal(format!("timeline range: {e}")))?
        {
            let (k, _) = entry.map_err(|e| OpError::Internal(format!("timeline row: {e}")))?;
            let key = k.value();
            if let Some(want) = session_filter {
                let session = u64::from_be_bytes(key[28..36].try_into().unwrap());
                if session != want {
                    continue;
                }
            }
            let mut mid = [0u8; 16];
            mid.copy_from_slice(&key[36..52]);
            ids.push(MemoryId::from_be_bytes(mid));
        }
    }

    let mode = if hard {
        TombstoneMode::Hard
    } else {
        TombstoneMode::Soft
    };
    let writer = downcast_writer_pub(ctx)?;
    let now = crate::clock::now_unix_nanos();
    let mut count = 0u64;
    for id in ids {
        let phase = Phase::Tombstone {
            target: TombstoneTarget::Memory { id, mode },
            reason: 1, // ClientRequest
            at_unix_nanos: now,
        };
        let write = Write::single(WriteId::new(), space, phase)
            .with_namespace(ctx.executor.caller_namespace);
        writer.submit(write).await.map_err(writer_err)?;
        count += 1;
    }
    Ok(count)
}
