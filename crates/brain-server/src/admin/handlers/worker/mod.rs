//! Admin HTTP handlers for `worker`.
//!
//! Routes:
//! - `GET /v1/workers[?shard=N]` → 200 + per-shard worker snapshots.
//! - `POST /v1/workers/{name}/{stop|start|run-now}` — the live
//!   control plane. `stop` pauses the loop's `run_cycle`; the
//!   loop keeps ticking on its interval so the worker can be resumed
//!   without restarting the shard. `start` resumes a paused worker
//!   (and kicks the wake channel so the next cycle runs without
//!   waiting out the current sleep). `run-now` triggers a single
//!   immediate cycle.

mod control;
mod list;

pub use control::control;
pub use list::list;

/// C0 always-on workers: the write/read-coherence and correctness set
/// that must never be pausable. Per CLAUDE.md §4, extraction (and the
/// downstream typed-graph correctness workers it feeds) is C0 — a
/// graph-populating step that cannot be toggled at deploy time or at
/// runtime without silently breaking graph-backed reads. Control
/// (pause / resume / run-now) rejects every name here regardless of
/// whether it is registered on a given shard.
///
/// This is the *explicit* C0 guard: the controllable set is otherwise
/// derived from the scheduler's live registration snapshot, so no
/// C2 worker is uncontrollable merely because a hand-list went stale,
/// and no C0 worker becomes pausable by being added to a hand-list.
pub(super) const C0_WORKERS: &[&str] = &[
    "extractor",
    "forget_cascade",
    "schema_migration",
    "statement_embed",
];

/// Outcome of validating a worker-control request against the C0 guard
/// and the live registration snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ControlDecision {
    /// Registered, non-C0 worker — the action may be applied.
    Allow,
    /// C0 always-on worker — control is forbidden.
    RejectC0,
    /// No worker by this name is registered anywhere.
    Unknown,
}

/// Classify a control request. The C0 guard takes precedence over
/// registration: a C0 worker is rejected with the C0 reason even if it
/// happens not to be registered on the queried shards. Any other name
/// is `Allow` iff it appears in the registration snapshot, else
/// `Unknown`.
pub(super) fn classify_control(name: &str, registered: &[&str]) -> ControlDecision {
    if C0_WORKERS.contains(&name) {
        ControlDecision::RejectC0
    } else if registered.contains(&name) {
        ControlDecision::Allow
    } else {
        ControlDecision::Unknown
    }
}

/// Control actions accepted on the deferred `POST /v1/workers/{name}/{action}`.
pub(super) const KNOWN_ACTIONS: &[&str] = &["stop", "start", "run-now"];

#[cfg(test)]
mod tests {
    use super::{classify_control, ControlDecision, C0_WORKERS};

    #[test]
    fn registered_c2_worker_is_controllable() {
        // A registered, non-C0 worker (deploy-time C2 maintenance / GC)
        // must be controllable — not rejected because a hand-list is
        // stale.
        let registered = ["decay", "entity_gc", "llm_cache_sweeper"];
        assert_eq!(
            classify_control("entity_gc", &registered),
            ControlDecision::Allow
        );
        assert_eq!(
            classify_control("decay", &registered),
            ControlDecision::Allow
        );
    }

    #[test]
    fn c0_worker_is_rejected() {
        // Every C0 worker is rejected, even when it is registered (the
        // common case) — the C0 guard takes precedence.
        let registered = ["extractor", "forget_cascade", "statement_embed", "decay"];
        for name in C0_WORKERS {
            assert_eq!(
                classify_control(name, &registered),
                ControlDecision::RejectC0,
                "{name} must be rejected as C0"
            );
        }
    }

    #[test]
    fn c0_worker_rejected_even_if_not_registered() {
        // The guard doesn't depend on registration: a C0 name never
        // falls through to Unknown.
        assert_eq!(
            classify_control("extractor", &[]),
            ControlDecision::RejectC0
        );
    }

    #[test]
    fn unregistered_name_is_unknown() {
        let registered = ["decay", "consolidation"];
        assert_eq!(
            classify_control("nonesuch", &registered),
            ControlDecision::Unknown
        );
    }
}
