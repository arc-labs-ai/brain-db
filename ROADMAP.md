# Roadmap

High-level implementation plan. Each phase is a step toward a working v1. Detailed sub-task breakdowns live in [`docs/phases/`](docs/phases/) — this file is the index.

For autonomous-mode operating rules, see [`AUTONOMY.md`](AUTONOMY.md).

---

## Phase 0 — Workspace skeleton ✓ provided by starter

**Status:** Scaffolded by the starter template. Verify before moving on.

**Provided:**

- `Cargo.toml` workspace with shared dependency table.
- 12 stub crates under `crates/`.
- `rustfmt.toml`, `clippy.toml`, `rust-toolchain.toml`.
- `.github/workflows/ci.yml` running build, test, clippy, fmt, miri, audit.
- `.gitignore`, `justfile`, `config/dev.toml`.
- `fuzz/` directory.
- `.claude/` with settings, hooks, slash commands, subagents.

**Verify (before starting Phase 1):**

- [ ] `just verify` is green.
- [ ] CI is green on first push.
- [ ] Tag the latest commit: `git tag phase-0-complete`.

**No detailed phase doc** — the work is just verification.

---

## Phase 1 — Wire Protocol & Core Types

**One-line:** Frame format, opcode codecs, fuzz target.

**Detailed plan:** [`docs/phases/phase-01-wire-protocol.md`](docs/phases/phase-01-wire-protocol.md)

**Crates touched:** `brain-core`, `brain-protocol`.

**Sub-tasks:** 11.

**Exit:** every opcode round-trips; fuzz finds no panics; tag `phase-1-complete`.

---

## Phase 2 — Storage: Arena + WAL + Recovery

**One-line:** Memory-mapped vector arena, write-ahead log with group commit, crash recovery.

**Detailed plan:** [`docs/phases/phase-02-storage.md`](docs/phases/phase-02-storage.md)

**Crates touched:** `brain-storage`.

**Sub-tasks:** 12.

**Exit:** 1000-iteration random-kill recovery test passes; miri clean; tag `phase-2-complete`.

---

## Phase 3 — Metadata + Graph (redb)

**One-line:** All 13 redb tables; idempotency; recovery integration with Phase 2.

**Detailed plan:** [`docs/phases/phase-03-metadata.md`](docs/phases/phase-03-metadata.md)

**Crates touched:** `brain-metadata`.

**Sub-tasks:** 12.

**Exit:** all tables present and tested; cross-crate recovery test passes; tag `phase-3-complete`.

---

## Phase 4 — ANN Index (HNSW)

**One-line:** Wrap `hnsw_rs` with the spec's parameters and lifecycle.

**Detailed plan:** [`docs/phases/phase-04-ann-index.md`](docs/phases/phase-04-ann-index.md)

**Crates touched:** `brain-index`.

**Sub-tasks:** 8.

**Exit:** recall@10 ≥ 0.95 at 100K vectors; persistence round-trip works; tag `phase-4-complete`.

---

## Phase 5 — Embedding Layer

**One-line:** BGE-small via candle, batching, caching, determinism.

**Detailed plan:** [`docs/phases/phase-05-embedding.md`](docs/phases/phase-05-embedding.md)

**Crates touched:** `brain-embed`.

**Sub-tasks:** 7.

**Exit:** ≥ 1K texts/sec; deterministic; tag `phase-5-complete`.

---

## Phase 6 — Query Planner & Executor

**One-line:** Logical plan tree, cost model, pull-based executor.

**Detailed plan:** [`docs/phases/phase-06-planner.md`](docs/phases/phase-06-planner.md)

**Crates touched:** `brain-planner`.

**Sub-tasks:** 8.

**Exit:** every operation type has a planner test; tag `phase-6-complete`.

---

## Phase 7 — Cognitive Operations

**One-line:** ENCODE, RECALL, PLAN, REASON, FORGET on top of the planner; idempotency.

**Detailed plan:** [`docs/phases/phase-07-operations.md`](docs/phases/phase-07-operations.md)

**Crates touched:** `brain-ops`.

**Sub-tasks:** 11.

**Exit:** correctness suite from spec §16/01 fully green; tag `phase-7-complete`.

---

## Phase 8 — Background Workers

**One-line:** All 12 workers running cooperatively.

**Detailed plan:** [`docs/phases/phase-08-workers.md`](docs/phases/phase-08-workers.md)

**Crates touched:** `brain-workers`.

**Sub-tasks:** 14.

**Exit:** each worker tested; performance regression test green; tag `phase-8-complete`.

---

## Phase 9 — `brain-server`: end-to-end wire-up

**One-line:** A runnable substrate. Tokio connection layer + Glommio shards.

**Detailed plan:** [`docs/phases/phase-09-server.md`](docs/phases/phase-09-server.md)

**Crates touched:** `brain-server`.

**Sub-tasks:** 10.

**Exit:** E2E smoke test passes 100 iterations; tag `phase-9-complete`.

---

## Phase 10 — Rust SDK & CLI

**One-line:** Polished `Client` + `brain-cli` covering every spec'd admin command.

**Detailed plan:** [`docs/phases/phase-10-sdk-cli.md`](docs/phases/phase-10-sdk-cli.md)

**Crates touched:** `brain-sdk-rust`, `brain-cli`.

**Sub-tasks:** 13.

**Exit:** SDK drives every operation; CLI covers every command; tag `phase-10-complete`.

---

## Phase 11 — Observability, Benchmarks, Acceptance

**One-line:** Production-ready: metrics, logs, tracing, dashboards, alerts, benchmarks, chaos suite, the v1 gate.

**Detailed plan:** [`docs/phases/phase-11-observability.md`](docs/phases/phase-11-observability.md)

**Crates touched:** all (instrumentation), plus `dashboards/`, `alerts/`, `benches/`, `tests/chaos/`.

**Sub-tasks:** 12.

**Exit:** all 10 acceptance gates pass; soak test recorded; tag `phase-11-complete` and `v1.0.0`.

---

## Strict ordering

Phase N+1 doesn't start before Phase N is exited and tagged. The dependencies aren't soft preferences — they're real:

- Phase 1's `Frame` is consumed by Phase 9's connection layer.
- Phase 2's `MetadataSink` trait is implemented by Phase 3.
- Phase 4's `HnswIndex` requires Phase 2's slot reads and Phase 3's tombstone state.
- Phase 7 wires Phases 2-6 together.
- Phase 9 wires everything.
- Phase 11 instruments everything.

Skipping ahead means stubbing types you'll have to revisit. Don't.

## How to track progress

- Each completed sub-task is a commit (per [`AUTONOMY.md`](AUTONOMY.md) §5).
- Each completed phase is a tag (`phase-N-complete`).
- Each phase doc has its own `[ ] / [x]` checkboxes per sub-task.
- `git log --oneline | grep "^[a-f0-9]* [0-9]*\."` shows all completed sub-tasks.
- `/status` (slash command) summarizes current position.

## Known limitations of v1

Documented up front so the scope is honest:

- **Single-node only.** Multi-node clustering is v2.
- **No replication.** Backups (snapshots) only. v2.
- **Rust SDK only.** Python/TypeScript/Go are v1.x.
- **Linux only.** Glommio + io_uring don't run elsewhere.

These aren't bugs — they're scope boundaries. Don't accidentally implement them.
