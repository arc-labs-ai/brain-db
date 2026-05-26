//! Two-tier lock-free wrapper for the PQ-flavour HNSW.
//!
//! Mirrors [`crate::shared::SharedHnswImpl`]'s shape — immutable
//! [`MainEpochImpl`] swapped via `ArcSwap`, mutable [`PendingBufferImpl`]
//! protected by `RwLock`. Differences:
//!
//! - Vector dim is fixed at [`VECTOR_DIM`] (the PQ codebook is trained
//!   for one shape). The const generic is `M` (subquantiser count)
//!   instead of `D`.
//! - Search composes ADC against main with exact cosine against
//!   pending, then re-ranks the merged candidate set against the
//!   full-precision arena via a caller-supplied closure.
//! - The pending buffer holds full-precision `f32` vectors so pending
//!   inserts are visible at exact cosine immediately; the closure
//!   isn't called for them.
//!
//! Spec: `spec/09_indexing/07_hnsw_pq.md` §§7-9.

use std::collections::HashSet;
use std::sync::Arc;

use arc_swap::ArcSwap;
use brain_core::MemoryId;
use parking_lot::RwLock;

use crate::hnsw::{HnswError, HnswIndexImpl};
use crate::params::{IndexParams, VECTOR_DIM};
use crate::pq::{rerank, BOOTSTRAP_M};

/// Public alias: the production shared HNSW handle, fixed at the
/// bootstrap PQ shape. Callers reach for [`SharedHnsw`] without
/// thinking about `M` — the alias keeps the PQ implementation
/// detail off the public surface.
pub type SharedHnsw = SharedHnswImpl<{ BOOTSTRAP_M }>;

/// Public alias for the single-writer handle that pairs with
/// [`SharedHnsw`].
pub type Writer = WriterImpl<{ BOOTSTRAP_M }>;

impl SharedHnswImpl<{ BOOTSTRAP_M }> {
    /// Convenience constructor: build an empty [`SharedHnsw`] backed
    /// by the [`crate::pq::bootstrap_codebook`] and a null arena
    /// reader. Production shards replace the arena via
    /// [`Self::with_arena`] before serving search traffic; tests
    /// stay on the null reader (search results come back ADC-ranked,
    /// no re-rank).
    ///
    /// Errors:
    /// - [`HnswError::InvalidParams`] if `params` doesn't validate.
    pub fn new(params: IndexParams) -> Result<(Self, Writer), HnswError> {
        let codebook = crate::pq::bootstrap_codebook();
        let idx = crate::hnsw::HnswIndex::new(params, (*codebook).clone())?;
        Ok(Self::from_index(
            idx,
            crate::arena_reader::null_arena_reader(),
        ))
    }

    /// Return a clone of this handle with `arena` swapped in as the
    /// re-rank reader. Production shards call this after constructing
    /// the arena to wire it in; the writer half is unchanged because
    /// writes don't consult the arena.
    #[must_use]
    pub fn with_arena(mut self, arena: Arc<dyn crate::arena_reader::ArenaReader>) -> Self {
        self.arena = arena;
        self
    }

    /// Persist the published main to a directory at the given basename.
    /// Writes four files per `spec/09_indexing/06_persistence.md §7`:
    /// `<basename>.hnsw.graph`, `<basename>.hnsw.data`,
    /// `<basename>.codebook`, and `<basename>.brain` (the wrapper, written
    /// **last** so its presence marks "snapshot complete"). The wrapper
    /// carries BLAKE3 hashes of the three sibling files so cross-file
    /// integrity is verifiable from the wrapper alone.
    ///
    /// `taken_at_lsn` is recorded in the wrapper header so recovery
    /// knows the WAL position to replay past. Pending-buffer state is
    /// **not** included; the convention is that the snapshot worker
    /// runs after a checkpoint, so any pending entry at that point
    /// either landed in the arena (will be replayed on load) or hadn't
    /// reached durable state yet (no WAL record → not visible at all).
    ///
    /// `dir` must already exist (matches `hnsw_rs::Hnsw::file_dump`).
    pub fn save_snapshot(
        &self,
        dir: &std::path::Path,
        basename: &str,
        taken_at_lsn: u64,
        shard_uuid: [u8; 16],
    ) -> Result<(), HnswError> {
        use crate::persistence::{compute_footer, Body, Header};

        // 1. Snapshot the published main. ArcSwap → no writer locking.
        let epoch = self.main.load();
        let idx = &epoch.index;
        let params = idx.params();

        // 2. Empty-main guard: `hnsw_rs::Hnsw::file_dump` errors on an
        //    empty graph. An empty snapshot has no value over the arena-
        //    rebuild fallback (no nodes to load), so skip the write
        //    silently. Recovery's `find_latest_snapshot_dir` won't find a
        //    `<basename>.brain` here and will run the rebuild path.
        if idx.is_empty() {
            return Ok(());
        }

        // 3. Dump the inner graph: hnsw_rs writes
        //    <basename>.hnsw.graph + <basename>.hnsw.data into `dir`.
        let _basename_used = idx.file_dump(dir, basename)?;

        // 3. Write the codebook sidecar.
        let codebook_bytes = (**idx.codebook()).clone().serialize();
        let codebook_path = dir.join(format!("{basename}.codebook"));
        std::fs::write(&codebook_path, &codebook_bytes)?;

        // 4. BLAKE3 each sibling file. Read-once, no streaming — the
        //    largest will be the graph file; load time will repeat the
        //    same hash before trusting the load (see load_snapshot).
        let graph_hash = blake3_file(&dir.join(format!("{basename}.hnsw.graph")))?;
        let data_hash = blake3_file(&dir.join(format!("{basename}.hnsw.data")))?;
        let codebook_hash = *blake3::hash(&codebook_bytes).as_bytes();

        // 5. Build the wrapper. Header.encode → 64 bytes; Body.encode →
        //    variable; Footer = BLAKE3(header || body) truncated.
        let header = Header::new::<{ crate::params::VECTOR_DIM }>(
            shard_uuid,
            taken_at_lsn,
            idx.id_map().len() as u64,
            params,
        );
        let body = Body::encode(
            idx.id_map(),
            idx.id_map().next_id(),
            idx.tombstones(),
            graph_hash,
            data_hash,
            codebook_hash,
        );

        let mut wrapper = Vec::with_capacity(crate::persistence::HEADER_LEN + body.bytes.len() + crate::persistence::FOOTER_LEN);
        wrapper.extend_from_slice(&header.encode());
        wrapper.extend_from_slice(&body.bytes);
        let footer = compute_footer(&wrapper);
        wrapper.extend_from_slice(&footer);

        // 6. `.brain` is the snapshot-complete marker. Write last so a
        //    partial directory (only graph + data written before crash)
        //    fails the "load .brain first" probe and the caller cleanly
        //    falls back to arena rebuild.
        std::fs::write(dir.join(format!("{basename}.brain")), &wrapper)?;
        Ok(())
    }

    /// Reload a snapshot triple+wrapper into `(HnswIndexImpl, taken_at_lsn)`.
    /// Verifies the wrapper's magic + version + header CRC + footer
    /// BLAKE3, refuses on a `shard_uuid` mismatch, then verifies each
    /// sibling file's BLAKE3 matches the wrapper body. Any failure
    /// returns a clear error so the caller can fall back to a fresh
    /// rebuild.
    ///
    /// Returns the bare index rather than a wrapped `(Self, Writer, _)`
    /// so the caller (`spawn_shard`) can `swap()` the loaded index into
    /// an already-constructed `SharedHnsw` without disturbing the
    /// writer it has already wired into the rest of the stack.
    pub fn load_snapshot(
        dir: &std::path::Path,
        basename: &str,
        expected_shard_uuid: [u8; 16],
    ) -> Result<(crate::hnsw::HnswIndexImpl<{ crate::pq::BOOTSTRAP_M }>, u64), HnswError> {
        use crate::persistence::{
            read_brain_file, verify_footer, BodyError, Header, HeaderError, ParsedBody,
            FOOTER_LEN, HEADER_LEN,
        };

        // 1. Wrapper.
        let wrapper_path = dir.join(format!("{basename}.brain"));
        let wrapper = read_brain_file(&wrapper_path)?;
        if !verify_footer(&wrapper) {
            return Err(HnswError::SnapshotCorrupt("wrapper footer BLAKE3 mismatch".into()));
        }
        if wrapper.len() < HEADER_LEN + FOOTER_LEN {
            return Err(HnswError::SnapshotCorrupt("wrapper smaller than header+footer".into()));
        }
        let header = Header::parse(&wrapper[..HEADER_LEN]).map_err(|e: HeaderError| {
            HnswError::SnapshotCorrupt(format!("wrapper header: {e:?}"))
        })?;
        if header.shard_uuid != expected_shard_uuid {
            return Err(HnswError::SnapshotCorrupt(format!(
                "shard_uuid mismatch: expected {:?}, got {:?}",
                expected_shard_uuid, header.shard_uuid
            )));
        }
        let body_bytes = &wrapper[HEADER_LEN..wrapper.len() - FOOTER_LEN];
        let body = ParsedBody::parse(body_bytes).map_err(|e: BodyError| {
            HnswError::SnapshotCorrupt(format!("wrapper body: {e:?}"))
        })?;

        // 2. Codebook sidecar.
        let codebook_path = dir.join(format!("{basename}.codebook"));
        let codebook_bytes = std::fs::read(&codebook_path)?;
        let computed_codebook_hash = *blake3::hash(&codebook_bytes).as_bytes();
        if computed_codebook_hash != body.codebook_hash {
            return Err(HnswError::SnapshotCorrupt("codebook BLAKE3 mismatch".into()));
        }
        let codebook = crate::pq::Codebook::<{ crate::pq::BOOTSTRAP_M }>::deserialize(
            &codebook_bytes,
        )
        .map_err(|e| HnswError::SnapshotCorrupt(format!("codebook deserialize: {e}")))?;

        // 3. Graph + data sibling-hash verification.
        let graph_path = dir.join(format!("{basename}.hnsw.graph"));
        let data_path = dir.join(format!("{basename}.hnsw.data"));
        let graph_hash = blake3_file(&graph_path)?;
        if graph_hash != body.graph_hash {
            return Err(HnswError::SnapshotCorrupt("graph BLAKE3 mismatch".into()));
        }
        let data_hash = blake3_file(&data_path)?;
        if data_hash != body.data_hash {
            return Err(HnswError::SnapshotCorrupt("data BLAKE3 mismatch".into()));
        }

        // 4. hnsw_rs reload with the deserialised codebook's PqDist.
        //    `load_hnsw_with_dist` returns `Hnsw<'b, ...>` where `'b`
        //    is bounded by the `HnswIo`'s lifetime. Brain stores the
        //    inner Hnsw as `'static` (the existing `HnswIndexImpl<M>`
        //    contract), so we leak the io. The leaked struct is small —
        //    `PathBuf + String + bool + Option<DataMap>` with DataMap
        //    unpopulated by `HnswIo::new` — and the leak is bounded by
        //    `O(shard restarts)`, freed at process exit. The graph itself
        //    is owned by `Hnsw` after `load_hnsw_with_dist` returns, so
        //    we're not leaking the graph bytes — only the small handle.
        let pq_dist = crate::pq::PqDist::<{ crate::pq::BOOTSTRAP_M }>::new(&codebook);
        let io_ref: &'static mut hnsw_rs::hnswio::HnswIo =
            Box::leak(Box::new(hnsw_rs::hnswio::HnswIo::new(dir, basename)));
        let inner = io_ref
            .load_hnsw_with_dist::<u8, _>(pq_dist)
            .map_err(|e| HnswError::SnapshotCorrupt(format!("hnsw_rs load: {e}")))?;

        // 5. Rebuild IdMap + TombstoneBitmap + params.
        let id_map = crate::idmap::IdMap::from_snapshot(body.id_map_entries, body.next_internal_id);
        let tombstones = crate::tombstones::TombstoneBitmap::from_snapshot(
            body.tombstone_words,
            body.tombstone_set_count as usize,
        );
        let params = crate::params::IndexParams::default_v1();

        // 6. Assemble. Caller decides whether to wrap with `from_index`
        //    (standalone usage / tests) or `swap` into an existing
        //    SharedHnsw (recovery in spawn_shard).
        let idx = crate::hnsw::HnswIndexImpl::<{ crate::pq::BOOTSTRAP_M }>::from_persisted_parts(
            params, codebook, inner, id_map, tombstones,
        );
        Ok((idx, header.taken_at_lsn))
    }
}

/// Stream BLAKE3 over a file. Used at save+load time to verify
/// cross-file integrity in the snapshot triple.
fn blake3_file(path: &std::path::Path) -> Result<[u8; 32], HnswError> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(*hasher.finalize().as_bytes())
}

/// Search-time over-fetch factor: pull `K * RERANK_FACTOR` ADC
/// candidates from the HNSW, re-rank against full-precision arena
/// vectors, keep the top `K`. Spec §09.07 §3.1 default.
const RERANK_FACTOR: usize = 4;

/// An immutable PQ-HNSW snapshot for a single published epoch.
struct MainEpochImpl<const M: usize> {
    index: HnswIndexImpl<M>,
    epoch_id: u64,
}

/// Recent inserts and tombstones that haven't yet been folded into
/// the main PQ-HNSW. Full-precision vectors live here — encoding
/// happens during the flush rebuild.
struct PendingBufferImpl {
    entries: Vec<PendingEntry>,
    tombstoned: HashSet<MemoryId>,
}

impl PendingBufferImpl {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            tombstoned: HashSet::new(),
        }
    }
}

/// A full-precision vector waiting to be PQ-encoded and folded into
/// the main HNSW. The `vector` field is kept verbatim so pending
/// search uses exact cosine.
#[derive(Clone, Debug)]
pub struct PendingEntry {
    pub memory_id: MemoryId,
    pub vector: [f32; VECTOR_DIM],
    pub tombstoned: bool,
}

/// Report returned by [`SharedHnswImpl::flush_with_rebuild`]. Same shape
/// as the pure-HNSW flush report so the maintenance worker can branch
/// on the flavour once and emit consistent metrics either way.
#[derive(Debug, Clone)]
pub struct FlushReport {
    pub entries_flushed: usize,
    pub new_epoch: u64,
    pub main_len_after: usize,
}

/// Cloneable reader handle for a PQ-flavour shared index.
#[derive(Clone)]
pub struct SharedHnswImpl<const M: usize> {
    main: Arc<ArcSwap<MainEpochImpl<M>>>,
    pending: Arc<RwLock<PendingBufferImpl>>,
    arena: Arc<dyn crate::arena_reader::ArenaReader>,
}

/// Single-writer handle. Not `Clone` — enforces single-writer-per-shard
/// at the type level, matching [`crate::shared::WriterImpl`].
pub struct WriterImpl<const M: usize> {
    /// Kept so the published main outlives the writer regardless of
    /// reader cloning patterns; never read directly.
    _main: Arc<ArcSwap<MainEpochImpl<M>>>,
    pending: Arc<RwLock<PendingBufferImpl>>,
}

impl<const M: usize> SharedHnswImpl<M> {
    /// Wrap an existing [`HnswIndex`] with an injected
    /// [`crate::ArenaReader`], returning the reader/writer pair.
    /// The arena reader is consulted for the PQ re-rank pass on every
    /// search — production callers pass a real arena handle; tests
    /// pass [`crate::arena_reader::null_arena_reader`].
    #[must_use]
    pub fn from_index(
        idx: HnswIndexImpl<M>,
        arena: Arc<dyn crate::arena_reader::ArenaReader>,
    ) -> (Self, WriterImpl<M>) {
        let epoch = Arc::new(MainEpochImpl {
            index: idx,
            epoch_id: 0,
        });
        let main = Arc::new(ArcSwap::new(epoch));
        let pending = Arc::new(RwLock::new(PendingBufferImpl::new()));
        let reader = Self {
            main: main.clone(),
            pending: pending.clone(),
            arena,
        };
        let writer = WriterImpl {
            _main: main,
            pending,
        };
        (reader, writer)
    }

    // ----- Reader methods --------------------------------------------------

    /// Top-`k` nearest neighbours of `query`, with the PQ re-rank pass
    /// applied. Returns `(MemoryId, cosine_similarity)` pairs sorted
    /// descending by similarity.
    ///
    /// Arena lookup goes through the [`crate::ArenaReader`] injected
    /// at construction; tombstoned-during-search candidates are
    /// silently dropped. `filter` runs as an extra predicate alongside
    /// the always-on tombstone filter.
    #[must_use]
    pub fn search<F>(
        &self,
        query: &[f32; VECTOR_DIM],
        k: usize,
        ef: Option<usize>,
        filter: F,
    ) -> Vec<(MemoryId, f32)>
    where
        F: Fn(MemoryId) -> bool,
    {
        if k == 0 {
            return Vec::new();
        }

        // Pending tombstone overlay wins everywhere.
        let pending_tombstones: HashSet<MemoryId> = self.pending.read().tombstoned.clone();

        // 1. Main: PQ-ADC top-K' where K' = k * RERANK_FACTOR.
        let epoch = self.main.load();
        let inflated_k = k.saturating_mul(RERANK_FACTOR);
        let main_candidates = epoch.index.search(query, inflated_k, ef, |id| {
            !pending_tombstones.contains(&id) && filter(id)
        });

        // Re-rank main candidates against the arena. Pending hits are
        // handled separately because their vectors live in memory.
        let arena = self.arena.clone();
        let main_reranked = rerank::<_>(&main_candidates, query, inflated_k, |id| arena.read(id));

        // 2. Pending: brute-force exact cosine. Tombstoned overlay
        //    already excluded above.
        let pending_hits = self.pending_search(query, inflated_k, &filter);

        // 3. Merge + dedupe by MemoryId, prefer pending's score on
        //    collision (latest vector wins).
        merge_dedupe_descending(main_reranked, pending_hits, k)
    }

    /// Top-`k` nearest neighbours, excluding tombstoned memories.
    /// Convenience for the common case.
    #[must_use]
    pub fn search_active(
        &self,
        query: &[f32; VECTOR_DIM],
        k: usize,
        ef: Option<usize>,
    ) -> Vec<(MemoryId, f32)> {
        self.search(query, k, ef, |_| true)
    }

    /// Is `memory_id` present (and not tombstoned) in either tier?
    #[must_use]
    pub fn contains(&self, memory_id: MemoryId) -> bool {
        let pending = self.pending.read();
        if pending.tombstoned.contains(&memory_id) {
            return false;
        }
        if pending
            .entries
            .iter()
            .any(|e| e.memory_id == memory_id && !e.tombstoned)
        {
            return true;
        }
        drop(pending);
        let epoch = self.main.load();
        epoch.index.contains(memory_id) && !epoch.index.is_tombstoned(memory_id)
    }

    /// Is `memory_id` tombstoned in either tier?
    #[must_use]
    pub fn is_tombstoned(&self, memory_id: MemoryId) -> bool {
        if self.pending.read().tombstoned.contains(&memory_id) {
            return true;
        }
        self.main.load().index.is_tombstoned(memory_id)
    }

    /// Approximate combined size: published main plus pending entries.
    #[must_use]
    pub fn len(&self) -> usize {
        let pending = self.pending.read();
        let pending_extra = pending.entries.iter().filter(|e| !e.tombstoned).count();
        self.main.load().index.len() + pending_extra
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        let pending = self.pending.read();
        if pending.entries.iter().any(|e| !e.tombstoned) {
            return false;
        }
        drop(pending);
        self.main.load().index.is_empty()
    }

    /// Combined tombstone count: main's bitmap plus pending overlay
    /// entries not already counted in main.
    #[must_use]
    pub fn tombstone_count(&self) -> usize {
        let epoch = self.main.load();
        let pending = self.pending.read();
        let mut count = epoch.index.tombstone_count();
        for id in &pending.tombstoned {
            if !epoch.index.is_tombstoned(*id) {
                count += 1;
            }
        }
        count
    }

    /// Index params of the published main.
    #[must_use]
    pub fn params(&self) -> IndexParams {
        self.main.load().index.params()
    }

    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.main.load().epoch_id
    }

    /// Recovery-only: insert a `(memory_id, vector)` pair directly
    /// into the pending buffer, bypassing the [`WriterImpl`].
    ///
    /// Used by `spawn_shard`'s snapshot-load → tail-replay path. The
    /// snapshot captures the memory HNSW at `taken_at_lsn`; arena
    /// records whose `encoded_at_lsn > taken_at_lsn` aren't in the
    /// loaded main and must be brought forward so the rebuilt index
    /// reflects every durable write. The writer has already been
    /// moved into `RealWriterHandle` by the time recovery runs in the
    /// current shard layout — exposing a direct pending insert avoids
    /// the alternative of reordering writer construction or holding
    /// two writers. Boot is single-threaded, so this is safe.
    ///
    /// Production write paths must continue to go through
    /// [`WriterImpl::insert`].
    pub fn insert_recovery(
        &self,
        memory_id: MemoryId,
        vector: &[f32; crate::params::VECTOR_DIM],
    ) {
        let mut pending = self.pending.write();
        pending.tombstoned.remove(&memory_id);
        if let Some(slot) = pending
            .entries
            .iter_mut()
            .find(|e| e.memory_id == memory_id)
        {
            slot.vector = *vector;
            slot.tombstoned = false;
        } else {
            pending.entries.push(PendingEntry {
                memory_id,
                vector: *vector,
                tombstoned: false,
            });
        }
    }

    /// Atomically replace the published main with `new_index` and
    /// clear pending. Used for bootstrap and snapshot-load paths
    /// where main was rebuilt from a source of truth that already
    /// reflects all writes.
    pub fn swap(&self, new_index: HnswIndexImpl<M>) {
        let prev = self.main.load();
        let next = Arc::new(MainEpochImpl {
            index: new_index,
            epoch_id: prev.epoch_id.wrapping_add(1),
        });
        self.main.store(next);
        let mut pending = self.pending.write();
        pending.entries.clear();
        pending.tombstoned.clear();
    }

    /// Snapshot pending entries, pass them to `build` to produce a new
    /// main, then atomically publish + drain the flushed ids. Same
    /// shape as [`crate::shared::SharedHnswImpl::flush_with_rebuild`].
    pub fn flush_with_rebuild<F>(&self, build: F) -> Result<FlushReport, HnswError>
    where
        F: FnOnce(&[PendingEntry]) -> Result<HnswIndexImpl<M>, HnswError>,
    {
        let snapshot: Vec<PendingEntry> = self.pending.read().entries.clone();
        let snapshot_count = snapshot.len();

        let new_index = build(&snapshot)?;

        let mut pending = self.pending.write();
        let prev_epoch = self.main.load();
        let new_epoch_id = prev_epoch.epoch_id.wrapping_add(1);
        let main_len_after = new_index.len();
        let new_epoch = Arc::new(MainEpochImpl {
            index: new_index,
            epoch_id: new_epoch_id,
        });
        self.main.store(new_epoch);

        let flushed: HashSet<MemoryId> = snapshot.iter().map(|e| e.memory_id).collect();
        pending.entries.retain(|e| !flushed.contains(&e.memory_id));
        pending.tombstoned.retain(|id| !flushed.contains(id));

        Ok(FlushReport {
            entries_flushed: snapshot_count,
            new_epoch: new_epoch_id,
            main_len_after,
        })
    }

    /// Clone the current pending entries — used by the maintenance
    /// worker's flush prep.
    #[must_use]
    pub fn pending_snapshot(&self) -> Vec<PendingEntry> {
        self.pending.read().entries.clone()
    }

    /// Count of live (non-tombstoned) pending entries.
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending
            .read()
            .entries
            .iter()
            .filter(|e| !e.tombstoned)
            .count()
    }

    // ----- Private helpers -------------------------------------------------

    /// Brute-force exact-cosine over the pending buffer. Pending holds
    /// full-precision vectors, so no re-rank needed.
    fn pending_search<F>(
        &self,
        query: &[f32; VECTOR_DIM],
        k: usize,
        filter: &F,
    ) -> Vec<(MemoryId, f32)>
    where
        F: Fn(MemoryId) -> bool,
    {
        let pending = self.pending.read();
        if pending.entries.is_empty() || k == 0 {
            return Vec::new();
        }
        let mut scored: Vec<(MemoryId, f32)> = pending
            .entries
            .iter()
            .filter(|e| !e.tombstoned && filter(e.memory_id))
            .map(|e| (e.memory_id, dot(query, &e.vector)))
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);
        scored
    }
}

impl<const M: usize> WriterImpl<M> {
    /// Insert a full-precision vector. The PQ encode happens at flush
    /// time inside the build closure; until then the vector lives in
    /// pending and reads at exact cosine.
    pub fn insert(
        &mut self,
        memory_id: MemoryId,
        vector: &[f32; VECTOR_DIM],
    ) -> Result<(), HnswError> {
        let mut pending = self.pending.write();
        // Re-insert after tombstone resurrects the entry.
        pending.tombstoned.remove(&memory_id);
        if let Some(slot) = pending
            .entries
            .iter_mut()
            .find(|e| e.memory_id == memory_id)
        {
            slot.vector = *vector;
            slot.tombstoned = false;
        } else {
            pending.entries.push(PendingEntry {
                memory_id,
                vector: *vector,
                tombstoned: false,
            });
        }
        Ok(())
    }

    /// Mark a memory tombstoned. Visible immediately via
    /// [`SharedHnswImpl::is_tombstoned`].
    pub fn mark_tombstoned(&mut self, memory_id: MemoryId) -> Result<(), HnswError> {
        let mut pending = self.pending.write();
        pending.tombstoned.insert(memory_id);
        if let Some(slot) = pending
            .entries
            .iter_mut()
            .find(|e| e.memory_id == memory_id)
        {
            slot.tombstoned = true;
        }
        Ok(())
    }
}

// ===== Helpers =============================================================

/// Dot product of two equal-length `f32` vectors. With L2-normalised
/// inputs (BGE-small output, spec `§04/03 §1`) this equals cosine
/// similarity.
fn dot(a: &[f32; VECTOR_DIM], b: &[f32; VECTOR_DIM]) -> f32 {
    let mut sum = 0.0_f32;
    for i in 0..VECTOR_DIM {
        sum += a[i] * b[i];
    }
    sum
}

/// Merge main and pending hit lists, dedupe by `MemoryId` (pending
/// wins on collision), sort descending by similarity, truncate to `k`.
fn merge_dedupe_descending(
    main: Vec<(MemoryId, f32)>,
    pending: Vec<(MemoryId, f32)>,
    k: usize,
) -> Vec<(MemoryId, f32)> {
    use std::collections::HashMap;
    let mut by_id: HashMap<MemoryId, f32> = HashMap::with_capacity(main.len() + pending.len());
    for (id, score) in main {
        by_id.insert(id, score);
    }
    for (id, score) in pending {
        // Pending overrides main if both present.
        by_id.insert(id, score);
    }
    let mut merged: Vec<(MemoryId, f32)> = by_id.into_iter().collect();
    merged.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    merged.truncate(k);
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pq::{Codebook, PQ_CENTROIDS_PER_SUBSPACE};

    fn mid(slot: u8) -> MemoryId {
        MemoryId::pack(1, slot as u64, 1)
    }

    fn arithmetic_codebook<const M: usize>() -> Codebook<M> {
        let sub_dim = VECTOR_DIM / M;
        let mut centroids = vec![0.0_f32; M * PQ_CENTROIDS_PER_SUBSPACE * sub_dim];
        for s in 0..M {
            for k in 0..PQ_CENTROIDS_PER_SUBSPACE {
                let offset = (s * PQ_CENTROIDS_PER_SUBSPACE + k) * sub_dim;
                centroids[offset] = k as f32;
            }
        }
        Codebook::<M>::from_trained(centroids, sub_dim).unwrap()
    }

    fn unit_at_angle(angle_radians: f32) -> [f32; VECTOR_DIM] {
        let mut v = [0.0_f32; VECTOR_DIM];
        v[0] = angle_radians.cos();
        v[1] = angle_radians.sin();
        v
    }

    fn pq_params_default() -> IndexParams {
        IndexParams::default_v1()
    }

    fn build_shared() -> (SharedHnswImpl<8>, WriterImpl<8>) {
        let idx = HnswIndexImpl::<8>::new(pq_params_default(), arithmetic_codebook::<8>()).unwrap();
        SharedHnswImpl::<8>::from_index(idx, crate::null_arena_reader())
    }

    #[test]
    fn empty_search_returns_empty() {
        let (reader, _writer) = build_shared();
        let results = reader.search_active(&unit_at_angle(0.0), 5, None);
        assert!(results.is_empty());
    }

    #[test]
    fn pending_insert_visible_to_reader_before_flush() {
        let (reader, mut writer) = build_shared();
        let v = unit_at_angle(0.0);
        writer.insert(mid(1), &v).unwrap();
        assert!(reader.contains(mid(1)));
        assert_eq!(reader.pending_len(), 1);

        // Pending hit ranks via exact cosine (1.0 against itself).
        let results = reader.search_active(&v, 1, None);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, mid(1));
        assert!((results[0].1 - 1.0).abs() < 1e-5);
    }

    #[test]
    fn pending_tombstone_hides_from_reader() {
        let (reader, mut writer) = build_shared();
        let v = unit_at_angle(0.0);
        writer.insert(mid(2), &v).unwrap();
        writer.mark_tombstoned(mid(2)).unwrap();
        assert!(!reader.contains(mid(2)));
        assert!(reader.is_tombstoned(mid(2)));

        let results = reader.search_active(&v, 5, None);
        assert!(results.iter().all(|(id, _)| *id != mid(2)));
    }

    #[test]
    fn swap_clears_pending_and_bumps_epoch() {
        let (reader, mut writer) = build_shared();
        writer.insert(mid(1), &unit_at_angle(0.0)).unwrap();
        assert_eq!(reader.pending_len(), 1);
        let before = reader.epoch();

        let replacement =
            HnswIndexImpl::<8>::new(pq_params_default(), arithmetic_codebook::<8>()).unwrap();
        reader.swap(replacement);

        assert_eq!(reader.epoch(), before.wrapping_add(1));
        assert_eq!(reader.pending_len(), 0);
        // The swapped main is empty; the inserted memory is gone.
        assert!(!reader.contains(mid(1)));
    }

    #[test]
    fn flush_folds_pending_into_main() {
        let (reader, mut writer) = build_shared();
        let v = unit_at_angle(0.0);
        writer.insert(mid(1), &v).unwrap();
        writer.insert(mid(2), &v).unwrap();
        assert_eq!(reader.pending_len(), 2);

        let codebook_for_build = arithmetic_codebook::<8>();
        let report = reader
            .flush_with_rebuild(|snapshot| {
                let mut new_idx =
                    HnswIndexImpl::<8>::new(pq_params_default(), codebook_for_build).unwrap();
                for entry in snapshot {
                    if !entry.tombstoned {
                        new_idx.insert(entry.memory_id, &entry.vector).unwrap();
                    }
                }
                Ok(new_idx)
            })
            .unwrap();
        assert_eq!(report.entries_flushed, 2);
        assert_eq!(report.main_len_after, 2);
        assert_eq!(reader.pending_len(), 0);
        assert!(reader.contains(mid(1)));
        assert!(reader.contains(mid(2)));
    }

    #[test]
    fn merge_dedupe_prefers_pending_on_collision() {
        let main = vec![(mid(1), 0.5), (mid(2), 0.7)];
        let pending = vec![(mid(1), 0.95), (mid(3), 0.6)];
        let merged = merge_dedupe_descending(main, pending, 5);

        // mid(1) appears once with pending's 0.95, not main's 0.5.
        let m1 = merged.iter().find(|(id, _)| *id == mid(1)).unwrap();
        assert!((m1.1 - 0.95).abs() < 1e-6);
        // Sorted descending.
        for w in merged.windows(2) {
            assert!(w[0].1 >= w[1].1);
        }
    }

    // ----- Snapshot persistence (Task 3) ---------------------------------

    fn unit_vec(seed: u64) -> [f32; crate::params::VECTOR_DIM] {
        // Build a deterministic non-zero vector so the HNSW has actual
        // structure to dump. L2-normalise so PQ distances behave.
        use crate::params::VECTOR_DIM;
        let mut v = [0.0_f32; VECTOR_DIM];
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let mut norm_sq = 0.0_f32;
        for slot in &mut v {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let f = (((x >> 33) as u32) as f32 / u32::MAX as f32) - 0.5;
            *slot = f;
            norm_sq += f * f;
        }
        let inv = 1.0 / norm_sq.sqrt();
        for slot in &mut v {
            *slot *= inv;
        }
        v
    }

    fn fixture_with_writes(n: usize) -> (SharedHnsw, Writer, Vec<MemoryId>) {
        let (shared, mut writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
        let mut ids = Vec::with_capacity(n);
        for i in 0..n {
            let id = MemoryId::pack(0, (i + 1) as u64, 1);
            writer.insert(id, &unit_vec(i as u64 + 1)).unwrap();
            ids.push(id);
        }
        // Move pending into main: rebuild yields a freshly-published
        // epoch the snapshot can capture from.
        shared
            .flush_with_rebuild(|pending| {
                let params = shared.params();
                let codebook = crate::pq::bootstrap_codebook();
                let pairs: Vec<(MemoryId, [f32; crate::params::VECTOR_DIM])> = pending
                    .iter()
                    .map(|e| (e.memory_id, e.vector))
                    .collect();
                let (idx, _) = crate::rebuild::rebuild_impl::<{ crate::pq::BOOTSTRAP_M }, _>(
                    params, codebook, pairs,
                )?;
                Ok(idx)
            })
            .unwrap();
        (shared, writer, ids)
    }

    #[test]
    fn save_load_round_trips_epoch_and_lsn() {
        let dir = tempfile::tempdir().unwrap();
        let basename = "hnsw";
        let shard_uuid: [u8; 16] = [0xAA; 16];
        let taken_at_lsn = 12345_u64;

        let (shared, _writer, ids) = fixture_with_writes(5);
        let len_before = shared.len();
        shared
            .save_snapshot(dir.path(), basename, taken_at_lsn, shard_uuid)
            .expect("save_snapshot");

        let (loaded_idx, lsn) = SharedHnsw::load_snapshot(dir.path(), basename, shard_uuid)
            .expect("load_snapshot");
        assert_eq!(lsn, taken_at_lsn);
        assert_eq!(loaded_idx.len(), len_before);
        // Every inserted id is in the rehydrated index.
        for id in &ids {
            assert!(loaded_idx.contains(*id), "memory {:?} missing after reload", id);
        }
    }

    #[test]
    fn load_rejects_shard_uuid_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let (shared, _w, _) = fixture_with_writes(2);
        shared
            .save_snapshot(dir.path(), "hnsw", 1, [0xAA; 16])
            .unwrap();
        match SharedHnsw::load_snapshot(dir.path(), "hnsw", [0xBB; 16]) {
            Err(HnswError::SnapshotCorrupt(msg)) => assert!(msg.contains("shard_uuid")),
            Err(other) => panic!("expected SnapshotCorrupt(shard_uuid), got {other:?}"),
            Ok(_) => panic!("expected error on uuid mismatch"),
        }
    }

    #[test]
    fn load_rejects_corrupted_graph_file() {
        let dir = tempfile::tempdir().unwrap();
        let (shared, _w, _) = fixture_with_writes(2);
        shared.save_snapshot(dir.path(), "hnsw", 1, [0xAA; 16]).unwrap();

        // Flip a byte in the graph file. The wrapper's BLAKE3 over the
        // graph won't match → SnapshotCorrupt.
        let graph_path = dir.path().join("hnsw.hnsw.graph");
        let mut bytes = std::fs::read(&graph_path).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xFF;
        std::fs::write(&graph_path, bytes).unwrap();

        match SharedHnsw::load_snapshot(dir.path(), "hnsw", [0xAA; 16]) {
            Err(HnswError::SnapshotCorrupt(msg)) => {
                assert!(msg.contains("graph") || msg.contains("BLAKE3"), "msg: {msg}");
            }
            Err(other) => panic!("expected SnapshotCorrupt(graph), got {other:?}"),
            Ok(_) => panic!("expected error on graph corruption"),
        }
    }

    #[test]
    fn load_rejects_missing_wrapper() {
        let dir = tempfile::tempdir().unwrap();
        match SharedHnsw::load_snapshot(dir.path(), "hnsw", [0xAA; 16]) {
            Err(HnswError::SnapshotIo(_)) => {}
            Err(other) => panic!("expected SnapshotIo(missing), got {other:?}"),
            Ok(_) => panic!("expected error on missing wrapper"),
        }
    }
}
