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

### Task 3.2 — Memory metadata table ✅
**Reads:** `spec/07_metadata_graph/03_memory_table.md`
**Writes:** `crates/brain-metadata/src/tables/memory.rs` (and `tables/mod.rs` + `lib.rs` `pub mod tables;`).

**What was built:**
- `MemoryMetadata` — 20-field struct (~140 B/row) per spec §07/03 §1. Stores brain-core types as byte representations (`[u8; 16]` for `MemoryId`/`AgentId`, `u64` for `ContextId`, `u8` for `MemoryKind`); typed getters convert at the API boundary.
- `MEMORIES_TABLE: TableDefinition<[u8; 16], MemoryMetadata>` keyed by `MemoryId::to_be_bytes()`.
- `redb::Value` impl backed by rkyv 0.7 with `#[archive(check_bytes)]` validation. Deserialize-on-read (owned `MemoryMetadata`); zero-copy view deferred to a profiling-driven follow-up.
- `flags` module — `ACTIVE`, `HARD_FORGOTTEN`, `PINNED`, `STALE`, `RESERVED_MASK` per §07/03 §2.7.
- `MemoryKind` ↔ `u8` mapping duplicated locally from `brain_storage::wal::payload`; note to promote to brain-core if a third caller appears.
- `MemoryMetadata::new_active(...)` constructor; `is_active`/`is_pinned`/etc. flag accessors; `set_flag(mask, on)`.

**Patterns set for the rest of Phase 3:**
- Byte-array key encoding (`[u8; 16]` for ID types).
- rkyv-backed `redb::Value` via deserialize-on-read with `expect`-on-corrupt.
- `type_name` includes `::v1` for type-confused mismatch detection.
- Module per table under `tables/`.

**Done when:** [x] Insert/get/scan-by-(agent, context)/delete round-trip tests pass. Plus update, missing-key, Option round-trip, flag manipulation, brain-core type round-trip, encoding stability. **9 tests.**

### Task 3.3 — Agents and contexts tables ✅
**Reads:** `spec/07_metadata_graph/05_context_table.md`, `02_table_layout.md` §12.
**Writes:** `crates/brain-metadata/src/tables/agent.rs`, `tables/context.rs`.

**What was built (4 of the 13 tables):**
- `AGENTS_TABLE: TableDefinition<[u8; 16], AgentMetadata>` — `AgentMetadata` carries `display_name`, `created_at`, `last_active_at`, denormalized `memory_count`/`context_count`. v1 defers "configuration overrides" from spec §07/02 §12 (field-addition follow-up via spec §02/09 §2).
- `CONTEXTS_TABLE: TableDefinition<u64, ContextMetadata>` — `ContextMetadata` per spec §07/05 §2.1 (8 fields including `Vec<String> tags`).
- `CONTEXT_NAMES_TABLE: TableDefinition<(&[u8; 16], &str), u64>` — name index for agent-scoped lookup.
- `AGENT_CONTEXTS_TABLE: TableDefinition<([u8; 16], u64), ()>` — agent→[context_ids] membership, supports prefix range scan.

**Composite keys via redb v4's tuple `Key` impl.** Worked out of the box — no fallback to manual byte concatenation needed. Fixed-width agent_id prefix means range scans by agent are clean prefix scans.

**Helper constants:** `RESERVED_NAME_PREFIX = "_"` and `DEFAULT_CONTEXT_NAME = "_default"` per spec §07/05 §6. Writer-task (Phase 9) enforces the reservation against client input; storage doesn't validate.

**Done when:** [x] Both tables CRUD-tested. 10 tests covering agent insert/update/delete/typed-getter, context insert by ID, name-index lookup with hit/miss, agent-prefix range scan, cross-agent name isolation (spec §07/05 §13), and `Vec<String>` + `Option<String>` rkyv round-trip.

### Task 3.4 — Edge storage ✅
**Reads:** `spec/07_metadata_graph/04_edge_storage.md`, `spec/02_data_model/06_edges.md`.
**Writes:** `crates/brain-metadata/src/tables/edge.rs`.

**What was built (2 more tables — 7 of 13):**
- `EDGES_OUT_TABLE: TableDefinition<EdgeKey, EdgeData>` keyed by `(source, kind, target)`.
- `EDGES_IN_TABLE: TableDefinition<EdgeKey, EdgeData>` keyed by `(target, kind, source)`.
- `EdgeData` (rkyv: weight, origin, derived_by, created_at, annotation).
- `link` / `unlink` / `list_edges_from` / `list_edges_to` helpers — take pre-opened table handles to avoid redb's "table already open" error.
- **Symmetric edge handling.** `SimilarTo` and `Contradicts` write 4 rows (direct + reverse-index + mirror + mirror-reverse). Self-symmetric edges skip the mirror (would be redundant).
- Byte-mapping constant modules: `origin::{EXPLICIT, AUTO_DERIVED}` and `derived_by::{CLIENT, CONSOLIDATION_WORKER, SIMILARITY_WORKER}`.

**Done when:** [x] LINK / UNLINK / list-edges-from / list-edges-to all work; symmetric edges stored both directions. 12 tests covering EdgeData round-trip, asymmetric and symmetric link/unlink, self-symmetric (2 rows not 4), range queries with and without kind filter on both tables, list-edges-to picking up symmetric mirror, and update-via-relink.

**Mid-flight bug found and fixed across all tables.** rkyv 0.7's `from_bytes` requires 8-byte-aligned input; redb returns bytes at arbitrary alignment. `MemoryMetadata`'s 3.2 tests happened to pass by luck of alignment; `EdgeData` failed deterministically with `Underaligned { expected_align: 8, actual_align: 1 }`. Fix: copy into `rkyv::AlignedVec` before `from_bytes` in each `redb::Value` impl. Applied to `MemoryMetadata`, `AgentMetadata`, `ContextMetadata`, and `EdgeData` (preemptive).

### Task 3.5 — Idempotency table with TTL ✅
**Reads:** `spec/07_metadata_graph/06_idempotency.md`
**Writes:** `crates/brain-metadata/src/tables/idempotency.rs` (new), `crates/brain-metadata/src/tables/mod.rs` (add `pub mod idempotency;`), `docs/spec-deviations.md` (SD-3.5-1).

**What was built (1 more table — 8 of 13):**
- `IDEMPOTENCY_TABLE: TableDefinition<[u8; 16], IdempotencyEntry>` — keyed by `RequestId::to_be_bytes()` (16-byte UUIDv7).
- `IdempotencyEntry` — rkyv-derived: `response_kind: u8`, `memory_id_bytes: Option<[u8; 16]>`, `response_payload: Vec<u8>`, `request_hash: [u8; 32]`, `created_at_unix_nanos: u64`. The fifth field (`request_hash`) is **SD-3.5-1** — needed for spec §5's conflict detection in O(1) byte compare; canonical-request bytes aren't reversible from the response payload.
- `response_kind` byte module: `UNKNOWN=0, ENCODE=1, FORGET=2, LINK=3, UNLINK=4, UPDATE_KIND=5, UPDATE_CONTEXT=6, TXN_BEGIN=7, TXN_COMMIT=8` per spec §17. Same 4th-occurrence-of-u8-mapping pattern; still deferred to the brain-core promotion bundle.
- `DEFAULT_TTL_NANOS = 24h` per spec §6.
- `prune_expired(table, now_unix_nanos, ttl_nanos) -> Result<u64, StorageError>` — pure function; collects victims via `iter()`, then `remove`s. Saturating arithmetic on `created_at + ttl_nanos` so `u64::MAX` doesn't wrap.
- `IdempotencyEntry::memory_id()` typed getter at the API boundary.

**Done when:** [x] 11 tests covering CRUD, missing-key, update, `Option<MemoryId>` round-trip, 256-byte payload round-trip, `request_hash` byte compare, prune-removes-old, prune-keeps-fresh, prune-mixed (3-old + 2-fresh), prune-saturating (entry at `u64::MAX`), and `type_name` v1-marker guard. Total in brain-metadata: 47 tests.

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
