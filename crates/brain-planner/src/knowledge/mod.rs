//! Phase 23 — hybrid-query planner pieces.
//!
//! Lands as a per-sub-task module under one umbrella so the
//! router (23.3), RRF fusion (23.4), filter chain (23.5),
//! planner (23.6), and executor (23.7) share types without
//! polluting the substrate planner.

pub mod router;
