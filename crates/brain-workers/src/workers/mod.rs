//! Twelve concrete workers (sub-tasks 8.2 – 8.13). Each is a peer
//! module here; the crate root re-exports the public surface.

pub mod access_boost;
pub mod cache_evict;
pub mod consolidation;
pub mod counter_reconcile;
pub mod decay;
pub mod edge_scrub;
pub mod hnsw_maint;
pub mod idempotency_cleanup;
pub mod slot_reclaim;
pub mod snapshot;
pub mod statistics;
pub mod wal_retention;
