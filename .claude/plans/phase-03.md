# Phase 3 — Metadata + Graph (redb)

**Goal:** stand up the `brain-metadata` crate — a redb-backed metadata store with 13 tables, a `MetadataDb` public type, an idempotent `MetadataSink` implementation that recovery feeds, and an end-to-end integration test that drives `Wal::append → recover → MetadataDb` and verifies the post-recovery state.

**Classification:** moderate, with low surprise budget. Mostly mechanical (13 tables of straightforward CRUD) on top of redb (already in workspace deps). The interesting parts are (a) the key-encoding seam for `MemoryId`/`AgentId` etc., (b) the `MetadataSink` impl wiring redb's write-transaction model into Phase 2's recovery driver, and (c) the cross-crate integration test in 3.12 which is the phase exit gate.

**Phase doc:** `docs/phases/phase-03-metadata.md` — 12 sub-tasks (3.1–3.12), exit-checklist as written.

**Pre-existing infrastructure consumed:**
- Phase 2's `MetadataSink` / `MetadataSinkError` (`brain-storage::recovery`).
- Phase 2's `WalPayload` family (`brain-storage::wal::payload`).
- Phase 2's `Wal` / `recover` (for the 3.12 integration test).
- brain-core's `MemoryId`, `AgentId`, `ContextId`, `RequestId`, `TxnId`, `EdgeKind`, `MemoryKind`, `Salience`.
- redb `2.x` and rkyv `0.7` already pinned in `Cargo.toml`'s workspace deps.

---

## 1. Reading list (spec §07/00–§07/12)

Critical sections (read before starting sub-tasks they apply to):

| Spec file | Drives |
|---|---|
| `00_purpose.md` | overall philosophy |
| `01_redb_choice.md` | redb v2.x; pure-Rust; ACID single-writer + MVCC reads |
| `02_table_layout.md` (full table catalog) | every sub-task |
| `03_memory_table.md` | 3.2 |
| `04_edge_storage.md` | 3.4 (edges_out + edges_in duplication) |
| `05_context_table.md` | 3.3 |
| `06_idempotency.md` | 3.5 (24h TTL) |
| `07_text_storage.md` | 3.6 |
| `08_transactions.md` | 3.10 (single-writer-per-shard discipline) |
| `09_concurrency.md` | 3.10 |
| `10_failure_modes.md` | 3.11 |
| `11_open_questions.md` | flag any new spec ambiguities here |
| `02_data_model/09_schema_evolution.md` | 3.1 (schema versioning) |
| `05_storage_arena_wal/08_recovery.md` | 3.11 (sink semantics, already familiar from 2.10) |
| `05_storage_arena_wal/09_checkpointing.md` | 3.9 (checkpoint table, already familiar from 2.12) |

---

## 2. The 13 tables (spec §07/02 §1)

| # | Table | Key | Value | Sub-task |
|---|---|---|---|---|
| 1 | `memories` | `MemoryId` (16 B BE) | `MemoryMetadata` (rkyv) | 3.2 |
| 2 | `texts` | `MemoryId` | `Vec<u8>` UTF-8 | 3.6 |
| 3 | `edges_out` | `(MemoryId, EdgeKind, MemoryId)` | `EdgeData` | 3.4 |
| 4 | `edges_in` | `(MemoryId, EdgeKind, MemoryId)` | `EdgeData` | 3.4 |
| 5 | `contexts` | `ContextId` (u64) | `ContextMetadata` | 3.3 |
| 6 | `context_names` | `(AgentId, &str)` | `ContextId` | 3.3 |
| 7 | `agent_contexts` | `(AgentId, ContextId)` | `()` | 3.3 |
| 8 | `idempotency` | `RequestId` (16 B) | `IdempotencyEntry` | 3.5 |
| 9 | `agents` | `AgentId` (16 B) | `AgentMetadata` | 3.3 |
| 10 | `model_fingerprints` | `[u8; 16]` | `ModelInfo` | 3.2 (lives with memory; or 3.10) |
| 11 | `checkpoints` | `u64` | `CheckpointInfo` | 3.9 |
| 12 | `next_lsn` | `()` (singleton) | `u64` | 3.9 (or 3.11) |
| 13 | `slot_versions` | `u64` (slot_id) | `u32` | 3.7 (alongside tombstone) |

Plus a 14th internal table from 3.1: `__schema_meta` (key `()`, value `SchemaVersion`). Reserved-name; not in the spec catalog but needed for §07/02 §6 schema evolution.

`tombstones` table (spec §07/02 — actually inferred from §07/02 §1's intent + sub-task 3.7's brief): not explicitly named in §07/02 §1's table list. Will surface when reading 3.7's spec section. Cross-check before implementing — may collapse into the `memories` table as a flag rather than a separate table. (Possible spec ambiguity #1.)

Counters (sub-task 3.8): also not in §07/02 §1's catalog. Likely a singleton-like table with `()` key and a `Counters` struct value. Cross-check.

---

## 3. Cross-cutting design decisions

### 3.1 Key encoding via byte arrays, not orphan-impls on brain-core types

`redb::Key` trait must be implemented for every key type. `MemoryId`, `AgentId`, etc. live in `brain-core` (no redb dep). Implementing `redb::Key` for them in `brain-metadata` would violate orphan rules (we don't own either crate fully).

**Approach:** use `[u8; N]` byte representations as redb keys, with conversion via `to_be_bytes`/`from_be_bytes` (or equivalent) at the API boundary.

- `MemoryId` → `[u8; 16]` (BE, per spec §02/03 §2.2; brain-core already exposes `to_be_bytes`/`from_be_bytes`).
- `AgentId` → `[u8; 16]` (UUID raw bytes; brain-core has `From<[u8; 16]>`).
- `RequestId` → same.
- `TxnId` → same.
- `ContextId` → `u64::to_be_bytes` (8 bytes).
- `EdgeKind` → `u8` discriminant.

The composite keys (`(MemoryId, EdgeKind, MemoryId)`, `(AgentId, &str)`, `(AgentId, ContextId)`) are constructed by concatenating byte arrays. redb supports `(K1, K2, K3)` tuple keys natively when each component implements `Key`; we'll lean on that where possible, falling back to manual concatenation when the spec's lexicographic order requirements dictate.

**The user-facing API** of each table module accepts brain-core types (e.g., `MemoryTable::insert(&mut self, id: MemoryId, ...)`), converts to bytes internally. This isolates redb's key encoding from the rest of the codebase.

### 3.2 Value encoding: rkyv 0.7

Spec §07/02 §5 mandates rkyv. We already pin rkyv 0.7 in workspace deps (used by brain-protocol). rkyv's `Archive` + `Serialize` + `Deserialize` derives are stable for our value types.

`redb::Value` requires a trait impl. We'll write a thin `RkyvValue<T>` wrapper or implement `Value` directly on each `*Metadata` struct that derives the rkyv traits. Concrete approach decided per-sub-task.

**Risk:** rkyv 0.7's serialization API changed in subtle ways from 0.6. Each `Archive` derive will need verification. If we hit friction, fall back to `bincode` (already in deps via... actually no, it isn't; not pulling in a new dep). Keep rkyv 0.7.

### 3.3 The `MetadataSink` impl

`brain-metadata` adds `brain-storage` as a path dep (for `MetadataSink`, `MetadataSinkError`, `WalPayload`). `impl MetadataSink for MetadataDb` lives in 3.11.

The implementation's `apply(lsn, payload)` opens a redb write transaction, dispatches on the payload variant, updates the relevant tables, commits. Strict idempotency comes from:
- `MemoryId` keys: re-inserting the same row is a no-op.
- `RequestId` keys: same.
- For LSN-keyed metadata: the `next_lsn` singleton advances monotonically; `apply` sets `next_lsn = max(current, lsn + 1)`.

On `CheckpointEnd`, `apply` updates the `checkpoints` table AND advances the singleton `durable_lsn` cache (so `MetadataSink::durable_lsn()` returns it without a fresh read).

### 3.4 Single-writer-per-shard discipline (spec §07/08 §3)

redb itself is single-writer (one `begin_write` at a time blocks others). The Phase 2 `Wal` already enforces `&mut self` for writes; `MetadataDb` will mirror that: `&mut self` for write methods, `&self` for read methods. The borrow checker enforces the discipline at the type level.

### 3.5 Cross-crate dep graph

```
brain-metadata
├── brain-core (path dep) ───── for MemoryId, AgentId, ContextId, etc.
├── brain-storage (path dep) ── for MetadataSink trait, WalPayload
├── redb 2.x (workspace dep)
├── rkyv 0.7 (workspace dep, "validation" feature)
├── thiserror, bytemuck (workspace deps)
└── (dev-dep) tempfile, proptest, ...
```

`brain-metadata` does **not** depend on libc directly — redb owns its own syscalls. The Linux-only gate from `brain-storage` doesn't propagate; `brain-metadata` compiles on macOS for tooling. (Confirm before adding the Linux gate.)

### 3.6 Phase-2 lessons that carry forward

- **Spec ambiguities are common.** Phase 2 had 5+ "spec literal vs natural reading" issues. Expect 2–3 per phase. Each plan file should have a §3 surfacing them.
- **Deviation log.** `docs/spec-deviations.md` is the durable record. New entries get SD-3.x-N IDs.
- **Plan-first.** Each sub-task gets a plan file in `.claude/plans/phase-03-task-NN.md`; user approval required before implementing.
- **Verify in dev container.** Same gate as Phase 2: `cargo fmt`, `cargo clippy -D warnings`, `cargo test`, `./scripts/check-skills.sh`, and `cargo doc -p brain-metadata` for the phase-exit.

---

## 4. Sub-task ordering and parallelism

The sub-tasks have a natural linear order, but several are independent and could parallelize if the user wanted to spawn agents. Per current workflow (plan-first, one task at a time, user approves each), we'll go linear:

| Order | Sub-task | Depends on | Notes |
|---|---|---|---|
| 1 | 3.1 (schema) | — | Foundational: every table consults `__schema_meta` on open. |
| 2 | 3.2 (memories) | 3.1 | First "real" table; sets the rkyv + key-encoding patterns. |
| 3 | 3.3 (agents + contexts + 2 indexes) | 3.1 | 3 tables; CRUD-tested. |
| 4 | 3.4 (edges) | 3.2 (uses MemoryId) | edges_out + edges_in pair; symmetric-edge mirroring. |
| 5 | 3.5 (idempotency) | 3.1 | 24h TTL prune primitive (worker calls it later). |
| 6 | 3.6 (text blobs) | 3.2 (separate table keyed by MemoryId) | Optional compression flagged in spec. |
| 7 | 3.7 (tombstones + slot_versions) | 3.2 | Two related tables co-located. |
| 8 | 3.8 (counters) | — | Singleton-ish; reconciles from full scan. |
| 9 | 3.9 (checkpoint table + next_lsn) | 3.1 | Wires to Phase 2's CheckpointEnd records. |
| 10 | 3.10 (`MetadataDb`) | all of 3.1–3.9 | Public wrapper; opens all tables. |
| 11 | 3.11 (`MetadataSink` impl) | 3.10 | Phase 2's seam fills in. |
| 12 | 3.12 (integration test) | 3.11 | Phase exit gate. |

Sub-tasks 3.5, 3.6, 3.7, 3.8 are mutually independent and could run in any order after 3.4. Keeping linear for plan-flow simplicity.

---

## 5. Risks and ambiguity buckets to watch

- **Tombstones table existence.** §07/02 §1's catalog doesn't list a separate `tombstones` table, but sub-task 3.7's brief implies one. Likely interpretation: tombstone state is a flag on `MemoryMetadata` (in `memories`) plus a "scheduled-for-reclaim" entry in a small auxiliary table. Surface in 3.7's plan.
- **Counters table existence.** Same situation. Likely a singleton or per-agent rollup.
- **rkyv `Archive` ergonomics for variable-length fields.** `MemoryMetadata` carries a `Vec<EdgeId>` or similar — rkyv handles, but the access pattern (deref vs deserialize) needs a one-time decision. Picked in 3.2's plan.
- **`(AgentId, &str)` composite key.** The `&str` part is variable-length. redb supports it; we use the natural `&str` encoding (length-prefixed UTF-8). Compose via redb's `Key for (K1, K2)` blanket impl.
- **Symmetric edges (spec §07/04 §3).** `EdgeKind::SimilarTo` and `EdgeKind::Contradicts` are stored both directions in `edges_out` *and* `edges_in`. Need careful handling to avoid double-counting.
- **Idempotency TTL clock source.** Real-wall-clock vs WAL-derived timestamp. Spec §07/06 says wall-clock at insert time. Note in 3.5.
- **Schema versioning interaction with existing files.** What happens if we open a v0 file with code expecting v1? Spec says refuse to open. Tested.
- **Phase 9 picks up the data path.** `MetadataDb` is the seam Phase 9 will plug into for the server's request-handling. Don't over-design now; deliver exactly what 3.12's integration test needs.

---

## 6. Cargo.toml for brain-metadata

A first draft:

```toml
[package]
name = "brain-metadata"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
description = "redb-backed metadata store for Brain."

[dependencies]
brain-core = { path = "../brain-core" }
brain-storage = { path = "../brain-storage" }
redb.workspace = true
rkyv.workspace = true
thiserror.workspace = true
bytemuck.workspace = true
tracing.workspace = true

[dev-dependencies]
proptest.workspace = true
tempfile.workspace = true
```

`brain-storage` as a regular dep (not dev) is what makes 3.11's `impl MetadataSink for MetadataDb` reachable.

---

## 7. Phase-exit gate (mirrors phase doc)

- [ ] All 12 sub-tasks complete with `phase-03-task-NN.md` plans approved.
- [ ] `just verify` green inside the dev container.
- [ ] Recovery integration test (3.12) passes 100 random-seed iterations.
- [ ] All 13 spec'd tables present (count `tables/*.rs`).
- [ ] `cargo doc -p brain-metadata` warnings-clean.
- [ ] `just miri` still passes (we're not adding syscalls; should be a no-op for the miri scope).
- [ ] Tagged `phase-3-complete` after merge to main.

---

## 8. Workflow note

This is a phase-level plan (overview + cross-cutting). Each sub-task gets its own plan file (`.claude/plans/phase-03-task-NN.md`) with detailed scope, spec quotes, ambiguities, and tests — same shape as Phase 2's plans. **User approval required per sub-task plan before implementing.**

The current commit ends Phase 2; this is the entry-point plan for Phase 3. After approval, I'll start with sub-task 3.1.

---

PLAN READY: see `.claude/plans/phase-03.md` — confirm to proceed to sub-task 3.1 planning.
