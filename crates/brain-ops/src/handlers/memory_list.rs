//! `MEMORY_LIST` handler — paginated enumeration of the caller's memories.
//!
//! This is not RECALL. There is no cue, no ranking, no relevance
//! suppression: it walks the caller's `(namespace, space)` timeline in a
//! stable order and returns one keyset page plus an opaque resume cursor.
//!
//! Tombstone note: the timeline index drops a memory's row when it is
//! tombstoned (so a dead memory never surfaces as a temporal
//! predecessor), so v1 enumerates only active memories. `include_tombstoned`
//! is honored at the row level, but no tombstoned rows are in the index to
//! surface — resurfacing them is a follow-up needing a tombstone-retaining
//! secondary index. The exclude path is exact.
//!
//! v1 ships the `created_at` sort axis with real keyset pagination over
//! [`brain_metadata::tables::memory::MEMORIES_BY_SPACE_TIMELINE_TABLE`].
//! The other sort axes (salience / occurred / last_accessed) and the
//! `text_contains` filter have no tenant-scoped index yet and are
//! rejected with a precise `InvalidRequest` rather than served by an
//! O(N) full scan.

use brain_metadata::tables::memory::{
    memory_timeline_page, MemoryMetadata, MemoryTimelineFilter, SPACE_TIMELINE_KEY_LEN,
};
use brain_metadata::tables::text::TEXTS_TABLE;
use brain_metadata::RowScope;
use brain_protocol::{
    MemoryListDirWire, MemoryListItem, MemoryListRequest, MemoryListResponseFrame,
    MemoryListSortWire, MemoryListTimeAxisWire,
};

use crate::context::OpsContext;
use crate::error::OpError;

/// Cursor wire format version. A mismatch means the client echoed a
/// cursor minted by an incompatible server build → treat as stale.
const CURSOR_VERSION: u8 = 1;

/// `[version(1)][sort(1)][dir(1)][filter_sig(8)][timeline_key(52)]`.
const CURSOR_LEN: usize = 1 + 1 + 1 + 8 + SPACE_TIMELINE_KEY_LEN;

const MAX_LIMIT: u32 = 100;

pub async fn handle_memory_list(
    req: MemoryListRequest,
    ctx: &OpsContext,
) -> Result<MemoryListResponseFrame, OpError> {
    if req.limit == 0 || req.limit > MAX_LIMIT {
        return Err(OpError::InvalidRequest(format!(
            "limit must be in 1..={MAX_LIMIT}"
        )));
    }
    // v1 offers only the created_at axis, because it is the one backed by
    // a tenant-scoped index. Serving the others would mean an unbounded
    // re-sort of the whole pile — exactly what enumeration must not do.
    if req.sort != MemoryListSortWire::Created {
        return Err(OpError::InvalidRequest(
            "sort not yet supported; use created".into(),
        ));
    }
    // occurred_at has no memory index; a range on it would be a full scan.
    if req.time_axis == MemoryListTimeAxisWire::Occurred {
        return Err(OpError::InvalidRequest(
            "occurred_at time axis not yet supported; use created".into(),
        ));
    }
    if !req.text_contains.is_empty() {
        return Err(OpError::InvalidRequest(
            "text filter not yet supported".into(),
        ));
    }
    if req.salience_min > req.salience_max {
        return Err(OpError::InvalidRequest(
            "salience_min must be <= salience_max".into(),
        ));
    }
    if req.from_unix_nanos != 0 && req.to_unix_nanos != 0 && req.from_unix_nanos > req.to_unix_nanos
    {
        return Err(OpError::InvalidRequest(
            "from_unix_nanos must be <= to_unix_nanos".into(),
        ));
    }

    let descending = matches!(req.dir, MemoryListDirWire::Desc);
    let filter = build_filter(&req);
    let filter_sig = filter_signature(&req);

    // A non-empty cursor must match the current sort/dir/filters exactly,
    // or the resumed page would silently belong to a different result set.
    let after_key: Option<[u8; SPACE_TIMELINE_KEY_LEN]> = if req.cursor.is_empty() {
        None
    } else {
        Some(decode_cursor(&req.cursor, &req, &filter_sig)?)
    };

    let scope = RowScope::new(ctx.executor.caller_namespace, ctx.executor.caller_space);

    let rtxn = ctx
        .executor
        .metadata
        .read_txn()
        .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;

    let page = memory_timeline_page(
        &rtxn,
        scope,
        descending,
        after_key.as_ref().map(|k| k.as_slice()),
        req.limit as usize,
        &filter,
    )
    .map_err(|e| OpError::Internal(format!("memory timeline scan: {e}")))?;

    let texts = rtxn
        .open_table(TEXTS_TABLE)
        .map_err(|e| OpError::Internal(format!("open texts: {e}")))?;

    let mut items: Vec<MemoryListItem> = Vec::with_capacity(page.rows.len());
    for row in &page.rows {
        let text = texts
            .get(&row.memory_id_bytes)
            .map_err(|e| OpError::Internal(format!("text read: {e}")))?
            .and_then(|g| String::from_utf8(g.value().to_vec()).ok())
            .unwrap_or_default();
        let (statement_count, entity_count, relation_count) =
            graph_counts(&rtxn, scope, row.memory_id_bytes);
        let mut item = row_to_item(row, text);
        item.statement_count = statement_count;
        item.entity_count = entity_count;
        item.relation_count = relation_count;
        items.push(item);
    }

    // Resume token: the EXACT timeline index key of the last row we
    // returned (from `page.last_key`, not reconstructed from the row's
    // fields — the index key and the row can disagree on `created_at`).
    // Present only when the scan proved more matching rows exist.
    let next_cursor = if page.has_more {
        match page.last_key {
            Some(key) => encode_cursor(&req, &filter_sig, &key),
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };

    let cumulative_count = items.len() as u32;
    Ok(MemoryListResponseFrame {
        items,
        next_cursor,
        cumulative_count,
        is_final: true,
    })
}

/// Per-memory typed-graph provenance counts — the "graph handles" a client
/// uses to link a memory row into the graph explorer: how many statements
/// and relations this memory produced, and how many entities it mentions.
///
/// Cheap reverse-index scans only (statements/relations `*_BY_EVIDENCE`, plus
/// `Mentions` edges) — no full-row fetches. Counts include not-yet-reclaimed
/// evidence rows for tombstoned artifacts; the timeline page is small
/// (`limit <= 100`), so the per-row cost is bounded.
fn graph_counts(
    rtxn: &redb::ReadTransaction,
    scope: RowScope,
    memory_id_bytes: [u8; 16],
) -> (u32, u32, u32) {
    use brain_core::{EdgeKindRef, MemoryId, NodeRef};
    use brain_metadata::tables::edge::walk_outgoing;
    use brain_metadata::tables::statement::STATEMENTS_BY_EVIDENCE_TABLE;

    let memory_id = MemoryId::from_be_bytes(memory_id_bytes);

    // Statements sourced by this memory (evidence reverse index).
    let statements = rtxn
        .open_table(STATEMENTS_BY_EVIDENCE_TABLE)
        .ok()
        .and_then(|t| {
            let lo = (
                scope.namespace_id,
                scope.space_id_bytes,
                memory_id_bytes,
                [0u8; 16],
            );
            let hi = (
                scope.namespace_id,
                scope.space_id_bytes,
                memory_id_bytes,
                [0xFFu8; 16],
            );
            t.range(lo..=hi).ok().map(Iterator::count)
        })
        .unwrap_or(0);

    // Entities this memory mentions (Mentions edges out of the memory node).
    let entities = walk_outgoing(
        rtxn,
        NodeRef::Memory(memory_id),
        Some(EdgeKindRef::Mentions),
    )
    .map(|v| v.len())
    .unwrap_or(0);

    // Relations sourced by this memory (evidence reverse index).
    let relations = brain_metadata::relations_with_evidence(rtxn, scope, memory_id)
        .map(|v| v.len())
        .unwrap_or(0);

    (
        u32::try_from(statements).unwrap_or(u32::MAX),
        u32::try_from(entities).unwrap_or(u32::MAX),
        u32::try_from(relations).unwrap_or(u32::MAX),
    )
}

fn build_filter(req: &MemoryListRequest) -> MemoryTimelineFilter {
    MemoryTimelineFilter {
        // `MemoryKindWire` is `#[repr(u8)]` with the same discriminants
        // the metadata layer stores, so the cast is the identity map.
        kinds: req.kinds.iter().map(|k| *k as u8).collect(),
        include_tombstoned: req.include_tombstoned,
        created_from: (req.from_unix_nanos != 0).then_some(req.from_unix_nanos),
        created_to: (req.to_unix_nanos != 0).then_some(req.to_unix_nanos),
        salience_min: req.salience_min,
        salience_max: req.salience_max,
    }
}

fn row_to_item(row: &MemoryMetadata, text: String) -> MemoryListItem {
    MemoryListItem {
        memory_id: row.memory_id_bytes,
        space_id: row.space_id_bytes,
        session_id: row.session_id,
        text,
        kind: row.kind,
        state: u8::from(!row.is_active()),
        created_at_unix_nanos: row.created_at_unix_nanos,
        occurred_at_unix_nanos: row.occurred_at_unix_nanos.unwrap_or(0),
        last_accessed_at_unix_nanos: row.last_accessed_at_unix_nanos,
        salience: row.salience,
        access_count: row.access_count,
        // v1: memory rows don't persist the originating request id, so
        // there is no cheap way to surface it here.
        source_request_id: [0u8; 16],
        // v1: reverse evidence index (statement/relation → source memory)
        // is not yet wired, and a per-memory forward scan would be O(N);
        // the drawer fetches these lazily via the typed-graph LIST ops.
        statement_count: 0,
        entity_count: 0,
        relation_count: 0,
    }
}

/// BLAKE3 over the filter-defining fields (everything but `limit`,
/// `cursor`, and `act_as`). Two requests that would enumerate the same
/// ordered result set produce the same signature; any filter change
/// flips it, invalidating an in-flight cursor.
fn filter_signature(req: &MemoryListRequest) -> [u8; 8] {
    let mut h = blake3::Hasher::new();
    h.update(&[req.sort as u8, req.dir as u8, req.time_axis as u8]);
    h.update(&[u8::from(req.include_tombstoned)]);
    for k in &req.kinds {
        h.update(&[*k as u8]);
    }
    // Length-delimit the kinds list so `[0,1]` and `[1]`+something can't
    // collide.
    h.update(&(req.kinds.len() as u32).to_le_bytes());
    h.update(&req.from_unix_nanos.to_le_bytes());
    h.update(&req.to_unix_nanos.to_le_bytes());
    h.update(&req.salience_min.to_le_bytes());
    h.update(&req.salience_max.to_le_bytes());
    h.update(req.text_contains.as_bytes());
    let full = h.finalize();
    let mut out = [0u8; 8];
    out.copy_from_slice(&full.as_bytes()[..8]);
    out
}

fn encode_cursor(
    req: &MemoryListRequest,
    sig: &[u8; 8],
    key: &[u8; SPACE_TIMELINE_KEY_LEN],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(CURSOR_LEN);
    out.push(CURSOR_VERSION);
    out.push(req.sort as u8);
    out.push(req.dir as u8);
    out.extend_from_slice(sig);
    out.extend_from_slice(key);
    out
}

fn decode_cursor(
    cursor: &[u8],
    req: &MemoryListRequest,
    sig: &[u8; 8],
) -> Result<[u8; SPACE_TIMELINE_KEY_LEN], OpError> {
    let stale = || OpError::InvalidRequest("stale_cursor: filters changed".into());
    if cursor.len() != CURSOR_LEN || cursor[0] != CURSOR_VERSION {
        return Err(stale());
    }
    if cursor[1] != req.sort as u8 || cursor[2] != req.dir as u8 {
        return Err(stale());
    }
    if &cursor[3..11] != sig.as_slice() {
        return Err(stale());
    }
    let mut key = [0u8; SPACE_TIMELINE_KEY_LEN];
    key.copy_from_slice(&cursor[11..]);
    Ok(key)
}
