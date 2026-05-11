//! `HnswIndex<const D: usize>` — const-generic wrapper around `hnsw_rs::Hnsw<f32, DistCosine>`.
//!
//! Spec references:
//! - `spec/06_ann_index/02_parameters.md` — defaults and ranges.
//! - `spec/06_ann_index/01_hnsw_primer.md` §7 — distance metric: cosine on
//!   L2-normalised vectors (BGE-small output, so cosine = dot product).
//! - `spec/06_ann_index/03_insertion.md` §1–2, §10 — id_map pattern;
//!   duplicate-MemoryId is a bug we detect rather than letting hnsw_rs
//!   silently overwrite.
//! - `spec/06_ann_index/04_search.md` §1 — search returns sorted ascending
//!   by distance.
//!
//! ## Current surface (through sub-task 4.2)
//!
//! - [`HnswIndex::new`] — construct with [`crate::params::IndexParams`].
//! - [`HnswIndex::insert`] — `&mut self` + [`MemoryId`] + `&[f32; D]`.
//!   Returns [`HnswError::DuplicateMemoryId`] on re-insert.
//! - [`HnswIndex::search`] — `&self` + `&[f32; D]` + `k` + optional ef
//!   override (clamped to `[k, params.ef_search_max]`).
//!   Returns `Vec<(MemoryId, f32)>` sorted ascending by distance.
//! - [`HnswIndex::contains`], [`HnswIndex::len`], [`HnswIndex::is_empty`].
//!
//! ## What's NOT here yet
//!
//! - **Tombstone bitmap** — sub-task 4.3.
//! - **Search post-filter / tombstone awareness** — sub-task 4.4.
//! - **Persistence** — sub-task 4.5 (writes both the hnsw_rs graph and
//!   the [`crate::idmap::IdMap`] contents).
//! - **Rebuild from external iterator** — sub-task 4.6.
//! - **Concurrency wrapper** (`ArcSwap` + pending buffer) — sub-task 4.8.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use brain_core::MemoryId;
use hnsw_rs::api::AnnT;
use hnsw_rs::prelude::{DistCosine, Hnsw, HnswIo, Neighbour};
use thiserror::Error;

use crate::idmap::{IdMap, IdMapError};
use crate::params::{IndexParams, IndexParamsError, DEFAULT_CAPACITY_HINT, MAX_LAYER};
use crate::persistence::{self, Body, BodyError, Header, HeaderError, FOOTER_LEN, HEADER_LEN};
use crate::tombstones::TombstoneBitmap;

/// Default over-fetch multiplier for post-filter search. Spec §09 §2.
/// Initial fetch is `k * OVER_FACTOR`; the bailout loop escalates by
/// doubling on each retry, bounded by the index size (no point asking
/// hnsw_rs for more candidates than exist).
const OVER_FACTOR: usize = 2;

/// HNSW index parameterised by vector dimension `D`. Wraps
/// `hnsw_rs::Hnsw<f32, DistCosine>` with Brain's parameter discipline.
///
/// **Single-writer:** `insert` takes `&mut self`. hnsw_rs itself only
/// requires `&self` (it uses internal locking for its unused
/// multi-writer mode, spec `§06/08 §8`), but Brain's discipline
/// (CLAUDE.md §5 invariant 2) tightens this at the type level.
pub struct HnswIndex<const D: usize> {
    inner: Hnsw<'static, f32, DistCosine>,
    params: IndexParams,
    id_map: IdMap,
    tombstones: TombstoneBitmap,
}

/// Errors from [`HnswIndex`] construction and operations.
///
/// Persistence (4.5) and rebuild (4.6) will extend this enum with I/O
/// variants.
#[derive(Debug, Error)]
pub enum HnswError {
    #[error("invalid params: {0}")]
    InvalidParams(#[from] IndexParamsError),

    /// `memory_id` was already inserted. Per spec §06/03 §10 re-inserting
    /// an existing MemoryId is a caller bug; we detect rather than let
    /// hnsw_rs silently overwrite.
    #[error("duplicate memory_id: {memory_id_bytes:?}")]
    DuplicateMemoryId { memory_id_bytes: [u8; 16] },

    /// The internal `u32` id_map allocator hit `u32::MAX`. Spec's
    /// per-shard ceiling is ~10M memories — this is unreachable in
    /// practice; the check is defensive.
    #[error("id_map exhausted: u32::MAX internal ids allocated")]
    IdMapExhausted,

    /// State-changing operation referenced a `MemoryId` not present in
    /// the id_map. Spec `§06/05` calls re-tombstoning known memories;
    /// an unknown id is a caller bug.
    ///
    /// Note: the read-only [`HnswIndex::is_tombstoned`] returns `false`
    /// rather than this error — query paths are fail-soft.
    #[error("memory_id not found in id_map: {memory_id_bytes:?}")]
    MemoryIdNotFound { memory_id_bytes: [u8; 16] },

    // ---- 4.5 snapshot errors ----------------------------------------
    #[error("snapshot I/O error: {0}")]
    SnapshotIo(#[from] std::io::Error),

    #[error("snapshot magic mismatch: expected BHN0, got {0:?}")]
    SnapshotBadMagic([u8; 4]),

    #[error("snapshot format_version {0} not supported by this binary")]
    SnapshotUnsupportedVersion(u32),

    #[error("snapshot shard_uuid mismatch: expected {expected:?}, got {got:?}")]
    SnapshotShardMismatch { expected: [u8; 16], got: [u8; 16] },

    #[error("snapshot vector dim mismatch: expected {expected}, got {got}")]
    SnapshotDimMismatch { expected: usize, got: u32 },

    #[error("snapshot header CRC mismatch: expected {expected:08x}, got {got:08x}")]
    SnapshotBadHeaderCrc { expected: u32, got: u32 },

    #[error("snapshot BLAKE3 footer mismatch: file corrupted")]
    SnapshotBadFooter,

    #[error("snapshot truncated: expected at least {expected} bytes, got {got}")]
    SnapshotTruncated { expected: usize, got: usize },

    #[error("snapshot body malformed: {0}")]
    SnapshotBadBody(&'static str),

    #[error("hnsw_rs load failed: {0}")]
    HnswLoadFailed(String),
}

impl From<HeaderError> for HnswError {
    fn from(e: HeaderError) -> Self {
        match e {
            HeaderError::Truncated { expected, got } => {
                HnswError::SnapshotTruncated { expected, got }
            }
            HeaderError::BadMagic(m) => HnswError::SnapshotBadMagic(m),
            HeaderError::UnsupportedVersion(v) => HnswError::SnapshotUnsupportedVersion(v),
            HeaderError::BadCrc { expected, got } => {
                HnswError::SnapshotBadHeaderCrc { expected, got }
            }
        }
    }
}

impl From<BodyError> for HnswError {
    fn from(e: BodyError) -> Self {
        match e {
            BodyError::Truncated => HnswError::SnapshotBadBody("truncated mid-section"),
            BodyError::TrailingBytes(_) => HnswError::SnapshotBadBody("trailing bytes after body"),
        }
    }
}

impl From<IdMapError> for HnswError {
    fn from(e: IdMapError) -> Self {
        match e {
            IdMapError::AlreadyInserted { memory_id_bytes } => {
                HnswError::DuplicateMemoryId { memory_id_bytes }
            }
            IdMapError::Exhausted => HnswError::IdMapExhausted,
        }
    }
}

impl<const D: usize> HnswIndex<D> {
    /// Build a fresh empty index using the given parameters.
    ///
    /// Validates `params` against `spec/06_ann_index/02_parameters.md`'s
    /// ranges. Pre-allocates internal tables sized to
    /// [`crate::params::DEFAULT_CAPACITY_HINT`]; this is a hint, not a cap.
    pub fn new(params: IndexParams) -> Result<Self, HnswError> {
        params.validate()?;
        let inner = Hnsw::<f32, DistCosine>::new(
            params.m,
            DEFAULT_CAPACITY_HINT,
            MAX_LAYER,
            params.ef_construction,
            DistCosine,
        );
        Ok(Self {
            inner,
            params,
            id_map: IdMap::new(),
            tombstones: TombstoneBitmap::new(),
        })
    }

    /// Insert `vector` under `memory_id`. Single-writer per shard —
    /// encoded via `&mut self`.
    ///
    /// Returns [`HnswError::DuplicateMemoryId`] if `memory_id` was
    /// already inserted; the index is unchanged on the duplicate path
    /// (no internal id burned). Spec §06/03 §10.
    pub fn insert(&mut self, memory_id: MemoryId, vector: &[f32; D]) -> Result<(), HnswError> {
        let internal_id = self.id_map.insert(memory_id)?;
        // `Hnsw::insert_slice` takes a `(&[T], usize)` tuple.
        self.inner
            .insert_slice((vector.as_slice(), internal_id as usize));
        Ok(())
    }

    /// Search for the `k` nearest neighbours of `query`, post-filtered
    /// by `filter`. Returns `(MemoryId, similarity)` tuples **sorted
    /// descending by similarity** (best match first).
    ///
    /// **Similarity, not distance.** Per spec §04 §1, results carry
    /// `similarity = 1.0 - distance`, in `[-1, 1]`: 1.0 = identical,
    /// 0 = orthogonal, -1 = opposite (for L2-normalised input).
    ///
    /// **Tombstoned memories are always excluded** (spec §06/05 §2);
    /// the tombstone filter is implicit and applies regardless of
    /// `filter`'s return value.
    ///
    /// **Over-fetch + bailout retry** (spec §09 §2 + §7). The search
    /// initially requests `k * OVER_FACTOR` candidates from hnsw_rs.
    /// If fewer than `k` survive the filter chain, the loop scales up:
    /// first the fetch multiplier (capped at `OVER_FACTOR_CAP`), then
    /// `ef` doubles up to `params.ef_search_max`. If even that doesn't
    /// gather `k`, returns fewer-than-`k` results (per spec §09 §7).
    ///
    /// `ef` argument overrides the per-query search width:
    /// - `None` → uses `params.ef_search`.
    /// - `Some(v)` → clamped to `[k, params.ef_search_max]` per
    ///   `spec/06_ann_index/02_parameters.md` §5 + §8.
    #[must_use]
    pub fn search<F>(
        &self,
        query: &[f32; D],
        k: usize,
        ef: Option<usize>,
        filter: F,
    ) -> Vec<(MemoryId, f32)>
    where
        F: Fn(MemoryId) -> bool,
    {
        // Empty index: nothing to search.
        if k == 0 || self.is_empty() {
            return Vec::new();
        }

        let total_nodes = self.len();
        let mut ef = self.resolve_ef(k, ef);
        let mut fetch_multiplier = OVER_FACTOR;
        let mut results: Vec<(MemoryId, f32)> = Vec::with_capacity(k);

        loop {
            results.clear();
            let fetch_k = k.saturating_mul(fetch_multiplier).min(total_nodes);
            let neighbours: Vec<Neighbour> = self.inner.search(query.as_slice(), fetch_k, ef);

            for n in neighbours {
                if results.len() >= k {
                    break;
                }
                let Ok(internal_id) = u32::try_from(n.d_id) else {
                    continue;
                };
                // Implicit tombstone filter (spec §06/05 §2).
                if self.tombstones.is_set(internal_id) {
                    continue;
                }
                let Some(memory_id) = self.id_map.lookup_reverse(internal_id) else {
                    tracing::warn!(
                        internal_id,
                        "hnsw_rs returned an internal id with no MemoryId mapping; dropping",
                    );
                    continue;
                };
                if !filter(memory_id) {
                    continue;
                }
                let similarity = 1.0 - n.distance;
                results.push((memory_id, similarity));
            }

            if results.len() >= k {
                break;
            }

            // Bailout escalation (spec §09 §7). Grow both axes:
            // - fetch_multiplier doubles, bounded by total_nodes.
            // - ef doubles, bounded by params.ef_search_max.
            // Stop when both are saturated.
            let fetch_saturated = fetch_k >= total_nodes;
            let ef_saturated = ef >= self.params.ef_search_max;
            if fetch_saturated && ef_saturated {
                tracing::debug!(
                    requested_k = k,
                    returned = results.len(),
                    "search bailout exhausted; returning partial results",
                );
                break;
            }
            if !fetch_saturated {
                fetch_multiplier = fetch_multiplier.saturating_mul(2);
            }
            if !ef_saturated {
                ef = ef.saturating_mul(2).min(self.params.ef_search_max);
            }
        }

        // hnsw_rs returns ascending by distance → ascending = best first
        // for similarity (since similarity = 1 - distance, higher
        // similarity = lower distance). The output of the loop above
        // preserves hnsw_rs's order, which is "best similarity first"
        // (descending by similarity). No additional sort needed.
        results
    }

    /// Convenience: search with no extra filter (tombstoned memories
    /// are still excluded — the tombstone filter is always implicit).
    /// Equivalent to `search(query, k, ef, |_| true)`.
    #[must_use]
    pub fn search_active(
        &self,
        query: &[f32; D],
        k: usize,
        ef: Option<usize>,
    ) -> Vec<(MemoryId, f32)> {
        self.search(query, k, ef, |_| true)
    }

    /// Does this index hold a vector for `memory_id`?
    #[must_use]
    pub fn contains(&self, memory_id: MemoryId) -> bool {
        self.id_map.contains(memory_id)
    }

    /// Mark `memory_id` as tombstoned. The node stays in the graph
    /// (spec `§06/05 §2`); search filtering at sub-task 4.4 drops
    /// tombstoned candidates from results.
    ///
    /// Returns [`HnswError::MemoryIdNotFound`] if `memory_id` isn't in
    /// the id_map.
    pub fn mark_tombstoned(&mut self, memory_id: MemoryId) -> Result<(), HnswError> {
        let internal_id =
            self.id_map
                .lookup_forward(memory_id)
                .ok_or(HnswError::MemoryIdNotFound {
                    memory_id_bytes: memory_id.to_be_bytes(),
                })?;
        self.tombstones.set(internal_id);
        Ok(())
    }

    /// Is `memory_id` tombstoned? Returns `false` for unknown ids —
    /// query paths are fail-soft.
    #[must_use]
    pub fn is_tombstoned(&self, memory_id: MemoryId) -> bool {
        match self.id_map.lookup_forward(memory_id) {
            Some(id) => self.tombstones.is_set(id),
            None => false,
        }
    }

    /// Running count of tombstoned memories in this index. O(1) per
    /// spec `§06/05 §13`'s `tombstone_ratio` metric expectation.
    #[must_use]
    pub fn tombstone_count(&self) -> usize {
        self.tombstones.count()
    }

    /// Number of vectors inserted. Cheap.
    #[must_use]
    pub fn len(&self) -> usize {
        self.id_map.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.id_map.is_empty()
    }

    /// The parameters this index was built with. Useful when sub-task 4.5
    /// (persistence) writes the snapshot header.
    #[must_use]
    pub fn params(&self) -> IndexParams {
        self.params
    }

    /// Build a fresh index from an iterator of `(MemoryId, vector)`
    /// pairs. The caller filters out tombstoned and corrupted memories
    /// upstream (spec §06/06 §3, §07 §12); `rebuild` simply iterates
    /// and inserts.
    ///
    /// Returns the new index plus a [`crate::rebuild::RebuildReport`]
    /// with insert count and wall-clock duration. The new index starts
    /// with no tombstones — spec §06/06 §3's "compaction" property.
    ///
    /// This is the **Build** phase only (spec §07 §5 step 1).
    /// Catch-up, atomic swap with the existing index, and old-index
    /// cleanup are the caller's responsibility (Phase 8 maintenance
    /// worker, composing with 4.8's `ArcSwap` wrapper).
    pub fn rebuild<I>(
        params: IndexParams,
        source: I,
    ) -> Result<(Self, crate::rebuild::RebuildReport), HnswError>
    where
        I: IntoIterator<Item = (MemoryId, [f32; D])>,
    {
        crate::rebuild::rebuild_impl(params, source)
    }

    /// Compute the effective `ef` for a search per spec §02 §5 + §8:
    ///
    /// - Floor at `k` (hnsw_rs requires `ef >= k` for k results).
    /// - Ceiling at `params.ef_search_max`.
    fn resolve_ef(&self, k: usize, override_ef: Option<usize>) -> usize {
        let base = override_ef.unwrap_or(self.params.ef_search);
        base.max(k).min(self.params.ef_search_max)
    }

    /// Save this index as a snapshot in `dir` under `basename`. Writes
    /// three files: `<basename>.hnsw.graph`, `<basename>.hnsw.data`
    /// (hnsw_rs's serialisation), and `<basename>.brain` (our wrapper
    /// with id_map + tombstones + integrity footer).
    ///
    /// Per spec `§06/06`, the snapshot is an *optional* fast-restart
    /// artifact — the arena + metadata remain the source of truth.
    /// Spec `§06/06 §5.3` mandates corruption detection: if any of the
    /// three files is missing or invalid at load time, the caller
    /// falls back to a full `rebuild` from arena + metadata (sub-task
    /// 4.6).
    ///
    /// The `.brain` file is written **last**, so its presence is the
    /// marker for "snapshot complete."
    pub fn save_snapshot(
        &self,
        dir: &Path,
        basename: &str,
        taken_at_lsn: u64,
        shard_uuid: [u8; 16],
    ) -> Result<(), HnswError> {
        fs::create_dir_all(dir)?;

        // 1. Remove any stale snapshot files at this basename so
        //    hnsw_rs's overwrite-avoidance doesn't pick a random
        //    suffix.
        for ext in [".hnsw.graph", ".hnsw.data", ".brain"] {
            let p = dir.join(format!("{basename}{ext}"));
            if p.exists() {
                fs::remove_file(&p)?;
            }
        }

        // 2. hnsw_rs writes the two `.hnsw.*` files. Skip on empty
        //    index — hnsw_rs's `file_dump` errors on zero nodes; the
        //    `.brain` file alone carries enough state to restore
        //    (loader notices `graph_node_count == 0` and constructs a
        //    fresh empty inner).
        if !self.is_empty() {
            self.inner
                .file_dump(dir, basename)
                .map_err(|e| HnswError::HnswLoadFailed(format!("file_dump: {e}")))?;
        }

        // 3. Build the `.brain` body bytes.
        let header = Header::new::<D>(
            shard_uuid,
            taken_at_lsn,
            self.id_map.len() as u64,
            self.params,
        );
        let header_bytes = header.encode();
        let body = Body::encode(&self.id_map, self.id_map.next_id(), &self.tombstones);

        let mut file_bytes = Vec::with_capacity(HEADER_LEN + body.bytes.len() + FOOTER_LEN);
        file_bytes.extend_from_slice(&header_bytes);
        file_bytes.extend_from_slice(&body.bytes);
        let footer = persistence::compute_footer(&file_bytes);
        file_bytes.extend_from_slice(&footer);

        // 4. Atomic write of the `.brain` file: tempfile + rename.
        let final_path = dir.join(format!("{basename}.brain"));
        let tmp_path: PathBuf = dir.join(format!("{basename}.brain.tmp"));
        {
            let mut f = fs::File::create(&tmp_path)?;
            f.write_all(&file_bytes)?;
            f.sync_all()?;
        }
        fs::rename(&tmp_path, &final_path)?;
        // fsync the directory so the rename is durable.
        if let Ok(dir_handle) = fs::File::open(dir) {
            let _ = dir_handle.sync_all();
        }

        tracing::info!(
            dir = %dir.display(),
            basename,
            graph_node_count = self.id_map.len(),
            taken_at_lsn,
            "saved HNSW snapshot",
        );
        Ok(())
    }

    /// Load an index from a snapshot. Validates magic, format version,
    /// shard_uuid, header CRC, BLAKE3 footer, and the body's
    /// structural integrity. Rebuilds the id_map's reverse direction
    /// from the forward direction.
    ///
    /// Returns the loaded `(HnswIndex, taken_at_lsn)`. Callers (Phase
    /// 8 maintenance worker) compare the LSN against the metadata
    /// store's `durable_lsn` to detect stale snapshots per spec
    /// `§06/06 §5.3`.
    pub fn load_snapshot(
        dir: &Path,
        basename: &str,
        expected_shard_uuid: [u8; 16],
    ) -> Result<(Self, u64), HnswError> {
        // 1. Read and validate `.brain`.
        let brain_path = dir.join(format!("{basename}.brain"));
        let file_bytes = persistence::read_brain_file(&brain_path)?;

        // 2. Verify the BLAKE3 footer.
        if !persistence::verify_footer(&file_bytes) {
            return Err(HnswError::SnapshotBadFooter);
        }
        if file_bytes.len() < HEADER_LEN + FOOTER_LEN {
            return Err(HnswError::SnapshotTruncated {
                expected: HEADER_LEN + FOOTER_LEN,
                got: file_bytes.len(),
            });
        }

        // 3. Parse the header.
        let header = Header::parse(&file_bytes[..HEADER_LEN])?;
        if header.shard_uuid != expected_shard_uuid {
            return Err(HnswError::SnapshotShardMismatch {
                expected: expected_shard_uuid,
                got: header.shard_uuid,
            });
        }
        if header.vector_dim as usize != D {
            return Err(HnswError::SnapshotDimMismatch {
                expected: D,
                got: header.vector_dim,
            });
        }

        // 4. Parse the body (between header and footer).
        let body_start = HEADER_LEN;
        let body_end = file_bytes.len() - FOOTER_LEN;
        let parsed_body = persistence::ParsedBody::parse(&file_bytes[body_start..body_end])?;

        // 5. Load the hnsw_rs graph from `<basename>.hnsw.*`, or
        //    construct a fresh empty inner if the snapshot recorded
        //    zero nodes (saved-empty case — hnsw_rs's `file_dump`
        //    errors on empty graphs, so we omit the `.hnsw.*` files
        //    on save and reconstruct here).
        //
        // For the non-empty path: `HnswIo::load_hnsw_with_dist` returns
        // a `Hnsw<'b, T, D>` whose lifetime `'b` is tied to the
        // `HnswIo`'s `'a` (with `'a: 'b`). We hold `Hnsw<'static, ...>`
        // to keep `HnswIndex` lifetime-free. In non-mmap mode the
        // returned graph owns all its data — the `'b` lifetime is
        // artificial — but the API doesn't expose that. We `Box::leak`
        // the `HnswIo` so the borrow is `'static`. Per-load overhead:
        // the `HnswIo` is a small struct (PathBuf + String +
        // ReloadOptions), and snapshot loads are startup-time (one per
        // shard per restart), so the leak is bounded by the shard count.
        let inner: Hnsw<'static, f32, DistCosine> = if header.graph_node_count == 0 {
            Hnsw::<f32, DistCosine>::new(
                header.m as usize,
                DEFAULT_CAPACITY_HINT,
                MAX_LAYER,
                header.ef_construction as usize,
                DistCosine,
            )
        } else {
            let io: &'static HnswIo = Box::leak(Box::new(HnswIo::new(dir, basename)));
            io.load_hnsw_with_dist(DistCosine)
                .map_err(|e| HnswError::HnswLoadFailed(format!("{e}")))?
        };

        // 6. Reconstruct id_map + tombstones from parsed body.
        let id_map = IdMap::from_snapshot(parsed_body.id_map_entries, parsed_body.next_internal_id);
        let tombstones = TombstoneBitmap::from_snapshot(
            parsed_body.tombstone_words,
            parsed_body.tombstone_set_count as usize,
        );

        // 7. Build the IndexParams back from the header.
        let params = IndexParams {
            m: header.m as usize,
            ef_construction: header.ef_construction as usize,
            ef_search: header.ef_search as usize,
            ef_search_max: header.ef_search_max as usize,
        };

        let idx = Self {
            inner,
            params,
            id_map,
            tombstones,
        };
        Ok((idx, header.taken_at_lsn))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vec4(a: f32, b: f32, c: f32, d: f32) -> [f32; 4] {
        // Normalise so cosine distance behaves cleanly.
        let n = (a * a + b * b + c * c + d * d).sqrt();
        [a / n, b / n, c / n, d / n]
    }

    fn mid(slot: u64) -> MemoryId {
        MemoryId::pack(1, slot, 1)
    }

    fn params_d4() -> IndexParams {
        IndexParams::default_v1()
    }

    #[test]
    fn new_with_defaults() {
        let idx = HnswIndex::<4>::new(params_d4()).unwrap();
        assert_eq!(idx.len(), 0);
        assert!(idx.is_empty());
        assert_eq!(idx.params(), IndexParams::default_v1());
    }

    #[test]
    fn new_rejects_invalid_params() {
        let mut bad = IndexParams::default_v1();
        bad.m = 0;
        // `HnswIndex` doesn't impl `Debug` (hnsw_rs's `Hnsw` doesn't either),
        // so we match the `Err` manually rather than `.unwrap_err()`.
        match HnswIndex::<4>::new(bad) {
            Err(HnswError::InvalidParams(IndexParamsError::MOutOfRange(0))) => {}
            Err(e) => panic!("wrong error: {e}"),
            Ok(_) => panic!("expected validation failure"),
        }
    }

    #[test]
    fn insert_with_memory_id_increments_len() {
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        idx.insert(mid(1), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        idx.insert(mid(2), &vec4(0.0, 1.0, 0.0, 0.0)).unwrap();
        idx.insert(mid(3), &vec4(0.0, 0.0, 1.0, 0.0)).unwrap();
        assert_eq!(idx.len(), 3);
        assert!(!idx.is_empty());
    }

    #[test]
    fn identical_vector_self_match_returns_memory_id() {
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        let v = vec4(0.5, 0.5, 0.5, 0.5);
        let id = mid(42);
        idx.insert(id, &v).unwrap();
        let results = idx.search_active(&v, 1, None);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, id);
        // Similarity for identical vectors is ~1.0 (= 1 - distance 0).
        assert!(
            results[0].1 > 1.0 - 1e-5,
            "expected similarity ~1.0, got {}",
            results[0].1
        );
    }

    #[test]
    fn search_returns_at_most_k() {
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        for i in 1..=5u8 {
            let f = f32::from(i);
            idx.insert(mid(u64::from(i)), &vec4(f, f * 2.0, f * 3.0, f * 4.0))
                .unwrap();
        }
        let q = vec4(1.0, 2.0, 3.0, 4.0);
        let results = idx.search_active(&q, 3, None);
        assert!(results.len() <= 3, "got {} results", results.len());
    }

    #[test]
    fn search_results_are_sorted_descending_by_similarity() {
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        idx.insert(mid(1), &vec4(1.0, 0.1, 0.0, 0.0)).unwrap();
        idx.insert(mid(2), &vec4(0.9, 0.5, 0.0, 0.0)).unwrap();
        idx.insert(mid(3), &vec4(0.5, 0.9, 0.0, 0.0)).unwrap();
        idx.insert(mid(4), &vec4(0.1, 1.0, 0.0, 0.0)).unwrap();
        idx.insert(mid(5), &vec4(0.0, 0.0, 1.0, 1.0)).unwrap();
        let q = vec4(1.0, 0.0, 0.0, 0.0);
        let results = idx.search_active(&q, 5, None);
        // Similarities should be non-increasing (best match first).
        for w in results.windows(2) {
            assert!(
                w[0].1 >= w[1].1 - 1e-6,
                "similarities out of order: {} < {}",
                w[0].1,
                w[1].1
            );
        }
    }

    #[test]
    fn ef_search_max_caps_per_query_override() {
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        idx.insert(mid(1), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        idx.insert(mid(2), &vec4(0.0, 1.0, 0.0, 0.0)).unwrap();
        let q = vec4(1.0, 0.0, 0.0, 0.0);
        // 9999 well above ef_search_max=500; clamps inside resolve_ef.
        let results = idx.search_active(&q, 2, Some(9999));
        assert!(results.len() <= 2);
        // Top hit is mid(1) (most similar to the query).
        assert_eq!(results[0].0, mid(1));
    }

    #[test]
    fn empty_index_search_returns_empty() {
        let idx = HnswIndex::<4>::new(params_d4()).unwrap();
        let q = vec4(1.0, 0.0, 0.0, 0.0);
        let results = idx.search_active(&q, 5, None);
        assert!(results.is_empty());
    }

    #[test]
    fn resolve_ef_clamps_to_k_and_ef_search_max() {
        let idx = HnswIndex::<4>::new(IndexParams::default_v1()).unwrap();
        // None → ef_search (64), bumped to k=128 → still ≤ ef_search_max (500).
        assert_eq!(idx.resolve_ef(128, None), 128);
        // None with k below ef_search → uses ef_search.
        assert_eq!(idx.resolve_ef(10, None), 64);
        // Override above ef_search_max → clamped.
        assert_eq!(idx.resolve_ef(10, Some(9999)), 500);
        // Override below k → bumped to k.
        assert_eq!(idx.resolve_ef(100, Some(50)), 100);
    }

    // ----- 4.2-specific tests --------------------------------------------

    #[test]
    fn duplicate_memory_id_returns_error() {
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        idx.insert(mid(1), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        // Second insert of the same MemoryId rejects.
        match idx.insert(mid(1), &vec4(0.0, 1.0, 0.0, 0.0)) {
            Err(HnswError::DuplicateMemoryId { memory_id_bytes }) => {
                assert_eq!(memory_id_bytes, mid(1).to_be_bytes());
            }
            Err(e) => panic!("wrong error: {e}"),
            Ok(()) => panic!("expected DuplicateMemoryId"),
        }
        assert_eq!(idx.len(), 1, "duplicate insert must not advance len");
    }

    #[test]
    fn search_results_carry_memory_ids() {
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        idx.insert(mid(100), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        idx.insert(mid(200), &vec4(0.0, 1.0, 0.0, 0.0)).unwrap();
        let results = idx.search_active(&vec4(1.0, 0.1, 0.0, 0.0), 2, None);
        let ids: Vec<MemoryId> = results.iter().map(|(id, _)| *id).collect();
        assert!(
            ids.contains(&mid(100)),
            "expected mid(100) in {:?}",
            results
        );
        assert!(
            ids.contains(&mid(200)),
            "expected mid(200) in {:?}",
            results
        );
    }

    #[test]
    fn contains_after_insert() {
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        assert!(!idx.contains(mid(7)));
        idx.insert(mid(7), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        assert!(idx.contains(mid(7)));
        assert!(!idx.contains(mid(8)));
    }

    // ----- 4.3-specific tests --------------------------------------------

    #[test]
    fn mark_tombstoned_consults_idmap() {
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        idx.insert(mid(1), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        assert!(!idx.is_tombstoned(mid(1)));
        idx.mark_tombstoned(mid(1)).unwrap();
        assert!(idx.is_tombstoned(mid(1)));
        assert_eq!(idx.tombstone_count(), 1);
    }

    #[test]
    fn mark_tombstoned_unknown_returns_error() {
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        match idx.mark_tombstoned(mid(999)) {
            Err(HnswError::MemoryIdNotFound { memory_id_bytes }) => {
                assert_eq!(memory_id_bytes, mid(999).to_be_bytes());
            }
            Err(e) => panic!("wrong error: {e}"),
            Ok(()) => panic!("expected MemoryIdNotFound"),
        }
        assert_eq!(idx.tombstone_count(), 0);
    }

    #[test]
    fn is_tombstoned_unknown_returns_false() {
        // Query path is fail-soft: unknown MemoryId is not an error.
        let idx = HnswIndex::<4>::new(params_d4()).unwrap();
        assert!(!idx.is_tombstoned(mid(999)));
    }

    #[test]
    fn tombstone_count_pin() {
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        idx.insert(mid(1), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        idx.insert(mid(2), &vec4(0.0, 1.0, 0.0, 0.0)).unwrap();
        idx.insert(mid(3), &vec4(0.0, 0.0, 1.0, 0.0)).unwrap();
        assert_eq!(idx.tombstone_count(), 0);
        idx.mark_tombstoned(mid(1)).unwrap();
        idx.mark_tombstoned(mid(2)).unwrap();
        assert_eq!(idx.tombstone_count(), 2);
        // mid(3) untouched.
        assert!(idx.is_tombstoned(mid(1)));
        assert!(idx.is_tombstoned(mid(2)));
        assert!(!idx.is_tombstoned(mid(3)));
    }

    // ----- 4.4-specific tests --------------------------------------------

    #[test]
    fn tombstoned_memories_excluded_from_search() {
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        idx.insert(mid(1), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        idx.insert(mid(2), &vec4(0.9, 0.1, 0.0, 0.0)).unwrap();
        idx.insert(mid(3), &vec4(0.8, 0.2, 0.0, 0.0)).unwrap();
        idx.mark_tombstoned(mid(2)).unwrap();
        let results = idx.search_active(&vec4(1.0, 0.0, 0.0, 0.0), 5, None);
        let ids: Vec<MemoryId> = results.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&mid(1)));
        assert!(ids.contains(&mid(3)));
        assert!(
            !ids.contains(&mid(2)),
            "tombstoned mid(2) leaked into results: {ids:?}"
        );
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn custom_filter_excludes() {
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        // Insert mid(1)..=mid(5); a filter keeping only even slot ids
        // returns mid(2) and mid(4) only.
        for i in 1..=5u64 {
            let f = i as f32;
            idx.insert(mid(i), &vec4(f, 0.5, 0.0, 0.0)).unwrap();
        }
        let q = vec4(3.0, 0.5, 0.0, 0.0);
        let results = idx.search(&q, 5, None, |m| m.slot() % 2 == 0);
        let ids: Vec<u64> = results.iter().map(|(id, _)| id.slot()).collect();
        for slot in &ids {
            assert!(slot % 2 == 0, "filter let odd slot {slot} through");
        }
        assert!(!ids.is_empty(), "expected at least one even-slot result");
    }

    #[test]
    fn filter_composition_with_tombstones() {
        // Both filters apply: tombstone filter (implicit) AND user filter.
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        idx.insert(mid(1), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        idx.insert(mid(2), &vec4(0.9, 0.1, 0.0, 0.0)).unwrap();
        idx.insert(mid(3), &vec4(0.8, 0.2, 0.0, 0.0)).unwrap();
        idx.insert(mid(4), &vec4(0.7, 0.3, 0.0, 0.0)).unwrap();
        // mid(1) tombstoned; user filter drops mid(2).
        idx.mark_tombstoned(mid(1)).unwrap();
        let results = idx.search(&vec4(1.0, 0.0, 0.0, 0.0), 5, None, |m| m != mid(2));
        let ids: Vec<MemoryId> = results.iter().map(|(id, _)| *id).collect();
        assert!(!ids.contains(&mid(1)), "tombstoned mid(1) leaked");
        assert!(!ids.contains(&mid(2)), "filtered mid(2) leaked");
        assert!(ids.contains(&mid(3)));
        assert!(ids.contains(&mid(4)));
    }

    #[test]
    fn search_active_excludes_tombstones() {
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        idx.insert(mid(1), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        idx.mark_tombstoned(mid(1)).unwrap();
        let results = idx.search_active(&vec4(1.0, 0.0, 0.0, 0.0), 5, None);
        assert!(
            results.is_empty(),
            "search_active should exclude tombstoned mid(1), got {results:?}"
        );
    }

    #[test]
    fn bailout_returns_partial_results_when_filter_drops_most() {
        // Insert 20 vectors; mark 18 tombstoned. The remaining 2
        // should still come back when k=2 even though the implicit
        // filter drops 90%.
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        for i in 1..=20u64 {
            let f = i as f32;
            idx.insert(mid(i), &vec4(f, 0.5, 0.0, 0.0)).unwrap();
        }
        for i in 1..=18u64 {
            idx.mark_tombstoned(mid(i)).unwrap();
        }
        let results = idx.search_active(&vec4(10.0, 0.5, 0.0, 0.0), 2, None);
        // Should return both surviving memories (mid(19), mid(20)) —
        // the bailout retry should find them even though only 2 of
        // 20 candidates pass the implicit tombstone filter.
        assert_eq!(results.len(), 2, "got {results:?}");
        let ids: Vec<u64> = results.iter().map(|(m, _)| m.slot()).collect();
        for slot in &ids {
            assert!(*slot == 19 || *slot == 20, "unexpected slot {slot}");
        }
    }

    #[test]
    fn always_false_filter_returns_empty_no_infinite_loop() {
        // Pathological filter: rejects everything. Bailout must
        // terminate; returns empty.
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        for i in 1..=10u64 {
            let f = i as f32;
            idx.insert(mid(i), &vec4(f, 0.5, 0.0, 0.0)).unwrap();
        }
        let results = idx.search(&vec4(5.0, 0.5, 0.0, 0.0), 5, None, |_| false);
        assert!(results.is_empty(), "always-false filter must return []");
    }

    #[test]
    fn similarity_score_in_unit_range() {
        // For L2-normalised input vectors, cosine similarity is in
        // [-1, 1]. Spot-check that the values look sensible.
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        // Insert one orthogonal vector to query.
        idx.insert(mid(1), &vec4(0.0, 1.0, 0.0, 0.0)).unwrap();
        // Insert one identical vector to query.
        idx.insert(mid(2), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        let q = vec4(1.0, 0.0, 0.0, 0.0);
        let results = idx.search_active(&q, 2, None);
        assert_eq!(results.len(), 2);
        for (id, sim) in &results {
            assert!(
                (-1.001..=1.001).contains(sim),
                "similarity for {id:?} = {sim} outside [-1, 1]"
            );
        }
        // The identical match (mid(2)) should be first; similarity ~1.
        // The orthogonal one (mid(1)) should be second; similarity ~0.
        assert_eq!(results[0].0, mid(2));
        assert!(results[0].1 > 1.0 - 1e-5);
        assert_eq!(results[1].0, mid(1));
        assert!(results[1].1.abs() < 1e-5);
    }

    // ----- 4.5-specific tests --------------------------------------------

    const TEST_UUID: [u8; 16] = [0xCD; 16];

    fn populated_index() -> HnswIndex<4> {
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        idx.insert(mid(1), &vec4(1.0, 0.0, 0.0, 0.0)).unwrap();
        idx.insert(mid(2), &vec4(0.0, 1.0, 0.0, 0.0)).unwrap();
        idx.insert(mid(3), &vec4(0.0, 0.0, 1.0, 0.0)).unwrap();
        idx
    }

    #[test]
    fn round_trip_empty_index() {
        let dir = tempfile::tempdir().unwrap();
        let idx = HnswIndex::<4>::new(params_d4()).unwrap();
        idx.save_snapshot(dir.path(), "test", 42, TEST_UUID)
            .unwrap();

        let (loaded, lsn) = HnswIndex::<4>::load_snapshot(dir.path(), "test", TEST_UUID).unwrap();
        assert_eq!(loaded.len(), 0);
        assert!(loaded.is_empty());
        assert_eq!(lsn, 42);
    }

    #[test]
    fn round_trip_with_memories() {
        let dir = tempfile::tempdir().unwrap();
        let idx = populated_index();
        let q = vec4(1.0, 0.0, 0.0, 0.0);
        let pre = idx.search_active(&q, 3, None);

        idx.save_snapshot(dir.path(), "snap", 100, TEST_UUID)
            .unwrap();
        let (loaded, _) = HnswIndex::<4>::load_snapshot(dir.path(), "snap", TEST_UUID).unwrap();

        assert_eq!(loaded.len(), 3);
        let post = loaded.search_active(&q, 3, None);
        assert_eq!(pre.len(), post.len());
        for (a, b) in pre.iter().zip(post.iter()) {
            assert_eq!(a.0, b.0, "MemoryId mismatch after round trip");
            assert!(
                (a.1 - b.1).abs() < 1e-5,
                "similarity mismatch {} vs {}",
                a.1,
                b.1
            );
        }
    }

    #[test]
    fn round_trip_with_tombstones() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = populated_index();
        idx.mark_tombstoned(mid(1)).unwrap();
        idx.mark_tombstoned(mid(3)).unwrap();
        assert_eq!(idx.tombstone_count(), 2);

        idx.save_snapshot(dir.path(), "t", 0, TEST_UUID).unwrap();
        let (loaded, _) = HnswIndex::<4>::load_snapshot(dir.path(), "t", TEST_UUID).unwrap();

        assert!(loaded.is_tombstoned(mid(1)));
        assert!(!loaded.is_tombstoned(mid(2)));
        assert!(loaded.is_tombstoned(mid(3)));
        assert_eq!(loaded.tombstone_count(), 2);
    }

    #[test]
    fn round_trip_preserves_next_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = HnswIndex::<4>::new(params_d4()).unwrap();
        for i in 1..=5u64 {
            idx.insert(mid(i), &vec4(i as f32, 0.0, 0.0, 0.0)).unwrap();
        }
        idx.save_snapshot(dir.path(), "n", 0, TEST_UUID).unwrap();

        let (mut loaded, _) = HnswIndex::<4>::load_snapshot(dir.path(), "n", TEST_UUID).unwrap();
        // Next insert should succeed without colliding with id 0..=4.
        loaded
            .insert(mid(99), &vec4(99.0, 0.0, 0.0, 0.0))
            .expect("insert after load should succeed");
        assert_eq!(loaded.len(), 6);
    }

    #[test]
    fn load_returns_taken_at_lsn() {
        let dir = tempfile::tempdir().unwrap();
        let idx = populated_index();
        idx.save_snapshot(dir.path(), "lsn", 0xDEAD_BEEF, TEST_UUID)
            .unwrap();
        let (_, lsn) = HnswIndex::<4>::load_snapshot(dir.path(), "lsn", TEST_UUID).unwrap();
        assert_eq!(lsn, 0xDEAD_BEEF);
    }

    #[test]
    fn load_rejects_wrong_shard_uuid() {
        let dir = tempfile::tempdir().unwrap();
        let idx = populated_index();
        idx.save_snapshot(dir.path(), "s", 0, TEST_UUID).unwrap();
        let wrong = [0x01; 16];
        match HnswIndex::<4>::load_snapshot(dir.path(), "s", wrong) {
            Err(HnswError::SnapshotShardMismatch { expected, got }) => {
                assert_eq!(expected, wrong);
                assert_eq!(got, TEST_UUID);
            }
            Err(e) => panic!("expected SnapshotShardMismatch, got error {e}"),
            Ok(_) => panic!("expected SnapshotShardMismatch, got Ok"),
        }
    }

    #[test]
    fn load_rejects_wrong_dim() {
        let dir = tempfile::tempdir().unwrap();
        let idx = populated_index(); // HnswIndex<4>
        idx.save_snapshot(dir.path(), "d", 0, TEST_UUID).unwrap();
        // Attempt to load as HnswIndex<8>.
        match HnswIndex::<8>::load_snapshot(dir.path(), "d", TEST_UUID) {
            Err(HnswError::SnapshotDimMismatch {
                expected: 8,
                got: 4,
            }) => {}
            Err(e) => panic!("expected SnapshotDimMismatch, got error {e}"),
            Ok(_) => panic!("expected SnapshotDimMismatch, got Ok"),
        }
    }

    #[test]
    fn load_rejects_corrupted_brain_footer() {
        let dir = tempfile::tempdir().unwrap();
        let idx = populated_index();
        idx.save_snapshot(dir.path(), "c", 0, TEST_UUID).unwrap();

        // Flip a byte near the end of the .brain file (inside the
        // footer hash).
        let path = dir.path().join("c.brain");
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();

        match HnswIndex::<4>::load_snapshot(dir.path(), "c", TEST_UUID) {
            Err(HnswError::SnapshotBadFooter) => {}
            Err(e) => panic!("expected SnapshotBadFooter, got error {e}"),
            Ok(_) => panic!("expected SnapshotBadFooter, got Ok"),
        }
    }

    #[test]
    fn load_rejects_missing_hnsw_files() {
        let dir = tempfile::tempdir().unwrap();
        let idx = populated_index();
        idx.save_snapshot(dir.path(), "m", 0, TEST_UUID).unwrap();
        // Delete the hnsw graph file to simulate partial / corrupt snapshot.
        std::fs::remove_file(dir.path().join("m.hnsw.graph")).unwrap();

        match HnswIndex::<4>::load_snapshot(dir.path(), "m", TEST_UUID) {
            Err(HnswError::HnswLoadFailed(_)) => {}
            Err(e) => panic!("expected HnswLoadFailed, got error {e}"),
            Ok(_) => panic!("expected HnswLoadFailed, got Ok"),
        }
    }

    #[test]
    fn load_rejects_unsupported_version() {
        let dir = tempfile::tempdir().unwrap();
        let idx = populated_index();
        idx.save_snapshot(dir.path(), "v", 0, TEST_UUID).unwrap();
        // Tamper: bump format_version to 99 and recompute header CRC
        // + BLAKE3 footer so that only the version check fires.
        let path = dir.path().join("v.brain");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[4..8].copy_from_slice(&99u32.to_le_bytes());
        let new_header_crc = crc32c::crc32c(&bytes[..60]);
        bytes[60..64].copy_from_slice(&new_header_crc.to_le_bytes());
        let split = bytes.len() - 8;
        let footer = crate::persistence::compute_footer(&bytes[..split]);
        bytes[split..].copy_from_slice(&footer);
        std::fs::write(&path, &bytes).unwrap();

        match HnswIndex::<4>::load_snapshot(dir.path(), "v", TEST_UUID) {
            Err(HnswError::SnapshotUnsupportedVersion(99)) => {}
            Err(e) => panic!("expected SnapshotUnsupportedVersion(99), got error {e}"),
            Ok(_) => panic!("expected SnapshotUnsupportedVersion(99), got Ok"),
        }
    }

    // ----- 4.6-specific tests --------------------------------------------

    #[test]
    fn rebuild_search_returns_correct_results() {
        let source = vec![
            (mid(1), vec4(1.0, 0.0, 0.0, 0.0)),
            (mid(2), vec4(0.0, 1.0, 0.0, 0.0)),
            (mid(3), vec4(0.0, 0.0, 1.0, 0.0)),
        ];
        let (idx, _) = HnswIndex::<4>::rebuild(IndexParams::default_v1(), source).unwrap();
        let results = idx.search_active(&vec4(1.0, 0.0, 0.0, 0.0), 1, None);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, mid(1));
        assert!(results[0].1 > 1.0 - 1e-5);
    }

    #[test]
    fn rebuild_report_records_duration() {
        let source = (1..=10u64)
            .map(|i| (mid(i), vec4(i as f32, 0.5, 0.0, 0.0)))
            .collect::<Vec<_>>();
        let (_, report) = HnswIndex::<4>::rebuild(IndexParams::default_v1(), source).unwrap();
        // Duration is populated and non-zero on a real rebuild. We
        // don't assert a numeric bound (CI variance); just that it's
        // not the default value.
        assert_eq!(report.memories_inserted, 10);
        assert!(
            report.duration > std::time::Duration::ZERO,
            "duration should be non-zero, got {:?}",
            report.duration
        );
    }

    #[test]
    fn rebuild_then_save_then_load() {
        // End-to-end: rebuild from iter → save_snapshot → load_snapshot
        // → search returns the same MemoryIds. Pins that 4.5 and 4.6
        // compose correctly.
        let dir = tempfile::tempdir().unwrap();
        let source = vec![
            (mid(10), vec4(1.0, 0.0, 0.0, 0.0)),
            (mid(20), vec4(0.0, 1.0, 0.0, 0.0)),
            (mid(30), vec4(0.0, 0.0, 1.0, 0.0)),
        ];
        let (idx, _) = HnswIndex::<4>::rebuild(IndexParams::default_v1(), source).unwrap();
        let q = vec4(1.0, 0.0, 0.0, 0.0);
        let pre = idx.search_active(&q, 3, None);

        idx.save_snapshot(dir.path(), "rb", 7, TEST_UUID).unwrap();
        let (loaded, lsn) = HnswIndex::<4>::load_snapshot(dir.path(), "rb", TEST_UUID).unwrap();
        assert_eq!(lsn, 7);
        assert_eq!(loaded.len(), 3);
        let post = loaded.search_active(&q, 3, None);
        let pre_ids: Vec<MemoryId> = pre.iter().map(|(id, _)| *id).collect();
        let post_ids: Vec<MemoryId> = post.iter().map(|(id, _)| *id).collect();
        assert_eq!(pre_ids, post_ids, "save+load order must match pre-snapshot");
    }
}
