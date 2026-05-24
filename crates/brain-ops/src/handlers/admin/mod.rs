//! Admin handlers — internal-tooling surfaces that don't (yet) have
//! dedicated wire opcodes. CLI / future admin protocol layers call into
//! these directly. Each function builds a `Write` and submits through
//! the unified writer path, so admin actions land in the WAL and audit
//! tables the same way wire ops do.
//!
//! `backfill` is the exception: it carries the wire ↔ worker
//! adapters for the `ADMIN_BACKFILL` / `ADMIN_BACKFILL_CANCEL`
//! opcodes (wire surface allocated; full handler wiring lands when
//! the per-shard worker handle threads into `OpsContext`).

pub mod backfill;
pub mod merge_review;
