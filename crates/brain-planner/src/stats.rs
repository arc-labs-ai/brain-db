//! Per-shard runtime statistics consulted by the planner.
//!
//! `Default::default()` returns all-zero values so tests can
//! construct a `PlannerContext` without a live observability layer.
//! Real values are wired from the monitoring layer later.
//!
//! Two fields the cost model uses right away:
//! - `memory_count` — feeds `ann_search_cost(n, ef)`.
//! - `tombstone_ratio` — biases ef upward when tombstones dominate

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
    /// the observability layer. Zero until then.
    pub avg_search_latency_ms: f32,
    pub avg_encode_latency_ms: f32,
}
