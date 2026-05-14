//! Admin HTTP handlers for `profile` + `debug-snapshot`
//! (spec §14/06 §9; sub-task 10.12).
//!
//! Routes:
//! - `POST /v1/diagnostics/profile?shard=N[&duration_secs=D]` → 501.
//!   Real Glommio profiler is deferred to phase-11; operators today
//!   can run `perf record` against the server PID.
//! - `GET /v1/diagnostics/debug-snapshot?shard=N` → 200 + JSON.

mod debug_snapshot;
mod profile;

pub use debug_snapshot::debug_snapshot;
pub use profile::profile;

/// v1 always reports these spec'd fields as not yet populated. As
/// primitives land (active task registry, dispatch queue depth,
/// recent-error ring buffer, arena/HNSW counters), entries drop out
/// of this array. Consumed by `debug_snapshot`.
pub(super) const DEFERRED_FIELDS: &[&str] = &[
    "active_tasks",
    "pending_requests",
    "recent_errors",
    "in_memory_state_summary",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_fields_match_plan() {
        // Spec §14/06 §9 lists 5 fields; one (worker_statuses) is
        // populated, four remain deferred in v1.
        assert!(DEFERRED_FIELDS.contains(&"active_tasks"));
        assert!(DEFERRED_FIELDS.contains(&"pending_requests"));
        assert!(DEFERRED_FIELDS.contains(&"recent_errors"));
        assert!(DEFERRED_FIELDS.contains(&"in_memory_state_summary"));
    }
}
