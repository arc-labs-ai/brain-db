# Contributing to Brain

Thanks for considering a contribution. Brain has strong design
constraints — read this end-to-end before you start.

## TL;DR

1. The [spec](spec/) is authoritative. Code disagreements get
   fixed in the code, not the spec. Spec changes go through
   the maintainer.
2. Every sub-task: **read the spec → write a plan in
   `.claude/plans/<short-name>.md` → wait for approval →
   implement → verify → commit**.
3. No `unwrap()` outside tests. Use `expect("invariant:
   <reason>")` for unreachable.
4. Run `just verify` before opening a PR. On macOS (or any
   non-Linux host) use `just docker-verify` — the dev container
   is the supported path; the server can't build natively
   (glommio / `io_uring`).

See [`AUTONOMY.md`](AUTONOMY.md) for the full operating
contract Brain's autonomous mode runs under — much of it
applies to human contributors too.

## Architecture in one paragraph

Linux server. Connection layer (Tokio) accepts TCP; each
request dispatches to one of N **shards**. Each shard runs a
**Glommio** executor (thread-per-core, io_uring) and owns its
data: a memory-mapped **arena** for vectors, a **WAL** with
O_DIRECT + `pwritev2(RWF_DSYNC)` group commit, a **redb**
B-tree for metadata, an **HNSW** index in RAM.
Single-writer-per-shard, lock-free reads via
**ArcSwap** + **crossbeam-epoch**. When a schema is declared,
the same shard additionally owns entity / statement HNSWs,
two **tantivy** indexes, an LLM extractor cache, and runs the
three-tier extractor pipeline.

## Where to start reading

- [`README.md`](README.md) — what Brain is + capability tour.
- [`spec/00_overview/`](spec/00_overview/00_index.md) — design
  start.
- [`ROADMAP.md`](ROADMAP.md) — phase index.
- [`CLAUDE.md`](CLAUDE.md) — operating rules + invariants.
- [`AUTONOMY.md`](AUTONOMY.md) — contributor workflow + commit
  conventions.

## Core invariants — DO NOT violate

Code that violates these is wrong regardless of test results:

1. **WAL-before-acknowledge.** No operation returns success
   until its WAL record is fsynced.
2. **Single writer per shard.** No locks needed; the discipline
   enforces it.
3. **CRC everywhere.** Every WAL record + arena slot.
4. **Slot version on `MemoryId`.** Stale references →
   `NotFound`.
5. **Idempotency by `RequestId`.** 24h TTL. Same params →
   cached response. Different params → `Conflict`.
6. **Tombstone grace before reclamation.** Default 7 days. Hard
   FORGET zeroes immediately.
7. **No silent corruption.** Fail-stop and alert.

## Anti-patterns

- Don't add Tokio inside a shard. Shards use Glommio.
- Don't hold a lock across `.await`.
- Don't allocate in the hot path (encode/recall serving).
- Don't add `Send + Sync` to per-shard types.
- Don't use `tokio::fs` in shard code.
- Don't introduce a thread pool for parallel work. Sharding is
  the parallelism.
- Don't trust user input. All wire input is untrusted.
- Don't `panic!` on user-input errors.

## Workflow

### 1. Pick a sub-task

A task from [`ROADMAP.md`](ROADMAP.md)'s convergence list, or an
open issue. The numbered implementation phases are complete;
remaining work to v1.0 is convergence (see ROADMAP).

### 2. Read the spec

The spec section that section governs the work. Don't infer
from the code if the spec covers it — read the spec.

### 3. Plan

Write `.claude/plans/<short-name>.md` with:
- Scope.
- Spec references.
- Architecture sketch.
- Trade-offs considered.
- Risks / open questions.
- Test plan.
- Commit shape.
- Confirmation questions.

Wait for approval before coding. This isn't ceremony — most
mistakes are caught at the plan step.

### 4. Implement

Follow the plan. Deviations go back through plan → approval.

### 5. Verify

```bash
just verify          # fmt + build + clippy -D warnings + test + check-skills
# or, on macOS / any non-Linux host (the dev container is the supported path):
just docker-verify
```

### 6. Commit

One commit per task. Commit subject:

```
<type>(<scope>): <summary>
```

Types: `feat`, `fix`, `refactor`, `test`, `docs`, `chore`,
`perf`.

**Never** add a `Co-Authored-By: Claude` trailer. The user is
the sole author of these commits.

## Code conventions

- Edition: Rust 2021. MSRV: stable latest minus one.
- Errors: `thiserror` for libs; `anyhow` for binaries. Stable
  error taxonomy per spec §03/10.
- No `unwrap()` outside tests. Use `expect("invariant:
  <reason>")` for unreachable.
- Public APIs: rustdoc + at least one example for non-trivial.
- No `unsafe` outside `crates/brain-storage`. That crate needs
  it for mmap. Every `unsafe` block: `// SAFETY:` comment,
  smallest scope.
- Formatting: rustfmt defaults.
- Lints: clippy default warnings as errors in CI. Pedantic is
  aspirational; not enforced on stubs.
- Naming: snake_case items, CamelCase types — Rust standard.

## Testing

- Unit tests colocated.
- Integration tests in `tests/` per crate.
- Property tests with `proptest` for parsers, allocators,
  recovery.
- Fuzz with `cargo-fuzz` for the wire protocol.
- Loom for concurrency-critical paths.
- Miri for `crates/brain-storage`'s unsafe.
- Chaos tests for recovery (kill-during-operation).
- Benchmarks with `criterion` per phase.

**Runner.** Unit + integration tests run under
[`cargo-nextest`](https://nexte.st) (`just test` — process-per-test,
parallel, faster on the large suite); doctests stay on
`cargo test --doc` (nextest doesn't run them). The dev container bakes
nextest in; install for host use with
`cargo install --locked cargo-nextest`.

**`#[ignore]` is for *gated* tests, not dead ones.** Reserve it for
tests that genuinely can't run in the default suite — they need a real
model (`BRAIN_NER_MODEL_PATH`, `BRAIN_RERANK_MODEL_DIR`), a live API
key, long wall-time (chaos sweeps, load tests), or are
workstation-tuned perf gates. Always give a reason
(`#[ignore = "requires …"]`) and run them on demand with
`cargo nextest run --run-ignored all` (CI runs them in its live tier).
Don't `#[ignore]` a test to silence a failure — fix it or delete it.

New behaviour → new test. Spec change → corresponding test
change.

### Naming tests

A test name is documentation: a reader scanning failures should know
what broke and why **without opening the file**. Two rules.

**Test functions — name the behaviour, not the mechanism.** Use
`<subject>_<scenario>_<expected_outcome>` (a lowercase snake_case
sentence). The name states the *problem the test guards*, and the
body's assertions should read back as that sentence.

- ✅ `forget_memory_not_found_returns_flag_not_error`
- ✅ `per_ip_connection_cap_rejects_beyond_max`
- ✅ `recency_breaks_relevance_ties_toward_recent_event_time`
- ❌ `round_trip` — round-trip of *what*, expecting *what*?
- ❌ `two_hop`, `type_filter`, `exact_match` — a mechanism, not a
  behaviour; relies entirely on the module for meaning.

A bare mechanism word (`round_trip`, `rebuild`, `dispatch`) is only
acceptable with a subject and an outcome attached
(`encode_payload_round_trips_preserving_request_hash`). If you can't
say the outcome in the name, you don't yet know what the test proves.

**Test files — group by the unit/feature under test (Rust norm), and
open with a `//!` header stating the problem-space.** Keep filenames
like `encode.rs`, `recall.rs`, `statement_wire.rs`,
`recovery_integration.rs` — a reader expects `tests/encode.rs` to test
ENCODE. The *"what problem does this cover"* lives in the file's `//!`
header (the invariants it guards + any non-obvious setup), not in the
filename. Every test file — `src` module-test blocks and `tests/*.rs`
integration files alike — gets a `//!` header.

## Reporting bugs / security issues

- Functional bugs: open a GitHub issue with a reproducer.
- Security issues: see [`SECURITY.md`](SECURITY.md).

## License

By contributing, you agree your contribution is licensed under
the project's [Apache 2.0 license](LICENSE).
