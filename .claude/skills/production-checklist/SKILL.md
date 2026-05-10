---
name: production-checklist
description: Pre-merge checklist — error handling, no unwrap, tracing, tests, no spec drift, no wire bump. Use before merging feature/* into dev or tagging phase-N-complete.
when-to-use: |
  Triggers:
    - User says "ready to merge" / "ship it" / "production-ready?"
    - About to merge feature/<topic> → dev
    - Pre-phase-exit: walking the phase exit checklist
    - PR review pre-flight
spec-refs:
  - AUTONOMY.md
  - CLAUDE.md
---

# Production Checklist

## When to use

Before merging a feature branch into `dev` (per AUTONOMY §6's `feature → dev → main` flow), or before tagging `phase-N-complete`. Catches the easy-to-miss prod-readiness items.

## The checklist

Run each as a quick pass on the diff (or the branch since it diverged from `dev`).

### Correctness

- [ ] **No `.unwrap()` outside tests.** `grep -nE 'unwrap\(\)' <changed>` — every hit is in a `#[test]` or paired with a `// SAFETY` / `expect("invariant: ...")`.
- [ ] **No `panic!` outside genuinely-unreachable paths.** Same approach.
- [ ] **No `todo!()`, `unimplemented!()`, or `unreachable!()` without a `// TODO(phase-N): <what>` comment.**
- [ ] **`cargo clippy --workspace --all-targets -- -D warnings`** clean.
- [ ] **`cargo test --workspace --all-targets`** clean.
- [ ] **`cargo test --workspace --doc`** clean.
- [ ] **`cargo fmt --all -- --check`** clean.
- [ ] **`just check-skills`** clean.

### Spec alignment

- [ ] **No spec drift.** Run `brain-spec-invariant` for any MUST clauses in the touched sections.
- [ ] **No wire-format drift.** Run `brain-protocol-version-bump` if `crates/brain-protocol/src/{header,opcode,frame,request,response,error}.rs` changed.
- [ ] **CLAUDE.md §5 invariants hold.** Run `brain-invariants` for any storage/ops/workers/server changes.
- [ ] **No edits to `spec/`.** AUTONOMY §2 forbids; spec changes require user.

### Observability

- [ ] **`tracing` spans on every public op.** New op → new span at info or debug. Use structured fields, not formatted strings.
- [ ] **Errors logged at the right level.** Spec §03/10 §12 maps category → level (Validation/NotFound/Conflict = INFO; Auth = WARN; Protocol = WARN; ResourceExhausted = WARN; Internal/Unavailable = ERROR).
- [ ] **No PII in logs or error messages.** Don't log token contents or raw query data.
- [ ] **Metrics emitted** for new code paths (latency histograms, error counters). Phase 11 wires this fully; for now confirm hooks exist.

### Tests

- [ ] **Golden case covered.** A test exercises the happy path.
- [ ] **At least one error path covered.** Returning `Err` is half the contract.
- [ ] **Edge cases** if the spec calls them out: empty input, max input, boundary values.
- [ ] **Property tests** where the input space is large (parsers, allocators, recovery).
- [ ] **Round-trip tests** for any encode/decode pair.

### Code quality

- [ ] **Public APIs have rustdoc.** Non-trivial items have at least one example.
- [ ] **Module has `//!` doc comment** explaining scope and which spec section it implements.
- [ ] **No commented-out code.** Either delete or convert to a `// TODO(phase-N): <what>` with a tracking comment.
- [ ] **No `// removed`, `// renamed`, `// re-export for compat`** noise (CLAUDE.md "Tone and style").
- [ ] **Imports sorted, no unused** (clippy enforces).

### Performance

- [ ] **No allocation in measured hot paths.** Use the `rust-perf` skill if uncertain.
- [ ] **No `Mutex` across `.await`.**
- [ ] **No new thread pool** (sharding is the parallelism — CLAUDE.md §9).
- [ ] **No `Send + Sync` on per-shard types.**
- [ ] **No `tokio::*` inside a Glommio shard.**
- [ ] **Benchmarks** still pass against spec §16/02 latency targets (Phase 11 enforces; for now spot-check via `just bench`).

### Workflow

- [ ] **Commit message format matches AUTONOMY §5** (`<phase>.<task>: <imperative summary>` + Refs).
- [ ] **Plan exists** in `.claude/plans/` for substantial sub-tasks (AUTONOMY §21).
- [ ] **Phase doc updated** — `[ ]` → `[x]` for the completed sub-task.
- [ ] **No `CONTEXT.md`** at the repo root (a stop-and-surface marker).

## Output format

```
PRODUCTION CHECKLIST — feature/<topic>

Correctness        ✓
Spec alignment     ✓
Observability      ✗  No tracing span in brain-ops/src/encode.rs::handle
Tests              ✓
Code quality       ✓
Performance        ✓
Workflow           ✓

Findings:
- brain-ops/src/encode.rs:42  -- public op `handle` lacks a tracing span
- brain-ops/src/encode.rs:68  -- error logged at debug level; should be warn for OutOfSlots

Recommend: fix the two findings, then re-run.
```

If all pass: "All checks green; safe to merge."
If any fail: list the findings; do not autofix without confirmation.

## Examples

### Golden — pre-merge for `feature/brain-protocol`

Walks the checklist; finds:
- `cargo test` green (96 tests).
- `clippy` clean.
- `fmt` clean.
- `check-skills` clean.
- No spec drift (verified phase doc footnotes for endianness).
- Wire-bump check N/A (we're at v1, no opcode renumbering).
- Public APIs documented.
- Tests cover golden + 7 frame rejection cases + property tests.

Verdict: green; merge to `dev`.

### Counter — silent unwrap

Diff adds `let cfg = Config::load().unwrap();` to a non-test path. Reject; replace with `?` propagation or `expect("invariant: config validated at startup")`.

## Cross-references

- AUTONOMY §6 (branching), §5 (commit format), §21 (plan-first).
- CLAUDE.md §5 (invariants), §7 (conventions), §9 (anti-patterns), §10 (testing).
- `brain-invariants`, `brain-spec-invariant`, `brain-protocol-version-bump` — companion audits.
- `verify` (built-in) — runs the cargo-level checks.

## Source / Adaptations

Project-local.
