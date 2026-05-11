# Phase 3 — Metadata + Graph (redb)

## Goal

Implement the `redb`-backed metadata store: agents, contexts, memory metadata, edges, idempotency cache, and the durable LSN checkpoint. Wire it into recovery so that storage and metadata stay consistent across crashes.

## Prerequisites

- [x] Phase 2 complete (`phase-2-complete` tag).
- `MetadataSink` trait exists from Phase 2.10.

## Reading list

1. [`spec/07_metadata_graph/00_purpose.md`](../../spec/07_metadata_graph/00_purpose.md)
2. [`spec/07_metadata_graph/01_redb_choice.md`](../../spec/07_metadata_graph/01_redb_choice.md)
3. [`spec/07_metadata_graph/02_table_layout.md`](../../spec/07_metadata_graph/02_table_layout.md) — **all 13 tables.**
4. [`spec/07_metadata_graph/03_memory_table.md`](../../spec/07_metadata_graph/03_memory_table.md)
5. [`spec/07_metadata_graph/04_edge_storage.md`](../../spec/07_metadata_graph/04_edge_storage.md)
6. [`spec/07_metadata_graph/05_context_table.md`](../../spec/07_metadata_graph/05_context_table.md)
7. [`spec/07_metadata_graph/06_idempotency.md`](../../spec/07_metadata_graph/06_idempotency.md) — 24h TTL.
8. [`spec/07_metadata_graph/07_text_storage.md`](../../spec/07_metadata_graph/07_text_storage.md)
9. [`spec/07_metadata_graph/08_transactions.md`](../../spec/07_metadata_graph/08_transactions.md)

## Outputs

- `crates/brain-metadata` exports `MetadataDb`, table definitions, and an implementation of `MetadataSink` from Phase 2.
- Schema versioning header.
- Tag: `phase-3-complete`.

## Sub-tasks

### Task 3.1 — Schema versioning header ✅
**Reads:** `spec/02_data_model/09_schema_evolution.md`, `spec/07_metadata_graph/02_table_layout.md` §6.
**Writes:** `crates/brain-metadata/Cargo.toml` (real deps), `crates/brain-metadata/src/lib.rs` (real skeleton), `crates/brain-metadata/src/schema.rs` (new). Also bumped workspace `redb = "2"` → `"4"` (v4.1.0 picked up).
**What was built:**
- `CURRENT_SCHEMA_VERSION: u32 = 1`, `SCHEMA_META_TABLE: TableDefinition<&str, u32>` keyed by `"schema_version"`.
- `open_or_init_schema(&Database) -> Result<u32, SchemaError>`. Fresh DB → writes v1. Same version → returns it. Older version → returns it (placeholder for v1.1+ migration registry). Newer version → `SchemaVersionTooNew`.
- **Single global version row instead of per-table versions.** Spec §07/02 §6 reads "each table has a format version"; we use one global row covering the whole metadata file. The 13 tables co-evolve from the same crate; per-table machinery (13× the open-time checks + migration registry entries) adds bookkeeping for no benefit at v1. Documented inline in the module doc.
- Tests gated `#[cfg(all(test, not(miri)))]` for consistency with Phase 2 (redb uses mmap internally).
**Done when:** [x] `__schema_meta` records `schema_version=1` and refuses to open mismatched versions — `future_version_refuses_to_open` covers the rejection path; `fresh_db_initializes_at_v1`, `reopen_reads_existing_version`, `idempotent_reinit_returns_same_version`, `table_present_but_row_missing_initializes_to_v1` cover the rest. 5 tests.

### Task 3.2 — Memory metadata table
**Reads:** `spec/07_metadata_graph/03_memory_table.md`
**Writes:** `crates/brain-metadata/src/tables/memory.rs`
**Done when:** Insert + get + scan-by-(agent, context) + delete pass round-trip tests.

### Task 3.3 — Agents and contexts tables
**Reads:** `spec/07_metadata_graph/05_context_table.md`
**Writes:** `crates/brain-metadata/src/tables/agent.rs`, `context.rs`
**Done when:** Both tables CRUD-tested.

### Task 3.4 — Edge storage
**Reads:** `spec/07_metadata_graph/04_edge_storage.md`
**Writes:** `crates/brain-metadata/src/tables/edge.rs`
**Done when:** LINK / UNLINK / list-edges-from / list-edges-to all work; symmetric edges stored both directions.

### Task 3.5 — Idempotency table with TTL
**Reads:** `spec/07_metadata_graph/06_idempotency.md`
**Writes:** `crates/brain-metadata/src/tables/idempotency.rs`
**Done when:** RequestId → cached response with insert-time; expiry sweep removes entries > 24h old.

### Task 3.6 — Text blob storage
**Reads:** `spec/07_metadata_graph/07_text_storage.md`
**Writes:** `crates/brain-metadata/src/tables/text.rs`
**Done when:** Memory's text field stored separately, fetched on demand. Optional compression per spec.

### Task 3.7 — Tombstone table
**Reads:** `spec/07_metadata_graph/02_table_layout.md`
**Writes:** `crates/brain-metadata/src/tables/tombstone.rs`
**Done when:** Tombstone insertion records `(memory_id, tombstoned_at, grace_until)`. Slot reclamation reads from this.

### Task 3.8 — Counters and statistics
**Reads:** `spec/07_metadata_graph/02_table_layout.md`
**Writes:** `crates/brain-metadata/src/tables/counters.rs`
**Done when:** Per-shard counters (memory count, edge count, etc.) reconcile from full scans.

### Task 3.9 — Checkpoint table
**Reads:** `spec/07_metadata_graph/02_table_layout.md`, `spec/05_storage_arena_wal/09_checkpointing.md`
**Writes:** `crates/brain-metadata/src/tables/checkpoint.rs`
**Done when:** `durable_lsn` persists across reopens.

### Task 3.10 — `MetadataDb` public type
**Reads:** `spec/07_metadata_graph/08_transactions.md`
**Writes:** `crates/brain-metadata/src/db.rs`
**Done when:** All tables accessible via `MetadataDb`. Read txns and write txns wrap redb's primitives. Single-writer-per-shard discipline enforced via `&mut self` on writes.

### Task 3.11 — `MetadataSink` impl for recovery
**Reads:** `spec/05_storage_arena_wal/08_recovery.md`
**Writes:** `crates/brain-metadata/src/sink.rs`
**Done when:** `impl MetadataSink for MetadataDb` consumes WAL records and updates tables idempotently. End-to-end recovery test (storage + metadata) passes.

### Task 3.12 — Cross-crate integration test
**Reads:** all of phases 2–3.
**Writes:** `crates/brain-metadata/tests/recovery_integration.rs`
**Done when:** Test that drives `Wal::append → MetadataDb` then crashes and recovers. Final state matches expected.

## Phase exit checklist

- [ ] All sub-tasks complete.
- [ ] `just verify` green.
- [ ] Recovery integration test passes 100 random-seed iterations.
- [ ] All 13 spec'd tables present (count `tables/*.rs`).
- [ ] Tag `phase-3-complete`.
