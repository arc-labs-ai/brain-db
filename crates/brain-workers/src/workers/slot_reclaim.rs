//! Slot reclamation worker.
//!
//! Scans for memories whose `tombstoned_at_unix_nanos + grace_period`
//! is past, and reclaims them: delete the MEMORIES row + adjacent
//! edges + the plaintext (TEXTS) + the write-artifact bundle (embedding
//! vector + derived graph) that a soft FORGET deferred to grace expiry —
//! one wtxn per memory keeps lock duration small.
//!
//! ## v1 deviations (documented)
//!
//! - **No arena / free-list push.** Reclamation wants the slot id
//!   pushed back onto the arena free list. v1 has no arena and never
//!   reuses slots (each ENCODE mints a fresh slot from a monotonic
//!   counter), so the push is a no-op for now.
//! - **No SLOT_VERSIONS table / version bump.** v1 doesn't reuse
//!   slots, so the version bump has no observable effect.
//!   `MemoryId` already encodes the version, and stale references
//!   already mismatch via the existing slot-version field.
//! - **Adjacent edges only.**: we delete `EDGES_OUT` where
//!   `source = id` and `EDGES_IN` where `target = id`. Other-direction
//!   dangling edges (`EDGES_OUT` where `target = id`) survive — the
//!   edge-scrub worker cleans those up.
//! - **HNSW node: tombstoned at forget, dropped at rebuild.** The
//!   memory's HNSW node is marked tombstoned when the FORGET commits
//!   (the writer's `Tombstone(Memory)` side-effect), so the semantic
//!   lane already excludes it; the tombstoned node itself is dropped
//!   from the graph on the next HNSW maintenance rebuild. Reclamation
//!   does not touch the HNSW.
//! - **HyPE question-vectors purged here as a backstop.** FORGET
//!   removes a memory's durable HyPE rows promptly, but reclamation
//!   re-runs the same idempotent delete so a memory tombstoned before
//!   that path existed (or via a code path that skipped it) can't leave
//!   the redb rows around past grace. The delete is a no-op when the
//!   memory owns no HyPE rows. The tiny `(memory_id -> neighborhood
//!   hash)` gate row is left behind — it's only ever read by the HyPE
//!   refresh worker, which never runs on a gone memory.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use brain_core::MemoryId;
use brain_metadata::tables::edge::{EDGES_REVERSE_TABLE, EDGES_TABLE};
use brain_metadata::tables::memory::MEMORIES_TABLE;
use brain_metadata::tables::text::TEXTS_TABLE;
use redb::ReadableTable;
use tracing::trace;

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

/// — default 7-day FORGET grace window. Configurable per
/// worker via [`SlotReclamationWorker::with_grace_period`].
pub const DEFAULT_FORGET_GRACE: Duration = Duration::from_secs(7 * 24 * 3600);

pub struct SlotReclamationWorker {
    config: WorkerConfig,
    grace_period: Duration,
}

impl SlotReclamationWorker {
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: WorkerConfig::defaults_for(WorkerKind::SlotReclamation),
            grace_period: DEFAULT_FORGET_GRACE,
        }
    }

    #[must_use]
    pub fn with_config(mut self, cfg: WorkerConfig) -> Self {
        self.config = cfg;
        self
    }

    #[must_use]
    pub fn with_grace_period(mut self, d: Duration) -> Self {
        self.grace_period = d;
        self
    }

    #[must_use]
    pub fn grace_period(&self) -> Duration {
        self.grace_period
    }
}

impl Default for SlotReclamationWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl Worker for SlotReclamationWorker {
    fn name(&self) -> &'static str {
        WorkerKind::SlotReclamation.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::SlotReclamation
    }
    fn config(&self) -> WorkerConfig {
        self.config.clone()
    }
    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + 'a>> {
        Box::pin(do_reclaim_cycle(self, ctx))
    }
}

async fn do_reclaim_cycle(
    worker: &SlotReclamationWorker,
    ctx: &WorkerContext,
) -> Result<usize, WorkerError> {
    let cfg = worker.config.clone();
    if cfg.batch_size == 0 {
        return Ok(0);
    }
    let grace_nanos = u64::try_from(worker.grace_period.as_nanos()).unwrap_or(u64::MAX);
    let now_nanos = now_unix_nanos();
    let cutoff_nanos = now_nanos.saturating_sub(grace_nanos);
    let metadata = ctx.ops.executor.metadata.clone();
    let started = Instant::now();

    // ── Scan phase: collect candidates above the cutoff. Bounded by
    //    batch_size + max_runtime. Reads run inside one read txn
    //    with the mutex held; no .await crosses the lock. ─────────
    let candidates: Vec<MemoryId> = {
        let rtxn = metadata
            .read_txn()
            .map_err(|e| WorkerError::Ops(format!("reclaim read_txn: {e:?}")))?;
        let table = rtxn
            .open_table(MEMORIES_TABLE)
            .map_err(|e| WorkerError::Ops(format!("open MEMORIES: {e:?}")))?;
        let mut out = Vec::with_capacity(cfg.batch_size.min(1024));
        for entry in table
            .iter()
            .map_err(|e| WorkerError::Ops(format!("iter MEMORIES: {e:?}")))?
        {
            let (key, value) = entry.map_err(|e| WorkerError::Ops(format!("row: {e:?}")))?;
            let meta = value.value();
            let Some(ts) = meta.tombstoned_at_unix_nanos else {
                continue;
            };
            if ts >= cutoff_nanos {
                continue;
            }
            out.push(MemoryId::from_be_bytes(key.value()));
            if out.len() >= cfg.batch_size {
                break;
            }
        }
        out
    };

    if candidates.is_empty() {
        return Ok(0);
    }

    // ── Reclaim phase: one wtxn per memory. ────────────
    let mut reclaimed = 0usize;
    for id in candidates {
        if started.elapsed() >= cfg.max_runtime {
            break;
        }
        if ctx.is_shutdown() {
            break;
        }
        if reclaim_one(&metadata, id, cutoff_nanos)? {
            reclaimed += 1;
        }
        // Yield between reclamations so we don't monopolise the mutex.
        glommio::executor().yield_if_needed().await;
    }

    trace!(
        reclaimed,
        cycle_ms = started.elapsed().as_millis() as u64,
        "slot reclamation cycle"
    );
    Ok(reclaimed)
}

/// Atomically delete one tombstoned memory + its adjacent edges.
/// Returns `true` if the row was reclaimed; `false` if the race-guard
/// rejected it (row gone, no longer tombstoned, or no longer past
/// cutoff). All in one wtxn.
fn reclaim_one(
    metadata: &brain_planner::SharedMetadataDb,
    id: MemoryId,
    cutoff_nanos: u64,
) -> Result<bool, WorkerError> {
    let wtxn = metadata
        .write_txn()
        .map_err(|e| WorkerError::Ops(format!("reclaim_one write_txn: {e:?}")))?;
    let did_remove = {
        let mut memories = wtxn
            .open_table(MEMORIES_TABLE)
            .map_err(|e| WorkerError::Ops(format!("open MEMORIES: {e:?}")))?;
        let key = id.to_be_bytes();
        let row = memories
            .get(key)
            .map_err(|e| WorkerError::Ops(format!("memories get: {e:?}")))?
            .map(|a| a.value());
        // Race guards:
        //   - row gone (vanished between scan and reclaim) → false.
        //   - tombstoned_at unset (defensive; covers a future
        //     ADMIN_RESTORE) → false.
        //   - tombstoned_at >= cutoff (set-once but defensive) → false.
        let eligible = matches!(
            row.as_ref().and_then(|m| m.tombstoned_at_unix_nanos),
            Some(ts) if ts < cutoff_nanos
        );
        if eligible {
            memories
                .remove(key)
                .map_err(|e| WorkerError::Ops(format!("memories remove: {e:?}")))?;
        }
        eligible
    };

    if did_remove {
        purge_adjacent_edges(&wtxn, id)?;

        // Soft FORGET deliberately keeps the plaintext + write-artifact
        // bundle recoverable during the grace window (only a hard FORGET
        // purges them at tombstone time). Reclamation is the grace-expiry
        // path, so it must purge that recoverable data now — otherwise a
        // default (soft) FORGET would leave plaintext in TEXTS_TABLE and the
        // embedding + derived graph in MEMORY_ARTIFACTS/MEMORY_VECTORS
        // forever (breaks invariant #6, grows unbounded, and keeps the
        // vector resolvable via get_artifact_vector). All in this same wtxn
        // as the row + edge deletes so the memory disappears atomically.
        {
            let mut texts = wtxn
                .open_table(TEXTS_TABLE)
                .map_err(|e| WorkerError::Ops(format!("open TEXTS: {e:?}")))?;
            let _ = texts
                .remove(&id.to_be_bytes())
                .map_err(|e| WorkerError::Ops(format!("TEXTS remove: {e:?}")))?;
        }
        // Removes the MEMORY_ARTIFACTS bundle AND the raw MEMORY_VECTORS row,
        // so get_artifact_vector returns None after reclamation.
        brain_ops::memory_artifact::delete_memory_artifact(&wtxn, id.to_be_bytes())
            .map_err(|e| WorkerError::Ops(format!("reclaim artifact delete: {e}")))?;

        // Idempotent backstop: FORGET already dropped these at tombstone
        // time, but re-run the delete so a memory tombstoned before that
        // path existed can't leave orphan HyPE rows past grace. No-op
        // (removes 0) when the memory owns none.
        brain_metadata::hype_vectors_delete_memory(&wtxn, id)
            .map_err(|e| WorkerError::Ops(format!("reclaim hype delete: {e:?}")))?;
    }

    wtxn.commit()
        .map_err(|e| WorkerError::Ops(format!("reclaim commit: {e:?}")))?;
    Ok(did_remove)
}

/// Remove all rows from `EDGES_OUT` keyed by `(id, *, *)` and from
/// `EDGES_IN` keyed by `(id, *, *)` — "adjacent" edges only;
/// dangling edges from other memories pointing to `id` are left for
/// the edge-scrub worker.
fn purge_adjacent_edges(wtxn: &redb::WriteTransaction, id: MemoryId) -> Result<(), WorkerError> {
    let prefix = brain_core::NodeRef::Memory(id).to_bytes();
    // Range upper bound: prefix + saturated 0xFF tail covering every
    // possible (kind, to, disambiguator) suffix.
    let mut hi = prefix.to_vec();
    hi.extend_from_slice(&[0xFFu8; brain_core::EdgeKindRef::MAX_BYTES + 17 + 16]);

    // Forward edges anchored at `id`.
    {
        let mut out = wtxn
            .open_table(EDGES_TABLE)
            .map_err(|e| WorkerError::Ops(format!("open EDGES: {e:?}")))?;
        let victims: Vec<Vec<u8>> = out
            .range::<&[u8]>(prefix.as_slice()..=hi.as_slice())
            .map_err(|e| WorkerError::Ops(format!("EDGES range: {e:?}")))?
            .map(|entry| match entry {
                Ok((k, _)) => Ok(k.value().to_vec()),
                Err(e) => Err(WorkerError::Ops(format!("EDGES row: {e:?}"))),
            })
            .collect::<Result<Vec<_>, _>>()?;
        for k in victims {
            out.remove(k.as_slice())
                .map_err(|e| WorkerError::Ops(format!("EDGES remove: {e:?}")))?;
        }
    }

    // Reverse rows anchored at `id`. The forward-table mirror lives
    // on the OTHER memory's `(from=other, kind, to=id)` row — that
    // row points AT the reclaimed slot but is keyed at `other`, not
    // `id`. Leaving the forward mirror dangling is intentional:
    // slot reclamation only purges rows adjacent to the slot; the
    // edge-scrub worker reaps dangling forward rows on its own
    // schedule.
    {
        let mut rev = wtxn
            .open_table(EDGES_REVERSE_TABLE)
            .map_err(|e| WorkerError::Ops(format!("open EDGES_REVERSE: {e:?}")))?;
        let victims: Vec<Vec<u8>> = rev
            .range::<&[u8]>(prefix.as_slice()..=hi.as_slice())
            .map_err(|e| WorkerError::Ops(format!("EDGES_REVERSE range: {e:?}")))?
            .map(|entry| match entry {
                Ok((k, _)) => Ok(k.value().to_vec()),
                Err(e) => Err(WorkerError::Ops(format!("EDGES_REVERSE row: {e:?}"))),
            })
            .collect::<Result<Vec<_>, _>>()?;
        for k_bytes in victims {
            rev.remove(k_bytes.as_slice())
                .map_err(|e| WorkerError::Ops(format!("EDGES_REVERSE remove: {e:?}")))?;
        }
    }

    Ok(())
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use brain_core::{MemoryKind, NamespaceId, SessionId, SpaceId};
    use brain_metadata::tables::memory::{flags, MemoryMetadata};
    use brain_metadata::MetadataDb;

    fn open_shared() -> (tempfile::TempDir, Arc<MetadataDb>) {
        let dir = tempfile::tempdir().unwrap();
        let db = MetadataDb::open(dir.path().join("meta.redb")).unwrap();
        (dir, Arc::new(db))
    }

    /// Seed a tombstoned (soft-forgotten) memory: the MEMORIES row (inactive,
    /// tombstoned_at stamped), its plaintext (TEXTS), and its write-artifact
    /// bundle (MEMORY_ARTIFACTS + raw MEMORY_VECTORS). This mirrors the state
    /// a default soft FORGET leaves behind — the text/artifact are deferred to
    /// reclamation, not purged at tombstone time.
    fn seed_soft_forgotten(db: &MetadataDb, id: MemoryId, tombstoned_at: u64) {
        let mut row = MemoryMetadata::new_active(
            id,
            NamespaceId::SYSTEM,
            SpaceId::default(),
            SessionId(1),
            id.slot(),
            id.version(),
            MemoryKind::Episodic,
            [0xAB; 16],
            0.5,
            11,
            1_000,
        );
        row.flags &= !flags::ACTIVE;
        row.tombstoned_at_unix_nanos = Some(tombstoned_at);

        let key = id.to_be_bytes();
        let wtxn = db.write_txn().unwrap();
        {
            let mut m = wtxn.open_table(MEMORIES_TABLE).unwrap();
            m.insert(&key, &row).unwrap();
        }
        {
            let mut t = wtxn.open_table(TEXTS_TABLE).unwrap();
            t.insert(&key, b"hello world".as_slice()).unwrap();
        }
        wtxn.commit().unwrap();

        // Write the artifact bundle + raw by-id vector through the ops helper
        // so get_artifact_vector resolves the seeded memory pre-reclaim.
        let wtxn = db.write_txn().unwrap();
        let dim = brain_embed::VECTOR_DIM;
        let record = brain_ops::memory_artifact::sync_record(key, 0, 0.5, 1_000, 0, dim as u32, 11);
        brain_ops::memory_artifact::put_sync_artifact(
            &wtxn,
            key,
            vec![0.1_f32; dim],
            record,
            Vec::new(),
        )
        .unwrap();
        wtxn.commit().unwrap();
    }

    /// A soft FORGET defers plaintext + artifact purge to grace expiry, so
    /// reclamation (the grace path) must remove the MEMORIES row, the TEXTS
    /// row, and the whole artifact bundle (leaving get_artifact_vector at
    /// None and the raw MEMORY_VECTORS row gone). Otherwise invariant #6 is
    /// violated and a forgotten memory's vector stays id-resolvable forever.
    #[test]
    fn reclaim_purges_text_and_artifact_after_grace() {
        let (_dir, db) = open_shared();
        let id = MemoryId::pack(1, 5, 0);
        let key = id.to_be_bytes();
        let tombstoned_at = 10_000u64;
        seed_soft_forgotten(&db, id, tombstoned_at);

        // Pre-reclaim: everything is present.
        {
            let rtxn = db.read_txn().unwrap();
            assert!(rtxn
                .open_table(MEMORIES_TABLE)
                .unwrap()
                .get(&key)
                .unwrap()
                .is_some());
            assert!(rtxn
                .open_table(TEXTS_TABLE)
                .unwrap()
                .get(&key)
                .unwrap()
                .is_some());
            assert!(brain_ops::memory_artifact::get_artifact_vector(&rtxn, key).is_some());
        }

        // Grace has expired: cutoff sits strictly after tombstoned_at.
        let reclaimed = reclaim_one(&db, id, tombstoned_at + 1).unwrap();
        assert!(reclaimed, "an eligible tombstone must be reclaimed");

        // Post-reclaim: row, text, artifact, and raw vector are all gone.
        let rtxn = db.read_txn().unwrap();
        assert!(
            rtxn.open_table(MEMORIES_TABLE)
                .unwrap()
                .get(&key)
                .unwrap()
                .is_none(),
            "MEMORIES row must be reclaimed"
        );
        assert!(
            rtxn.open_table(TEXTS_TABLE)
                .unwrap()
                .get(&key)
                .unwrap()
                .is_none(),
            "plaintext must be purged after grace"
        );
        assert!(
            brain_ops::memory_artifact::get_artifact_vector(&rtxn, key).is_none(),
            "artifact vector must be unresolvable after grace"
        );
        // The raw by-id vector row underlying get_artifact_vector is gone.
        use brain_metadata::tables::memory_vector::MEMORY_VECTORS_TABLE;
        assert!(
            rtxn.open_table(MEMORY_VECTORS_TABLE)
                .unwrap()
                .get(&key)
                .unwrap()
                .is_none(),
            "raw memory-vector row must be purged after grace"
        );
    }

    /// Reclamation is not eligible before grace expires (cutoff <= tombstoned).
    #[test]
    fn reclaim_skips_before_grace() {
        let (_dir, db) = open_shared();
        let id = MemoryId::pack(1, 6, 0);
        let tombstoned_at = 10_000u64;
        seed_soft_forgotten(&db, id, tombstoned_at);

        // cutoff == tombstoned_at → not strictly past grace → skip.
        let reclaimed = reclaim_one(&db, id, tombstoned_at).unwrap();
        assert!(!reclaimed, "must not reclaim before grace expiry");

        let rtxn = db.read_txn().unwrap();
        assert!(rtxn
            .open_table(TEXTS_TABLE)
            .unwrap()
            .get(&id.to_be_bytes())
            .unwrap()
            .is_some());
    }
}
