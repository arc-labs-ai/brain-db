//! Twelve concrete workers (sub-tasks 8.2 – 8.13). Each is a peer
//! module here; the crate root re-exports the public surface.

pub mod access_boost;
pub mod audit_log_sweeper;
pub mod auto_edge;
pub mod backfill;
pub mod cache_evict;
pub mod consolidation;
pub mod counter_reconcile;
pub mod decay;
pub mod edge_scrub;
pub mod entity_gc;
pub mod extractor;
pub mod forget_cascade;
pub mod hnsw_maint;
pub mod idempotency_cleanup;
pub mod llm_cache_sweeper;
pub mod schema_migration;
pub mod slot_reclaim;
pub mod snapshot;
pub mod stale_extraction_detector;
pub mod statistics;
pub mod supersession_sweeper;
pub mod wal_retention;
