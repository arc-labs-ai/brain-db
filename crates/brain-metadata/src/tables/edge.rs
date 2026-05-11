//! `edges_out` and `edges_in` tables, plus LINK/UNLINK/list helpers.
//!
//! See `spec/07_metadata_graph/04_edge_storage.md` (full) and
//! `spec/02_data_model/06_edges.md` for the edge kind catalog +
//! symmetric flag.
//!
//! ## Two indexes, same data
//!
//! - [`EDGES_OUT_TABLE`] keyed by `(source, kind, target)` — forward
//!   queries ("what does memory X cause?").
//! - [`EDGES_IN_TABLE`] keyed by `(target, kind, source)` — reverse
//!   queries ("what was caused by X?").
//!
//! ## Symmetric edges
//!
//! Spec §02/06 §2 marks `SimilarTo` and `Contradicts` as symmetric.
//! A logical symmetric edge A↔B is stored as **four** physical rows:
//!
//! - `edges_out[A, K, B]` (direct) + `edges_in[B, K, A]` (reverse of direct)
//! - `edges_out[B, K, A]` (mirror)  + `edges_in[A, K, B]` (reverse of mirror)
//!
//! For self-symmetric edges (`A == B`), the mirror is skipped — `(A, K, A)`
//! is its own reverse. Without the guard we'd insert a redundant row.
//!
//! ## Cross-table count maintenance is out-of-scope here
//!
//! Spec §07/04 §5/§6 says LINK/UNLINK also update the `memories`
//! table's `edges_out_count` / `edges_in_count`. That's cross-table;
//! the `MetadataDb` wrapper in sub-task 3.10 composes [`link`] /
//! [`unlink`] with the count update inside one redb transaction.

use brain_core::{EdgeKind, MemoryId};
use redb::{ReadOnlyTable, Table, TableDefinition};

// ---------------------------------------------------------------------------
// Tables.
// ---------------------------------------------------------------------------

/// `(source_bytes, kind_u8, target_bytes)` composite key. Used by both
/// `edges_out` and `edges_in` (the components' meaning differs by table).
pub type EdgeKey = ([u8; 16], u8, [u8; 16]);

pub const EDGES_OUT_TABLE: TableDefinition<'static, EdgeKey, EdgeData> =
    TableDefinition::new("edges_out");

pub const EDGES_IN_TABLE: TableDefinition<'static, EdgeKey, EdgeData> =
    TableDefinition::new("edges_in");

// ---------------------------------------------------------------------------
// origin / derived_by byte mappings.
// ---------------------------------------------------------------------------

/// `EdgeData::origin` byte values. Mirrors `brain_core::EdgeOrigin`.
///
/// Duplicates a mapping that also lives in
/// `brain_storage::wal::payload::edge_origin_to_u8` and tracker on
/// `MemoryKind` in `tables::memory`. This is the third occurrence; the
/// follow-up is to promote the `EdgeKind`/`EdgeOrigin`/`MemoryKind` byte
/// mappings to brain-core helpers.
pub mod origin {
    pub const EXPLICIT: u8 = 0;
    pub const AUTO_DERIVED: u8 = 1;
}

/// `EdgeData::derived_by` byte values. Spec §07/04 §4 says "which worker
/// created it" but doesn't enumerate. v1 assignment:
pub mod derived_by {
    pub const CLIENT: u8 = 0;
    pub const CONSOLIDATION_WORKER: u8 = 1;
    pub const SIMILARITY_WORKER: u8 = 2;
    // 3..=255 reserved for future workers.
}

// ---------------------------------------------------------------------------
// EdgeData.
// ---------------------------------------------------------------------------

/// Per-edge data stored in both tables. Spec §07/04 §4.
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
        // rkyv 0.7's `from_bytes` validates alignment; redb returns bytes
        // at arbitrary alignment. Copy into an AlignedVec first.
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
        redb::TypeName::new("brain_metadata::EdgeData::v1")
    }
}

// ---------------------------------------------------------------------------
// EdgeKind ↔ u8 helpers.
// ---------------------------------------------------------------------------

#[derive(thiserror::Error, Debug)]
pub enum EdgeQueryError {
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("corrupt EdgeKind byte {0} in stored key (not in 0..=7)")]
    BadKind(u8),
}

fn edge_kind_from_u8(b: u8) -> Result<EdgeKind, EdgeQueryError> {
    Ok(match b {
        0 => EdgeKind::Caused,
        1 => EdgeKind::FollowedBy,
        2 => EdgeKind::DerivedFrom,
        3 => EdgeKind::SimilarTo,
        4 => EdgeKind::Contradicts,
        5 => EdgeKind::Supports,
        6 => EdgeKind::References,
        7 => EdgeKind::PartOf,
        other => return Err(EdgeQueryError::BadKind(other)),
    })
}

// ---------------------------------------------------------------------------
// LINK / UNLINK helpers.
// ---------------------------------------------------------------------------

/// Insert (or update; spec §07/04 §12) the edge in both indexes.
/// Symmetric edges write the mirror direction too.
pub fn link(
    edges_out: &mut Table<'_, EdgeKey, EdgeData>,
    edges_in: &mut Table<'_, EdgeKey, EdgeData>,
    source: MemoryId,
    kind: EdgeKind,
    target: MemoryId,
    data: &EdgeData,
) -> Result<(), redb::StorageError> {
    let kind_byte = kind as u8;
    let s = source.to_be_bytes();
    let t = target.to_be_bytes();

    edges_out.insert(&(s, kind_byte, t), data)?;
    edges_in.insert(&(t, kind_byte, s), data)?;

    // Symmetric mirror; skip on self-edge (its own reverse).
    if kind.is_symmetric() && source != target {
        edges_out.insert(&(t, kind_byte, s), data)?;
        edges_in.insert(&(s, kind_byte, t), data)?;
    }
    Ok(())
}

/// Remove the edge from both indexes (and the mirror, if symmetric).
/// Returns `true` if the canonical `(source, kind, target)` row was
/// found in `edges_out`.
pub fn unlink(
    edges_out: &mut Table<'_, EdgeKey, EdgeData>,
    edges_in: &mut Table<'_, EdgeKey, EdgeData>,
    source: MemoryId,
    kind: EdgeKind,
    target: MemoryId,
) -> Result<bool, redb::StorageError> {
    let kind_byte = kind as u8;
    let s = source.to_be_bytes();
    let t = target.to_be_bytes();

    let removed = edges_out.remove(&(s, kind_byte, t))?.is_some();
    edges_in.remove(&(t, kind_byte, s))?;

    if kind.is_symmetric() && source != target {
        edges_out.remove(&(t, kind_byte, s))?;
        edges_in.remove(&(s, kind_byte, t))?;
    }
    Ok(removed)
}

// ---------------------------------------------------------------------------
// Range scans.
// ---------------------------------------------------------------------------

/// All edges with `source` as the source, optionally filtered by kind.
///
/// Returns `(kind, target, data)`. With `kind = Some(k)`, all results
/// have the same kind; with `kind = None`, results are sorted by kind
/// then target.
pub fn list_edges_from(
    edges_out: &ReadOnlyTable<EdgeKey, EdgeData>,
    source: MemoryId,
    kind: Option<EdgeKind>,
) -> Result<Vec<(EdgeKind, MemoryId, EdgeData)>, EdgeQueryError> {
    let s = source.to_be_bytes();
    range_scan(edges_out, s, kind, /* is_in_table */ false)
}

/// All edges with `target` as the target, optionally filtered by kind.
///
/// Returns `(kind, source, data)`.
pub fn list_edges_to(
    edges_in: &ReadOnlyTable<EdgeKey, EdgeData>,
    target: MemoryId,
    kind: Option<EdgeKind>,
) -> Result<Vec<(EdgeKind, MemoryId, EdgeData)>, EdgeQueryError> {
    let t = target.to_be_bytes();
    range_scan(edges_in, t, kind, /* is_in_table */ true)
}

fn range_scan(
    table: &ReadOnlyTable<EdgeKey, EdgeData>,
    prefix: [u8; 16],
    kind: Option<EdgeKind>,
    _is_in_table: bool,
) -> Result<Vec<(EdgeKind, MemoryId, EdgeData)>, EdgeQueryError> {
    let (kind_lo, kind_hi) = match kind {
        Some(k) => (k as u8, k as u8),
        None => (0u8, u8::MAX),
    };
    let lo = (prefix, kind_lo, [0u8; 16]);
    let hi = (prefix, kind_hi, [0xFFu8; 16]);

    let mut out = Vec::new();
    for entry in table.range(lo..=hi)? {
        let (k, v) = entry?;
        let (_, kind_byte, other_bytes) = k.value();
        let kind = edge_kind_from_u8(kind_byte)?;
        out.push((kind, MemoryId::from_be_bytes(other_bytes), v.value()));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use brain_core::{EdgeKind, MemoryId};
    use redb::{Database, ReadableDatabase, ReadableTable};

    fn mid(slot: u64) -> MemoryId {
        MemoryId::pack(1, slot, 1)
    }

    fn fresh_db(dir: &tempfile::TempDir) -> Database {
        Database::create(dir.path().join("test.redb")).expect("create redb")
    }

    fn data(weight: f32) -> EdgeData {
        EdgeData::new(
            weight,
            origin::EXPLICIT,
            derived_by::CLIENT,
            1_700_000_000_000_000_000,
        )
    }

    // ----- EdgeData round-trip ------------------------------------------

    #[test]
    fn edge_data_round_trip_with_annotation() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let mut d = data(0.75);
        d.annotation = Some("annotated".to_string());

        let wtxn = db.begin_write().unwrap();
        {
            let mut out = wtxn.open_table(EDGES_OUT_TABLE).unwrap();
            out.insert(&(mid(1).to_be_bytes(), 0, mid(2).to_be_bytes()), &d)
                .unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let out = rtxn.open_table(EDGES_OUT_TABLE).unwrap();
        let v = out
            .get(&(mid(1).to_be_bytes(), 0, mid(2).to_be_bytes()))
            .unwrap()
            .unwrap();
        assert_eq!(v.value().annotation.as_deref(), Some("annotated"));
        assert!((v.value().weight - 0.75).abs() < f32::EPSILON);
    }

    #[test]
    fn edge_data_round_trip_without_annotation() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let d = data(0.5);

        let wtxn = db.begin_write().unwrap();
        {
            let mut out = wtxn.open_table(EDGES_OUT_TABLE).unwrap();
            out.insert(&(mid(1).to_be_bytes(), 0, mid(2).to_be_bytes()), &d)
                .unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let out = rtxn.open_table(EDGES_OUT_TABLE).unwrap();
        let v = out
            .get(&(mid(1).to_be_bytes(), 0, mid(2).to_be_bytes()))
            .unwrap()
            .unwrap();
        assert_eq!(v.value().annotation, None);
    }

    // ----- Asymmetric link/unlink ---------------------------------------

    #[test]
    fn caused_link_writes_both_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let d = data(1.0);
        let (a, b) = (mid(1), mid(2));

        let wtxn = db.begin_write().unwrap();
        {
            let mut out = wtxn.open_table(EDGES_OUT_TABLE).unwrap();
            let mut in_ = wtxn.open_table(EDGES_IN_TABLE).unwrap();
            link(&mut out, &mut in_, a, EdgeKind::Caused, b, &d).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let out = rtxn.open_table(EDGES_OUT_TABLE).unwrap();
        let in_ = rtxn.open_table(EDGES_IN_TABLE).unwrap();
        let kind_byte = EdgeKind::Caused as u8;
        // Direct entry in edges_out: (A, Caused, B).
        assert!(out
            .get(&(a.to_be_bytes(), kind_byte, b.to_be_bytes()))
            .unwrap()
            .is_some());
        // Reverse-index entry in edges_in: (B, Caused, A).
        assert!(in_
            .get(&(b.to_be_bytes(), kind_byte, a.to_be_bytes()))
            .unwrap()
            .is_some());
        // No mirror (asymmetric).
        assert!(out
            .get(&(b.to_be_bytes(), kind_byte, a.to_be_bytes()))
            .unwrap()
            .is_none());
    }

    #[test]
    fn asymmetric_unlink_removes_both() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let d = data(1.0);
        let (a, b) = (mid(1), mid(2));

        let wtxn = db.begin_write().unwrap();
        {
            let mut out = wtxn.open_table(EDGES_OUT_TABLE).unwrap();
            let mut in_ = wtxn.open_table(EDGES_IN_TABLE).unwrap();
            link(&mut out, &mut in_, a, EdgeKind::Caused, b, &d).unwrap();
            assert!(unlink(&mut out, &mut in_, a, EdgeKind::Caused, b).unwrap());
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let out = rtxn.open_table(EDGES_OUT_TABLE).unwrap();
        let in_ = rtxn.open_table(EDGES_IN_TABLE).unwrap();
        let kind_byte = EdgeKind::Caused as u8;
        assert!(out
            .get(&(a.to_be_bytes(), kind_byte, b.to_be_bytes()))
            .unwrap()
            .is_none());
        assert!(in_
            .get(&(b.to_be_bytes(), kind_byte, a.to_be_bytes()))
            .unwrap()
            .is_none());
    }

    // ----- Symmetric link/unlink ----------------------------------------

    #[test]
    fn similar_link_writes_four_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let d = data(0.9);
        let (a, b) = (mid(10), mid(20));

        let wtxn = db.begin_write().unwrap();
        {
            let mut out = wtxn.open_table(EDGES_OUT_TABLE).unwrap();
            let mut in_ = wtxn.open_table(EDGES_IN_TABLE).unwrap();
            link(&mut out, &mut in_, a, EdgeKind::SimilarTo, b, &d).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let out = rtxn.open_table(EDGES_OUT_TABLE).unwrap();
        let in_ = rtxn.open_table(EDGES_IN_TABLE).unwrap();
        let k = EdgeKind::SimilarTo as u8;
        // Four expected rows.
        assert!(out
            .get(&(a.to_be_bytes(), k, b.to_be_bytes()))
            .unwrap()
            .is_some());
        assert!(out
            .get(&(b.to_be_bytes(), k, a.to_be_bytes()))
            .unwrap()
            .is_some());
        assert!(in_
            .get(&(b.to_be_bytes(), k, a.to_be_bytes()))
            .unwrap()
            .is_some());
        assert!(in_
            .get(&(a.to_be_bytes(), k, b.to_be_bytes()))
            .unwrap()
            .is_some());
    }

    #[test]
    fn symmetric_unlink_removes_all_four() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let d = data(0.9);
        let (a, b) = (mid(10), mid(20));

        let wtxn = db.begin_write().unwrap();
        {
            let mut out = wtxn.open_table(EDGES_OUT_TABLE).unwrap();
            let mut in_ = wtxn.open_table(EDGES_IN_TABLE).unwrap();
            link(&mut out, &mut in_, a, EdgeKind::SimilarTo, b, &d).unwrap();
            assert!(unlink(&mut out, &mut in_, a, EdgeKind::SimilarTo, b).unwrap());
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let out = rtxn.open_table(EDGES_OUT_TABLE).unwrap();
        let in_ = rtxn.open_table(EDGES_IN_TABLE).unwrap();
        let k = EdgeKind::SimilarTo as u8;
        assert!(out
            .get(&(a.to_be_bytes(), k, b.to_be_bytes()))
            .unwrap()
            .is_none());
        assert!(out
            .get(&(b.to_be_bytes(), k, a.to_be_bytes()))
            .unwrap()
            .is_none());
        assert!(in_
            .get(&(b.to_be_bytes(), k, a.to_be_bytes()))
            .unwrap()
            .is_none());
        assert!(in_
            .get(&(a.to_be_bytes(), k, b.to_be_bytes()))
            .unwrap()
            .is_none());
    }

    #[test]
    fn self_symmetric_edge_writes_two_rows_not_four() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let d = data(1.0);
        let a = mid(42);

        let wtxn = db.begin_write().unwrap();
        {
            let mut out = wtxn.open_table(EDGES_OUT_TABLE).unwrap();
            let mut in_ = wtxn.open_table(EDGES_IN_TABLE).unwrap();
            link(&mut out, &mut in_, a, EdgeKind::SimilarTo, a, &d).unwrap();
        }
        wtxn.commit().unwrap();

        // Count rows: one in edges_out, one in edges_in. No mirror.
        let rtxn = db.begin_read().unwrap();
        let out = rtxn.open_table(EDGES_OUT_TABLE).unwrap();
        let in_ = rtxn.open_table(EDGES_IN_TABLE).unwrap();
        let out_count = out.iter().unwrap().filter_map(Result::ok).count();
        let in_count = in_.iter().unwrap().filter_map(Result::ok).count();
        assert_eq!(out_count, 1);
        assert_eq!(in_count, 1);
    }

    // ----- Range queries ------------------------------------------------

    #[test]
    fn list_edges_from_all_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let d = data(1.0);
        let a = mid(1);

        let wtxn = db.begin_write().unwrap();
        {
            let mut out = wtxn.open_table(EDGES_OUT_TABLE).unwrap();
            let mut in_ = wtxn.open_table(EDGES_IN_TABLE).unwrap();
            link(&mut out, &mut in_, a, EdgeKind::Caused, mid(2), &d).unwrap();
            link(&mut out, &mut in_, a, EdgeKind::FollowedBy, mid(3), &d).unwrap();
            link(&mut out, &mut in_, a, EdgeKind::References, mid(4), &d).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let out = rtxn.open_table(EDGES_OUT_TABLE).unwrap();
        let results = list_edges_from(&out, a, None).unwrap();
        assert_eq!(results.len(), 3);
        // Sorted by kind: Caused(0), FollowedBy(1), References(6).
        assert_eq!(results[0].0, EdgeKind::Caused);
        assert_eq!(results[1].0, EdgeKind::FollowedBy);
        assert_eq!(results[2].0, EdgeKind::References);
    }

    #[test]
    fn list_edges_from_filtered_by_kind() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let d = data(1.0);
        let a = mid(1);

        let wtxn = db.begin_write().unwrap();
        {
            let mut out = wtxn.open_table(EDGES_OUT_TABLE).unwrap();
            let mut in_ = wtxn.open_table(EDGES_IN_TABLE).unwrap();
            link(&mut out, &mut in_, a, EdgeKind::Caused, mid(2), &d).unwrap();
            link(&mut out, &mut in_, a, EdgeKind::Caused, mid(3), &d).unwrap();
            link(&mut out, &mut in_, a, EdgeKind::References, mid(4), &d).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let out = rtxn.open_table(EDGES_OUT_TABLE).unwrap();
        let results = list_edges_from(&out, a, Some(EdgeKind::Caused)).unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|(k, _, _)| *k == EdgeKind::Caused));
    }

    #[test]
    fn list_edges_to_all_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let d = data(1.0);
        let b = mid(99);

        let wtxn = db.begin_write().unwrap();
        {
            let mut out = wtxn.open_table(EDGES_OUT_TABLE).unwrap();
            let mut in_ = wtxn.open_table(EDGES_IN_TABLE).unwrap();
            link(&mut out, &mut in_, mid(1), EdgeKind::Caused, b, &d).unwrap();
            link(&mut out, &mut in_, mid(2), EdgeKind::Supports, b, &d).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let in_ = rtxn.open_table(EDGES_IN_TABLE).unwrap();
        let results = list_edges_to(&in_, b, None).unwrap();
        assert_eq!(results.len(), 2);
        // Sorted by kind: Caused(0), Supports(5).
        assert_eq!(results[0].0, EdgeKind::Caused);
        assert_eq!(results[1].0, EdgeKind::Supports);
    }

    #[test]
    fn list_edges_to_includes_symmetric_mirror() {
        // If A→B is a symmetric SimilarTo, then list_edges_to(A, SimilarTo)
        // includes B (because the mirror writes (B, SimilarTo, A) into
        // edges_out, whose reverse-index lands in edges_in).
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let d = data(0.9);
        let (a, b) = (mid(10), mid(20));

        let wtxn = db.begin_write().unwrap();
        {
            let mut out = wtxn.open_table(EDGES_OUT_TABLE).unwrap();
            let mut in_ = wtxn.open_table(EDGES_IN_TABLE).unwrap();
            link(&mut out, &mut in_, a, EdgeKind::SimilarTo, b, &d).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let in_ = rtxn.open_table(EDGES_IN_TABLE).unwrap();
        let results = list_edges_to(&in_, a, Some(EdgeKind::SimilarTo)).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1, b);
    }

    // ----- Update via relink --------------------------------------------

    #[test]
    fn relink_overwrites_edge_data() {
        // Spec §07/04 §12: second LINK with same key updates the data.
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let (a, b) = (mid(1), mid(2));

        let wtxn = db.begin_write().unwrap();
        {
            let mut out = wtxn.open_table(EDGES_OUT_TABLE).unwrap();
            let mut in_ = wtxn.open_table(EDGES_IN_TABLE).unwrap();
            link(&mut out, &mut in_, a, EdgeKind::Caused, b, &data(0.3)).unwrap();
            link(&mut out, &mut in_, a, EdgeKind::Caused, b, &data(0.9)).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        let out = rtxn.open_table(EDGES_OUT_TABLE).unwrap();
        let v = out
            .get(&(a.to_be_bytes(), EdgeKind::Caused as u8, b.to_be_bytes()))
            .unwrap()
            .unwrap();
        assert!((v.value().weight - 0.9).abs() < f32::EPSILON);
    }
}
