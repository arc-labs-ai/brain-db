//! Unified edge storage — one forward table plus one reverse table,
//! keyed by `(NodeRef from, EdgeKindRef kind, NodeRef to, disambiguator)`.
//!
//! ## Why one table instead of two
//!
//! Previously, substrate edges (`Memory → Memory`) and typed
//! typed-graph relations (`Entity → Entity`) lived in separate redb
//! tables. Mention edges (`Memory → Entity`) had no home at all. The
//! unified table keys both endpoints as [`NodeRef`] and the label as
//! [`EdgeKindRef`], so every edge in Brain — substrate, mention, or
//! typed — lives in the same redb pair. Graph traversal is one BFS;
//! recovery, the change feed, and FORGET cascade work over one
//! structure.
//!
//! ## Two physical tables, one logical relation
//!
//! - [`EDGES_TABLE`]         keyed by `(from, kind, to, disambiguator)`
//!   → [`EdgeData`].
//! - [`EDGES_REVERSE_TABLE`] keyed by `(to, kind, from, disambiguator)`
//!   → [`EdgeData`].
//!
//! The reverse table duplicates [`EdgeData`] rather than storing `()`
//! so an incoming-edge scan is a single contiguous range read — no
//! per-hit point lookup back into the forward table. The redundancy
//! is < 64 bytes per edge and trades trivial extra disk for ~2x
//! latency improvement on `walk_incoming` against high-fan-in nodes.
//!
//! ## Symmetric builtin edges
//!
//! `EdgeKind::is_symmetric()` triggers auto-mirroring: a logical
//! symmetric edge `A↔B` (asymmetric kinds excluded) writes four
//! physical rows — `(A, K, B)` + `(B, K, A)` forward, each mirrored
//! into the reverse table. Self-edges (`A == A`) skip the mirror
//! because the row is its own reverse. **Auto-mirroring only applies
//! to substrate `Builtin` kinds**; typed-relation symmetry is a
//! sidecar property handled at the `relation_ops` layer (it walks the
//! canonicalised endpoint pair), not at the edge table.
//!
//! ## `EdgeKey` layout
//!
//! ```text
//!   from        NodeRef        17 bytes
//!   kind        EdgeKindRef    1..5 bytes
//!   to          NodeRef        17 bytes
//!   disambig    [u8; 16]       16 bytes  (RelationId for Typed; zeros otherwise)
//!
//!   total:                     51..55 bytes
//! ```
//!
//! The disambiguator is required because multiple typed relations of
//! the same type between the same `(from, to)` pair would otherwise
//! collide on the key. For `Builtin` and `Mentions` it's always
//! `[0; 16]`; for `Typed(RelationTypeId)` it's the `RelationId`. This
//! keeps the prefix scan `EDGES_TABLE[(from, *, *)]` and
//! `EDGES_TABLE[(from, Typed(rt_id), *)]` cheap while preserving
//! per-relation identity for typed edges.

use brain_core::{EdgeKind, EdgeKindRef, EdgeKindRefError, MemoryId, NodeRef, NodeRefError};
use redb::{ReadOnlyTable, ReadTransaction, Table, TableDefinition};

// ---------------------------------------------------------------------------
// Tables.
// ---------------------------------------------------------------------------

pub const EDGES_TABLE: TableDefinition<'static, &[u8], EdgeData> = TableDefinition::new("edges");

pub const EDGES_REVERSE_TABLE: TableDefinition<'static, &[u8], EdgeData> =
    TableDefinition::new("edges_reverse");

// ---------------------------------------------------------------------------
// origin / derived_by byte mappings.
// ---------------------------------------------------------------------------

/// `EdgeData::origin` byte values. Mirrors `brain_core::EdgeOrigin`.
pub mod origin {
    pub const EXPLICIT: u8 = 0;
    pub const AUTO_DERIVED: u8 = 1;
}

/// `EdgeData::derived_by` byte values.
pub mod derived_by {
    pub const CLIENT: u8 = 0;
    pub const CONSOLIDATION_WORKER: u8 = 1;
    pub const SIMILARITY_WORKER: u8 = 2;
    /// TemporalEdgeWorker — writes `FollowedBy` edges keyed on
    /// `(agent_id, context_id, created_at)` adjacency.
    pub const TEMPORAL_WORKER: u8 = 3;
    /// CausalEdgeWorker — writes `Caused` edges from extractor-
    /// derived causal statements. Reserved for the v1 implementation.
    pub const CAUSAL_WORKER: u8 = 4;
    // 5..=255 reserved for future workers.
}

// ---------------------------------------------------------------------------
// EdgeData (value).
// ---------------------------------------------------------------------------

/// Per-edge metadata stored in both the forward and reverse tables.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct EdgeData {
    pub weight: f32,
    pub origin: u8,
    pub derived_by: u8,
    pub created_at_unix_nanos: u64,
    pub annotation: Option<String>,
}

impl EdgeData {
    #[must_use]
    pub fn new(weight: f32, origin: u8, derived_by: u8, created_at_unix_nanos: u64) -> Self {
        Self {
            weight,
            origin,
            derived_by,
            created_at_unix_nanos,
            annotation: None,
        }
    }
}

impl redb::Value for EdgeData {
    type SelfType<'a> = EdgeData;
    type AsBytes<'a> = Vec<u8>;

    fn fixed_width() -> Option<usize> {
        None
    }

    fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a>
    where
        Self: 'a,
    {
        let mut buf = rkyv::AlignedVec::with_capacity(data.len());
        buf.extend_from_slice(data);
        rkyv::from_bytes::<EdgeData>(&buf)
            .expect("EdgeData bytes failed rkyv validation; redb file is corrupt")
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
    where
        Self: 'a,
        Self: 'b,
    {
        rkyv::to_bytes::<_, 256>(value)
            .expect("EdgeData is rkyv-serializable")
            .into_vec()
    }

    fn type_name() -> redb::TypeName {
        redb::TypeName::new("brain_metadata::EdgeData")
    }
}

// ---------------------------------------------------------------------------
// EdgeKey (variable-length key encoded inline).
// ---------------------------------------------------------------------------

/// Composite key for the unified edge tables. Encoded into a flat byte
/// buffer because redb's `TableDefinition` requires a sized key type;
/// callers serialise via [`EdgeKey::encode`] and parse back via
/// [`EdgeKey::decode`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct EdgeKey {
    pub from: NodeRef,
    pub kind: EdgeKindRef,
    pub to: NodeRef,
    /// Per-relation discriminator for `Typed` edges (carries the
    /// `RelationId.to_bytes()`). `[0; 16]` for `Builtin` and
    /// `Mentions` — those edge kinds are unique within `(from, kind,
    /// to)` already.
    pub disambiguator: [u8; 16],
}

const DISAMBIGUATOR_LEN: usize = 16;
const ZERO_DISAMBIGUATOR: [u8; DISAMBIGUATOR_LEN] = [0u8; DISAMBIGUATOR_LEN];

impl EdgeKey {
    /// Encoded byte length: 17 (from) + kind_len + 17 (to) + 16
    /// (disambiguator).
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        NodeRef::BYTES + self.kind.encoded_len() + NodeRef::BYTES + DISAMBIGUATOR_LEN
    }

    /// Encode this key into a fresh byte buffer.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_len());
        out.extend_from_slice(&self.from.to_bytes());
        self.kind.encode_into(&mut out);
        out.extend_from_slice(&self.to.to_bytes());
        out.extend_from_slice(&self.disambiguator);
        out
    }

    /// Decode an `EdgeKey` from its byte representation. Strict —
    /// trailing bytes after the disambiguator are an error.
    pub fn decode(bytes: &[u8]) -> Result<Self, EdgeKeyError> {
        if bytes.len() < NodeRef::BYTES {
            return Err(EdgeKeyError::Short(bytes.len()));
        }
        let mut from_bytes = [0u8; NodeRef::BYTES];
        from_bytes.copy_from_slice(&bytes[..NodeRef::BYTES]);
        let from = NodeRef::from_bytes(from_bytes).map_err(EdgeKeyError::BadNodeRef)?;

        let kind_slice = &bytes[NodeRef::BYTES..];
        let (kind, kind_len) =
            EdgeKindRef::decode_from(kind_slice).map_err(EdgeKeyError::BadEdgeKind)?;

        let to_offset = NodeRef::BYTES + kind_len;
        if bytes.len() < to_offset + NodeRef::BYTES + DISAMBIGUATOR_LEN {
            return Err(EdgeKeyError::Short(bytes.len()));
        }
        let mut to_bytes = [0u8; NodeRef::BYTES];
        to_bytes.copy_from_slice(&bytes[to_offset..to_offset + NodeRef::BYTES]);
        let to = NodeRef::from_bytes(to_bytes).map_err(EdgeKeyError::BadNodeRef)?;

        let disamb_offset = to_offset + NodeRef::BYTES;
        let mut disambiguator = [0u8; DISAMBIGUATOR_LEN];
        disambiguator.copy_from_slice(&bytes[disamb_offset..disamb_offset + DISAMBIGUATOR_LEN]);

        if bytes.len() != disamb_offset + DISAMBIGUATOR_LEN {
            return Err(EdgeKeyError::Trailing(
                bytes.len() - (disamb_offset + DISAMBIGUATOR_LEN),
            ));
        }
        Ok(EdgeKey {
            from,
            kind,
            to,
            disambiguator,
        })
    }

    /// Prefix bytes for a scan of every edge anchored at `from` —
    /// just `from.to_bytes()`. The scan's upper bound is constructed
    /// by appending a 0xFF-saturated tail.
    #[must_use]
    pub fn from_prefix(from: NodeRef) -> [u8; NodeRef::BYTES] {
        from.to_bytes()
    }

    /// Prefix bytes for a scan of every edge `(from, kind, *, *)` —
    /// `from.to_bytes() || kind.encode()`.
    #[must_use]
    pub fn from_kind_prefix(from: NodeRef, kind: EdgeKindRef) -> Vec<u8> {
        let mut out = Vec::with_capacity(NodeRef::BYTES + kind.encoded_len());
        out.extend_from_slice(&from.to_bytes());
        kind.encode_into(&mut out);
        out
    }
}

#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum EdgeKeyError {
    #[error("short read: {0} bytes")]
    Short(usize),
    #[error("trailing {0} byte(s) after disambiguator")]
    Trailing(usize),
    #[error("NodeRef decode: {0}")]
    BadNodeRef(NodeRefError),
    #[error("EdgeKindRef decode: {0}")]
    BadEdgeKind(EdgeKindRefError),
}

// ---------------------------------------------------------------------------
// EdgeOpError.
// ---------------------------------------------------------------------------

#[derive(thiserror::Error, Debug)]
pub enum EdgeOpError {
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),
    #[error("edge key decode error: {0}")]
    KeyDecode(#[from] EdgeKeyError),
}

// ---------------------------------------------------------------------------
// Disambiguator helpers.
// ---------------------------------------------------------------------------

/// Return the disambiguator a caller MUST use for `kind`. For typed
/// relations the caller supplies the `RelationId.to_bytes()` directly
/// to [`link`] / [`unlink`]; for everything else this returns the
/// canonical zero discriminator.
#[must_use]
pub fn zero_disambiguator() -> [u8; DISAMBIGUATOR_LEN] {
    ZERO_DISAMBIGUATOR
}

// ---------------------------------------------------------------------------
// LINK / UNLINK helpers.
// ---------------------------------------------------------------------------

/// Insert `(from, kind, to)` into both tables. For symmetric `Builtin`
/// kinds with `from != to`, also writes the mirror `(to, kind, from)`
/// plus its reverse. The `disambiguator` is the per-key tie-breaker;
/// callers for typed relations pass `RelationId.to_bytes()`, all
/// others pass `zero_disambiguator()`.
pub fn link(
    edges: &mut Table<'_, &[u8], EdgeData>,
    edges_reverse: &mut Table<'_, &[u8], EdgeData>,
    from: NodeRef,
    kind: EdgeKindRef,
    to: NodeRef,
    disambiguator: [u8; DISAMBIGUATOR_LEN],
    data: &EdgeData,
) -> Result<(), EdgeOpError> {
    insert_pair(edges, edges_reverse, from, kind, to, disambiguator, data)?;
    if symmetric_auto_mirror(kind) && from != to {
        insert_pair(edges, edges_reverse, to, kind, from, disambiguator, data)?;
    }
    Ok(())
}

fn insert_pair(
    edges: &mut Table<'_, &[u8], EdgeData>,
    edges_reverse: &mut Table<'_, &[u8], EdgeData>,
    from: NodeRef,
    kind: EdgeKindRef,
    to: NodeRef,
    disambiguator: [u8; DISAMBIGUATOR_LEN],
    data: &EdgeData,
) -> Result<(), EdgeOpError> {
    let fwd = EdgeKey {
        from,
        kind,
        to,
        disambiguator,
    };
    let rev = EdgeKey {
        from: to,
        kind,
        to: from,
        disambiguator,
    };
    edges.insert(fwd.encode().as_slice(), data)?;
    edges_reverse.insert(rev.encode().as_slice(), data)?;
    Ok(())
}

/// Remove the canonical `(from, kind, to)` row from both tables (and
/// the mirror if `Builtin` symmetric). Returns `true` iff the canonical
/// forward row was present.
pub fn unlink(
    edges: &mut Table<'_, &[u8], EdgeData>,
    edges_reverse: &mut Table<'_, &[u8], EdgeData>,
    from: NodeRef,
    kind: EdgeKindRef,
    to: NodeRef,
    disambiguator: [u8; DISAMBIGUATOR_LEN],
) -> Result<bool, EdgeOpError> {
    let fwd = EdgeKey {
        from,
        kind,
        to,
        disambiguator,
    };
    let rev = EdgeKey {
        from: to,
        kind,
        to: from,
        disambiguator,
    };
    let removed = edges.remove(fwd.encode().as_slice())?.is_some();
    edges_reverse.remove(rev.encode().as_slice())?;
    if symmetric_auto_mirror(kind) && from != to {
        let mirror_fwd = EdgeKey {
            from: to,
            kind,
            to: from,
            disambiguator,
        };
        let mirror_rev = EdgeKey {
            from,
            kind,
            to,
            disambiguator,
        };
        edges.remove(mirror_fwd.encode().as_slice())?;
        edges_reverse.remove(mirror_rev.encode().as_slice())?;
    }
    Ok(removed)
}

/// Auto-mirroring only applies to substrate builtin kinds. Typed
/// relations express symmetry via canonical-pair ordering at the
/// `relation_ops` layer; mention edges are intrinsically directional.
fn symmetric_auto_mirror(kind: EdgeKindRef) -> bool {
    matches!(kind, EdgeKindRef::Builtin(k) if k.is_symmetric())
}

// ---------------------------------------------------------------------------
// Point lookup.
// ---------------------------------------------------------------------------

/// Fetch a specific edge row from the forward table.
pub fn edge_get(
    rtxn: &ReadTransaction,
    from: NodeRef,
    kind: EdgeKindRef,
    to: NodeRef,
    disambiguator: [u8; DISAMBIGUATOR_LEN],
) -> Result<Option<EdgeData>, EdgeOpError> {
    let t = rtxn.open_table(EDGES_TABLE)?;
    let key = EdgeKey {
        from,
        kind,
        to,
        disambiguator,
    }
    .encode();
    Ok(t.get(key.as_slice())?.map(|g| g.value()))
}

// ---------------------------------------------------------------------------
// Range scans.
// ---------------------------------------------------------------------------

/// One row returned by [`walk_outgoing`] / [`walk_incoming`]:
/// `(kind, other-end-node, disambiguator, edge-data)`.
pub type EdgeRow = (EdgeKindRef, NodeRef, [u8; DISAMBIGUATOR_LEN], EdgeData);

/// All edges anchored at `from`. With `kind_filter = Some(k)`, only
/// rows whose `EdgeKindRef` equals `k`. The returned vector is sorted
/// in `(kind, to, disambiguator)` byte-lexicographic order.
pub fn walk_outgoing(
    rtxn: &ReadTransaction,
    from: NodeRef,
    kind_filter: Option<EdgeKindRef>,
) -> Result<Vec<EdgeRow>, EdgeOpError> {
    let t = rtxn.open_table(EDGES_TABLE)?;
    range_scan(&t, from, kind_filter)
}

/// All edges anchored at `to`. Same shape as [`walk_outgoing`] but
/// reads from the reverse table. The second tuple element is the
/// **source** node.
pub fn walk_incoming(
    rtxn: &ReadTransaction,
    to: NodeRef,
    kind_filter: Option<EdgeKindRef>,
) -> Result<Vec<EdgeRow>, EdgeOpError> {
    let t = rtxn.open_table(EDGES_REVERSE_TABLE)?;
    range_scan(&t, to, kind_filter)
}

fn range_scan(
    table: &ReadOnlyTable<&'static [u8], EdgeData>,
    anchor: NodeRef,
    kind_filter: Option<EdgeKindRef>,
) -> Result<Vec<EdgeRow>, EdgeOpError> {
    let (lo, hi) = match kind_filter {
        Some(k) => {
            let prefix = EdgeKey::from_kind_prefix(anchor, k);
            let mut hi = prefix.clone();
            // Pad to the maximum key length with 0xFF so the inclusive
            // upper bound covers every (to, disambiguator) suffix.
            hi.extend_from_slice(&[0xFF; NodeRef::BYTES + DISAMBIGUATOR_LEN]);
            (prefix, hi)
        }
        None => {
            let prefix = EdgeKey::from_prefix(anchor).to_vec();
            let mut hi = prefix.clone();
            hi.extend_from_slice(
                &[0xFF; EdgeKindRef::MAX_BYTES + NodeRef::BYTES + DISAMBIGUATOR_LEN],
            );
            (prefix, hi)
        }
    };

    let mut out = Vec::new();
    for entry in table.range::<&[u8]>(lo.as_slice()..=hi.as_slice())? {
        let (k, v) = entry?;
        let key = EdgeKey::decode(k.value())?;
        if key.from != anchor {
            // Defensive: range bounds should already exclude this.
            continue;
        }
        if let Some(want) = kind_filter {
            if key.kind != want {
                continue;
            }
        }
        out.push((key.kind, key.to, key.disambiguator, v.value()));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Memory-only edge convenience.
// ---------------------------------------------------------------------------

/// Memory-only outgoing walk. Returns `(EdgeKind, MemoryId,
/// EdgeData)` triples for every Memory↔Memory `Builtin` edge anchored
/// at `from`. Non-builtin kinds and non-memory neighbours are filtered
/// out — graph retrievers that don't know about entities or typed
/// relations rely on this to keep returning the memory-anchored view.
pub fn list_memory_edges_from(
    rtxn: &ReadTransaction,
    from: MemoryId,
    kind: Option<EdgeKind>,
) -> Result<Vec<(EdgeKind, MemoryId, EdgeData)>, EdgeOpError> {
    let filter = kind.map(EdgeKindRef::Builtin);
    let rows = walk_outgoing(rtxn, NodeRef::Memory(from), filter)?;
    Ok(rows
        .into_iter()
        .filter_map(|(k, to, _disamb, d)| match (k, to) {
            (EdgeKindRef::Builtin(ek), NodeRef::Memory(m)) => Some((ek, m, d)),
            _ => None,
        })
        .collect())
}

/// Substrate-only incoming walk; mirror of [`list_memory_edges_from`].
pub fn list_memory_edges_to(
    rtxn: &ReadTransaction,
    to: MemoryId,
    kind: Option<EdgeKind>,
) -> Result<Vec<(EdgeKind, MemoryId, EdgeData)>, EdgeOpError> {
    let filter = kind.map(EdgeKindRef::Builtin);
    let rows = walk_incoming(rtxn, NodeRef::Memory(to), filter)?;
    Ok(rows
        .into_iter()
        .filter_map(|(k, from, _disamb, d)| match (k, from) {
            (EdgeKindRef::Builtin(ek), NodeRef::Memory(m)) => Some((ek, m, d)),
            _ => None,
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use brain_core::{
        ids::{EntityId, RelationId, RelationTypeId},
        EdgeKind, MemoryId,
    };
    use proptest::prelude::*;
    use redb::{Database, ReadableDatabase, ReadableTable};

    fn mid(slot: u64) -> MemoryId {
        MemoryId::pack(1, slot, 1)
    }

    fn eid(byte: u8) -> EntityId {
        let mut b = [0u8; 16];
        b[15] = byte;
        EntityId::from_bytes(b)
    }

    fn rid(byte: u8) -> RelationId {
        let mut b = [0u8; 16];
        b[15] = byte;
        RelationId::from_bytes(b)
    }

    fn fresh_db(dir: &tempfile::TempDir) -> Database {
        Database::create(dir.path().join("test.redb")).unwrap()
    }

    fn data(weight: f32) -> EdgeData {
        EdgeData::new(
            weight,
            origin::EXPLICIT,
            derived_by::CLIENT,
            1_700_000_000_000_000_000,
        )
    }

    fn mem_node(slot: u64) -> NodeRef {
        NodeRef::Memory(mid(slot))
    }

    fn ent_node(byte: u8) -> NodeRef {
        NodeRef::Entity(eid(byte))
    }

    // ----- EdgeKey codec ------------------------------------------------

    #[test]
    fn edge_key_roundtrip_builtin_memory_memory() {
        let key = EdgeKey {
            from: mem_node(1),
            kind: EdgeKindRef::Builtin(EdgeKind::Caused),
            to: mem_node(2),
            disambiguator: zero_disambiguator(),
        };
        let bytes = key.encode();
        assert_eq!(bytes.len(), 17 + 2 + 17 + 16);
        let back = EdgeKey::decode(&bytes).unwrap();
        assert_eq!(back, key);
    }

    #[test]
    fn edge_key_roundtrip_mentions_memory_entity() {
        let key = EdgeKey {
            from: mem_node(1),
            kind: EdgeKindRef::Mentions,
            to: ent_node(0xAA),
            disambiguator: zero_disambiguator(),
        };
        let bytes = key.encode();
        assert_eq!(bytes.len(), 17 + 1 + 17 + 16);
        assert_eq!(EdgeKey::decode(&bytes).unwrap(), key);
    }

    #[test]
    fn edge_key_roundtrip_typed_entity_entity() {
        let rel = rid(0x42);
        let key = EdgeKey {
            from: ent_node(0xAA),
            kind: EdgeKindRef::Typed(RelationTypeId::from(7)),
            to: ent_node(0xBB),
            disambiguator: rel.to_bytes(),
        };
        let bytes = key.encode();
        assert_eq!(bytes.len(), 17 + 5 + 17 + 16);
        assert_eq!(EdgeKey::decode(&bytes).unwrap(), key);
    }

    #[test]
    fn edge_key_decode_rejects_trailing_bytes() {
        let key = EdgeKey {
            from: mem_node(1),
            kind: EdgeKindRef::Builtin(EdgeKind::Caused),
            to: mem_node(2),
            disambiguator: zero_disambiguator(),
        };
        let mut bytes = key.encode();
        bytes.push(0);
        match EdgeKey::decode(&bytes) {
            Err(EdgeKeyError::Trailing(1)) => {}
            other => panic!("expected Trailing(1), got {other:?}"),
        }
    }

    // ----- Link + walk_outgoing -----------------------------------------

    #[test]
    fn link_memory_memory_then_walk_outgoing() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let d = data(0.7);
        let (a, b) = (mid(1), mid(2));

        let wtxn = db.begin_write().unwrap();
        {
            let mut e = wtxn.open_table(EDGES_TABLE).unwrap();
            let mut r = wtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
            link(
                &mut e,
                &mut r,
                NodeRef::Memory(a),
                EdgeKindRef::Builtin(EdgeKind::Caused),
                NodeRef::Memory(b),
                zero_disambiguator(),
                &d,
            )
            .unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let outs = walk_outgoing(&rtxn, NodeRef::Memory(a), None).unwrap();
        assert_eq!(outs.len(), 1);
        assert_eq!(outs[0].0, EdgeKindRef::Builtin(EdgeKind::Caused));
        assert_eq!(outs[0].1, NodeRef::Memory(b));
        assert_eq!(outs[0].3, d);
    }

    #[test]
    fn symmetric_builtin_writes_four_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let d = data(0.9);
        let (a, b) = (mid(10), mid(20));

        let wtxn = db.begin_write().unwrap();
        {
            let mut e = wtxn.open_table(EDGES_TABLE).unwrap();
            let mut r = wtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
            link(
                &mut e,
                &mut r,
                NodeRef::Memory(a),
                EdgeKindRef::Builtin(EdgeKind::SimilarTo),
                NodeRef::Memory(b),
                zero_disambiguator(),
                &d,
            )
            .unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let e = rtxn.open_table(EDGES_TABLE).unwrap();
        let r = rtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
        assert_eq!(e.iter().unwrap().count(), 2, "fwd: (A,B) + mirror (B,A)");
        assert_eq!(r.iter().unwrap().count(), 2, "rev: same pair, swapped");
    }

    #[test]
    fn symmetric_self_loop_writes_one_pair() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let d = data(0.9);
        let a = mid(42);

        let wtxn = db.begin_write().unwrap();
        {
            let mut e = wtxn.open_table(EDGES_TABLE).unwrap();
            let mut r = wtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
            link(
                &mut e,
                &mut r,
                NodeRef::Memory(a),
                EdgeKindRef::Builtin(EdgeKind::SimilarTo),
                NodeRef::Memory(a),
                zero_disambiguator(),
                &d,
            )
            .unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let e = rtxn.open_table(EDGES_TABLE).unwrap();
        let r = rtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
        assert_eq!(e.iter().unwrap().count(), 1);
        assert_eq!(r.iter().unwrap().count(), 1);
    }

    #[test]
    fn asymmetric_builtin_writes_one_pair() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let d = data(1.0);
        let (a, b) = (mid(1), mid(2));

        let wtxn = db.begin_write().unwrap();
        {
            let mut e = wtxn.open_table(EDGES_TABLE).unwrap();
            let mut r = wtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
            link(
                &mut e,
                &mut r,
                NodeRef::Memory(a),
                EdgeKindRef::Builtin(EdgeKind::Caused),
                NodeRef::Memory(b),
                zero_disambiguator(),
                &d,
            )
            .unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let e = rtxn.open_table(EDGES_TABLE).unwrap();
        let r = rtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
        assert_eq!(e.iter().unwrap().count(), 1);
        assert_eq!(r.iter().unwrap().count(), 1);
    }

    #[test]
    fn typed_edge_does_not_auto_mirror() {
        // Typed edges express symmetry at the sidecar level (via
        // canonical-pair ordering at write time). The edge table must
        // never auto-mirror them — that would double-count the row in
        // walk_outgoing / walk_incoming.
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let d = data(0.8);
        let (a, b) = (eid(0xAA), eid(0xBB));
        let rel = rid(0x55);

        let wtxn = db.begin_write().unwrap();
        {
            let mut e = wtxn.open_table(EDGES_TABLE).unwrap();
            let mut r = wtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
            link(
                &mut e,
                &mut r,
                NodeRef::Entity(a),
                EdgeKindRef::Typed(RelationTypeId::from(3)),
                NodeRef::Entity(b),
                rel.to_bytes(),
                &d,
            )
            .unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let e = rtxn.open_table(EDGES_TABLE).unwrap();
        assert_eq!(e.iter().unwrap().count(), 1, "no mirror");
    }

    #[test]
    fn memory_entity_edge_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let d = data(1.0);
        let (m, e) = (mid(7), eid(0x33));

        let wtxn = db.begin_write().unwrap();
        {
            let mut et = wtxn.open_table(EDGES_TABLE).unwrap();
            let mut rt = wtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
            link(
                &mut et,
                &mut rt,
                NodeRef::Memory(m),
                EdgeKindRef::Mentions,
                NodeRef::Entity(e),
                zero_disambiguator(),
                &d,
            )
            .unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let outs = walk_outgoing(&rtxn, NodeRef::Memory(m), None).unwrap();
        assert_eq!(outs.len(), 1);
        assert_eq!(outs[0].0, EdgeKindRef::Mentions);
        assert_eq!(outs[0].1, NodeRef::Entity(e));

        let ins = walk_incoming(&rtxn, NodeRef::Entity(e), None).unwrap();
        assert_eq!(ins.len(), 1);
        assert_eq!(ins[0].1, NodeRef::Memory(m));
    }

    #[test]
    fn entity_entity_typed_edge_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let d = data(0.95);
        let (a, b) = (eid(0xAA), eid(0xBB));
        let rt = RelationTypeId::from(11);
        let rel = rid(0x77);

        let wtxn = db.begin_write().unwrap();
        {
            let mut et = wtxn.open_table(EDGES_TABLE).unwrap();
            let mut rev = wtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
            link(
                &mut et,
                &mut rev,
                NodeRef::Entity(a),
                EdgeKindRef::Typed(rt),
                NodeRef::Entity(b),
                rel.to_bytes(),
                &d,
            )
            .unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let got = edge_get(
            &rtxn,
            NodeRef::Entity(a),
            EdgeKindRef::Typed(rt),
            NodeRef::Entity(b),
            rel.to_bytes(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(got, d);
    }

    #[test]
    fn prefix_scan_sort_substrate_first() {
        // walk_outgoing must return Builtin (tag 0) edges before
        // Mentions (tag 1) before Typed (tag 2) when no kind filter
        // is supplied.
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let d = data(1.0);
        let a = mid(1);
        let other_mem = mid(2);
        let other_ent = eid(0xAA);
        let rt = RelationTypeId::from(5);
        let rel = rid(0x10);

        let wtxn = db.begin_write().unwrap();
        {
            let mut e = wtxn.open_table(EDGES_TABLE).unwrap();
            let mut r = wtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
            // Insert in deliberately scrambled order — table sort
            // will reorder.
            link(
                &mut e,
                &mut r,
                NodeRef::Memory(a),
                EdgeKindRef::Typed(rt),
                NodeRef::Entity(other_ent),
                rel.to_bytes(),
                &d,
            )
            .unwrap();
            link(
                &mut e,
                &mut r,
                NodeRef::Memory(a),
                EdgeKindRef::Mentions,
                NodeRef::Entity(other_ent),
                zero_disambiguator(),
                &d,
            )
            .unwrap();
            link(
                &mut e,
                &mut r,
                NodeRef::Memory(a),
                EdgeKindRef::Builtin(EdgeKind::Caused),
                NodeRef::Memory(other_mem),
                zero_disambiguator(),
                &d,
            )
            .unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let outs = walk_outgoing(&rtxn, NodeRef::Memory(a), None).unwrap();
        assert_eq!(outs.len(), 3);
        assert!(matches!(outs[0].0, EdgeKindRef::Builtin(EdgeKind::Caused)));
        assert!(matches!(outs[1].0, EdgeKindRef::Mentions));
        assert!(matches!(outs[2].0, EdgeKindRef::Typed(_)));
    }

    #[test]
    fn walk_outgoing_kind_filter_typed_returns_only_typed() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let d = data(1.0);
        let a = eid(0xAA);
        let b = eid(0xBB);
        let c = eid(0xCC);
        let rt = RelationTypeId::from(9);
        let rel1 = rid(0x01);
        let rel2 = rid(0x02);

        let wtxn = db.begin_write().unwrap();
        {
            let mut e = wtxn.open_table(EDGES_TABLE).unwrap();
            let mut r = wtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
            // Two typed edges of the same type, different targets +
            // different relation ids (so the disambiguator differs).
            link(
                &mut e,
                &mut r,
                NodeRef::Entity(a),
                EdgeKindRef::Typed(rt),
                NodeRef::Entity(b),
                rel1.to_bytes(),
                &d,
            )
            .unwrap();
            link(
                &mut e,
                &mut r,
                NodeRef::Entity(a),
                EdgeKindRef::Typed(rt),
                NodeRef::Entity(c),
                rel2.to_bytes(),
                &d,
            )
            .unwrap();
            // One typed edge of a different type.
            link(
                &mut e,
                &mut r,
                NodeRef::Entity(a),
                EdgeKindRef::Typed(RelationTypeId::from(99)),
                NodeRef::Entity(b),
                rid(0x99).to_bytes(),
                &d,
            )
            .unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let outs = walk_outgoing(&rtxn, NodeRef::Entity(a), Some(EdgeKindRef::Typed(rt))).unwrap();
        assert_eq!(outs.len(), 2);
        for (kind, _, _, _) in &outs {
            assert_eq!(*kind, EdgeKindRef::Typed(rt));
        }
    }

    #[test]
    fn unlink_returns_false_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let (a, b) = (mid(1), mid(2));

        let wtxn = db.begin_write().unwrap();
        let removed = {
            let mut e = wtxn.open_table(EDGES_TABLE).unwrap();
            let mut r = wtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
            unlink(
                &mut e,
                &mut r,
                NodeRef::Memory(a),
                EdgeKindRef::Builtin(EdgeKind::Caused),
                NodeRef::Memory(b),
                zero_disambiguator(),
            )
            .unwrap()
        };
        wtxn.commit().unwrap();
        assert!(!removed);
    }

    #[test]
    fn unlink_first_true_second_false() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let d = data(1.0);
        let (a, b) = (mid(1), mid(2));

        let wtxn = db.begin_write().unwrap();
        {
            let mut e = wtxn.open_table(EDGES_TABLE).unwrap();
            let mut r = wtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
            link(
                &mut e,
                &mut r,
                NodeRef::Memory(a),
                EdgeKindRef::Builtin(EdgeKind::Caused),
                NodeRef::Memory(b),
                zero_disambiguator(),
                &d,
            )
            .unwrap();
        }
        wtxn.commit().unwrap();

        let wtxn = db.begin_write().unwrap();
        let first = {
            let mut e = wtxn.open_table(EDGES_TABLE).unwrap();
            let mut r = wtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
            unlink(
                &mut e,
                &mut r,
                NodeRef::Memory(a),
                EdgeKindRef::Builtin(EdgeKind::Caused),
                NodeRef::Memory(b),
                zero_disambiguator(),
            )
            .unwrap()
        };
        wtxn.commit().unwrap();
        assert!(first);

        let wtxn = db.begin_write().unwrap();
        let second = {
            let mut e = wtxn.open_table(EDGES_TABLE).unwrap();
            let mut r = wtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
            unlink(
                &mut e,
                &mut r,
                NodeRef::Memory(a),
                EdgeKindRef::Builtin(EdgeKind::Caused),
                NodeRef::Memory(b),
                zero_disambiguator(),
            )
            .unwrap()
        };
        wtxn.commit().unwrap();
        assert!(!second);
    }

    #[test]
    fn list_memory_edges_from_filters_non_memory_neighbours() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let d = data(1.0);
        let a = mid(1);
        let b = mid(2);
        let e = eid(0xAA);

        let wtxn = db.begin_write().unwrap();
        {
            let mut et = wtxn.open_table(EDGES_TABLE).unwrap();
            let mut rt = wtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
            link(
                &mut et,
                &mut rt,
                NodeRef::Memory(a),
                EdgeKindRef::Builtin(EdgeKind::Caused),
                NodeRef::Memory(b),
                zero_disambiguator(),
                &d,
            )
            .unwrap();
            link(
                &mut et,
                &mut rt,
                NodeRef::Memory(a),
                EdgeKindRef::Mentions,
                NodeRef::Entity(e),
                zero_disambiguator(),
                &d,
            )
            .unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let rows = list_memory_edges_from(&rtxn, a, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, EdgeKind::Caused);
        assert_eq!(rows[0].1, b);
    }

    // -----------------------------------------------------------------
    // proptest helpers.
    // -----------------------------------------------------------------

    fn arb_node_ref() -> impl Strategy<Value = NodeRef> {
        prop_oneof![
            proptest::array::uniform16(any::<u8>())
                .prop_map(|b| NodeRef::Memory(MemoryId::from_be_bytes(b))),
            proptest::array::uniform16(any::<u8>())
                .prop_map(|b| NodeRef::Entity(EntityId::from_bytes(b))),
        ]
    }

    fn builtin_from_u8(b: u8) -> EdgeKind {
        match b % 8 {
            0 => EdgeKind::Caused,
            1 => EdgeKind::FollowedBy,
            2 => EdgeKind::DerivedFrom,
            3 => EdgeKind::SimilarTo,
            4 => EdgeKind::Contradicts,
            5 => EdgeKind::Supports,
            6 => EdgeKind::References,
            _ => EdgeKind::PartOf,
        }
    }

    fn arb_edge_kind_ref() -> impl Strategy<Value = EdgeKindRef> {
        prop_oneof![
            (0u8..8).prop_map(|b| EdgeKindRef::Builtin(builtin_from_u8(b))),
            Just(EdgeKindRef::Mentions),
            any::<u32>().prop_map(|n| EdgeKindRef::Typed(RelationTypeId::from(n))),
        ]
    }

    proptest! {
        #[test]
        fn edge_key_codec_roundtrip(
            from_tag in 0u8..=1,
            from_id in proptest::array::uniform16(any::<u8>()),
            kind_tag in 0u8..=2,
            kind_byte in 0u8..=7,
            type_raw in any::<u32>(),
            to_tag in 0u8..=1,
            to_id in proptest::array::uniform16(any::<u8>()),
            disamb in proptest::array::uniform16(any::<u8>()),
        ) {
            let from = match from_tag {
                0 => NodeRef::Memory(MemoryId::from_be_bytes(from_id)),
                _ => NodeRef::Entity(EntityId::from_bytes(from_id)),
            };
            let to = match to_tag {
                0 => NodeRef::Memory(MemoryId::from_be_bytes(to_id)),
                _ => NodeRef::Entity(EntityId::from_bytes(to_id)),
            };
            let kind = match kind_tag {
                0 => EdgeKindRef::Builtin(builtin_from_u8(kind_byte)),
                1 => EdgeKindRef::Mentions,
                _ => EdgeKindRef::Typed(RelationTypeId::from(type_raw)),
            };
            let key = EdgeKey { from, kind, to, disambiguator: disamb };
            let bytes = key.encode();
            prop_assert_eq!(EdgeKey::decode(&bytes).unwrap(), key);
        }

        /// A `Vec<EdgeKey>` sorted by the encoded byte representation
        /// must match the natural `(from, kind, to, disambiguator)`
        /// tuple ordering after decoding. This is the contract the
        /// unified edge table relies on for prefix scans — if it
        /// breaks, `walk_outgoing` will surface duplicates or miss
        /// rows because the redb range iteration is byte-ordered.
        #[test]
        fn encoded_byte_order_matches_natural_tuple_order(
            keys in proptest::collection::vec(
                (arb_node_ref(), arb_edge_kind_ref(), arb_node_ref(), proptest::array::uniform16(any::<u8>())),
                50..80,
            ),
        ) {
            // Build EdgeKeys and sort by their encoded bytes.
            let mut by_bytes: Vec<(Vec<u8>, EdgeKey)> = keys
                .into_iter()
                .map(|(from, kind, to, disamb)| {
                    let k = EdgeKey { from, kind, to, disambiguator: disamb };
                    (k.encode(), k)
                })
                .collect();
            by_bytes.sort_by(|a, b| a.0.cmp(&b.0));

            // Each adjacent pair: the bytewise order must imply the
            // natural tuple order (from, kind, to, disambiguator).
            for win in by_bytes.windows(2) {
                let a = &win[0].1;
                let b = &win[1].1;
                let natural = (a.from, a.kind, a.to, a.disambiguator)
                    .cmp(&(b.from, b.kind, b.to, b.disambiguator));
                // bytewise sort yielded `a` before `b`, so `a <= b`.
                prop_assert!(
                    natural != std::cmp::Ordering::Greater,
                    "bytewise sort placed {a:?} before {b:?} but natural cmp says {natural:?}",
                );
            }
        }
    }

    // -----------------------------------------------------------------
    // Symmetric BFS invariants — link → walk_outgoing ↔ walk_incoming.
    // -----------------------------------------------------------------

    proptest! {
        #![proptest_config(ProptestConfig { cases: 24, .. ProptestConfig::default() })]

        /// For any edge `(a, k, b, d)`: after `link`, `walk_outgoing(a)`
        /// surfaces `(k, b, d)` *and* `walk_incoming(b)` surfaces
        /// `(k, a, d)`. The two tables stay in sync.
        #[test]
        fn walk_outgoing_in_sync_with_walk_incoming(
            from in arb_node_ref(),
            to in arb_node_ref(),
            kind in arb_edge_kind_ref(),
            disamb in proptest::array::uniform16(any::<u8>()),
            weight in 0.0f32..=1.0,
        ) {
            // Asymmetric self-edges of symmetric Builtin kinds collapse
            // into 1 row; skip the equality case so the assertion is
            // unambiguous for the (a != b) regime.
            prop_assume!(from != to);

            // Typed edges require a non-zero disambiguator in practice
            // (the RelationId carries identity); a zero disamb is still
            // legal for the table, so we keep arbitrary disambs.
            let _ = disamb;

            let dir = tempfile::tempdir().unwrap();
            let db = fresh_db(&dir);
            let d = EdgeData::new(weight, origin::EXPLICIT, derived_by::CLIENT, 1);

            let wtxn = db.begin_write().unwrap();
            {
                let mut e = wtxn.open_table(EDGES_TABLE).unwrap();
                let mut r = wtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
                link(&mut e, &mut r, from, kind, to, disamb, &d).unwrap();
            }
            wtxn.commit().unwrap();

            let rtxn = db.begin_read().unwrap();
            let outs = walk_outgoing(&rtxn, from, None).unwrap();
            let ins = walk_incoming(&rtxn, to, None).unwrap();

            prop_assert!(
                outs.iter().any(|(k, n, ds, _)| *k == kind && *n == to && *ds == disamb),
                "walk_outgoing({from:?}) missing ({kind:?}, {to:?})",
            );
            prop_assert!(
                ins.iter().any(|(k, n, ds, _)| *k == kind && *n == from && *ds == disamb),
                "walk_incoming({to:?}) missing ({kind:?}, {from:?})",
            );
        }

        /// `link → unlink` returns the table to its initial (empty)
        /// state for both forward and reverse. Holds for every kind
        /// including symmetric Builtin where the mirror row is also
        /// removed.
        #[test]
        fn link_then_unlink_is_identity(
            from in arb_node_ref(),
            to in arb_node_ref(),
            kind in arb_edge_kind_ref(),
            disamb in proptest::array::uniform16(any::<u8>()),
        ) {
            let dir = tempfile::tempdir().unwrap();
            let db = fresh_db(&dir);
            let d = data(0.5);

            let wtxn = db.begin_write().unwrap();
            {
                let mut e = wtxn.open_table(EDGES_TABLE).unwrap();
                let mut r = wtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
                link(&mut e, &mut r, from, kind, to, disamb, &d).unwrap();
                let removed = unlink(&mut e, &mut r, from, kind, to, disamb).unwrap();
                prop_assert!(removed, "unlink should return true after a matching link");
            }
            wtxn.commit().unwrap();

            let rtxn = db.begin_read().unwrap();
            let e = rtxn.open_table(EDGES_TABLE).unwrap();
            let r = rtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
            prop_assert_eq!(e.iter().unwrap().count(), 0);
            prop_assert_eq!(r.iter().unwrap().count(), 0);
        }
    }

    /// Symmetric Builtin (`SimilarTo`, `Contradicts`) auto-mirror:
    /// inserting `(a, k, b)` with `a != b` produces a 4-row pattern —
    /// forward `(a→b)` + forward mirror `(b→a)` plus the two reverse
    /// rows. Both `walk_outgoing(a)` and `walk_outgoing(b)` must see
    /// the kind, and both `walk_incoming(a)` and `walk_incoming(b)`
    /// must see it as well. This is the contract the SimilarTo
    /// retriever relies on.
    #[test]
    fn symmetric_builtin_auto_mirror_4_row_pattern() {
        for k in [EdgeKind::SimilarTo, EdgeKind::Contradicts] {
            let dir = tempfile::tempdir().unwrap();
            let db = fresh_db(&dir);
            let d = data(0.7);
            let (a, b) = (mem_node(1), mem_node(2));

            let wtxn = db.begin_write().unwrap();
            {
                let mut e = wtxn.open_table(EDGES_TABLE).unwrap();
                let mut r = wtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
                link(
                    &mut e,
                    &mut r,
                    a,
                    EdgeKindRef::Builtin(k),
                    b,
                    zero_disambiguator(),
                    &d,
                )
                .unwrap();
            }
            wtxn.commit().unwrap();

            let rtxn = db.begin_read().unwrap();
            let e = rtxn.open_table(EDGES_TABLE).unwrap();
            let r = rtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
            assert_eq!(e.iter().unwrap().count(), 2, "{k:?}: forward (a,b)+(b,a)");
            assert_eq!(r.iter().unwrap().count(), 2, "{k:?}: reverse mirrors");

            let outs_a = walk_outgoing(&rtxn, a, None).unwrap();
            let outs_b = walk_outgoing(&rtxn, b, None).unwrap();
            let ins_a = walk_incoming(&rtxn, a, None).unwrap();
            let ins_b = walk_incoming(&rtxn, b, None).unwrap();

            assert!(
                outs_a.iter().any(|(_, n, _, _)| *n == b),
                "{k:?}: walk_outgoing(a) ⊇ {{({k:?}, b)}}"
            );
            assert!(
                outs_b.iter().any(|(_, n, _, _)| *n == a),
                "{k:?}: walk_outgoing(b) ⊇ {{({k:?}, a)}}"
            );
            assert!(
                ins_a.iter().any(|(_, n, _, _)| *n == b),
                "{k:?}: walk_incoming(a) ⊇ {{({k:?}, b)}}"
            );
            assert!(
                ins_b.iter().any(|(_, n, _, _)| *n == a),
                "{k:?}: walk_incoming(b) ⊇ {{({k:?}, a)}}"
            );
        }
    }

    /// Self-edge `(a, k, a)` for a symmetric Builtin must produce
    /// exactly 1 forward row + 1 reverse row — auto-mirror is
    /// suppressed because the row is its own mirror.
    #[test]
    fn symmetric_self_loop_writes_one_forward_one_reverse() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let d = data(0.9);
        let a = mem_node(42);

        let wtxn = db.begin_write().unwrap();
        {
            let mut e = wtxn.open_table(EDGES_TABLE).unwrap();
            let mut r = wtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
            link(
                &mut e,
                &mut r,
                a,
                EdgeKindRef::Builtin(EdgeKind::SimilarTo),
                a,
                zero_disambiguator(),
                &d,
            )
            .unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let e = rtxn.open_table(EDGES_TABLE).unwrap();
        let r = rtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
        assert_eq!(e.iter().unwrap().count(), 1);
        assert_eq!(r.iter().unwrap().count(), 1);

        let outs = walk_outgoing(&rtxn, a, None).unwrap();
        let ins = walk_incoming(&rtxn, a, None).unwrap();
        assert_eq!(outs.len(), 1);
        assert_eq!(ins.len(), 1);
        assert_eq!(outs[0].1, a);
        assert_eq!(ins[0].1, a);
    }

    /// Mention edges are intrinsically directional (memory → entity).
    /// `link` must NOT auto-mirror them — doing so would invent an
    /// `(entity, Mentions, memory)` row that flips the semantics
    /// ("an entity mentions a memory") and would corrupt the
    /// extractor pipeline's `walk_incoming(Entity)` view of "which
    /// memories mention me".
    #[test]
    fn mention_edge_is_asymmetric_no_auto_mirror() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let d = data(0.9);
        let memory = mem_node(1);
        let entity = ent_node(0xAA);

        let wtxn = db.begin_write().unwrap();
        {
            let mut e = wtxn.open_table(EDGES_TABLE).unwrap();
            let mut r = wtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
            link(
                &mut e,
                &mut r,
                memory,
                EdgeKindRef::Mentions,
                entity,
                zero_disambiguator(),
                &d,
            )
            .unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let e = rtxn.open_table(EDGES_TABLE).unwrap();
        let r = rtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
        // Exactly one forward + one reverse row. A symmetric kind
        // would have produced two forward + two reverse rows.
        assert_eq!(e.iter().unwrap().count(), 1, "no auto-mirror forward");
        assert_eq!(r.iter().unwrap().count(), 1, "no auto-mirror reverse");

        // walk_outgoing(memory) sees the entity.
        let outs_m = walk_outgoing(&rtxn, memory, None).unwrap();
        assert_eq!(outs_m.len(), 1);
        assert_eq!(outs_m[0].1, entity);
        // walk_outgoing(entity) sees nothing — the mention does not
        // flow back from the entity side.
        let outs_e = walk_outgoing(&rtxn, entity, None).unwrap();
        assert!(outs_e.is_empty(), "entity should have no outgoing Mention");
        // walk_incoming(entity) sees the memory (reverse index).
        let ins_e = walk_incoming(&rtxn, entity, None).unwrap();
        assert_eq!(ins_e.len(), 1);
        assert_eq!(ins_e[0].1, memory);
        // walk_incoming(memory) sees nothing — no one mentions a memory.
        let ins_m = walk_incoming(&rtxn, memory, None).unwrap();
        assert!(
            ins_m.is_empty(),
            "memory should have no incoming Mention edges from auto-mirror"
        );
    }

    /// All eight `Builtin` kinds round-trip codec-only (no DB).
    /// Catches a future `EdgeKind` renumbering / bit-pattern bug
    /// even when the higher-level link/walk tests don't exercise
    /// every kind.
    #[test]
    fn all_builtin_kinds_roundtrip_through_codec() {
        for k in [
            EdgeKind::Caused,
            EdgeKind::FollowedBy,
            EdgeKind::DerivedFrom,
            EdgeKind::SimilarTo,
            EdgeKind::Contradicts,
            EdgeKind::Supports,
            EdgeKind::References,
            EdgeKind::PartOf,
        ] {
            for disamb in [zero_disambiguator(), [0xFFu8; 16], {
                let mut b = [0u8; 16];
                b[0] = 0xAB;
                b[15] = 0xCD;
                b
            }] {
                let key = EdgeKey {
                    from: mem_node(1),
                    kind: EdgeKindRef::Builtin(k),
                    to: mem_node(2),
                    disambiguator: disamb,
                };
                let bytes = key.encode();
                assert_eq!(EdgeKey::decode(&bytes).unwrap(), key, "{k:?}/{disamb:?}");
            }
        }
    }
}
