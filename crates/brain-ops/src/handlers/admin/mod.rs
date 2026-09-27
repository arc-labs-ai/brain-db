//! Admin handlers — internal-tooling surfaces that don't (yet) have
//! dedicated wire opcodes. CLI / future admin protocol layers call into
//! these directly. Each function builds a `Write` and submits through
//! the unified writer path, so admin actions land in the WAL and audit
//! tables the same way wire ops do.
//!
//! Backfill control has no wire surface: `ADMIN_BACKFILL` /
//! `ADMIN_BACKFILL_CANCEL` reject at dispatch (admin is HTTP-only),
//! and the resumable BackfillWorker is driven from the HTTP admin
//! listener (`/v1/backfill`), which reaches the per-shard worker
//! handle through the shard message loop.

pub mod merge_review;
