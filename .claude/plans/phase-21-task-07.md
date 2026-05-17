# 21.7 — Phase 21 exit (bench + ROADMAP + tag)

Closes phase 21. Three threads:

1. **Restore the `pattern_extract` bench** broken by 21.3's
   async-trait refactor — `Extractor::run` now returns
   `ExtractionFuture` so the existing bench reads `r.items` on
   a `Pin<Box<Future>>` and fails to compile.
2. **Add a `llm_pipeline` criterion bench** covering the spec
   §16/02 §2.8 targets we can hit in CI: cache-hit, cost-budget
   skip, and a mock-client cache-miss (informational; the spec
   target is dominated by real-API latency).
3. **Phase exit:** flip phase-doc checkboxes with explicit
   scope-cut callouts for the §21.7 (Resolver tier 4) + §21.8
   (built-in `brain.preferences_llm`) sub-tasks that were
   reshuffled to phase 22+, update `ROADMAP.md` Phase 21 entry,
   and tag `phase-21-complete`.

## Scope reconciliation: phase doc vs. session sub-tasks

The phase doc (`docs/phases/phase-21-llm-extractor.md`) was
written with the original 21.1–21.9 layout. The actual delivery
under `.claude/plans/phase-21*.md` collapsed several of those
into a tighter set because the LLM cache + schema validation +
retry-once + cost budget all live inside the LlmExtractor impl.

| Phase-doc sub-task | Where it landed |
|---|---|
| 21.1 LLM client trait + backend | `.claude/plans/phase-21-task-01.md` (Anthropic) + `phase-21-task-02.md` (OpenAI) |
| 21.2 LLM extractor worker | `phase-21-task-03.md` (LlmExtractor + async trait) + `phase-21-task-05.md` (server wiring) |
| 21.3 LLM cache | reused phase-17 `LlmCacheDb`; threaded in `phase-21-task-03.md` |
| 21.4 Schema validation | `phase-21-task-03.md` §retry-once + jsonschema |
| 21.5 Retry-once | `phase-21-task-03.md` §retry-once |
| 21.6 Cost budgeting | `phase-21-task-03.md` + `phase-21-task-04.md` (CostBudget materializer translation) |
| **21.7 Resolver tier 4 (LLM)** | **deferred to phase 22+** (entity resolver bypass tier already at tier 3; tier 4 LLM-assisted resolution is post-tantivy) |
| **21.8 Built-in `brain.preferences_llm`** | **deferred — no built-in LLM extractor in v1** (operators declare their own; the system schema ships only pattern + classifier built-ins) |
| 21.9 Tests | `phase-21-task-06.md` (mock-client pipeline + wire smoke) |

The phase doc gets explicit scope-cut notes for 21.7 + 21.8 so a
future reader knows where the work moved.

## Files written / modified

| Path | Change |
|---|---|
| `crates/brain-extractors/benches/pattern_extract.rs` | Drive the async `Extractor::run` via `futures_lite::future::block_on`. Fixes broken-since-21.3 compile + restores real regex-matching cost (currently measures `Box::pin` allocation only). |
| `crates/brain-extractors/benches/llm_pipeline.rs` | New bench: cache-hit, cost-budget skip, mock-client miss. |
| `crates/brain-extractors/Cargo.toml` | `[[bench]] name = "llm_pipeline" harness = false`. |
| `docs/phases/phase-21-llm-extractor.md` | Flip every sub-task `[ ]` → `[x]` with a one-line mapping to the actual landed sub-task. Add a "Scope cuts" section calling out 21.7 + 21.8 deferrals. Mark the "Done-when" checklist. |
| `ROADMAP.md` | New "Phase 21 — LLM extractor ✓" entry following the phase-20 template (one-line / detailed plan link / crates touched / sub-tasks / Exit / Scope cut / Delivered / Deferred / Bench results). |
| `spec/22_extractors/07_open_questions.md` | If any open questions resolved during 21.1–21.6, mark them. (Likely just confirms § Q-llm-1 / Q-llm-2 stayed deferred.) |

## Pattern bench fix

```rust
use futures_lite::future::block_on;
// ...

c.bench_function("pattern_extract 4KiB / 5 regexes", |b| {
    b.iter(|| {
        let r = block_on(ext.run(&ctx, black_box(&mem)));
        black_box(r);
    });
});
```

Same change to the 256B variant. The `print_corpus_summary`
helper also needs `block_on` because it reads `r.items.len()`
post-await.

Phase-20 reported `pattern_extract 4KiB / 5 regexes: 43 µs`.
After the boxed-future overhead we expect a small bump (~5%
from one `Box::pin` allocation per call); still well under the
spec §2.7 100 µs p99 target. Plan checkpoints the new
measurement.

## LLM pipeline bench

Three criterion functions over a scripted mock client. The mock
client returns `Ok(...)` immediately — wall-time is the
LlmExtractor's overhead (cache lookup, JSON serde, projection),
not provider latency.

```rust
// 1. Cache hit. Pre-populates the cache with a known row;
//    every iteration short-circuits at cache_get. Spec target:
//    p50 1 ms / p99 5 ms (§16/02 §2.8).
fn bench_llm_cache_hit(c: &mut Criterion) { ... }

// 2. Cost-budget skip. Tiny `per_call_micro_usd: 1` budget +
//    big prompt → estimator always rejects. Zero LLM calls
//    issued. Spec target: p50 200 µs / p99 1 ms.
fn bench_llm_budget_skip(c: &mut Criterion) { ... }

// 3. Mock-client cache miss (informational). Drives the full
//    pipeline: cache lookup → estimator → client.complete() →
//    schema validation → cache write → projection. No spec
//    target (real-API latency dominates production); reported
//    so a future regression in the in-process overhead is
//    visible.
fn bench_llm_pipeline_mock_miss(c: &mut Criterion) { ... }
```

Setup: one `tempfile::tempdir()` per bench function, one
`LlmCacheDb::open(...)`, one extractor instance reused across
iterations. Memory text fixed to ~256 B so the input-hash cost
is constant.

The bench file uses no real `tokio::runtime` — `futures-lite`'s
`block_on` is enough because the mock client never `await`s
real I/O.

## ROADMAP.md Phase 21 entry

Pattern matches the phase-20 entry already in the file
(lines 406–473):

```markdown
## Phase 21 — LLM extractor ✓

**One-line:** Third extractor tier (LLM) lights up: Anthropic +
OpenAI clients behind a `LlmClient` trait, `LlmExtractor` with
cache (phase-17 `LlmCacheDb`) + schema validation + retry-once
+ per-call cost budget; server-side env-driven router + per-
shard cache wiring; integration tests against a mock client.

**Detailed plan:** [`.claude/plans/phase-21.md`](.claude/plans/phase-21.md)
(per-sub-task plans `phase-21-task-0[0-7].md`).

**Crates touched:** new `brain-llm`; `brain-extractors`,
`brain-ops`, `brain-server`.

**Sub-tasks:** 8 (21.0 spec backfill → 21.7 phase exit).

**Exit:** ...

**Scope cut:** Resolver tier 4 (LLM-assisted entity disambig)
and the built-in `brain.preferences_llm` extractor moved to
phase 22+ — see "Deferred" below.

**Delivered:**
- new `brain-llm` crate (~700 LOC)
- new `crates/brain-extractors/src/llm.rs` (~600 LOC) with cache
  / schema validation / retry-once / cost budget / projection
- `MaterializeDeps` bundle + `materialize_llm_extractor`
- server-side LLM router (`build_llm_deps`) + `OpsContext.llm_cache`
- 11 LlmExtractor unit tests + 11 materializer unit tests + 9
  server-side llm_setup tests + 7 integration tests + 2 wire
  smokes
- spec §22/09 (LLM extractor) + §16/02 §2.8 (LLM perf targets)
- async `Extractor::run` refactor — pattern + classifier wrap
  their sync bodies via `Box::pin(async move { ... })`

**Deferred to later phases:**
- Resolver tier 4 (LLM-assisted entity disambig) — phase 22+
  (§22/07 Q12).
- Built-in `brain.preferences_llm` extractor — post-v1; operators
  declare their own LLM extractors. (Phase-doc §21.8.)
- Live-provider opt-in tests — post-v1.
- Pricing TOML override — post-v1.
- Per-extractor model selection (operator declares model X, router
  serves model Y) — phase 22+ (§22/09 §2 prefix-only routing).
- Live-registry sync on SCHEMA_UPLOAD — phase 22+; uploaded
  extractors observable via EXTRACTOR_LIST but not yet dispatched
  by ENCODE.

**Bench results** (Linux Docker, --quick): filled in after
running the new bench harness.
```

## Phase-21 doc updates

For every `### 21.N` sub-task, append `**Landed in:** <plan
file>` and flip the leading `[ ]` (or add one if absent). For
21.7 (Resolver tier 4) and 21.8 (built-in preferences_llm),
mark `**Deferred:** phase 22+ — see ROADMAP §"Deferred"`.

Add at bottom of file:

```markdown
## Phase exit

- [x] Sub-tasks 21.1-21.6 + 21.9 landed (renumbered to .claude/plans 21.0-21.6).
- [x] 21.7 + 21.8 explicitly scope-cut.
- [x] `cargo bench -p brain-extractors --bench llm_pipeline --quick` runs green; results recorded in ROADMAP §"Bench results".
- [x] `just docker cargo test --workspace` passes (sans the pre-existing pre-handshake PING test).
- [x] Tag `phase-21-complete` cut.
```

## Verification gate

```
# 1. Fix the pattern bench, add llm bench, both compile.
cargo zigbuild --target x86_64-unknown-linux-gnu \
    -p brain-extractors --benches

# 2. Run the new benches (quick mode for CI sanity, full for the
#    numbers we paste into ROADMAP).
just docker cargo bench -p brain-extractors --bench pattern_extract -- --quick
just docker cargo bench -p brain-extractors --bench llm_pipeline   -- --quick

# 3. Full workspace re-verify.
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
just docker cargo test --workspace --lib --bins
```

## Risks

| Risk | Mitigation |
|---|---|
| Pattern bench regresses past 100 µs p99 after the `block_on` round-trip. | Highly unlikely (a Box::pin allocation costs ~10 ns); the spec target has 60+ µs of headroom. Report the new number; if it exceeds 100 µs we surface and don't tag. |
| LLM cache-hit bench misses the 1 ms p50 target. | The mock client makes redb dominate; if the file-backed lookup is too slow we either tune via `Database::create` opts or call out the gap explicitly and tag with the actual measurement plus a follow-up issue. |
| New criterion bench harness pulls in extra dev deps. | None expected — `criterion` + `futures-lite` + `tempfile` + `parking_lot` already on `[dev-dependencies]`. |
| The pre-existing `connection.rs` PING test failure throws off `--workspace` runs. | Already documented in the 21.3/21.5 commit messages; verify-suite excludes it via the existing test annotation or we accept the known-failure for tagging. |

## Out of scope

- Resolver tier 4 (phase-doc 21.7) — phase 22+.
- Built-in `brain.preferences_llm` (phase-doc 21.8) — post-v1.
- Live-registry sync on SCHEMA_UPLOAD — phase 22+ (gap recorded
  in `tests/knowledge_llm_extractor_wire.rs`).
- `brain-llm/benches/anthropic_request.rs` — drop. The mock-driven
  bench in `brain-extractors` exercises the whole pipeline; a
  serialization-only bench on the HTTP client adds little (and
  would require an in-process HTTP server to be honest).

## Single commit

`chore(extractors,docs): 21.7 — phase 21 exit (bench + ROADMAP + tag)`

Plus an annotated tag:

```
git tag -a phase-21-complete -m "Phase 21 — LLM extractor: \
  brain-llm (Anthropic + OpenAI), LlmExtractor with cache + \
  schema validation + retry-once + cost budget, server-side \
  router + per-shard cache, async Extractor trait refactor."
```

## Done criteria

- [ ] `pattern_extract` bench compiles and runs.
- [ ] `llm_pipeline` bench compiles and runs.
- [ ] Numbers pasted into ROADMAP §"Bench results".
- [ ] Phase-21 doc checkboxes flipped; scope-cuts documented.
- [ ] `phase-21-complete` tag cut (annotated) after user
      authorization (memory feedback: tag operations are
      pre-authorized in Brain).
- [ ] Single commit on the feature branch.
