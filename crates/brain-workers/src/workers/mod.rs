//! Twelve concrete workers. Each is a peer module here; the crate root
//! re-exports the public surface.

use brain_core::MemoryId;
use brain_metadata::tables::memory::MEMORIES_TABLE;

use crate::context::WorkerContext;

pub mod access_boost;
pub mod ambiguity_resolver;
pub mod audit_log_sweeper;
pub mod auto_edge;
pub mod backfill;
pub mod cache_evict;
pub mod causal_edge;
pub mod confidence_sweep;
pub mod consolidation;
pub mod counter_reconcile;
pub mod decay;
pub mod edge_scrub;
pub mod entity_gc;
pub mod extractor;
pub mod forget_cascade;
pub mod hnsw_maint;
pub mod hype;
pub mod idempotency_cleanup;
pub mod llm_cache_sweeper;
pub mod predicate_gc;
pub mod schema_migration;
pub mod slot_reclaim;
pub mod snapshot;
pub mod stale_extraction_detector;
pub mod statement_embed;
pub mod statement_reclaim;
pub mod statistics;
pub mod supersession_sweeper;
pub mod temporal_edge;
pub mod wal_retention;

/// A row's owning tenant scope: `(namespace_id, space_id_bytes)`.
///
/// Both halves are needed to place a derived row. A write carrying only the
/// space lands under `NamespaceId::SYSTEM`, because that is what
/// `Write::from_phases` defaults to, and the result is invisible to the
/// tenant that owns the memories it was derived from.
pub type Scope = (u32, [u8; 16]);

/// Read a memory's owning scope from `MEMORIES_TABLE`.
///
/// Returns `None` when the row is absent — a memory that cannot be
/// tenant-placed never receives a derived edge, which the apply layer's
/// Link tenant wall would enforce in any case.
pub fn load_memory_scope(ctx: &WorkerContext, memory_id: MemoryId) -> Option<Scope> {
    ctx.ops
        .executor
        .metadata
        .as_ref()
        .read_txn()
        .ok()
        .and_then(|rtxn| {
            rtxn.open_table(MEMORIES_TABLE)
                .ok()
                .and_then(|t| t.get(&memory_id.to_be_bytes()).ok().flatten())
                .map(|g| {
                    let meta = g.value();
                    (meta.namespace_id, meta.space_id_bytes)
                })
        })
}
