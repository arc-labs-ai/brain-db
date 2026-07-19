//! `MEMORY_INSPECT` handler — return the durable write-artifact bundle for one
//! memory, so any memory can be inspected in a friendly per-stage view (not just
//! the one just written via the ENCODE trace).
//!
//! Read-only. It resolves the memory under the caller's `(namespace, agent)`
//! scope (cross-tenant ids read as "not found", never leak), then returns the
//! memory text plus the stored artifact bundle. The bundle is populated
//! incrementally by the write path + async workers; a memory whose bundle has
//! not been written yet returns `found = true` with an empty artifact.

use brain_metadata::tables::memory::MEMORIES_TABLE;
use brain_metadata::tables::memory_artifacts::MEMORY_ARTIFACTS_TABLE;
use brain_metadata::tables::text::TEXTS_TABLE;
use brain_protocol::envelope::response::EncodeStageArtifact;
use brain_protocol::{MemoryInspectRequest, MemoryInspectResponse};

use crate::context::OpsContext;
use crate::error::OpError;

pub async fn handle_memory_inspect(
    req: MemoryInspectRequest,
    ctx: &OpsContext,
) -> Result<MemoryInspectResponse, OpError> {
    let memory_id = req.memory_id;
    let not_found = || MemoryInspectResponse {
        found: false,
        memory_id,
        text: String::new(),
        artifact: EncodeStageArtifact::default(),
    };

    let caller_ns = u32::from(ctx.executor.caller_namespace);
    let caller_agent: [u8; 16] = ctx.executor.caller_agent.into();

    let rtxn = ctx
        .executor
        .metadata
        .read_txn()
        .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;

    // Tenancy: the memory must exist and be owned by the caller. A memory id
    // that belongs to another `(namespace, agent)` is indistinguishable from a
    // missing one to this caller.
    {
        let memories = rtxn
            .open_table(MEMORIES_TABLE)
            .map_err(|e| OpError::Internal(format!("open memories: {e}")))?;
        let owned = memories
            .get(&memory_id)
            .map_err(|e| OpError::Internal(format!("memory read: {e}")))?
            .map(|g| {
                let m = g.value();
                m.namespace_id == caller_ns && m.agent_id_bytes == caller_agent
            })
            .unwrap_or(false);
        if !owned {
            return Ok(not_found());
        }
    }

    let text = {
        let texts = rtxn
            .open_table(TEXTS_TABLE)
            .map_err(|e| OpError::Internal(format!("open texts: {e}")))?;
        texts
            .get(&memory_id)
            .map_err(|e| OpError::Internal(format!("text read: {e}")))?
            .and_then(|g| String::from_utf8(g.value().to_vec()).ok())
            .unwrap_or_default()
    };

    let artifact = {
        let bundles = rtxn
            .open_table(MEMORY_ARTIFACTS_TABLE)
            .map_err(|e| OpError::Internal(format!("open memory_artifacts: {e}")))?;
        bundles
            .get(&memory_id)
            .map_err(|e| OpError::Internal(format!("artifact read: {e}")))?
            .and_then(|g| serde_json::from_str::<EncodeStageArtifact>(g.value()).ok())
            .unwrap_or_default()
    };

    Ok(MemoryInspectResponse {
        found: true,
        memory_id,
        text,
        artifact,
    })
}
