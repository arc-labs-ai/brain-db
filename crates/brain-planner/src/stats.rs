//! Per-shard runtime statistics consulted by the planner.
//!
//! Spec §08/07 §10 names the full shape; Phase 6 ships the struct
//! with `Default::default()` returning all-zero values so tests can
//! construct a `PlannerContext` without a live observability layer.
//! Phase 11+ wires real values from the monitoring layer.
//!
//! Two fields the cost model in 6.2 will use right away:
//! - `memory_count` — feeds `ann_search_cost(n, ef)` (spec §07 §3).
//! - `tombstone_ratio` — biases ef upward when tombstones dominate
//!   (spec §07 §4).

#[derive(Debug, Clone, Copy, Default)]
pub struct ShardStats {
    /// Live (non-tombstoned) memories on this shard.
    pub memory_count: u64,

    /// Tombstoned slots not yet reclaimed.
    pub tombstone_count: u64,

    /// `tombstone_count / (memory_count + tombstone_count)`. Carried
    /// explicitly so callers don't repeatedly recompute it.
    pub tombstone_ratio: f32,

    /// Unix-nanos of the last HNSW rebuild on this shard.
    pub last_rebuild_at_unix_nanos: u64,

    /// Rolling-window average over the last few minutes; populated by
    /// Phase 11 observability. Zero until then.
    pub avg_search_latency_ms: f32,
    pub avg_encode_latency_ms: f32,
}
