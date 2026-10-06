//! Admin HTTP handlers for `profile` + `debug-snapshot`.
//!
//! Routes:
//! - `POST /v1/diagnostics/profile?shard=N[&duration_secs=D]` → 501.
//!   The real Glommio profiler is not yet wired; operators today
//!   can run `perf record` against the server PID.
//! - `GET /v1/diagnostics/debug-snapshot?shard=N` → 200 + JSON.
//!   Populates `workers` (from `scheduler_snapshot`), `pending_requests`
//!   (the shard's dispatch-queue depth), and `in_memory_state_summary`
//!   (HNSW + arena/WAL/metadata counters). The two fields still in
//!   `deferred[]` — `active_tasks` and `recent_errors` — need runtime
//!   primitives that do not exist yet (see the module for the design).

mod debug_snapshot;
mod profile;

pub use debug_snapshot::debug_snapshot;
pub use profile::profile;

/// Spec'd debug-snapshot fields not yet populated in v1. Each needs a
/// runtime primitive Brain does not have:
///
/// - `active_tasks` — a per-shard registry of in-flight spawned tasks.
///   The executor drains its request channel serially and spawns
///   detached tasks with no live count; populating this means adding a
///   gauge bumped around each in-flight op inside the shard loop.
/// - `recent_errors` — a bounded ring buffer fed by a custom
///   `tracing_subscriber::Layer` capturing ERROR events, threaded into
///   `AdminState` the way `apply_log_level` is.
///
/// Consumed by `debug_snapshot` to emit the `deferred[]` array.
pub(super) const DEFERRED_FIELDS: &[&str] = &["active_tasks", "recent_errors"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_fields_match_plan() {
        // worker_statuses, pending_requests, and in_memory_state_summary
        // are populated; only these two remain deferred in v1.
        assert!(DEFERRED_FIELDS.contains(&"active_tasks"));
        assert!(DEFERRED_FIELDS.contains(&"recent_errors"));
        assert!(!DEFERRED_FIELDS.contains(&"pending_requests"));
        assert!(!DEFERRED_FIELDS.contains(&"in_memory_state_summary"));
    }
}
