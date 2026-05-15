# Sub-task 15.5 — Substrate-only mode regression test

> Per-sub-task plan. Plan-first convention. Last sub-task of Phase 15.

## Goal

Add an end-to-end test that proves the substrate primitives (ENCODE,
RECALL, FORGET) continue to work after Phase 15's storage extensions
landed. Pre-conditions:

- The 25 new knowledge-layer redb tables exist (empty) inside
  `metadata.redb` (15.1).
- The WAL kind discriminator accepts the 12 new knowledge kinds, but
  the substrate writer never produces them (15.2).
- `Shard::open()` creates the two tantivy directories and the LLM
  cache redb file (15.3 + 15.4).

After 15.5: a server brought up against this storage layout runs the
substrate primitives **end-to-end** with no schema declared, and the
test asserts the data round-trips correctly. On green: tag
`phase-15-complete`.

## What this test is and isn't

**It is** the schema-optional regression gate AUTONOMY §23 binds:
"when knowledge mode is off, substrate primitives perform identically
to a pre-knowledge-layer deployment." We prove the *functional* half
of that bargain (correctness) in 15.5.

**It is not** the latency-threshold gate. Tight P50/P99 ≤ 110% of
baseline assertions need:
- A baseline file (there isn't one today — Phase 13's criterion
  results live in target/ caches, not in `docs/performance/baselines-
  <date>.md`).
- Quiet reference hardware (CI runs on shared GitHub runners; ±30%
  jitter routine).

Both of those are operator-cadence concerns and belong in Phase 14
(substrate acceptance), not 15.5. 15.5 captures timings and logs
them for visibility, but only fails on absurdly large values
(e.g. P99 ≥ 500 ms — indicating something is fundamentally broken,
not noise).

## Reading list

1. `spec/16_benchmarks_acceptance/02_latency_targets.md` — the
   reference targets we cite (loosely) in the smoke check.
2. `crates/brain-server/tests/sdk_e2e.rs` — the test pattern to
   mirror.
3. `crates/brain-server/tests/support_harness/mod.rs` — the
   in-process server scaffold (`start()`).
4. `docs/performance/README.md` — confirms no committed
   `baselines-<date>.md` exists; tight thresholds require operator
   runs.
5. `AUTONOMY.md` §23 — knowledge-layer scope, schema-optional binding.

## Pre-flight findings

### F-1 — No baseline file to diff against

`docs/performance/baselines-*.md` doesn't exist. Phase 13's criterion
runs produce in-tree `target/criterion/*` reports but no markdown
rollup committed. Without a baseline:

- We **cannot** assert "within 110% of baseline" today.
- Phase 14 introduces the baseline file as part of substrate
  acceptance; that's where tight thresholds belong.

15.5 ships the *functional* regression and a *loose* latency smoke.
Tight thresholds are deferred to Phase 14 with a tracking note.

### F-2 — CI hardware is shared

GitHub-runner / shared CI machines see ±30% latency jitter per spec
§16/13. A tight threshold (110% of baseline) would flap in CI.
The phase doc's intent was P99 ≤ 110% of *baseline*, not absolute
spec §16/02 numbers. Same noise problem either way.

### F-3 — `support_harness::start()` is the right scaffold

Brings up N shards + connection listener + admin HTTP on ephemeral
ports inside the test process. Used by `e2e.rs`, `sdk_e2e.rs`,
`cli_e2e.rs`. Returns a `Server` with bound addresses. Linux-only
(`#![cfg(target_os = "linux")]`).

### F-4 — Test file location

`crates/brain-server/tests/knowledge_compat.rs` — matches the other
e2e test pattern. The plan-15 doc mentioned `tests/knowledge_compat.rs`
as a *workspace-level* path; that was bundle-shorthand. Project
convention is per-crate `tests/` dirs.

### F-5 — What the support harness expects

Looking at `support_harness/mod.rs` and `sdk_e2e.rs`, the canonical
mounts a test file declares at its crate root are documented in the
harness header (`MOUNTS` doc-comment): admin, config, connection,
dispatch, metrics, routing, shard, subscribe, tls. Our test mirrors
that boilerplate.

## Design decisions

### D1 — Test what, exactly

1. **ENCODE round-trip**: encode N=50 memories with deterministic
   text; assert each returns a non-NULL `MemoryId` with the expected
   slot count.
2. **RECALL round-trip**: recall on a substring of the encoded text;
   assert at least one result with `score > 0.0`.
3. **FORGET round-trip**: soft-forget one of the encoded memories;
   re-recall the same cue; assert the forgotten memory does NOT
   appear in results.
4. **Knowledge-layer tables stay empty**: open `metadata.redb`
   directly via `redb::Database::open` after the workload completes;
   assert every knowledge-layer table is empty (zero entries).
5. **WAL contains zero knowledge frames**: open the WAL reader on
   the test's shard dir; iterate every record; assert
   `!kind.is_knowledge()` for all of them.
6. **`llm_cache.redb` exists but is empty**: open the cache; assert
   both `llm_responses` and `llm_response_ttl` tables exist and have
   zero rows.

(1)–(3) prove the substrate's hot paths still work.
(4)–(6) prove the knowledge layer truly stays dormant when no schema
is declared — the AUTONOMY §23 binding.

### D2 — Latency smoke (loose, not gate-tight)

After (1)–(3) complete, log per-op p50/p99 of ENCODE+RECALL via
`tracing::info!`. Assert only the *upper bound* `p99 < 500 ms` — a
backstop against catastrophic regressions; well above any plausible
noise.

Tight `p99 ≤ 110% of baseline` is **out of scope**. Recorded as
Phase 14 follow-up in the test's doc-comment.

### D3 — Test does NOT declare a schema

`SCHEMA_UPLOAD` opcode doesn't exist yet (phase 19). The test relies
on the *absence* of any call that would activate the knowledge layer.
Once phase 19 ships, an additional test variant ("schema declared,
then schema removed → substrate-only again") joins this file. That's
a phase-24 acceptance concern, not 15.5.

### D4 — Workload size

- 50 ENCODEs (~5 ms each = 250 ms total).
- 20 RECALLs (~10 ms each = 200 ms total).
- 5 FORGETs.

Total test runtime target: <30 s with server startup + teardown.
Fast enough for CI; large enough to populate WAL + arena + HNSW.

### D5 — Tag `phase-15-complete` after this commit

On green, the workflow is:

```bash
git commit -m "test(server): 15.5 — knowledge_compat substrate-only regression"
git tag phase-15-complete
```

The tag closes Phase 15. Phase 16 (Entity layer) can then begin —
with its own per-task plan-first cadence.

## File plan

- `crates/brain-server/tests/knowledge_compat.rs` — **new** test
  file, ~250 lines including the canonical harness mounts.

No source-file changes — 15.5 is pure test addition.

## Done-when

- `cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests`
  clean.
- Test file compiles with the same `#[path]` mount boilerplate as
  other e2e tests.
- Test logic exercises ENCODE + RECALL + FORGET; asserts knowledge
  tables remain empty; asserts WAL contains no knowledge frames;
  asserts llm_cache.redb opens and is empty.
- One commit: `test(server): 15.5 — knowledge_compat substrate-only regression`.
- After commit: `git tag phase-15-complete`.

## Risk register

| Risk | Mitigation |
|---|---|
| Test flakes on shared CI hardware | Loose-only thresholds (P99 < 500 ms backstop); no tight-baseline gating. Operator-cadence tightening lands in Phase 14. |
| `support_harness::start()` boots slowly on macOS-via-container | Test is gated `#[cfg(target_os = "linux")]` like the other e2e tests. Local dev uses dev container; CI runs natively. |
| Knowledge tables are technically empty but the test fails to *prove* that | Open `metadata.redb` directly via `redb::Database::open` (not the typed `MetadataDb` wrapper) and iterate each new table. Concrete count assertion: `iter().count() == 0`. |
| WAL contains records the test author didn't expect (e.g. checkpoint) | Filter on `!is_knowledge()` rather than asserting an exact total count. Substrate records (Encode/Forget/CheckpointBegin/CheckpointEnd/etc.) are *expected*; knowledge records must be zero. |
| Test depends on a phase-19-only opcode by accident | Carefully avoid `SCHEMA_UPLOAD`, `ENTITY_CREATE`, etc. Only use substrate opcodes (ENCODE / RECALL / FORGET / LINK / UNLINK / SUBSCRIBE). |
| Future spec edit changes a knowledge-table name | Test uses the named constants from `tables/knowledge/`; rename ripples in one place. |

## Open questions for your approval

1. **Latency-gate scope (D2)** — loose P99 < 500 ms backstop only,
   defer tight `≤110% of baseline` to Phase 14? **Recommended: yes.**
   No baseline file exists to diff against; CI noise makes tight
   thresholds flap.
2. **Test file location (F-4)** —
   `crates/brain-server/tests/knowledge_compat.rs` per project
   convention, not workspace-level `tests/knowledge_compat.rs`?
   **Recommended: per-crate.** Matches the other e2e tests.
3. **Workload size (D4)** — 50 / 20 / 5 (ENCODE / RECALL / FORGET)
   adequate, or do you want larger volumes (1000 / 500 / 100) to
   stress the WAL more? **Recommended: 50 / 20 / 5.** Larger
   volumes risk CI timeouts and don't add coverage; what we're
   testing is *correctness of the substrate hot paths in the
   presence of new knowledge-layer storage structures*, not
   throughput.
4. **Phase-15-complete tag on the same commit?** Tag separately
   after the commit lands, or include the tag step in the commit
   workflow doc? **Recommended: tag separately.** A bad
   knowledge_compat test post-commit could surface late; tagging
   after one CI cycle protects the tag.

## Workflow

On your nod: implement, run
`cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests`,
commit as `test(server): 15.5 — knowledge_compat substrate-only regression`.
Then await your call on `git tag phase-15-complete`.
