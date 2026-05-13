# Seed prompt — sub-task 9.7 (fresh session)

Paste this as the first message in a new Claude Code session. It's self-contained: it doesn't rely on prior session context.

---

## Context

We're on Brain's `feature/brain-server` branch, mid-Phase 9. Phase 9 turns the previously-isolated crates into a runnable substrate (Glommio shards + Tokio connection layer). Sub-tasks 9.1–9.6 + 9.6a have shipped:

| ID | Title | Commit |
| -- | ----- | ------ |
| 9.1 | Config loading | `e65e263` |
| 9.2 | Tokio↔Glommio audit (`docs/phases/phase-09-glommio-port.md`) | `48767bd` |
| 9.3 | Routing (pure fn) | `8621848` |
| 9.4 | Shard scaffold (Glommio LocalExecutor + flume channel) | `7db0d4d` |
| 9.5 | Real arena hookup | `f91d6b9` |
| 9.6a | WAL io_uring port (brain-storage internal) | `ff0a2df` |
| 9.6 | Real WAL hookup (per-shard) | `312956d` |
| chore | devcontainer single-source config + 9.7 plan revision | `bf80fdd`, `60cbbfe` |

Verify clean: `just docker-verify` → 994 tests across 63 suites green.

You're picking up sub-task **9.7 — Per-shard OpsContext + WorkerScheduler**. The plan lives at `.claude/plans/phase-09-task-07.md`. **Read it end-to-end before doing anything.**

## What 9.7 actually is

Per the audit (`docs/phases/phase-09-glommio-port.md` §4 + §5.3 + §6 + §8.2 + §8.5), 9.7 is a **single atomic cascade** of five coupled changes. We tried to split it into 9.7a/b/c in the previous session — the build broke immediately. The split is not viable; the plan file is now correct on this.

The cascade, in dependency order:

1. **`brain-planner::WriterHandle`** drops `Send + Sync` from the trait + `+ Send` from every future return (6 methods).
2. **`brain-planner::ExecutorContext`** drops its `Send + Sync` compile-time assertion (would otherwise fail because `Arc<dyn WriterHandle>` is no longer Send).
3. **`brain-ops::RealWriterHandle`** + the `NopWriter` test fixture in `brain-ops/src/lib.rs` drop `+ Send` from 6 future returns each.
4. **`brain-ops::OpsContext`** swaps interior `Arc<TxnStore/EventBus/SubscriptionRegistry/AccessBuffer>` to `Rc<...>`. Drop the 5 `fn require<T: Send + Sync>()` compile-time assertion sites (lib.rs, writer.rs, context.rs, subscribe.rs, access_buffer.rs).
5. **`brain-workers::Worker` trait** drops `Send + Sync + 'static` and `+ Send` from `run_cycle`'s return.
6. **`brain-workers::WorkerContext`** swaps `Arc<OpsContext>` → `Rc<OpsContext>`; `watch::Receiver<bool>` → `Rc<Cell<bool>>` shutdown signal (audit §8.2).
7. **`brain-workers::WorkerScheduler`** ports: `tokio::spawn` → `glommio::spawn_local`; `tokio::time::sleep` → `glommio::timer::sleep`; `tokio::time::timeout` → manual `futures_lite::or` race; `tokio::select!` → manual await + flag-check; `JoinHandle` → `glommio::Task<()>`.
8. **Per-worker `tokio::task::yield_now`** sites (6 files) → `glommio::executor().yield_if_needed()`.
9. **Drop 14 `fn require<T: Send + Sync>()` assertions** in brain-workers (one per worker module).
10. **`brain-server` wire-up**: construct per-shard `OpsContext` inside the executor (after the WAL is open from 9.6). Construct `WorkerScheduler` per shard; register every Phase-8 worker. Replace `InMemoryMetadataSink` (9.6's stand-in) with the redb-backed `MetadataDb` sink from brain-metadata. Plus `brain-server/Cargo.toml` gains target-gated deps on brain-metadata + brain-index + brain-embed + brain-planner + brain-ops + brain-workers.

Estimated total: ~2300 LOC across 4 crates.

## Execution discipline

Do not edit cargo and run `just docker-verify` repeatedly hoping for green — the cascade is too wide for that. Instead:

1. **Edit crate-by-crate in dependency order**: brain-planner → brain-ops → brain-workers → brain-server. After each crate, run `just docker -- cargo check -p <crate>` (NOT verify — that's the workspace).
2. **Tests last.** Migrate `#[tokio::test]` worker tests after the source compiles. Each worker has ~10–30 tests; mechanical wrapping with a `glommio_run` helper.
3. **Full `just docker-verify` is the final gate.** Don't run it mid-port — it builds the workspace and you'll lose the focused error stream.

The `just docker-verify` command:
- `cargo fmt --all -- --check`
- `cargo test --workspace` (excludes benches — criterion's 100k HNSW build hangs on ARM Linux)
- `cargo clippy --workspace --all-targets -- -D warnings`

Container is persistent (`devcontainer up` reuses); incremental builds reuse target/ via a named volume. `CARGO_BUILD_JOBS=2` is set in `.devcontainer/devcontainer.json` to avoid OOM-kill of the linker.

## Pinned operating rules

Two non-negotiables (from MEMORY.md):

1. **Plan-first workflow.** Even though the plan exists, re-read it. Surface any deviation or scope change before writing code. No "trivial" skips.
2. **No `Co-Authored-By: Claude` trailer.** Commits show only Niraj's authorship.

If you discover during impl that the plan is wrong (as I did with the split), STOP and surface — don't power through.

## Suggested first moves

1. Read `.claude/plans/phase-09-task-07.md` (revised plan).
2. Read `docs/phases/phase-09-glommio-port.md` §4 + §6 + §8 (audit cascade detail).
3. Read `crates/brain-planner/src/executor/writer.rs` (the source of the cascade).
4. Read `crates/brain-workers/src/scheduler.rs` (the heaviest port — needs careful translation of the 5-phase committer-style loop to Glommio primitives; 9.6a's `crates/brain-storage/src/wal/group_commit.rs` is a working reference for the Glommio task + flume + timer pattern).
5. Read `crates/brain-server/src/shard.rs` (where the per-shard OpsContext + scheduler land at the end).
6. Run `just docker-verify` once to confirm baseline is green at `60cbbfe`.

Then proceed with the cascade.

## Risks specific to 9.7

| Risk | Mitigation |
| ---- | ---------- |
| `WorkerScheduler`'s 5-phase loop (receive first → gather → drain → flush → exit) is intricate; porting from `crossbeam::select!` to Glommio primitives is fiddly | 9.6a's `wal/group_commit.rs` is the working reference. Same pattern. |
| `WorkerContext` shutdown via `Rc<Cell<bool>>` breaks every worker test that constructs a `watch::channel` | All 14 worker test files need migration. Wrap test bodies with a `glommio_run` helper (sister to brain-storage's). |
| `brain-server::shard::spawn_shard` becomes large (recovery → arena → wal → metadata → ops → scheduler → main loop) | Split into helpers inside shard.rs. Don't introduce new modules until 9.8+. |
| `MetadataDb` (redb) is `Send + Sync` but per-shard usage means `Rc<MetadataDb>` is appropriate — but redb itself doesn't require !Send | Keep `Arc<MetadataDb>` for now (matches the existing `SharedMetadataDb` pattern); the Sync requirement is for ExecutorContext's pre-existing shape. |
| The redb-backed `MetadataSink` adapter may not exist yet in brain-metadata — confirm at impl time | Check `crates/brain-metadata/src/sink.rs`. If missing, scope expands. |

## Done criteria

- [ ] `WriterHandle` trait has no Send + Sync; no `+ Send` on future returns.
- [ ] `OpsContext` is `!Send` (compile-time check or just by construction via Rc).
- [ ] No `tokio::*` in `crates/brain-workers/src/` (post-port grep).
- [ ] Per-shard `WorkerScheduler` runs every Phase-8 worker in a Glommio executor.
- [ ] `Wal` recovery applies to `MetadataDb` (not `InMemoryMetadataSink`).
- [ ] `just docker-verify` green.
- [ ] Audit doc §12 status rows for §4 / §5.3 / §6.x / §8.2 / §8.5 flipped to **done**.
- [ ] Phase doc 9.7 marked `[x]`.
- [ ] Single commit on `feature/brain-server`. Subject: `feat(brain-server): per-shard OpsContext + worker scheduler (sub-task 9.7)`.

## What 9.7 explicitly defers

- **Cross-shard SUBSCRIBE fan-out** (audit §8.1). 9.11. EventBus stays per-shard.
- **OpenAI/Ollama Summarizer adapter.** 9.15. `DisabledSummarizer` (from Phase 8) stays the wired impl.
- **PLAN/REASON tombstone filter** (spec §16/01 §12 carry-over). 9.16.
- **ArcSwap + crossbeam-epoch publication of HNSW.** 9.12.
- **Connection layer (Tokio + TLS).** 9.9.
- **Frame dispatcher (Tokio↔Glommio boundary).** 9.10.

These will not unblock by working harder on 9.7. Don't scope-creep.

---

*Read the plan. Read the audit. Then start with `crates/brain-planner/src/executor/writer.rs`.*
