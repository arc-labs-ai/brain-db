//! `MEMORY_INSPECT` handler — return the durable write-artifact bundle for one
//! memory, so any memory can be inspected in a friendly per-stage view (not just
//! the one just written via the ENCODE trace).
//!
//! Read-only. It resolves the memory under the caller's `(namespace, space)`
//! scope, then returns the memory text plus the stored artifact bundle.
//!
//! An id that does not exist — or belongs to another `(namespace, space)`,
//! which is deliberately indistinguishable to this caller — is
//! `MemoryNotFound`. It used to be a success carrying `found: false`, the only
//! such flag in the protocol; every sibling read already answered with the
//! `NotFound` category, and a client should not need a second mechanism for
//! one verb.
//!
//! The bundle is populated incrementally by the write path + async workers; a
//! memory whose bundle has not been written yet is still a success, with an
//! empty artifact. That is a different condition from the memory being
//! absent, and keeping the two apart is the point.
//!
//! The edge view is capped per direction, and the response carries the full
//! count and a truncation flag.

use brain_core::{EdgeKind, MemoryId};
use brain_metadata::tables::edge::{list_memory_edges_from, list_memory_edges_to, MemoryEdgePage};
use brain_metadata::tables::memory::MEMORIES_TABLE;
use brain_metadata::tables::memory_artifacts::MEMORY_ARTIFACTS_TABLE;
use brain_metadata::tables::text::TEXTS_TABLE;
use brain_protocol::envelope::response::EncodeStageArtifact;
use brain_protocol::shared::primitives::EdgeKindWire;
use brain_protocol::{
    EdgeDirectionWire, MemoryEdgeWire, MemoryInspectRequest, MemoryInspectResponse,
};

use crate::context::OpsContext;
use crate::error::OpError;

/// Worst-case CBOR size of one `MemoryEdgeWire` carrying no annotation: the map
/// header, six text keys with their head bytes, a `u128` peer encoded as a
/// bignum, an `f32` weight, a `u64` timestamp, two one-byte enums, and a null
/// annotation. Those parts sum under 128 bytes. No write path sets an
/// annotation on a memory-to-memory edge — the extractor's annotated edges
/// anchor on entities, which this view excludes — but the wire type permits
/// one, so this is a worst case for the rows this verb can actually return and
/// not for the type.
const MEMORY_EDGE_WIRE_MAX_BYTES: usize = 128;

/// Edges per direction a frame could carry if the response held nothing else.
/// It holds a great deal else, so this is a ceiling the per-direction cap must
/// stay under, never the cap itself.
const FRAME_EDGE_CEILING_PER_DIRECTION: usize =
    brain_protocol::MAX_PAYLOAD_BYTES / MEMORY_EDGE_WIRE_MAX_BYTES / 2;

/// Per-direction cap on the memory-to-memory edges this verb returns.
///
/// Policy. The frame is a real ceiling but not a usable bound: `text` and the
/// whole encode `artifact` — the embedding vector included — share this
/// response's frame and neither is capped here, so an edge count derived from
/// the frame alone would promise a fit it cannot deliver, at a width that turns
/// a single-shot diagnostic into a sixteen-megabyte walk. Two hundred and
/// fifty-six per direction is what this view shows; `edge_total_count` and
/// `edges_truncated` report what it did not, and `GRAPH_FETCH` paginates the
/// rest. The budget is spent per direction rather than across the pair so that
/// a memory with thousands of outgoing edges still shows its incoming ones; a
/// single combined cap consumed in walk order would answer with an all-outgoing
/// list and let a reader conclude the memory has no incoming edges at all.
const MAX_EDGES_PER_DIRECTION: usize = 256;

const _: () = assert!(MAX_EDGES_PER_DIRECTION <= FRAME_EDGE_CEILING_PER_DIRECTION);

pub async fn handle_memory_inspect(
    req: MemoryInspectRequest,
    ctx: &OpsContext,
) -> Result<MemoryInspectResponse, OpError> {
    let memory_id = req.memory_id;
    // The redb tables are keyed by the id's big-endian bytes, which is what
    // gives them id order on a range scan. The wire carries the id itself.
    let memory_key = memory_id.to_be_bytes();
    // An absent memory is an error, not a success carrying a flag — the same
    // answer every sibling read gives. The generic `NotFound` is what the
    // dispatcher turns into the wire's `MemoryNotFound` (the reason
    // `EntityNotFound` and `StatementNotFound` exist as separate ops codes at
    // all), so the client branches on the taxonomy it already handles rather
    // than on a boolean that existed on this one response and nowhere else in
    // the protocol.
    let not_found = || OpError::NotFound {
        what: "memory",
        detail: format!("memory {memory_id} does not exist"),
    };

    let caller_ns = u32::from(ctx.executor.caller_namespace);
    let caller_space: [u8; 16] = ctx.executor.caller_space.into();

    let rtxn = ctx
        .executor
        .metadata
        .read_txn()
        .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;

    // Tenancy and liveness. A memory id that belongs to another
    // `(namespace, space)` is indistinguishable from a missing one to this
    // caller, and so is a tombstoned one.
    //
    // The tombstone check is not incidental. FORGET leaves the metadata row
    // behind — a hard forget zeroes the text and the vector but keeps the row
    // for the tombstone itself — so checking only ownership answered a
    // forgotten memory with a *success* carrying an empty `artifact`. That is
    // the same answer this verb gives a memory whose bundle has not been
    // written yet, which is a genuinely different condition, and keeping those
    // two apart is the whole reason the empty-artifact success exists. Every
    // sibling read already treats a tombstone as gone: MEMORY_LIST excludes
    // them unless asked, and RECALL never returns one.
    {
        let memories = rtxn
            .open_table(MEMORIES_TABLE)
            .map_err(|e| OpError::Internal(format!("open memories: {e}")))?;
        let readable = memories
            .get(&memory_key)
            .map_err(|e| OpError::Internal(format!("memory read: {e}")))?
            .map(|g| {
                let m = g.value();
                m.namespace_id == caller_ns && m.space_id_bytes == caller_space && m.is_active()
            })
            .unwrap_or(false);
        if !readable {
            return Err(not_found());
        }
    }

    let text = {
        let texts = rtxn
            .open_table(TEXTS_TABLE)
            .map_err(|e| OpError::Internal(format!("open texts: {e}")))?;
        texts
            .get(&memory_key)
            .map_err(|e| OpError::Internal(format!("text read: {e}")))?
            .and_then(|g| String::from_utf8(g.value().to_vec()).ok())
            .unwrap_or_default()
    };

    let artifact = {
        let bundles = rtxn
            .open_table(MEMORY_ARTIFACTS_TABLE)
            .map_err(|e| OpError::Internal(format!("open memory_artifacts: {e}")))?;
        let mut artifact = bundles
            .get(&memory_key)
            .map_err(|e| OpError::Internal(format!("artifact read: {e}")))?
            .and_then(|g| serde_json::from_str::<EncodeStageArtifact>(g.value()).ok())
            .unwrap_or_default();
        // The embedding is stored once, in the by-id vector table, not inside
        // the bundle — see `brain_ops::memory_artifact`. The inspect view is
        // the per-stage picture of the write, so the `embed` stage's output is
        // read from its own home and folded back in here. One point lookup on
        // a read path, where the bundle is being parsed anyway.
        artifact.vector = crate::memory_artifact::get_artifact_vector(&rtxn, memory_key)
            .map(|v| v.to_vec())
            .unwrap_or_default();
        artifact
    };

    // Memory edges, both directions. These are the edges LINK/UNLINK write;
    // `artifact.graph` above is the extractor's write-time view and never
    // contains them, which left LINK write-only over the wire.
    //
    // A storage failure here is reported, not swallowed: returning a memory
    // with `edges: []` when the edge table could not be read would be
    // indistinguishable from a memory that genuinely has no edges, and a
    // caller would conclude its LINK had been lost.
    let (edges, edge_total_count, edges_truncated) = {
        let id = MemoryId::from_raw(memory_id);
        let outgoing = list_memory_edges_from(&rtxn, id, None, MAX_EDGES_PER_DIRECTION)
            .map_err(|e| OpError::Internal(format!("edges_from: {e}")))?;
        let incoming = list_memory_edges_to(&rtxn, id, None, MAX_EDGES_PER_DIRECTION)
            .map_err(|e| OpError::Internal(format!("edges_to: {e}")))?;
        bounded_edges(outgoing, incoming)
    };

    Ok(MemoryInspectResponse {
        memory_id,
        text,
        artifact,
        edges,
        // The engine counted the anchored set, so it can report a number;
        // the absent case is for a server that has no count to give, not this one.
        edge_total_count: Some(edge_total_count),
        edges_truncated,
    })
}

/// Project capped edge reads onto the wire and report the totals counted by
/// those reads alongside whether any admitted rows were left out.
///
/// The count is of the whole set, not of what fits, so a caller reading
/// `edges_truncated` learns both that the view is partial and how much it is
/// missing. Rows are taken in the order the walk yields them, which is redb
/// key order and therefore stable across calls but not a ranking — the
/// returned subset is arbitrary with respect to weight or age.
fn bounded_edges(
    outgoing: MemoryEdgePage,
    incoming: MemoryEdgePage,
) -> (Vec<MemoryEdgeWire>, u32, bool) {
    let total = outgoing.total.saturating_add(incoming.total);
    let edge_total_count = total.try_into().unwrap_or(u32::MAX);
    let edges_truncated =
        outgoing.total > outgoing.rows.len() as u64 || incoming.total > incoming.rows.len() as u64;
    let mut edges = Vec::with_capacity(outgoing.rows.len() + incoming.rows.len());

    for (kind, peer, data) in &outgoing.rows {
        edges.push(MemoryEdgeWire {
            kind: edge_kind_to_wire(*kind),
            direction: EdgeDirectionWire::Outgoing,
            peer: peer.raw(),
            weight: data.weight,
            created_at_unix_nanos: data.created_at_unix_nanos,
            annotation: data.annotation.clone(),
        });
    }
    for (kind, peer, data) in &incoming.rows {
        edges.push(MemoryEdgeWire {
            kind: edge_kind_to_wire(*kind),
            direction: EdgeDirectionWire::Incoming,
            peer: peer.raw(),
            weight: data.weight,
            created_at_unix_nanos: data.created_at_unix_nanos,
            annotation: data.annotation.clone(),
        });
    }

    (edges, edge_total_count, edges_truncated)
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
    use brain_metadata::tables::edge::EdgeData;

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
        assert_eq!(
            sorted.len(),
            all.len(),
            "mapping is not injective: {mapped:?}"
        );
    }

    fn edge_row(slot: u128) -> (EdgeKind, MemoryId, EdgeData) {
        (
            EdgeKind::Caused,
            MemoryId::from_raw(slot),
            EdgeData::new(0.5, 1, 2, slot as u64),
        )
    }

    /// A complete page: every admitted edge fit, so `total` matches `rows`.
    fn edge_page(rows: Vec<(EdgeKind, MemoryId, EdgeData)>) -> MemoryEdgePage {
        let total = rows.len() as u64;
        MemoryEdgePage { rows, total }
    }

    /// The page a capped read returns when `admitted` edges were anchored at
    /// the memory: the first [`MAX_EDGES_PER_DIRECTION`] rows, and a `total`
    /// that still counts the ones the read declined to materialise.
    fn capped_page(admitted: Vec<(EdgeKind, MemoryId, EdgeData)>) -> MemoryEdgePage {
        let total = admitted.len() as u64;
        let rows = admitted
            .into_iter()
            .take(MAX_EDGES_PER_DIRECTION)
            .collect::<Vec<_>>();
        MemoryEdgePage { rows, total }
    }

    #[test]
    fn an_edge_set_within_the_cap_is_returned_whole() {
        let outgoing = [edge_row(1), edge_row(2), edge_row(3)];
        let incoming = [edge_row(4), edge_row(5)];

        let (edges, total, truncated) =
            bounded_edges(edge_page(outgoing.to_vec()), edge_page(incoming.to_vec()));

        assert_eq!(edges.len(), 5);
        assert!(edges[..3]
            .iter()
            .all(|edge| edge.direction == EdgeDirectionWire::Outgoing));
        assert!(edges[3..]
            .iter()
            .all(|edge| edge.direction == EdgeDirectionWire::Incoming));
        assert_eq!(total, 5);
        assert!(!truncated);
    }

    /// A flooded direction cannot starve the other, because a reader who sees
    /// no incoming edges concludes the memory has none.
    #[test]
    fn each_direction_is_capped_independently() {
        let outgoing: Vec<_> = (0..(MAX_EDGES_PER_DIRECTION + 44))
            .map(|slot| edge_row(slot as u128))
            .collect();
        let incoming: Vec<_> = (1000..1005).map(edge_row).collect();

        let (edges, _, truncated) = bounded_edges(capped_page(outgoing), capped_page(incoming));

        assert_eq!(
            edges
                .iter()
                .filter(|edge| edge.direction == EdgeDirectionWire::Outgoing)
                .count(),
            MAX_EDGES_PER_DIRECTION
        );
        assert_eq!(
            edges
                .iter()
                .filter(|edge| edge.direction == EdgeDirectionWire::Incoming)
                .count(),
            5
        );
        assert!(truncated);
    }

    /// The caller can always tell a partial view from a complete one.
    #[test]
    fn the_total_count_reports_the_edges_left_out() {
        let outgoing: Vec<_> = (0..(MAX_EDGES_PER_DIRECTION + 1))
            .map(|slot| edge_row(slot as u128))
            .collect();
        let incoming: Vec<_> = (1000..(1000 + MAX_EDGES_PER_DIRECTION + 2))
            .map(|slot| edge_row(slot as u128))
            .collect();

        let (edges, total, truncated) = bounded_edges(capped_page(outgoing), capped_page(incoming));

        assert_eq!(edges.len(), 2 * MAX_EDGES_PER_DIRECTION);
        assert_eq!(total, (2 * MAX_EDGES_PER_DIRECTION + 3) as u32);
        assert!(truncated);
    }

    #[test]
    fn an_edge_wire_carries_every_field_of_its_row() {
        let mut row = edge_row(42);
        row.0 = EdgeKind::Supports;
        row.2.weight = 0.9375;
        row.2.created_at_unix_nanos = 987654321;
        row.2.annotation = Some("diagnostic edge".to_owned());

        let (edges, total, truncated) = bounded_edges(edge_page(vec![row]), edge_page(Vec::new()));
        let edge = &edges[0];

        assert_eq!(edge.kind, EdgeKindWire::Supports);
        assert_eq!(edge.direction, EdgeDirectionWire::Outgoing);
        assert_eq!(edge.peer, 42);
        assert_eq!(edge.weight, 0.9375);
        assert_eq!(edge.created_at_unix_nanos, 987654321);
        assert_eq!(edge.annotation.as_deref(), Some("diagnostic edge"));
        assert_eq!(total, 1);
        assert!(!truncated);
    }
}
