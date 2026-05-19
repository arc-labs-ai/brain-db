//! Build an `ExecutorContext` clone attached to a `TxnSnapshot`
//! reflecting the current state of an active txn's buffer.
//!
//! The snapshot is read-only; PLAN/REASON/RECALL consult it to layer
//! pending writes on committed state without touching redb (spec
//! §09/08 §5: read-your-writes within a txn).

use std::sync::Arc;

use brain_planner::{ExecutorContext, PendingMemorySnapshot, TxnSnapshot};

use crate::context::OpsContext;
use crate::error::OpError;

/// Build a clone of `ctx.executor` with a snapshot of the txn's
/// buffer attached. Returns the original executor if `txn_id` is
/// `None`. Errors with `TxnNotFound` if the id was never created,
/// `TxnExpired` if it existed but is no longer Active.
pub fn build_executor_with_lens(
    ctx: &OpsContext,
    txn_id: Option<[u8; 16]>,
) -> Result<ExecutorContext, OpError> {
    let Some(txn_id) = txn_id else {
        return Ok(ctx.executor.clone());
    };
    let _ = ctx.txn_store.validate_active(txn_id)?;
    let snap = ctx.txn_store.with_buffer(txn_id, |buf| {
        let mut pending_links: Vec<(
            brain_core::MemoryId,
            brain_core::EdgeKind,
            brain_core::MemoryId,
            f32,
        )> = buf
            .links
            .iter()
            .map(|l| (l.source, l.kind, l.target, l.weight))
            .collect();
        // Inline encode-edges are also pending links.
        for enc in &buf.encodes {
            for edge in &enc.edges {
                pending_links.push((enc.memory_id, edge.kind, edge.target, edge.weight));
            }
        }
        let mut pending_memories = std::collections::HashMap::new();
        for enc in &buf.encodes {
            pending_memories.insert(
                enc.memory_id,
                PendingMemorySnapshot {
                    vector: enc.vector,
                    salience: enc.salience_initial,
                    kind: enc.kind,
                    context_id: enc.context_id,
                    created_at_unix_nanos: enc.created_at_unix_nanos,
                },
            );
        }
        Ok(TxnSnapshot {
            pending_links,
            pending_unlinks: buf.unlinked_edges.clone(),
            pending_memories,
            tombstoned: buf.tombstoned.clone(),
        })
    })?;
    Ok(ctx.executor.clone().with_txn(Arc::new(snap)))
}
