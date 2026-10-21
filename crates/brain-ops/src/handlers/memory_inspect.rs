//! `MEMORY_INSPECT` handler — return the durable write-artifact bundle for one
//! memory, so any memory can be inspected in a friendly per-stage view (not just
//! the one just written via the ENCODE trace).
//!
//! Read-only. It resolves the memory under the caller's `(namespace, space)`
//! scope (cross-tenant ids read as "not found", never leak), then returns the
//! memory text plus the stored artifact bundle. The bundle is populated
//! incrementally by the write path + async workers; a memory whose bundle has
//! not been written yet returns `found = true` with an empty artifact.

use brain_core::{EdgeKind, MemoryId};
use brain_metadata::tables::edge::{list_memory_edges_from, list_memory_edges_to};
use brain_metadata::tables::memory::MEMORIES_TABLE;
use brain_metadata::tables::memory_artifacts::MEMORY_ARTIFACTS_TABLE;
use brain_metadata::tables::text::TEXTS_TABLE;
use brain_protocol::envelope::response::EncodeStageArtifact;
use brain_protocol::shared::primitives::EdgeKindWire;
use brain_protocol::{EdgeDirectionWire, MemoryEdgeWire, MemoryInspectRequest, MemoryInspectResponse};

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
        edges: Vec::new(),
    };

    let caller_ns = u32::from(ctx.executor.caller_namespace);
    let caller_space: [u8; 16] = ctx.executor.caller_space.into();

    let rtxn = ctx
        .executor
        .metadata
        .read_txn()
        .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;

    // Tenancy: the memory must exist and be owned by the caller. A memory id
    // that belongs to another `(namespace, space)` is indistinguishable from a
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
                m.namespace_id == caller_ns && m.space_id_bytes == caller_space
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

    // Memory edges, both directions. These are the edges LINK/UNLINK write;
    // `artifact.graph` above is the extractor's write-time view and never
    // contains them, which left LINK write-only over the wire.
    //
    // A storage failure here is reported, not swallowed: returning a memory
    // with `edges: []` when the edge table could not be read would be
    // indistinguishable from a memory that genuinely has no edges, and a
    // caller would conclude its LINK had been lost.
    let edges = {
        let id = MemoryId::from_raw(u128::from_be_bytes(memory_id));
        let mut out = Vec::new();
        for (dir, rows) in [
            (
                EdgeDirectionWire::Outgoing,
                list_memory_edges_from(&rtxn, id, None)
                    .map_err(|e| OpError::Internal(format!("edges_from: {e}")))?,
            ),
            (
                EdgeDirectionWire::Incoming,
                list_memory_edges_to(&rtxn, id, None)
                    .map_err(|e| OpError::Internal(format!("edges_to: {e}")))?,
            ),
        ] {
            for (kind, peer, data) in rows {
                out.push(MemoryEdgeWire {
                    kind: edge_kind_to_wire(kind),
                    direction: dir,
                    peer: peer.raw(),
                    weight: data.weight,
                    created_at_unix_nanos: data.created_at_unix_nanos,
                    annotation: data.annotation,
                });
            }
        }
        out
    };

    Ok(MemoryInspectResponse {
        found: true,
        memory_id,
        text,
        artifact,
        edges,
    })
}

/// Total mapping, written out rather than cast from the discriminant so that
/// adding a kind to either enum fails this match instead of silently
/// renumbering every edge a client sees.
fn edge_kind_to_wire(kind: EdgeKind) -> EdgeKindWire {
    match kind {
        EdgeKind::Caused => EdgeKindWire::Caused,
        EdgeKind::FollowedBy => EdgeKindWire::FollowedBy,
        EdgeKind::DerivedFrom => EdgeKindWire::DerivedFrom,
        EdgeKind::SimilarTo => EdgeKindWire::SimilarTo,
        EdgeKind::Contradicts => EdgeKindWire::Contradicts,
        EdgeKind::Supports => EdgeKindWire::Supports,
        EdgeKind::References => EdgeKindWire::References,
        EdgeKind::PartOf => EdgeKindWire::PartOf,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `edge_kind_to_wire` must agree with the two enums' discriminants.
    ///
    /// The mapping is written out by hand so that adding a kind fails to
    /// compile rather than shifting numbers silently. This asserts the hand
    /// mapping is also *correct*: a transposed arm would compile fine and
    /// quietly relabel every edge a client reads — a `contradicts` edge
    /// arriving as `supports` inverts its meaning.
    #[test]
    fn edge_kind_mapping_preserves_the_discriminant() {
        let all = [
            EdgeKind::Caused,
            EdgeKind::FollowedBy,
            EdgeKind::DerivedFrom,
            EdgeKind::SimilarTo,
            EdgeKind::Contradicts,
            EdgeKind::Supports,
            EdgeKind::References,
            EdgeKind::PartOf,
        ];
        for kind in all {
            let wire = edge_kind_to_wire(kind);
            assert_eq!(
                kind as u8, wire as u8,
                "{kind:?} mapped to {wire:?}, which has a different discriminant",
            );
        }
        // And the arms are distinct — a duplicated arm would pass the check
        // above for one kind while losing another.
        let mapped: Vec<u8> = all.iter().map(|k| edge_kind_to_wire(*k) as u8).collect();
        let mut sorted = mapped.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len(), "mapping is not injective: {mapped:?}");
    }
}
