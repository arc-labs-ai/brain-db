//! Shared, authoritative-state rebuild routines for the shard's
//! derived in-RAM indexes.
//!
//! Every derived index is reconstructed from the redb metadata store
//! (the authoritative state) — never from the arena, which is empty in
//! a live run (it is populated only by WAL recovery on restart). These
//! routines are the single implementation shared by two callers:
//!
//! - the **boot recovery** path (`spawn_shard`), which rebuilds each
//!   index once at shard startup, and
//! - the **on-demand admin** path (`ShardRequest::RebuildIndex`, driven
//!   by `POST /v1/rebuild`), which rebuilds a chosen index while the
//!   shard keeps serving.
//!
//! Because the HNSW indexes live behind an `Arc<RwLock<…>>` shared with
//! the live retrievers, `rebuild()` swaps the underlying `hnsw_rs::Hnsw`
//! in place under the write lock — a rebuild is immediately visible on
//! the serve path with no handle swap, and a rebuild that fails leaves
//! the prior index intact (the write lock is only taken to install the
//! finished replacement).

use std::sync::Arc;

use brain_core::{EntityId, ShardId};
use brain_embed::{Dispatcher, VECTOR_DIM};
use brain_index::entity_hnsw::EntityHnswIndex;
use brain_index::hype_hnsw::HypeHnswIndex;
use brain_index::statement_question_hnsw::StatementQuestionHnswIndex;
use brain_metadata::MetadataDb;
use parking_lot::RwLock;
use tracing::{error, info};

/// Which derived index to rebuild.
///
/// Tantivy (`memory_text.tantivy/`, `statements.tantivy/`) is
/// deliberately absent: its on-disk rebuild is a directory rename-swap
/// that the live retriever's cached `IndexReader` (bound to the original
/// `Index`) never observes, and it would race the text-indexer drain
/// task's persistent `IndexWriter` (which holds tantivy's exclusive
/// writer lock for the shard's whole life). A hot tantivy rebuild is a
/// cross-cutting refactor (worker-pause + writer-close + reopen +
/// swappable ops handles), out of scope here; tantivy is rebuilt from
/// authoritative redb at boot instead (see `tantivy_recovery`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RebuildTarget {
    /// The memory HNSW (semantic recall). Rebuilt from the redb-backed
    /// vector snapshot via `SharedHnsw::flush_with_rebuild`.
    MemoryHnsw,
    /// The entity HNSW (resolver tier-3 embedding tie-break). Rebuilt
    /// from `ENTITIES_TABLE`, preferring the durable per-entity vector
    /// and re-embedding the canonical name only as a fallback.
    EntityHnsw,
    /// The HyPE pool (hypothetical-question embeddings). Rebuilt from
    /// the durable `hype_question_vectors` rows.
    HypeHnsw,
    /// The per-statement question-bridge pool. Rebuilt from the durable
    /// `statement_question_vectors` rows.
    StatementQuestionHnsw,
    /// Rebuild every HNSW target above, in turn. The report aggregates
    /// the per-target entry counts (sum) and the total elapsed time.
    All,
}

impl RebuildTarget {
    /// Parse the `?index=` admin query value. Returns `None` for an
    /// unknown target so the route can answer `400`.
    pub(crate) fn from_query(s: &str) -> Option<Self> {
        match s {
            "memory_hnsw" | "memory" | "ann" => Some(Self::MemoryHnsw),
            "entity_hnsw" | "entity" => Some(Self::EntityHnsw),
            "hype_hnsw" | "hype" => Some(Self::HypeHnsw),
            "statement_question_hnsw" | "statement_question" | "question_bridge" => {
                Some(Self::StatementQuestionHnsw)
            }
            "all" => Some(Self::All),
            _ => None,
        }
    }
}

/// Rebuild the entity HNSW from `ENTITIES_TABLE`.
///
/// Mirrors the boot-recovery block: prefers the durable per-entity
/// vector written at entity-create time; rows without one fall back to
/// re-embedding the canonical name. Logs at the same levels the boot
/// path used and returns the number of entities inserted into the new
/// index.
pub(crate) fn rebuild_entity_hnsw(
    index: &Arc<RwLock<EntityHnswIndex>>,
    metadata: &MetadataDb,
    dispatcher: &dyn Dispatcher,
    shard_id: ShardId,
) -> Result<usize, String> {
    let rtxn = metadata
        .read_txn()
        .map_err(|e| format!("entity HNSW rebuild: read_txn failed: {e}"))?;
    let entities = brain_metadata::entity::ops::entity_iter_all_live_with_vectors(&rtxn)
        .map_err(|e| format!("entity HNSW rebuild: metadata scan failed: {e:?}"))?;

    if entities.is_empty() {
        // Publish an empty index so a rebuild after every entity was
        // forgotten converges to empty rather than leaving stale nodes.
        index
            .write()
            .rebuild(Vec::<(EntityId, [f32; VECTOR_DIM])>::new())
            .map_err(|e| format!("entity HNSW rebuild: {e:?}"))?;
        info!(shard_id, "no entities to rebuild; entity HNSW is empty");
        return Ok(0);
    }

    let count = entities.len();
    let mut pairs: Vec<(EntityId, [f32; VECTOR_DIM])> = Vec::with_capacity(count);
    let mut from_stored = 0usize;
    let mut from_reembed = 0usize;
    let mut embed_failures = 0usize;
    for (id, name, stored) in entities {
        if let Some(v) = stored {
            pairs.push((id, v));
            from_stored += 1;
        } else {
            match dispatcher.embed(&name) {
                Ok(v) => {
                    pairs.push((id, v));
                    from_reembed += 1;
                }
                Err(_) => embed_failures += 1,
            }
        }
    }
    let report = index
        .write()
        .rebuild(pairs)
        .map_err(|e| format!("entity HNSW rebuild: {e:?}"))?;
    info!(
        shard_id,
        rebuilt = count,
        inserted = report.inserted,
        from_stored,
        from_reembed,
        embed_failures,
        "entity HNSW rebuilt from metadata"
    );
    Ok(report.inserted)
}

/// Rebuild the HyPE pool from the durable `hype_question_vectors` rows.
///
/// The vectors are the source of truth (no re-embed fallback); a
/// missing/empty table just means an empty pool.
pub(crate) fn rebuild_hype_hnsw(
    index: &Arc<RwLock<HypeHnswIndex>>,
    metadata: &MetadataDb,
    shard_id: ShardId,
) -> Result<usize, String> {
    let rtxn = metadata
        .read_txn()
        .map_err(|e| format!("HyPE HNSW rebuild: read_txn failed: {e}"))?;
    let points = match brain_metadata::hype_iter_all_vectors(&rtxn) {
        Ok(points) => points,
        Err(e) => {
            // A never-written table is expected on a fresh shard; treat
            // it as an empty pool rather than a hard error.
            info!(shard_id, error = ?e, "HyPE pool empty or table absent; pool is empty");
            index.write().rebuild(Vec::new());
            return Ok(0);
        }
    };
    let report = index.write().rebuild(points);
    info!(
        shard_id,
        inserted = report.inserted,
        memories = report.memories,
        "HyPE HNSW rebuilt from metadata"
    );
    Ok(report.inserted)
}

/// Rebuild the per-statement question-bridge pool from the durable
/// `statement_question_vectors` rows. Same durable-source model as HyPE.
pub(crate) fn rebuild_statement_question_hnsw(
    index: &Arc<RwLock<StatementQuestionHnswIndex>>,
    metadata: &MetadataDb,
    shard_id: ShardId,
) -> Result<usize, String> {
    let rtxn = metadata
        .read_txn()
        .map_err(|e| format!("statement question-bridge rebuild: read_txn failed: {e}"))?;
    let points = match brain_metadata::statement_question::ops::statement_question_iter_all(&rtxn) {
        Ok(points) => points,
        Err(e) => {
            info!(
                shard_id,
                error = ?e,
                "statement question-bridge empty or table absent; pool is empty"
            );
            index.write().rebuild(Vec::new());
            return Ok(0);
        }
    };
    let report = index.write().rebuild(points);
    info!(
        shard_id,
        inserted = report.inserted,
        statements = report.statements,
        "statement question-bridge rebuilt from metadata"
    );
    Ok(report.inserted)
}

/// Boot-path adapter: run a rebuild helper and, on failure, log at
/// ERROR and continue (a degraded index is better than a shard that
/// refuses to start). Keeps the boot call sites terse while the helpers
/// stay `Result`-typed for the on-demand path.
pub(crate) fn log_boot_result(shard_id: ShardId, what: &str, result: Result<usize, String>) {
    if let Err(e) = result {
        error!(shard_id, error = %e, "{what} startup rebuild failed; capability degraded");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_core::{Entity, EntityType, MemoryId, SessionId, Slot, StatementId};
    use brain_embed::EmbedError;
    use brain_index::entity_hnsw::EntityHnswParams;
    use brain_index::hype_hnsw::hype_default_params;
    use brain_index::statement_question_hnsw::statement_question_default_params;
    use brain_metadata::RowScope;
    use tempfile::TempDir;

    const NOW: u64 = 1_700_000_000_000_000_000;
    const SHARD: ShardId = 0;

    /// A distinct near-orthogonal unit vector per `axis` so cosine
    /// search can pick out an individual seeded item.
    fn onehot(axis: usize) -> [f32; VECTOR_DIM] {
        let mut v = [0.0f32; VECTOR_DIM];
        v[axis % VECTOR_DIM] = 1.0;
        v
    }

    struct StubDispatcher;
    impl Dispatcher for StubDispatcher {
        fn embed(&self, _: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
            Ok(onehot(0))
        }
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
            Ok(vec![onehot(0); texts.len()])
        }
        fn fingerprint(&self) -> [u8; 16] {
            [0; 16]
        }
    }

    fn open_db() -> (TempDir, MetadataDb) {
        let dir = TempDir::new().expect("tempdir");
        let db = MetadataDb::open(dir.path().join("meta.redb")).expect("open");
        (dir, db)
    }

    fn scope() -> RowScope {
        RowScope::from_bytes(brain_core::NamespaceId::SYSTEM.raw(), [0xAB; 16])
    }

    // ----- entity HNSW ---------------------------------------------------

    #[test]
    fn rebuild_entity_hnsw_matches_redb_and_is_searchable() {
        let (_dir, db) = open_db();
        // Seed three live entities, each with a distinct durable vector.
        let mut ids = Vec::new();
        {
            let wtxn = db.write_txn().unwrap();
            for i in 0..3u8 {
                let e = Entity::new_active(
                    brain_core::EntityId::new(),
                    EntityType::PERSON_ID,
                    format!("Person {i}"),
                    format!("person {i}"),
                    NOW,
                );
                let id = e.id;
                brain_metadata::entity::ops::entity_put(&wtxn, scope(), SessionId::DEFAULT, &e)
                    .unwrap();
                brain_metadata::entity::ops::entity_vector_put(&wtxn, id, &onehot(i as usize + 1))
                    .unwrap();
                ids.push(id);
            }
            wtxn.commit().unwrap();
        }

        let index = Arc::new(RwLock::new(
            EntityHnswIndex::new(EntityHnswParams::default_v1()).unwrap(),
        ));
        let entries = rebuild_entity_hnsw(&index, &db, &StubDispatcher, SHARD).unwrap();
        assert_eq!(entries, 3, "report entries match redb rows");
        assert_eq!(index.read().len(), 3);

        // The rebuilt index answers a search for a seeded entity.
        let hits = index.read().search(&onehot(2), 1).unwrap();
        assert_eq!(hits[0].0, ids[1], "search returns the seeded entity");

        // Idempotent: a second rebuild converges to the same count.
        let again = rebuild_entity_hnsw(&index, &db, &StubDispatcher, SHARD).unwrap();
        assert_eq!(again, 3);
        assert_eq!(index.read().len(), 3);
    }

    #[test]
    fn rebuild_entity_hnsw_empty_converges_to_empty() {
        let (_dir, db) = open_db();
        let index = Arc::new(RwLock::new(
            EntityHnswIndex::new(EntityHnswParams::default_v1()).unwrap(),
        ));
        let entries = rebuild_entity_hnsw(&index, &db, &StubDispatcher, SHARD).unwrap();
        assert_eq!(entries, 0);
        assert_eq!(index.read().len(), 0);
    }

    // ----- HyPE HNSW -----------------------------------------------------

    #[test]
    fn rebuild_hype_hnsw_matches_redb_and_is_searchable() {
        let (_dir, db) = open_db();
        let mids: Vec<MemoryId> = (1..=3u128).map(MemoryId::from_raw).collect();
        {
            let wtxn = db.write_txn().unwrap();
            for (i, mid) in mids.iter().enumerate() {
                brain_metadata::hype_vector_put(&wtxn, *mid, 0, &onehot(i + 1)).unwrap();
            }
            wtxn.commit().unwrap();
        }

        let index = Arc::new(RwLock::new(
            HypeHnswIndex::new(hype_default_params()).unwrap(),
        ));
        let entries = rebuild_hype_hnsw(&index, &db, SHARD).unwrap();
        assert_eq!(entries, 3);
        assert_eq!(index.read().len(), 3);

        let hits = index.read().search(&onehot(1), 1).unwrap();
        assert_eq!(hits[0].0, mids[0]);

        let again = rebuild_hype_hnsw(&index, &db, SHARD).unwrap();
        assert_eq!(again, 3);
        assert_eq!(index.read().len(), 3);
    }

    // ----- statement question-bridge HNSW --------------------------------

    #[test]
    fn rebuild_statement_question_hnsw_matches_redb_and_is_searchable() {
        let (_dir, db) = open_db();
        let sids: Vec<StatementId> = (0..3)
            .map(|i| {
                let mut b = [0u8; 16];
                b[0] = i + 1;
                StatementId::from_bytes(b)
            })
            .collect();
        {
            let wtxn = db.write_txn().unwrap();
            for (i, sid) in sids.iter().enumerate() {
                brain_metadata::statement_question::ops::statement_question_put(
                    &wtxn,
                    *sid,
                    0,
                    Slot::Object,
                    &onehot(i + 1),
                )
                .unwrap();
            }
            wtxn.commit().unwrap();
        }

        let index = Arc::new(RwLock::new(
            StatementQuestionHnswIndex::new(statement_question_default_params()).unwrap(),
        ));
        let entries = rebuild_statement_question_hnsw(&index, &db, SHARD).unwrap();
        assert_eq!(entries, 3);
        assert_eq!(index.read().len(), 3);

        let hits = index.read().search(&onehot(2), 1).unwrap();
        assert_eq!(hits[0].0, sids[1]);
        assert_eq!(hits[0].1, Slot::Object);

        let again = rebuild_statement_question_hnsw(&index, &db, SHARD).unwrap();
        assert_eq!(again, 3);
    }

    // ----- target parsing ------------------------------------------------

    #[test]
    fn rebuild_target_from_query_parses_known_and_rejects_unknown() {
        assert_eq!(
            RebuildTarget::from_query("memory_hnsw"),
            Some(RebuildTarget::MemoryHnsw)
        );
        assert_eq!(
            RebuildTarget::from_query("entity_hnsw"),
            Some(RebuildTarget::EntityHnsw)
        );
        assert_eq!(
            RebuildTarget::from_query("hype_hnsw"),
            Some(RebuildTarget::HypeHnsw)
        );
        assert_eq!(
            RebuildTarget::from_query("statement_question_hnsw"),
            Some(RebuildTarget::StatementQuestionHnsw)
        );
        assert_eq!(RebuildTarget::from_query("all"), Some(RebuildTarget::All));
        assert_eq!(RebuildTarget::from_query("tantivy"), None);
        assert_eq!(RebuildTarget::from_query(""), None);
    }
}
